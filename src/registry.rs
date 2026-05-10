//! Program-wide registry of callable items.
//!
//! Three kinds of "callable" coexist in a Lake program:
//!
//!   * **Built-in runtime functions** (`@rt(name)`) — declared by the
//!     compiler.  Their signatures live in [`crate::rt_registry`] and are
//!     loaded into [`ProgramRegistry::with_rt`] on construction.
//!   * **User machines** (`name is { ... }`) — defined in `.lake` files.
//!     Each module owns its machines; visibility (`pub`) controls whether
//!     they appear to importers.
//!   * **Foreign declarations** (`@ffi(name { params } { ret })`) — typed
//!     externs that the user supplies.  Live in the declaring module.
//!
//! The registry is structured for **multi-module programs from day one**:
//! a single `.lake` file is just a registry with one module entry.  When a
//! [`crate::loader::ModuleLoader`] eventually feeds in transitively-loaded
//! files, no shape change here is needed — we only add modules.
//!
//! Lifetime parameter `'src` ties all borrowed identifiers to the lifetime
//! of the underlying source strings (managed by the loader / caller).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use chumsky::span::Spanned;

use crate::api::ast::{Branch, Directive, Import, Item, Machine, MachineItem, Type};
use crate::error_handle::{LakeError, LakeErrors};
use crate::loader::ParsedProgram;

// ── primitive types ─────────────────────────────────────────────────────────

/// A function-like signature: parameter types in order, plus a return type.
///
/// Types are stored as plain `String` so the same `Signature` value can be
/// produced both from the static rt table (`&'static str` literals) and from
/// AST walks (where types come from `Type::Display`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub params: Vec<String>,
    pub ret: String,
}

/// Stable handle for a module within a single program.  Allocated by
/// [`ProgramRegistry::register_module`] in load order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModuleId(pub u32);

impl ModuleId {
    /// Reserved id for the program entry module.  Always present even when
    /// no `+import` directives appear in source.
    pub const ROOT: ModuleId = ModuleId(0);
}

/// Dotted module path, e.g. `core.io` is `["core", "io"]`.
///
/// The empty path denotes the program root (the entry file's module).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ModulePath(pub Vec<String>);

impl ModulePath {
    /// The implicit module of the entry file.
    pub fn root() -> Self {
        ModulePath(Vec::new())
    }

    /// Append a single segment, returning a new path.
    pub fn join_segment(&self, seg: &str) -> Self {
        let mut v = self.0.clone();
        v.push(seg.to_string());
        ModulePath(v)
    }

    /// Render the path as a relative filesystem path: `core.io` → `core/io.lake`.
    pub fn to_filesystem(&self) -> PathBuf {
        let mut p = PathBuf::new();
        for seg in &self.0 {
            p.push(seg);
        }
        p.set_extension("lake");
        p
    }

    /// True for the root module (`ModulePath::root`).
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Render in source form: `core.io`.  Empty path renders as `<root>`.
    pub fn display(&self) -> String {
        if self.0.is_empty() {
            "<root>".to_string()
        } else {
            self.0.join(".")
        }
    }
}

// ── per-module entries ──────────────────────────────────────────────────────

/// A `name is { ... }` definition belonging to some module.  Carries every
/// branch's parameter signature so callers can verify arity and types.
#[derive(Debug, Clone)]
pub struct MachineEntry<'src> {
    /// Source-borrowed machine name.
    pub name: &'src str,
    /// Module that owns this machine.
    pub module: ModuleId,
    /// `pub` makes this entry visible to other modules' imports.
    pub is_pub: bool,
    /// Per-branch parameter signatures.  Each branch's `ret` is `"pid"`
    /// because spawning a machine returns a process id; differentiation
    /// happens via parameter types when call-time dispatch fires.
    pub branches: Vec<Signature>,
}

impl<'src> MachineEntry<'src> {
    /// Convenience: the spawn return type.  Today always `"pid"`; a future
    /// typed-PID extension would specialise this per machine.
    pub fn spawn_ret(&self) -> &'static str {
        "pid"
    }

    /// True when at least one branch matches the given parameter count.
    /// Coarse check used by typeck before going branch-by-branch.
    pub fn has_arity(&self, n: usize) -> bool {
        self.branches.iter().any(|b| b.params.len() == n)
    }
}

