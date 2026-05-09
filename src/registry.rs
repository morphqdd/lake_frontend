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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// FFI declarations defined in this file.
    pub ffi_fns: HashMap<&'src str, Signature>,
    /// Aliases brought into scope by `+import` lines.
    /// Key = local binding name (alias or last path segment).
    /// Value = (target module, target item's source name).
    pub imports: HashMap<String, ImportBinding>,
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
        None
    }

    /// Resolve a dotted module path to its id (`core.io` → ModuleId of
    /// `core/io.lake`).  Returns `None` if no such module is registered.
    pub fn module_id_for_path(&self, path: &ModulePath) -> Option<ModuleId> {
        self.by_path.get(path).copied()
    }
}

/// Result of [`ProgramRegistry::resolve_bare`] / [`resolve_in_module`].
#[derive(Debug, Clone, Copy)]
pub enum Resolution<'a> {
    Rt(&'a Signature),
    Machine(&'a MachineEntry<'a>),
    Ffi(&'a Signature),
}

impl<'a> Resolution<'a> {
    /// The return type a caller would see from this callable.
    pub fn return_type(&self) -> &str {
        match self {
            Resolution::Rt(sig) | Resolution::Ffi(sig) => &sig.ret,
            Resolution::Machine(_) => "pid",
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
