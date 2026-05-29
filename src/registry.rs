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

use crate::api::ast::{
    Branch, Directive, EnumDecl, Ident, Import, Item, Machine, MachineItem, Type, VariantPayload,
};
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

    /// Render as a directory-style module path: `core.io` → `core/io/mod.lake`.
    /// Used as a fallback when a package is laid out as a directory whose
    /// entry-point is `mod.lake` (Rust-style).
    pub fn to_filesystem_mod(&self) -> PathBuf {
        let mut p = PathBuf::new();
        for seg in &self.0 {
            p.push(seg);
        }
        p.push("mod.lake");
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

    /// Canonical mangled symbol for a machine defined in this module.
    /// Root → bare `name`; otherwise `mod_seg1_mod_seg2__name`.
    /// Used for backend symbol emission so two `pub size` machines in
    /// different modules don't collide (bugs #097, #102).
    pub fn mangle(&self, name: &str) -> String {
        if self.0.is_empty() {
            name.to_string()
        } else {
            format!("{}__{}", self.0.join("_"), name)
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
    /// agent: generics-142 — declared `<T U ...>` type parameters.  Empty
    /// for non-generic machines.  Monomorphisation in phase 3 consumes
    /// this list.
    pub type_params: Vec<&'src str>,
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

/// A `Name is { field Type ... }` record type declaration belonging to some module.
#[derive(Debug, Clone)]
pub struct RecordEntry<'src> {
    pub name: &'src str,
    pub module: ModuleId,
    pub is_pub: bool,
    /// agent: generics-142 — declared `<T U ...>` type parameters.  Empty
    /// for non-generic records.
    pub type_params: Vec<&'src str>,
    /// Declared fields in source order; field N's offset in the tuple
    /// ABI is N * 8 bytes (each slot is a fat-pointer-sized i64).
    pub fields: Vec<(Spanned<Ident<'src>>, Spanned<Type<'src>>)>,
}

/// A `Name is enum { ... }` tagged-sum type declaration belonging to
/// some module.  Phase 1 — see docs/state/features/145_enums.md.
#[derive(Debug, Clone)]
pub struct EnumEntry<'src> {
    pub name: &'src str,
    pub module: ModuleId,
    /// Generic type parameters.  Empty until #142 lights up enum generics.
    pub type_params: Vec<&'src str>,
    /// Variants in declaration order; the index doubles as the runtime tag.
    pub variants: Vec<VariantEntry<'src>>,
}

/// One variant of an [`EnumEntry`].
#[derive(Debug, Clone)]
pub struct VariantEntry<'src> {
    pub name: &'src str,
    /// Declaration-order index; the resolver / lowering will use this as
    /// the runtime discriminant value when the rest of phase #145 lands.
    pub tag: u64,
    pub payload: VariantPayloadEntry<'src>,
}

/// Registry view of [`crate::api::ast::VariantPayload`].  Types are
/// borrowed from the parsed AST and so share its lifetime.
#[derive(Debug, Clone)]
pub enum VariantPayloadEntry<'src> {
    None,
    Tuple(Vec<Type<'src>>),
    Record(Vec<(&'src str, Type<'src>)>),
}

/// agent: protocols-146 — a `Name is proto { ... }` declaration.
///
/// `required` holds each method's name plus its parameter type strings and
/// return type, with `Self` left as the literal placeholder `"Self"`.
/// Bound verification substitutes `Self := <concrete type>` and looks up a
/// matching machine branch.  See docs/state/features/146_protos.md.
#[derive(Debug, Clone)]
pub struct ProtoEntry<'src> {
    pub name: &'src str,
    pub module: ModuleId,
    /// Parent protos (`+Eq`) whose requirements are inherited transitively.
    pub parents: Vec<&'src str>,
    /// `(method_name, param_type_strings, ret_type_string)` per method.
    pub required: Vec<(&'src str, Vec<String>, String)>,
}