/// All callable items declared in a single `.lake` file, plus the local
/// `+import` bindings that bring foreign names into scope.
#[derive(Debug, Default)]
pub struct ModuleScope<'src> {
    /// Module path (e.g. `["core", "io"]`).
    pub path: ModulePath,
    /// On-disk path of the source file (for diagnostics).  Empty for
    /// modules created without a backing file (rare; tests).
    pub source_path: Option<PathBuf>,
    /// Machines defined in this file, keyed by source name.
    pub machines: HashMap<&'src str, MachineEntry<'src>>,
    /// FFI declarations defined in this file.  Keyed by owned String so
    /// the resolver can populate without juggling source lifetimes for
    /// what is fundamentally compile-time-only metadata.
    pub ffi_fns: HashMap<String, Signature>,
    /// `const` declarations defined in this file, keyed by source name.
    pub consts: HashMap<&'src str, ConstEntry>,
    /// Aliases brought into scope by `+import` lines.
    /// Key = local binding name (alias or last path segment).
    /// Value = (target module, target item's source name).
    pub imports: HashMap<String, ImportBinding>,
}

/// Reduced form of a `const` decl, kept on the registry so codegen
/// can inline the value at every use site.  Values are extracted from
/// the literal AST node once, at populate time.
#[derive(Debug, Clone)]
pub struct ConstEntry {
    pub vis: bool,
    pub ty: String,
    pub value: ConstValue,
}

#[derive(Debug, Clone)]
pub enum ConstValue {
    Int(i64),
    Str(String),
    Bool(bool),
    /// The atom's source name; codegen folds it via `pure_expr::atom_id`.
    Atom(String),
}

/// One resolved row from a `+import` directive.
#[derive(Debug, Clone)]
pub struct ImportBinding {
    pub target_module: ModuleId,
    /// Item name in the *target* module's source (not the alias).
    pub target_item: String,
}

// ── top-level registry ──────────────────────────────────────────────────────

/// Aggregate of every module that participates in the current compilation,
/// plus the global rt-fn table.
#[derive(Debug)]
pub struct ProgramRegistry<'src> {
    /// Built-in runtime functions.  Populated from
    /// [`crate::rt_registry::builtin_signatures`] in `with_rt`.
    pub rt_fns: HashMap<String, Signature>,
    /// All loaded modules indexed by `ModuleId` (the index doubles as the
    /// id, so `modules[id.0 as usize]` is `O(1)`).
    pub modules: Vec<ModuleScope<'src>>,
    /// Module path lookup table for `+import` resolution.
    pub by_path: HashMap<ModulePath, ModuleId>,
    /// The module currently being resolved / type-checked.  Drives bare-name
    /// lookup order (rt → current → imports → other-modules-only-via-path).
    pub current: ModuleId,
}

impl<'src> ProgramRegistry<'src> {
    /// Construct a registry pre-populated with the built-in runtime
    /// functions and an empty root module.
    pub fn with_rt() -> Self {
        let mut rt_fns = HashMap::new();
        for (name, sig) in crate::rt_registry::builtin_signatures() {
            rt_fns.insert(name.to_string(), sig);
        }
        let root = ModuleScope {
            path: ModulePath::root(),
            ..ModuleScope::default()
        };
        let mut by_path = HashMap::new();
        by_path.insert(ModulePath::root(), ModuleId::ROOT);

        ProgramRegistry {
            rt_fns,
            modules: vec![root],
            by_path,
            current: ModuleId::ROOT,
        }
    }

    /// Register a fresh module and return its id.  If a module at this
    /// path is already known, returns the existing id (modules are
    /// uniqued by path).
    pub fn register_module(&mut self, path: ModulePath, source_path: Option<PathBuf>) -> ModuleId {
        if let Some(&id) = self.by_path.get(&path) {
            // Update source_path if not yet set.
            let scope = &mut self.modules[id.0 as usize];
            if scope.source_path.is_none() {
                scope.source_path = source_path;
            }
            return id;
        }
        let id = ModuleId(self.modules.len() as u32);
        self.modules.push(ModuleScope {
            path: path.clone(),
            source_path,
            ..ModuleScope::default()
        });
        self.by_path.insert(path, id);
        id
    }

    /// Switch the "current module" used for bare-name resolution.
    /// Resolver / typeck call this as they walk into each module's items.
    pub fn set_current(&mut self, id: ModuleId) {
        debug_assert!((id.0 as usize) < self.modules.len());
        self.current = id;
    }

    /// Mutable access to a specific module's scope (used by resolver to
    /// register machines / ffi / imports while walking the AST).
    pub fn module_mut(&mut self, id: ModuleId) -> &mut ModuleScope<'src> {
        &mut self.modules[id.0 as usize]
    }

    /// Read access to a specific module's scope.
    pub fn module(&self, id: ModuleId) -> &ModuleScope<'src> {
        &self.modules[id.0 as usize]
    }

    /// Resolve a bare identifier in the current module.  Lookup order:
    ///   1. rt fns (global)
    ///   2. machine in current module
    ///   3. ffi in current module
    ///   4. import binding in current module → resolve into target module's
    ///      machine or ffi
    pub fn resolve_bare(&self, name: &str) -> Option<Resolution<'_>> {
        if let Some(sig) = self.rt_fns.get(name) {
            return Some(Resolution::Rt(sig));
        }
        let cur = self.module(self.current);
        if let Some(m) = cur.machines.get(name) {
            return Some(Resolution::Machine(m));
        }
        if let Some(sig) = cur.ffi_fns.get(name) {
            return Some(Resolution::Ffi(sig));
        }
        if let Some(c) = cur.consts.get(name) {
            return Some(Resolution::Const(c));
        }
        if let Some(binding) = cur.imports.get(name) {
            return self.resolve_in_module(binding.target_module, &binding.target_item);
        }
        None
    }

    /// Resolve a bare name against a specific module's scope, without
    /// mutating `self.current`.  Same lookup chain as
    /// [`Self::resolve_bare`], but the caller picks which module's scope
    /// to use — convenient for multi-pass walks that visit multiple
    /// modules through an immutable registry borrow.
    pub fn resolve_bare_in(&self, current: ModuleId, name: &str) -> Option<Resolution<'_>> {
        if let Some(sig) = self.rt_fns.get(name) {
            return Some(Resolution::Rt(sig));
        }
        let cur = self.module(current);
        if let Some(m) = cur.machines.get(name) {
            return Some(Resolution::Machine(m));
        }
        if let Some(sig) = cur.ffi_fns.get(name) {
            return Some(Resolution::Ffi(sig));
        }
        if let Some(c) = cur.consts.get(name) {
            return Some(Resolution::Const(c));
        }
        if let Some(binding) = cur.imports.get(name) {
            return self.resolve_in_module(binding.target_module, &binding.target_item);
        }
        None
    }

    /// Resolve `name` in a specific module.  Used by inline qualified paths
    /// (`core:io:writer`) and to follow import bindings.  Skips imports of
    /// the target module — those are private to that module.
    pub fn resolve_in_module(&self, id: ModuleId, name: &str) -> Option<Resolution<'_>> {
        let scope = self.module(id);
        if let Some(m) = scope.machines.get(name) {
            return Some(Resolution::Machine(m));
        }
        if let Some(sig) = scope.ffi_fns.get(name) {
            return Some(Resolution::Ffi(sig));
        }
        if let Some(c) = scope.consts.get(name) {
            return Some(Resolution::Const(c));
        }
        None
    }

    /// Resolve a dotted module path to its id (`core.io` → ModuleId of
    /// `core/io.lake`).  Returns `None` if no such module is registered.
    pub fn module_id_for_path(&self, path: &ModulePath) -> Option<ModuleId> {
        self.by_path.get(path).copied()
    }

    /// Walk a [`ParsedProgram`] and populate the registry with every
    /// module's machines, ffi declarations, and import bindings.
    ///
    /// Two passes:
    ///   1. Register every module so its `ModuleId` exists.
    ///   2. Walk each module's items, registering machines / ffi and
    ///      resolving `+import` directives into local-name bindings.
    ///
    /// Borrowed identifiers (`MachineEntry::name`) keep the lifetime of
    /// the underlying parsed source.
    pub fn populate_from(
        &mut self,
        program: &ParsedProgram<'src>,
    ) -> Result<(), LakeErrors> {
        // Pass 1: ensure every loaded module has an id and a source path.
        for module in &program.modules {
            self.register_module(
                module.module_path.clone(),
                Some(module.source_path.clone()),
            );
        }

        // Pass 2: register items + imports.
        let mut errors: Vec<LakeError> = Vec::new();
        for module in &program.modules {
            let id = self
                .by_path
                .get(&module.module_path)
                .copied()
                .expect("module registered in pass 1");
            for item in &module.ast {
                match &item.inner {
                    Item::Machine(m) => {
                        let entry = build_machine_entry(&m.inner, id);
                        self.module_mut(id)
                            .machines
                            .insert(m.inner.ident.inner.0, entry);
                    }
                    Item::Directive(d) if d.inner.name.inner.0 == "ffi" => {
                        match build_ffi_signature(&d.inner) {
                            Ok((name, sig)) => {
                                self.module_mut(id).ffi_fns.insert(name, sig);
                            }
                            Err(e) => errors.push(e),
                        }
                    }
                    Item::Directive(_) => {
                        // `@rt(...)` and friends — handled out of band by
                        // the static rt registry.  Nothing to record per
                        // module.
                    }
                    Item::Const(c) => {
                        match build_const_entry(&c.inner) {
                            Ok((name, entry)) => {
                                self.module_mut(id)
                                    .consts
                                    .insert(name, entry);
                            }
                            Err(e) => errors.push(e),
                        }
                    }
                    Item::Import(rc) => {
                        if let Err(mut errs) =
                            register_import_bindings(rc, &[], id, self)
                        {
                            errors.append(&mut errs);
                        }
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(LakeErrors::new(errors))
        }
    }
}

// ── helpers for populate_from ──────────────────────────────────────────────

fn build_const_entry<'src>(
    decl: &crate::api::ast::ConstDecl<'src>,
) -> Result<(&'src str, ConstEntry), LakeError> {
    use crate::api::expr::Expr;
    let name = decl.ident.inner.0;
    let span = decl.value.span;
    let (ty, value) = match &decl.value.inner {
        Expr::Num(s, _) => {
            let v = crate::api::expr::parse_int_literal(s).map_err(|_| {
                LakeError::new(format!("invalid integer literal `{s}`"), span).code("E010")
            })?;
            ("i64".to_string(), ConstValue::Int(v))
        }
        Expr::String(s, _) => ("str".to_string(), ConstValue::Str(s.to_string())),
        Expr::Bool(b) => ("bool".to_string(), ConstValue::Bool(*b)),
        Expr::Atom(s) => ("atom".to_string(), ConstValue::Atom(s.to_string())),
        other => {
            return Err(LakeError::new(
                format!(
                    "const RHS must be a literal (int, str, bool, atom); got `{:?}`",
                    other
                ),
                span,
            )
            .code("E011"));
        }
    };
    Ok((
        name,
        ConstEntry {
            vis: decl.vis,
            ty,
            value,
        },
    ))
}

fn build_machine_entry<'src>(machine: &Machine<'src>, module: ModuleId) -> MachineEntry<'src> {
    let branches: Vec<Signature> = machine
        .items
        .iter()
        .filter_map(|item| match &item.inner {
            MachineItem::Branch(b) => Some(branch_signature(b)),
            _ => None,
        })
        .collect();
    MachineEntry {
        name: machine.ident.inner.0,
        module,
        is_pub: machine.vis,
        branches,
    }
}