/// agent: protocols-146 — an explicit `X is Eq Show` assertion.  Records
/// that the user *promised* `ty` implements every proto in `protos`; the
/// mono pass verifies the machines exist and errors (E042) if they drift.
#[derive(Debug, Clone)]
pub struct ImplEntry<'src> {
    pub ty: &'src str,
    pub protos: Vec<&'src str>,
    pub module: ModuleId,
    pub span: chumsky::span::SimpleSpan,
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
    /// Record type declarations defined in this file, keyed by source name.
    pub records: HashMap<&'src str, RecordEntry<'src>>,
    /// Enum type declarations defined in this file, keyed by source name.
    /// See docs/state/features/145_enums.md — resolver / typeck land later.
    pub enums: HashMap<&'src str, EnumEntry<'src>>,
    /// agent: protocols-146 — proto declarations defined in this file.
    pub protos: HashMap<&'src str, ProtoEntry<'src>>,
    /// agent: protocols-146 — explicit `X is Eq` assertions in this file.
    pub impls: Vec<ImplEntry<'src>>,
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
///
/// When `target_item` is `None` the binding points at the module
/// itself — a namespace import.  Bare-name lookup of such a binding
/// is meaningless (there's nothing to call); paths whose head is a
/// namespace alias (e.g. `bytes:at` after `+std.bytes`) are rewritten
/// at resolve time into the equivalent fully-qualified path.
#[derive(Debug, Clone)]
pub struct ImportBinding {
    pub target_module: ModuleId,
    /// Item name in the *target* module's source.  `None` means the
    /// alias points at the module itself (namespace import).
    pub target_item: Option<String>,
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
        // Convenience wrapper — the bulk of the lookup logic lives in
        // resolve_bare_in (which the rest of the pipeline also calls)
        // so they share the global-fallback behaviour for free.
        self.resolve_bare_in(self.current, name)
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
        if let Some(r) = cur.records.get(name) {
            return Some(Resolution::Record(r));
        }
        if let Some(binding) = cur.imports.get(name) {
            if let Some(item) = &binding.target_item {
                return self.resolve_in_module(binding.target_module, item);
            }
        }
        // Last-resort: lowering's flatten_paths collapses `ns:item` to
        // bare `item` for the backend's flat symbol space.  After that
        // collapse the typeck / resolver still want to find the entry,
        // so fall through to a global pub-item scan keyed by name.
        // Any unique pub match wins; ambiguous names are an error
        // surface for module-aware mangling (see #72) and are punted
        // for now — the backend's predeclare_machine already errors on
        // duplicate symbols, which is good enough until collisions
        // actually occur in stdlib.
        for module in &self.modules {
            if let Some(m) = module.machines.get(name) {
                if m.is_pub {
                    return Some(Resolution::Machine(m));
                }
            }
            if let Some(c) = module.consts.get(name) {
                if c.vis {
                    return Some(Resolution::Const(c));
                }
            }
        }
        None
    }

    /// Resolve a multi-segment path like `bytes:at` or `std:bytes:at`
    /// in the context of `current`.  Returns the target module id and
    /// the item name within it.
    ///
    /// Lookup proceeds in two steps: direct module lookup first
    /// (`std:bytes:at` → module path ["std", "bytes"] + item "at");
    /// if that fails, the head segment is expanded via the current
    /// module's namespace alias table (e.g. `bytes:at` after
    /// `+std.bytes` → expand "bytes" to ["std", "bytes"]).
    pub fn resolve_path<'a>(
        &self,
        current: ModuleId,
        segments: &'a [Spanned<crate::api::ast::Ident<'a>>],
    ) -> Option<(ModuleId, &'a str)> {
        if segments.len() < 2 {
            return None;
        }
        let last = segments.last().expect("len >= 2").inner.0;
        let module_segs: Vec<&str> = segments[..segments.len() - 1]
            .iter()
            .map(|s| s.inner.0)
            .collect();

        // Direct path attempt.
        let direct_path = ModulePath(module_segs.iter().map(|s| s.to_string()).collect());
        if let Some(id) = self.module_id_for_path(&direct_path) {
            return Some((id, last));
        }

        // Head-as-namespace-alias attempt.
        let head = *module_segs.first()?;
        let scope = self.module(current);
        let binding = scope.imports.get(head)?;
        if binding.target_item.is_some() {
            // Item alias — not a namespace, can't expand.
            return None;
        }
        let target_module_path = &self.module(binding.target_module).path;
        let mut new_segs: Vec<String> = target_module_path.0.clone();
        new_segs.extend(module_segs[1..].iter().map(|s| s.to_string()));
        let expanded = ModulePath(new_segs);
        let id = self.module_id_for_path(&expanded)?;
        Some((id, last))
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
        if let Some(r) = scope.records.get(name) {
            return Some(Resolution::Record(r));
        }
        None
    }

    /// Resolve a dotted module path to its id (`core.io` → ModuleId of
    /// `core/io.lake`).  Returns `None` if no such module is registered.
    pub fn module_id_for_path(&self, path: &ModulePath) -> Option<ModuleId> {
        self.by_path.get(path).copied()
    }

    /// Look up an [`EnumEntry`] by source name in the current module first,
    /// then any other module's pub-only enums.  Used by the resolver to
    /// recognise variant constructors / patterns — see
    /// `docs/state/features/145_enums.md` Phase 2.
    pub fn lookup_enum(&self, name: &str) -> Option<&EnumEntry<'src>> {
        if let Some(e) = self.module(self.current).enums.get(name) {
            return Some(e);
        }
        for module in &self.modules {
            if let Some(e) = module.enums.get(name) {
                return Some(e);
            }
        }
        None
    }

    /// agent: protocols-146 — look up a proto by source name across all
    /// modules (Lake's symbol space is flat).  Used by the mono pass to
    /// resolve `[T: Eq]` bounds and `+Parent` chains.
    pub fn lookup_proto(&self, name: &str) -> Option<&ProtoEntry<'src>> {
        for module in &self.modules {
            if let Some(p) = module.protos.get(name) {
                return Some(p);
            }
        }
        None
    }

    /// agent: protocols-146 — every explicit `X is Eq` assertion across
    /// all modules.  The mono pass walks these to enforce the drift lock.
    pub fn all_impls(&self) -> impl Iterator<Item = &ImplEntry<'src>> {
        self.modules.iter().flat_map(|m| m.impls.iter())
    }

    /// agent: protocols-146 — does a machine named `name` exist anywhere
    /// with a branch whose parameter type strings equal `params` and whose
    /// return type equals `ret`?  Used by proto bound verification: a type
    /// X satisfies a method-proto if the required machine branch exists
    /// after `Self := X` substitution (structural / auto-implement).
    ///
    /// Matching is exact-string on the rendered types.  `_` in the proto's
    /// declared param (rare) and trailing-arity differences are NOT special-
    /// cased — protos declare concrete shapes.
    pub fn has_method_with_sig(&self, name: &str, params: &[String], ret: &str) -> bool {
        for module in &self.modules {
            if let Some(m) = module.machines.get(name) {
                for b in &m.branches {
                    if params_loosely_match(params, &b.params)
                        && (ret == "_" || ty_base(&b.ret) == ty_base(ret) || ret == "pid")
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Same as [`lookup_enum`] but pinned to a specific module's scope.
    pub fn lookup_enum_in(&self, current: ModuleId, name: &str) -> Option<&EnumEntry<'src>> {
        if let Some(e) = self.module(current).enums.get(name) {
            return Some(e);
        }
        for module in &self.modules {
            if let Some(e) = module.enums.get(name) {
                return Some(e);
            }
        }
        None
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
        // Bug #126: tag every error with the module's source path so
        // the multi-file renderer routes diagnostics to the right file.
        let mut errors: Vec<LakeError> = Vec::new();
        for module in &program.modules {
            let id = self
                .by_path
                .get(&module.module_path)
                .copied()
                .expect("module registered in pass 1");
            let mp = module.source_path.clone();
            let tag = |mut e: LakeError| -> LakeError {
                if e.source_path.is_none() {
                    e.source_path = Some(mp.clone());
                }
                e
            };
            for item in &module.ast {
                match &item.inner {
                    Item::Machine(m) => {
                        let name = m.inner.ident.inner.0;
                        let cur = self.module(id);
                        if cur.records.contains_key(name) || cur.enums.contains_key(name) {
                            errors.push(tag(
                                LakeError::new(
                                    format!("duplicate symbol `{name}` in module"),
                                    m.inner.ident.span,
                                )
                                .code("E028"),
                            ));
                            continue;
                        }
                        let entry = build_machine_entry(&m.inner, id);
                        self.module_mut(id)
                            .machines
                            .insert(name, entry);
                    }
                    Item::Directive(d) if d.inner.name.inner.0 == "ffi" => {
                        match build_ffi_signature(&d.inner) {
                            Ok((name, sig)) => {
                                self.module_mut(id).ffi_fns.insert(name, sig);
                            }
                            Err(e) => errors.push(tag(e)),
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
                            Err(e) => errors.push(tag(e)),
                        }
                    }
                    Item::Import(rc) => {
                        if let Err(errs) =
                            register_import_bindings(rc, &[], id, self)
                        {
                            for e in errs {
                                errors.push(tag(e));
                            }
                        }
                    }
                    Item::Record(decl) => {
                        let name = decl.inner.ident.inner.0;
                        let cur = self.module(id);
                        // Cross-namespace collisions are always errors.
                        if cur.machines.contains_key(name)
                            || cur.ffi_fns.contains_key(name)
                            || cur.consts.contains_key(name)
                            || cur.enums.contains_key(name)
                        {
                            errors.push(tag(
                                LakeError::new(
                                    format!("duplicate symbol `{name}` in module"),
                                    decl.inner.ident.span,
                                )
                                .code("E028"),
                            ));
                            continue;
                        }
                        // populate_from is invoked twice (pre- and post-
                        // lowering); a record from round 1 sits in
                        // `records` when round 2 sees the same item.
                        // Treat idempotent re-insert as a no-op; a genuine
                        // duplicate-in-source would have been caught by
                        // the first round.
                        let new_fields: Vec<_> = decl.inner.fields.iter().map(|f| {
                            (f.inner.name.clone(), f.inner.ty.clone())
                        }).collect();
                        if let Some(existing) = cur.records.get(name) {
                            if existing.fields != new_fields {
                                errors.push(tag(
                                    LakeError::new(
                                        format!("duplicate symbol `{name}` in module"),
                                        decl.inner.ident.span,
                                    )
                                    .code("E028"),
                                ));
                            }
                            continue;
                        }
                        let entry = RecordEntry {
                            name,
                            module: id,
                            is_pub: decl.inner.vis,
                            // agent: generics-142
                            type_params: decl
                                .inner
                                .type_params
                                .iter()
                                .map(|p| p.inner.0)
                                .collect(),
                            fields: new_fields,
                        };
                        self.module_mut(id).records.insert(name, entry);
                    }
                    Item::Enum(decl) => {
                        let name = decl.inner.name.inner.0;
                        let cur = self.module(id);
                        // Cross-namespace collisions surface as E028, same
                        // shape as records (see above).
                        if cur.machines.contains_key(name)
                            || cur.ffi_fns.contains_key(name)
                            || cur.consts.contains_key(name)
                            || cur.records.contains_key(name)
                        {
                            errors.push(tag(
                                LakeError::new(
                                    format!("duplicate symbol `{name}` in module"),
                                    decl.inner.name.span,
                                )
                                .code("E028"),
                            ));
                            continue;
                        }
                        // duplicate-symbol checks unchanged below.
                        match build_enum_entry(&decl.inner, id) {
                            Ok(entry) => {
                                // Idempotent re-insert if we already saw this
                                // enum in an earlier `populate_from` round.
                                if let Some(existing) = cur.enums.get(name) {
                                    let variants_match = existing.variants.len()
                                        == entry.variants.len()
                                        && existing
                                            .variants
                                            .iter()
                                            .zip(entry.variants.iter())
                                            .all(|(a, b)| a.name == b.name);
                                    if !variants_match {
                                        errors.push(tag(
                                            LakeError::new(
                                                format!(
                                                    "duplicate symbol `{name}` in module"
                                                ),
                                                decl.inner.name.span,
                                            )
                                            .code("E028"),
                                        ));
                                    }
                                    continue;
                                }
                                self.module_mut(id).enums.insert(name, entry);
                            }
                            Err(e) => errors.push(tag(e)),
                        }
                    }
                    // agent: protocols-146 — record proto declarations.
                    Item::Proto(decl) => {
                        let entry = build_proto_entry(&decl.inner, id);
                        self.module_mut(id).protos.insert(entry.name, entry);
                    }
                    // agent: protocols-146 — record explicit impl asserts.
                    Item::Impl(decl) => {
                        let entry = ImplEntry {
                            ty: decl.inner.ty.inner.0,
                            protos: decl
                                .inner
                                .protos
                                .iter()
                                .map(|p| p.inner.0)
                                .collect(),
                            module: id,
                            span: decl.inner.ty.span,
                        };
                        self.module_mut(id).impls.push(entry);
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
        // agent: generics-142
        type_params: machine.generics.iter().map(|p| p.inner.0).collect(),
        branches,
    }
}

/// Base type name with any generic argument clause stripped:
/// `Vec[T]` → `Vec`, `i64` → `i64`.  Used by proto verification so a
/// generic container's method (`index[T] { v Vec[T] … }`, registered
/// with param string `Vec[T]`) satisfies a proto requirement whose
/// `Self` substituted to the bare base name `Vec`.
fn ty_base(s: &str) -> &str {
    // `Type::Display` renders generics with angle brackets (`Vec<T>`)
    // while source / mangling uses square brackets (`Vec[T]`) — accept
    // either so the base name is recovered regardless of origin.
    match s.find(['[', '<']) {
        Some(i) => s[..i].trim_end(),
        None => s,
    }
}

/// Proto-requirement param matcher: same arity, and each required param
/// equals the candidate either exactly, by base name (generic args
/// ignored), or via the `_` wildcard.  Looser than `==` only where
/// generics are involved — for fully concrete protos (`eq(Self Self)`)
/// it degenerates to exact comparison.
fn params_loosely_match(required: &[String], got: &[String]) -> bool {
    required.len() == got.len()
        && required
            .iter()
            .zip(got.iter())
            .all(|(r, g)| r == "_" || r == g || ty_base(r) == ty_base(g))
}

fn branch_signature(branch: &Branch<'_>) -> Signature {
    let params: Vec<String> = branch
        .patterns
        .iter()
        .filter(|p| !p.inner.is_wildcard())
        .map(|p| p.inner.ty.inner.to_string())
        .collect();
    // Ret-machines (`-> ret <ty>`) propagate their declared return type so
    // call-site let-inference (#45 buf, #66 sha256, etc.) sees the right
    // shape.  Spawn-style branches (no `ret_ty`) fall back to "pid".
    let ret = branch
        .ret_ty
        .as_ref()
        .map(|t| t.inner.to_string())
        .unwrap_or_else(|| "pid".to_string());
    Signature { params, ret }
}

/// Convert a parsed [`EnumDecl`] into its registry view.  Rejects
/// duplicate variant names within the same enum with diagnostic E029.
fn build_enum_entry<'src>(
    decl: &EnumDecl<'src>,
    module: ModuleId,
) -> Result<EnumEntry<'src>, LakeError> {
    let name = decl.name.inner.0;
    let mut seen: HashMap<&'src str, ()> = HashMap::new();
    let mut variants = Vec::with_capacity(decl.variants.len());
    for (tag, v) in decl.variants.iter().enumerate() {
        let vname = v.inner.name.inner.0;
        if seen.contains_key(vname) {
            return Err(LakeError::new(
                format!("duplicate variant `{vname}` in enum `{name}`"),
                v.inner.name.span,
            )
            .code("E029"));
        }
        seen.insert(vname, ());
        let payload = match &v.inner.payload {
            VariantPayload::None => VariantPayloadEntry::None,
            VariantPayload::Tuple(ts) => {
                VariantPayloadEntry::Tuple(ts.iter().map(|t| t.inner.clone()).collect())
            }
            VariantPayload::Record(fs) => VariantPayloadEntry::Record(
                fs.iter()
                    .map(|(n, t)| (n.inner.0, t.inner.clone()))
                    .collect(),
            ),
        };
        variants.push(VariantEntry {
            name: vname,
            tag: tag as u64,
            payload,
        });
    }
    Ok(EnumEntry {
        name,
        module,
        type_params: decl.type_params.iter().map(|p| p.inner.0).collect(),
        variants,
    })
}

/// agent: protocols-146 — convert a parsed [`ProtoDecl`] into its registry
/// view.  `Self` is preserved verbatim in the type strings; substitution
/// happens at bound-verification time.
fn build_proto_entry<'src>(
    decl: &crate::api::ast::ProtoDecl<'src>,
    module: ModuleId,
) -> ProtoEntry<'src> {
    let required = decl
        .methods
        .iter()
        .map(|m| {
            let params: Vec<String> =
                m.inner.params.iter().map(|t| t.inner.to_string()).collect();
            let ret = m
                .inner
                .ret
                .as_ref()
                .map(|t| t.inner.to_string())
                .unwrap_or_else(|| "pid".to_string());
            (m.inner.name.inner.0, params, ret)
        })
        .collect();
    ProtoEntry {
        name: decl.name.inner.0,
        module,
        parents: decl.parents.iter().map(|p| p.inner.0).collect(),
        required,
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
            // Prefer the namespace interpretation (prefix + ident as
            // a module file).  Fall back to the item interpretation
            // only when the namespace candidate isn't loaded.
            let mut ns_segs = prefix.to_vec();
            ns_segs.push(ident.inner.0.to_string());
            let ns_path = ModulePath(ns_segs);
            let local_name = alias
                .as_ref()
                .map(|a| a.inner.0.to_string())
                .unwrap_or_else(|| ident.inner.0.to_string());

            if let Some(ns_id) = registry.module_id_for_path(&ns_path) {
                registry.module_mut(current_module).imports.insert(
                    local_name,
                    ImportBinding {
                        target_module: ns_id,
                        target_item: None,
                    },
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
            registry.module_mut(current_module).imports.insert(
                local_name,
                ImportBinding {
                    target_module: target_id,
                    target_item: Some(ident.inner.0.to_string()),
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
    /// Nominal record type.  Constructor lowering lands in T7/T8.
    Record(&'a RecordEntry<'a>),
}

impl<'a> Resolution<'a> {
    /// The return type a caller would see from this callable.
    pub fn return_type(&self) -> &str {
        match self {
            Resolution::Rt(sig) | Resolution::Ffi(sig) => &sig.ret,
            // All branches of a single machine share the declared ret type
            // (`pid` for spawn-style, the explicit `ret <ty>` for ret-machines).
            // Pick branches[0] as canonical; empty-branches case is unreachable
            // post-resolver.
            Resolution::Machine(m) => m.branches.first().map(|b| b.ret.as_str()).unwrap_or("pid"),
            Resolution::Const(c) => &c.ty,
            // Constructor return type is the record's own name; real typing lands in T7/T8.
            Resolution::Record(r) => r.name,
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
            type_params: Vec::new(),
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
        assert_eq!(binding.target_item.as_deref(), Some("println"));

        // Resolving `p` should give us the `println` machine in core.io.
        let resolved = reg.resolve_bare("p").expect("resolves");
        match resolved {
            Resolution::Machine(m) => assert_eq!(m.name, "println"),
            _ => panic!("expected machine"),
        }
    }

    #[test]
    fn registry_populates_record() {
        let src = "Request is { method buf  path buf }";
        let ast = parse_one(src);
        let program = crate::loader::ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("main.lake"),
                ast,
            }],
        };
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        reg.populate_from(&program).expect("populate");
        let entry = reg.module(ModuleId::ROOT).records.get("Request").expect("registered");
        assert_eq!(entry.fields.len(), 2);
        assert_eq!(entry.fields[0].0.inner.0, "method");
    }

    #[test]
    fn registry_rejects_record_machine_collision_e028() {
        let src = "Foo is { x i64 }\nFoo is { _ -> ret i64 { ret 0 } }";
        let ast = parse_one(src);
        let program = crate::loader::ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("main.lake"),
                ast,
            }],
        };
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        let res = reg.populate_from(&program);
        let errs: Vec<LakeError> = res.err().expect("collision must produce an error").into_iter().collect();
        assert!(
            errs.iter().any(|e| e.code.as_deref() == Some("E028")),
            "expected E028, got {:?}",
            errs.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn registry_resolves_record_by_bare_name() {
        let src = "Request is { method buf }";
        let ast = parse_one(src);
        let program = crate::loader::ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("main.lake"),
                ast,
            }],
        };
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        reg.populate_from(&program).expect("populate");
        let resolution = reg.resolve_bare_in(ModuleId::ROOT, "Request");
        assert!(matches!(resolution, Some(Resolution::Record(_))));
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
                type_params: Vec::new(),
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
                target_item: Some("println".to_string()),
            },
        );

        let r = reg.resolve_bare("out").expect("alias resolves");
        assert!(matches!(r, Resolution::Machine(m) if m.name == "println"));
    }

    #[test]
    fn registry_populates_enum_with_sequential_tags() {
        let src = "HTTPMethod is enum {
          Get
          Post(buf)
          Delete
          Open { from i64 }
        }";
        let ast = parse_one(src);
        let program = crate::loader::ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("main.lake"),
                ast,
            }],
        };
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        reg.populate_from(&program).expect("populate");
        let entry = reg
            .module(ModuleId::ROOT)
            .enums
            .get("HTTPMethod")
            .expect("registered");
        assert_eq!(entry.variants.len(), 4);
        for (i, v) in entry.variants.iter().enumerate() {
            assert_eq!(v.tag, i as u64, "tags must be declaration-order");
        }
        assert_eq!(entry.variants[0].name, "Get");
        assert!(matches!(entry.variants[0].payload, VariantPayloadEntry::None));
        assert!(matches!(
            entry.variants[1].payload,
            VariantPayloadEntry::Tuple(_)
        ));
        assert!(matches!(
            entry.variants[3].payload,
            VariantPayloadEntry::Record(_)
        ));
    }

    #[test]
    fn registry_rejects_duplicate_variant_e029() {
        let src = "Bad is enum { A B A }";
        let ast = parse_one(src);
        let program = crate::loader::ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("main.lake"),
                ast,
            }],
        };
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        let res = reg.populate_from(&program);
        let errs: Vec<LakeError> = res.err().expect("duplicate must error").into_iter().collect();
        assert!(
            errs.iter().any(|e| e.code.as_deref() == Some("E029")),
            "expected E029, got {:?}",
            errs.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn registry_rejects_enum_machine_collision_e028() {
        let src = "Foo is enum { A B }\nFoo is { _ -> ret i64 { ret 0 } }";
        let ast = parse_one(src);
        let program = crate::loader::ParsedProgram {
            modules: vec![crate::loader::ParsedModule {
                module_path: ModulePath::root(),
                source_path: std::path::PathBuf::from("main.lake"),
                ast,
            }],
        };
        let mut reg: ProgramRegistry<'_> = ProgramRegistry::with_rt();
        let res = reg.populate_from(&program);
        let errs: Vec<LakeError> = res.err().expect("collision must error").into_iter().collect();
        assert!(
            errs.iter().any(|e| e.code.as_deref() == Some("E028")),
            "expected E028, got {:?}",
            errs.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }
}