fn branch_signature(branch: &Branch<'_>) -> Signature {
    let params: Vec<String> = branch
        .patterns
        .iter()
        .filter(|p| !p.inner.is_wildcard())
        .map(|p| p.inner.ty.inner.to_string())
        .collect();
    Signature {
        params,
        ret: "pid".to_string(),
    }
}

/// Extract `(name, signature)` from an `@ffi(name { params } { ret })`
/// directive.  Returns a diagnostic on malformed shape.
fn build_ffi_signature(directive: &Directive<'_>) -> Result<(String, Signature), LakeError> {
    let args = &directive.args;
    if args.len() != 3 {
        return Err(LakeError::new(
            format!(
                "@ffi expects `name {{ params }} {{ ret }}` (3 arguments), \
                 got {}",
                args.len()
            ),
            directive.name.span,
        )
        .code("F001"));
    }
    let Spanned { inner: Type::Named(name), .. } = &args[0] else {
        return Err(LakeError::new(
            "@ffi: first argument must be the function name",
            args[0].span,
        )
        .code("F002"));
    };
    let Type::Struct(params) = &args[1].inner else {
        return Err(LakeError::new(
            "@ffi: second argument must be `{ param_types ... }`",
            args[1].span,
        )
        .code("F003"));
    };
    let Type::Struct(ret) = &args[2].inner else {
        return Err(LakeError::new(
            "@ffi: third argument must be `{ ret_type }`",
            args[2].span,
        )
        .code("F004"));
    };
    let params: Vec<String> = params.iter().map(|p| p.inner.to_string()).collect();
    // Single-element `{ ret }` — render the type; fall back to `{}` for
    // empty `{}` which means "no return value" by convention.
    let ret = ret
        .first()
        .map(|t| t.inner.to_string())
        .unwrap_or_else(|| "{}".to_string());
    Ok((
        name.inner.0.to_string(),
        Signature { params, ret },
    ))
}

/// Walk an `Import` tree and add bindings to the named module's `imports`
/// table.  Each leaf produces one binding; the local name is the alias if
/// present, otherwise the leaf segment.
fn register_import_bindings<'src>(
    node: &Spanned<Rc<RefCell<Import<'src>>>>,
    prefix: &[String],
    current_module: ModuleId,
    registry: &mut ProgramRegistry<'src>,
) -> Result<(), Vec<LakeError>> {
    let mut errors: Vec<LakeError> = Vec::new();
    walk_import(node, prefix, current_module, registry, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn walk_import<'src>(
    node: &Spanned<Rc<RefCell<Import<'src>>>>,
    prefix: &[String],
    current_module: ModuleId,
    registry: &mut ProgramRegistry<'src>,
    errors: &mut Vec<LakeError>,
) {
    let import_span = node.span;
    let borrowed = node.inner.borrow();
    match &*borrowed {
        Import::Import(ident, Some(next), _alias) => {
            let mut new_prefix = prefix.to_vec();
            new_prefix.push(ident.inner.0.to_string());
            // `next` is an Rc held by `borrowed`'s Import variant; clone
            // the Spanned so we can drop the borrow before recursing.
            let next_cloned = next.clone();
            drop(borrowed);
            walk_import(&next_cloned, &new_prefix, current_module, registry, errors);
        }
        Import::Import(ident, None, alias) => {
            if prefix.is_empty() {
                errors.push(
                    LakeError::new(
                        format!(
                            "import `+{}` is too short — at least \
                             `+module.item` is required",
                            ident.inner.0
                        ),
                        import_span,
                    )
                    .code("M004"),
                );
                return;
            }
            let module_path = ModulePath(prefix.to_vec());
            let target_id = match registry.module_id_for_path(&module_path) {
                Some(id) => id,
                None => {
                    errors.push(
                        LakeError::new(
                            format!(
                                "import target module `{}` was not loaded",
                                module_path.display()
                            ),
                            import_span,
                        )
                        .code("M005"),
                    );
                    return;
                }
            };
            let local_name = alias
                .as_ref()
                .map(|a| a.inner.0.to_string())
                .unwrap_or_else(|| ident.inner.0.to_string());
            registry.module_mut(current_module).imports.insert(
                local_name,
                ImportBinding {
                    target_module: target_id,
                    target_item: ident.inner.0.to_string(),
                },
            );
        }
        Import::MultiImport(entries) => {
            // Snapshot then drop borrow so we can call walk_import which
            // also borrows the registry mutably.
            let entries_clone: Vec<_> = entries.clone();
            drop(borrowed);
            for entry in &entries_clone {
                walk_import(entry, prefix, current_module, registry, errors);
            }
        }
    }
}

/// Result of [`ProgramRegistry::resolve_bare`] / [`resolve_in_module`].
#[derive(Debug, Clone, Copy)]
pub enum Resolution<'a> {
    Rt(&'a Signature),
    Machine(&'a MachineEntry<'a>),
    Ffi(&'a Signature),
    /// Compile-time constant.  Use sites look up the entry to inline its
    /// value at codegen — never a call.
    Const(&'a ConstEntry),
}

impl<'a> Resolution<'a> {
    /// The return type a caller would see from this callable.
    pub fn return_type(&self) -> &str {
        match self {
            Resolution::Rt(sig) | Resolution::Ffi(sig) => &sig.ret,
            Resolution::Machine(_) => "pid",
            Resolution::Const(c) => &c.ty,
        }
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_path_filesystem_mapping() {
        let p = ModulePath(vec!["core".to_string(), "io".to_string()]);
        assert_eq!(p.to_filesystem(), Path::new("core/io.lake"));
    }

    #[test]
    fn root_module_is_pre_registered() {
        let reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        assert_eq!(reg.current, ModuleId::ROOT);
        assert!(reg.module_id_for_path(&ModulePath::root()).is_some());
    }

    #[test]
    fn rt_lookup_works() {
        let reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        let r = reg.resolve_bare("rt_listen_tcp").expect("rt_listen_tcp present");
        assert!(matches!(r, Resolution::Rt(_)));
        assert_eq!(r.return_type(), "i64");
    }

    #[test]
    fn machine_lookup_after_register() {
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        let entry = MachineEntry {
            name: "counter",
            module: ModuleId::ROOT,
            is_pub: true,
            branches: vec![Signature {
                params: vec!["i64".to_string()],
                ret: "pid".to_string(),
            }],
        };
        reg.module_mut(ModuleId::ROOT).machines.insert("counter", entry);

        let r = reg.resolve_bare("counter").expect("found");
        assert!(matches!(r, Resolution::Machine(_)));
        assert_eq!(r.return_type(), "pid");
    }

    #[test]
    fn registering_same_path_returns_existing_id() {
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        let p = ModulePath(vec!["core".to_string(), "io".to_string()]);
        let a = reg.register_module(p.clone(), None);
        let b = reg.register_module(p, None);
        assert_eq!(a, b);
    }

    fn parse_one(src: &str) -> Vec<Spanned<Item<'_>>> {
        use chumsky::input::Input;
        use chumsky::Parser;
        let tokens = crate::lexer::lexer().parse(src).into_result().unwrap();
        crate::parser::program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .unwrap()
    }

    #[test]
    fn populate_registers_machines_in_root_module() {
        // Use loader-style inputs so we can call populate_from.
        let src = "@rt(rt_write)\npub greet is { _ -> { } }\nmain is { _ -> { } }";
        let ast = parse_one(src);

        let program = crate::loader::ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("main.lake"),
                ast,
            }],
        };

        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        reg.populate_from(&program).expect("populates");

        let root = reg.module(ModuleId::ROOT);
        assert!(root.machines.contains_key("greet"));
        assert!(root.machines.contains_key("main"));
        assert!(root.machines["greet"].is_pub);
        assert!(!root.machines["main"].is_pub);
    }

    #[test]
    fn populate_collects_branch_signatures() {
        let src = "counter is { 0 i64 -> { } n i64 -> { self(n) } }";
        let ast = parse_one(src);
        let program = crate::loader::ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("main.lake"),
                ast,
            }],
        };
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        reg.populate_from(&program).expect("populates");

        let entry = &reg.module(ModuleId::ROOT).machines["counter"];
        assert_eq!(entry.branches.len(), 2);
        assert_eq!(entry.branches[0].params, vec!["i64".to_string()]);
        assert_eq!(entry.branches[1].params, vec!["i64".to_string()]);
        assert_eq!(entry.spawn_ret(), "pid");
    }

    #[test]
    fn populate_records_imports_with_alias() {
        let core_io_src = "pub println is { s str -> { } }";
        let main_src = "+core.io.println as p\nmain is { _ -> { p(\"hi\") } }";

        let core_ast = parse_one(core_io_src);
        let main_ast = parse_one(main_src);

        let program = crate::loader::ParsedProgram {
            modules: vec![
                crate::loader::ParsedModule {
                    module_path: ModulePath::root(),
                    source_path: std::path::PathBuf::from("main.lake"),
                    ast: main_ast,
                },
                crate::loader::ParsedModule {
                    module_path: ModulePath(vec!["core".to_string(), "io".to_string()]),
                    source_path: std::path::PathBuf::from("core/io.lake"),
                    ast: core_ast,
                },
            ],
        };

        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        reg.populate_from(&program).expect("populates");

        let imports = &reg.module(ModuleId::ROOT).imports;
        assert!(imports.contains_key("p"), "alias `p` should be in scope");
        let binding = &imports["p"];
        assert_eq!(binding.target_item, "println");

        // Resolving `p` should give us the `println` machine in core.io.
        let resolved = reg.resolve_bare("p").expect("resolves");
        match resolved {
            Resolution::Machine(m) => assert_eq!(m.name, "println"),
            _ => panic!("expected machine"),
        }
    }

    #[test]
    fn import_binding_resolves_into_target_module() {
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        let core_io = reg.register_module(
            ModulePath(vec!["core".to_string(), "io".to_string()]),
            None,
        );

        // Define `pub println` in core.io.
        reg.module_mut(core_io).machines.insert(
            "println",
            MachineEntry {
                name: "println",
                module: core_io,
                is_pub: true,
                branches: vec![Signature {
                    params: vec!["str".to_string()],
                    ret: "pid".to_string(),
                }],
            },
        );

        // Root imports core.io.println as `out`.
        reg.module_mut(ModuleId::ROOT).imports.insert(
            "out".to_string(),
            ImportBinding {
                target_module: core_io,
                target_item: "println".to_string(),
            },
        );

        let r = reg.resolve_bare("out").expect("alias resolves");
        assert!(matches!(r, Resolution::Machine(m) if m.name == "println"));
    }
}
