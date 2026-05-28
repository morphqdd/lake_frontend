//! Multi-file module loader.
//!
//! Walks the import graph rooted at an entry file: reads every transitively
//! imported `.lake` file from disk, detects cycles, and produces a
//! [`ProgramSources`] container that owns the source strings.  The caller
//! then calls [`ProgramSources::parse_all`] to produce a [`ParsedProgram`]
//! whose ASTs borrow from the owned sources.
//!
//! Two-phase architecture chosen so the borrow checker stays happy:
//!
//!   * **Phase 1** ([`ProgramSources::load`]) — pure file I/O.  Each file is
//!     read into a `String`, momentarily parsed to extract just the import
//!     references, and the temporary AST is dropped before recursing.
//!     Owning Strings (rather than &str) means later mutations of `files`
//!     don't invalidate any borrow we hold across the recursion.
//!
//!   * **Phase 2** ([`ProgramSources::parse_all`]) — re-parses each loaded
//!     file with stable `&'src str` borrows into the now-frozen `files`
//!     vector.  The resulting [`ParsedProgram`] borrows from the
//!     `ProgramSources` for the rest of compilation.
//!
//! Re-parsing is the cost of avoiding an arena.  Parse cost is small
//! relative to codegen, and the simplicity of the borrow architecture is
//! worth it for an early implementation.
//!
//! Module path resolution today is intentionally minimal — only the entry
//! file's parent directory is searched.  Env-var search paths
//! (`LAKE_PATH`, `<NAME>_PATH`) are tracked under #29 and slot in here.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use chumsky::{Parser, input::Input, span::Spanned};

use crate::api::ast::{Branch, Import, Item, Machine, MachineItem};
use crate::api::expr::Expr;
use crate::error_handle::{LakeError, LakeErrors};
use crate::lexer::lexer;
use crate::parser::program;
use crate::registry::ModulePath;

// ── search paths ────────────────────────────────────────────────────────────

/// Where the loader looks for a module file when resolving an `+import`
/// directive.  Lookup order, first match wins:
///
///   1. **Project root** — entry file's parent directory.  Set by
///      [`ProgramSources::load`] after construction; tests using
///      [`ProgramSources::load_with_search`] may override.
///   2. **Per-library `<NAME>_PATH` env var** — when the import path's
///      first segment is `<name>`, the env var `<NAME_UPPER>_PATH` (if
///      set) is treated as a root, and the FIRST segment is consumed by
///      the env-var name.  Example: with `STD_PATH=/opt/lake/std` set,
///      `+std.io.println` resolves to `/opt/lake/std/io.lake`.
///   3. **`LAKE_PATH`** — colon-separated list of fallback root
///      directories.  Each entry is tried with the FULL module path:
///      `+std.io.println` → `<entry>/std/io.lake`.
///
/// Search paths are read from the process environment by
/// [`SearchPaths::from_env`]; tests pass [`SearchPaths::default`] and
/// populate fields manually.
#[derive(Debug, Clone, Default)]
pub struct SearchPaths {
    /// Project root.  Always tried first.  Set by `load` after the
    /// caller passes the entry file path.
    pub project_root: Option<PathBuf>,
    /// Per-library overrides keyed by the lowercased first import
    /// segment.  Looked up via uppercase `<NAME>_PATH` env vars.
    pub lib_roots: HashMap<String, PathBuf>,
    /// Generic fallback list (`LAKE_PATH`).  Tried after `lib_roots`,
    /// in declaration order.
    pub lake_paths: Vec<PathBuf>,
}

impl SearchPaths {
    /// Read `LAKE_PATH` and any matching `<NAME>_PATH` vars from the
    /// process environment.  Project root is left unset — callers fill
    /// it from the entry file's parent directory.
    pub fn from_env() -> Self {
        let mut sp = Self::default();
        if let Ok(raw) = std::env::var("LAKE_PATH") {
            for part in raw.split(':').filter(|p| !p.is_empty()) {
                sp.lake_paths.push(PathBuf::from(part));
            }
        }
        // Scan all env vars for `<NAME>_PATH` with NAME uppercase ASCII.
        // We can't know in advance which library names a program will
        // import, so we collect every `*_PATH` that fits the convention.
        for (key, value) in std::env::vars() {
            if let Some(prefix) = key.strip_suffix("_PATH") {
                if prefix == "LAKE" {
                    continue; // already handled above
                }
                if !prefix.is_empty()
                    && prefix.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                {
                    sp.lib_roots
                        .insert(prefix.to_ascii_lowercase(), PathBuf::from(value));
                }
            }
        }
        sp
    }

    /// Try each registered root in order, returning the first existing
    /// `.lake` file that resolves the given module path.  Returns
    /// `None` (and the list of attempted candidates for diagnostics) if
    /// nothing matches.
    pub fn resolve(&self, module: &ModulePath) -> ResolveOutcome {
        let mut tried = Vec::new();
        if let Some(root) = &self.project_root {
            let candidate = root.join(module.to_filesystem());
            if candidate.is_file() {
                return ResolveOutcome::Found(candidate);
            }
            tried.push(candidate);
            // Directory-style: <module>/mod.lake.
            let mod_candidate = root.join(module.to_filesystem_mod());
            if mod_candidate.is_file() {
                return ResolveOutcome::Found(mod_candidate);
            }
            tried.push(mod_candidate);
        }
        // Per-lib root: first segment names the library, env var holds
        // the directory the library's tree starts at.  The first
        // segment is *consumed* by the var name — module path
        // `std.io.println` under `STD_PATH=/x` resolves to `/x/io.lake`,
        // not `/x/std/io.lake`.
        //
        // #133: when the path is a single segment (`+lash.{ parser }`
        // with item_fallback module path = `lash`), try
        // `<LIB_ROOT>/<first>.lake` so single-file packages whose
        // entry name matches the package name resolve via their env
        // var instead of requiring users to thread `LAKE_PATH`.
        if let Some(first) = module.0.first() {
            if let Some(root) = self.lib_roots.get(first) {
                let rest = ModulePath(module.0.iter().skip(1).cloned().collect());
                if rest.0.is_empty() {
                    let candidate = root.join(format!("{first}.lake"));
                    if candidate.is_file() {
                        return ResolveOutcome::Found(candidate);
                    }
                    tried.push(candidate);
                } else {
                    let candidate = root.join(rest.to_filesystem());
                    if candidate.is_file() {
                        return ResolveOutcome::Found(candidate);
                    }
                    tried.push(candidate);
                    let mod_candidate = root.join(rest.to_filesystem_mod());
                    if mod_candidate.is_file() {
                        return ResolveOutcome::Found(mod_candidate);
                    }
                    tried.push(mod_candidate);
                }
            }
        }
        for root in &self.lake_paths {
            let candidate = root.join(module.to_filesystem());
            if candidate.is_file() {
                return ResolveOutcome::Found(candidate);
            }
            tried.push(candidate);
            let mod_candidate = root.join(module.to_filesystem_mod());
            if mod_candidate.is_file() {
                return ResolveOutcome::Found(mod_candidate);
            }
            tried.push(mod_candidate);
        }
        ResolveOutcome::NotFound { tried }
    }
}

/// Outcome of a module-path resolution attempt.  `NotFound` carries the
/// list of locations tried so diagnostics can show the user where the
/// loader looked.
pub enum ResolveOutcome {
    Found(PathBuf),
    NotFound { tried: Vec<PathBuf> },
}

// ── owned sources ──────────────────────────────────────────────────────────

/// One file that has been read from disk.  Once a `LoadedFile` lands in
/// [`ProgramSources::files`] it is never mutated; later passes hold
/// `&'src str` borrows into `src` for the entire compilation.
#[derive(Debug)]
pub struct LoadedFile {
    pub module_path: ModulePath,
    pub source_path: PathBuf,
    pub src: String,
}

/// Container owning every transitively-loaded source file.  Borrowing from
/// a `ProgramSources` produces ASTs whose lifetime is tied to the container.
#[derive(Debug, Default)]
pub struct ProgramSources {
    files: Vec<LoadedFile>,
    /// Module paths that have been loaded already (memoization).  Indexed
    /// into `files` lazily by re-scanning; small N → O(N) is fine.
    loaded: HashSet<ModulePath>,
}

impl ProgramSources {
    /// Load the entry file plus everything it transitively imports.
    /// `entry_path` may be relative or absolute; its **parent directory**
    /// becomes the first search root for resolving `+core.io.writer`-style
    /// imports.  Additional roots come from environment variables — see
    /// [`SearchPaths::from_env`] for the full lookup order.
    pub fn load<P: AsRef<Path>>(entry_path: P) -> Result<Self, LakeErrors> {
        Self::load_with_search(entry_path, SearchPaths::from_env())
    }

    /// Like [`Self::load`] but with an explicit [`SearchPaths`] — useful
    /// for tests that want deterministic resolution without poking the
    /// process environment.
    pub fn load_with_search<P: AsRef<Path>>(
        entry_path: P,
        mut search: SearchPaths,
    ) -> Result<Self, LakeErrors> {
        let entry_path = entry_path.as_ref().to_path_buf();
        let project_root = entry_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        // Project root is always the first place we look — prepend so it
        // beats env vars on collision (familiar Cargo/Rust behaviour).
        search.project_root = Some(project_root);

        let mut sources = ProgramSources::default();
        let mut in_progress: HashSet<ModulePath> = HashSet::new();

        sources.load_recursive(
            ModulePath::root(),
            entry_path,
            &search,
            &mut in_progress,
        )?;
        Ok(sources)
    }

    /// Returns every loaded file in load order.
    pub fn files(&self) -> &[LoadedFile] {
        &self.files
    }

    /// Phase 2 — parse every loaded file.  ASTs borrow from `self`.
    ///
    /// Errors from any file are surfaced together; partial results are
    /// returned as `Err` so the caller can render every diagnostic before
    /// bailing.
    pub fn parse_all(&self) -> Result<ParsedProgram<'_>, ParseErrors> {
        let mut modules = Vec::with_capacity(self.files.len());
        let mut errors = ParseErrors::default();

        for f in &self.files {
            let tokens = match lexer().parse(f.src.as_str()).into_result() {
                Ok(t) => t,
                Err(errs) => {
                    // Bug #126: tag per-file so multi-file rendering routes
                    // diagnostics to the correct source.
                    errors.push(
                        &f.source_path,
                        LakeErrors::from_lex_errs(errs).tag_source_path(&f.source_path),
                    );
                    continue;
                }
            };
            let ast = match program()
                .parse(tokens[..].split_spanned((0..f.src.len()).into()))
                .into_result()
            {
                Ok(a) => a,
                Err(errs) => {
                    errors.push(
                        &f.source_path,
                        LakeErrors::from_parse_errs(errs).tag_source_path(&f.source_path),
                    );
                    continue;
                }
            };
            modules.push(ParsedModule {
                module_path: f.module_path.clone(),
                source_path: f.source_path.clone(),
                ast,
            });
        }

        if errors.is_empty() {
            Ok(ParsedProgram { modules })
        } else {
            Err(errors)
        }
    }

    fn load_recursive(
        &mut self,
        module_path: ModulePath,
        source_path: PathBuf,
        search: &SearchPaths,
        in_progress: &mut HashSet<ModulePath>,
    ) -> Result<(), LakeErrors> {
        // Order matters: check in_progress BEFORE loaded.  A module is in
        // `loaded` only after every transitive dependency finished, so any
        // re-entry while still recursing means we have a cycle, not a
        // legitimate "already done" hit.
        if in_progress.contains(&module_path) {
            return Err(LakeErrors::new(vec![cycle_error(&module_path)]));
        }
        if self.loaded.contains(&module_path) {
            return Ok(());
        }

        let src = fs::read_to_string(&source_path).map_err(|e| {
            LakeErrors::new(vec![io_error(&source_path, &e.to_string())])
        })?;

        in_progress.insert(module_path.clone());
        self.files.push(LoadedFile {
            module_path: module_path.clone(),
            source_path: source_path.clone(),
            src,
        });

        // Extract imports + Path-implied modules via a temporary parse —
        // drops at end of scope so we can mutate `self.files` later
        // without holding a borrow.  Path pre-scan: every `Expr::Path`
        // with >=2 segments names a module-qualified item; the owning
        // module's file is queued for load alongside explicit imports.
        let (imports, path_modules) = {
            let last = self
                .files
                .last()
                .expect("just pushed");
            let tokens = match lexer().parse(last.src.as_str()).into_result() {
                Ok(t) => t,
                Err(errs) => {
                    in_progress.remove(&module_path);
                    // Bug #126: tag with the file the error came from.
                    return Err(LakeErrors::from_lex_errs(errs)
                        .tag_source_path(&source_path));
                }
            };
            let ast = match program()
                .parse(tokens[..].split_spanned((0..last.src.len()).into()))
                .into_result()
            {
                Ok(a) => a,
                Err(errs) => {
                    in_progress.remove(&module_path);
                    return Err(LakeErrors::from_parse_errs(errs)
                        .tag_source_path(&source_path));
                }
            };
            (collect_referenced_modules(&ast), prescan_path_modules(&ast))
        };

        for cand in imports {
            // Try the namespace path first.  If that file exists, load
            // it and skip the fallback — the leaf is a namespace
            // import.  Otherwise drop to item interpretation.
            let ns_outcome = search.resolve(&cand.namespace);
            if let ResolveOutcome::Found(p) = ns_outcome {
                self.load_recursive(cand.namespace, p, search, in_progress)?;
                continue;
            }
            // Item-style fallback.
            let fb_outcome = search.resolve(&cand.item_fallback);
            match fb_outcome {
                ResolveOutcome::Found(p) => {
                    self.load_recursive(cand.item_fallback, p, search, in_progress)?;
                }
                ResolveOutcome::NotFound { tried } => {
                    // Namespace candidate also missed; surface the
                    // longer path in the diagnostic since that is what
                    // the source spelled.  Both attempted locations are
                    // shown so the user can see what files were probed.
                    let ns_tried = match search.resolve(&cand.namespace) {
                        ResolveOutcome::NotFound { tried } => tried,
                        _ => vec![],
                    };
                    let mut all_tried = ns_tried;
                    all_tried.extend(tried);
                    return Err(LakeErrors::new(vec![module_not_found(
                        &cand.namespace,
                        &all_tried,
                    )]));
                }
            }
        }

        // Path pre-scan: each candidate is the owning module of an
        // `<a>.<b>.<item>` expression.  Permissive — modules that fail
        // to resolve are silently skipped; typeck surfaces the real
        // error (M005 "unknown module path") with proper spans.  Loaded
        // modules dedup via `self.loaded`, so this is idempotent and
        // safe to invoke alongside explicit `+import` directives.
        for cand in path_modules {
            if self.loaded.contains(&cand) || in_progress.contains(&cand) {
                continue;
            }
            if let ResolveOutcome::Found(p) = search.resolve(&cand) {
                self.load_recursive(cand, p, search, in_progress)?;
            }
        }

        // Mark loaded ONLY after every dependency finished — see the
        // cycle-vs-already-done distinction at the top of this function.
        in_progress.remove(&module_path);
        self.loaded.insert(module_path);
        Ok(())
    }
}

// ── parsed program ─────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ParsedModule<'src> {
    pub module_path: ModulePath,
    pub source_path: PathBuf,
    pub ast: Vec<Spanned<Item<'src>>>,
}

#[derive(Debug)]
pub struct ParsedProgram<'src> {
    pub modules: Vec<ParsedModule<'src>>,
}

impl<'src> ParsedProgram<'src> {
    /// Find a parsed module by its module path.
    pub fn by_path(&self, path: &ModulePath) -> Option<&ParsedModule<'src>> {
        self.modules.iter().find(|m| &m.module_path == path)
    }

    /// The entry / root module (always at index 0 because it is loaded
    /// first).  Convenient access for callers that don't care about the
    /// rest yet.
    pub fn root(&self) -> &ParsedModule<'src> {
        &self.modules[0]
    }
}

// ── error aggregation across files ─────────────────────────────────────────

#[derive(Debug, Default)]
pub struct ParseErrors {
    pub per_file: Vec<(PathBuf, LakeErrors)>,
}

impl ParseErrors {
    pub fn push<P: AsRef<Path>>(&mut self, path: P, errs: LakeErrors) {
        self.per_file.push((path.as_ref().to_path_buf(), errs));
    }
    pub fn is_empty(&self) -> bool {
        self.per_file.is_empty()
    }
    pub fn display(&self, sources: &ProgramSources) {
        for (path, errs) in &self.per_file {
            let src = sources
                .files
                .iter()
                .find(|f| &f.source_path == path)
                .map(|f| f.src.as_str())
                .unwrap_or("");
            errs.display(src, path);
        }
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Collect every module file referenced by the imports in `ast`.  Returns a
/// One leaf-style import resolves to a (preferred, fallback) pair so a
/// shape like `+std.bytes` can mean either:
///   * **namespace**: load `std/bytes.lake`, alias the local name to
///     that module (preferred when the file exists)
///   * **item**: load `std.lake`, alias the local name to item
///     `bytes` inside it (fallback)
///
/// `+std.bytes.{ at }` and `+std.bytes.foo` use the recursive form
/// and never land in the leaf branch with a "namespace" candidate —
/// they always carry an explicit child segment, which fixes the
/// interpretation as item-style.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ImportCandidate {
    /// Try loading this path first.  If it resolves, the leaf becomes
    /// a namespace import.
    namespace: ModulePath,
    /// Fall back to this path when the namespace candidate has no
    /// file.  This is the legacy "item from module" path.
    item_fallback: ModulePath,
}

/// Walk the parsed AST for every `+import` statement and return the
/// deduplicated list of candidate import shapes.
fn collect_referenced_modules(ast: &[Spanned<Item<'_>>]) -> Vec<ImportCandidate> {
    let mut out = HashSet::<ImportCandidate>::new();
    for item in ast {
        if let Item::Import(rc) = &item.inner {
            collect_from_import(&rc.inner, &[], &mut out);
        }
    }
    let mut v: Vec<_> = out.into_iter().collect();
    // Stable order so error messages and load sequence are deterministic.
    v.sort_by(|a, b| a.namespace.0.cmp(&b.namespace.0));
    v
}

/// Walk the AST for every `Expr::Path` (module-qualified value such as
/// `std.bytes.set`) and return the deduplicated set of owning module
/// paths the program references.  A Path with N>=2 segments names item
/// `segs[N-1]` inside module `segs[..N-1]`; the loader uses this set to
/// pull transitive modules into the load queue even when the source
/// never spells an explicit `+import`.
///
/// Single-segment paths (bare `Var`) and segment lists shorter than two
/// are skipped — those resolve to local names, not foreign modules.
fn prescan_path_modules(ast: &[Spanned<Item<'_>>]) -> Vec<ModulePath> {
    let mut out = HashSet::<ModulePath>::new();
    for item in ast {
        match &item.inner {
            Item::Machine(m) => prescan_machine(&m.inner, &mut out),
            Item::Const(c) => prescan_expr(&c.inner.value, &mut out),
            Item::Import(_) | Item::Directive(_) | Item::Record(_) | Item::Enum(_) => {}
        }
    }
    let mut v: Vec<_> = out.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn prescan_machine(machine: &Machine<'_>, out: &mut HashSet<ModulePath>) {
    for mi in &machine.items {
        if let MachineItem::Branch(b) = &mi.inner {
            prescan_branch(b, out);
        }
    }
}

fn prescan_branch(branch: &Branch<'_>, out: &mut HashSet<ModulePath>) {
    for e in &branch.body {
        prescan_expr(e, out);
    }
}

fn prescan_expr(expr: &Spanned<Expr<'_>>, out: &mut HashSet<ModulePath>) {
    match &expr.inner {
        Expr::Path(segments) if segments.len() >= 2 => {
            let module_segs: Vec<String> = segments[..segments.len() - 1]
                .iter()
                .map(|s| s.inner.0.to_string())
                .collect();
            out.insert(ModulePath(module_segs));
        }
        Expr::Path(_) => {}
        Expr::Let { default, .. } => {
            if let Some(d) = default {
                prescan_expr(d, out);
            }
        }
        Expr::Jump { ident, args } => {
            prescan_expr(ident, out);
            for a in args {
                prescan_expr(a, out);
            }
        }
        Expr::MethodCall { receiver, args, .. } => {
            prescan_expr(receiver, out);
            for a in args {
                prescan_expr(a, out);
            }
        }
        Expr::AtAccess { receiver, .. } | Expr::DotAccess { receiver, .. } => {
            prescan_expr(receiver, out);
        }
        Expr::StructInit { base, fields } => {
            prescan_expr(base, out);
            for f in fields {
                prescan_expr(f, out);
            }
        }
        Expr::When { cond, branches } => {
            prescan_expr(cond, out);
            for (pat, body) in branches {
                prescan_expr(pat, out);
                for e in body {
                    prescan_expr(e, out);
                }
            }
        }
        Expr::Wait { handlers, filter } => {
            for f in filter {
                prescan_expr(f, out);
            }
            for h in handlers {
                prescan_branch(&h.inner, out);
            }
        }
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Eq(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r)
        | Expr::BAnd(l, r)
        | Expr::BOr(l, r)
        | Expr::BXor(l, r)
        | Expr::Shl(l, r)
        | Expr::Shr(l, r) => {
            prescan_expr(l, out);
            prescan_expr(r, out);
        }
        Expr::Neg(inner) | Expr::Ret(inner) | Expr::Pin(inner) => prescan_expr(inner, out),
        Expr::Tuple(elems) => {
            for e in elems {
                prescan_expr(e, out);
            }
        }
        Expr::TupleIndex { receiver, .. } => prescan_expr(receiver, out),
        Expr::Index { receiver, index } => {
            prescan_expr(receiver, out);
            prescan_expr(index, out);
        }
        Expr::LetTuple { default, .. } => prescan_expr(default, out),
        Expr::Var(_, _)
        | Expr::Num(_, _)
        | Expr::String(_, _)
        | Expr::Bool(_)
        | Expr::Atom(_)
        | Expr::Unit => {}
        Expr::RecordLiteral { fields, .. } => {
            for (_, val) in fields {
                prescan_expr(val, out);
            }
        }
    }
}

fn collect_from_import(
    node: &Rc<RefCell<Import<'_>>>,
    prefix: &[String],
    out: &mut HashSet<ImportCandidate>,
) {
    match &*node.borrow() {
        Import::Import(ident, Some(next), _alias) => {
            let mut new_prefix = prefix.to_vec();
            new_prefix.push(ident.inner.0.to_string());
            collect_from_import(&next.inner, &new_prefix, out);
        }
        Import::Import(ident, None, _alias) => {
            // Leaf reached.  Generate two candidates:
            //   namespace = prefix + leaf (file: prefix/leaf.lake)
            //   item_fallback = prefix    (file: prefix.lake, leaf is
            //                              an item inside)
            if !prefix.is_empty() {
                let item_fallback = ModulePath(prefix.to_vec());
                let mut ns = prefix.to_vec();
                ns.push(ident.inner.0.to_string());
                let namespace = ModulePath(ns);
                out.insert(ImportCandidate { namespace, item_fallback });
            }
        }
        Import::MultiImport(entries) => {
            for entry in entries {
                collect_from_import(&entry.inner, prefix, out);
            }
        }
    }
}

// ── error helpers ──────────────────────────────────────────────────────────

fn cycle_error(path: &ModulePath) -> LakeError {
    LakeError::new(
        format!("import cycle detected at module `{}`", path.display()),
        // No source span available for transitive cycle errors yet.
        (0..0).into(),
    )
    .code("M001")
    .help("break the cycle by removing one of the import edges")
}

fn module_not_found(module: &ModulePath, tried: &[PathBuf]) -> LakeError {
    let tried_list = if tried.is_empty() {
        "(no search paths configured)".to_string()
    } else {
        tried
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    LakeError::new(
        format!(
            "module `{}` not found (looked for `{}`)",
            module.display(),
            module.to_filesystem().display(),
        ),
        (0..0).into(),
    )
    .code("M002")
    .note(format!("tried:\n{tried_list}"))
    .help(
        "create the missing file, or set LAKE_PATH / <name>_PATH to point at the directory \
         containing the library",
    )
}

fn io_error(path: &Path, message: &str) -> LakeError {
    LakeError::new(
        format!("could not read `{}`: {}", path.display(), message),
        (0..0).into(),
    )
    .code("M003")
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, rel: &str, content: &str) -> PathBuf {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(&path).unwrap();
        write!(f, "{}", content).unwrap();
        path
    }

    #[test]
    fn loads_entry_file_only_when_no_imports() {
        let dir = TempDir::new().unwrap();
        let main = write_file(&dir, "main.lake", "main is { _ -> { } }");
        let sources = ProgramSources::load(&main).expect("loads");
        assert_eq!(sources.files().len(), 1);
        assert!(sources.files()[0].module_path.is_root());
    }

    #[test]
    fn loads_imported_module() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "core/io.lake", "pub println is { s str -> { } }");
        let main = write_file(
            &dir,
            "main.lake",
            "+core.io.println\nmain is { _ -> { } }",
        );
        let sources = ProgramSources::load(&main).expect("loads");
        let mods: Vec<_> = sources
            .files()
            .iter()
            .map(|f| f.module_path.display())
            .collect();
        assert!(mods.contains(&"<root>".to_string()), "found {mods:?}");
        assert!(mods.contains(&"core.io".to_string()), "found {mods:?}");
    }

    #[test]
    fn missing_module_errors() {
        let dir = TempDir::new().unwrap();
        let main = write_file(
            &dir,
            "main.lake",
            "+missing.module.thing\nmain is { _ -> { } }",
        );
        let result = ProgramSources::load(&main);
        assert!(result.is_err());
    }

    #[test]
    fn cycle_detected() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "a.lake", "+b.x\npub x is { _ -> { } }");
        write_file(&dir, "b.lake", "+a.x\npub x is { _ -> { } }");
        let main = write_file(&dir, "main.lake", "+a.x\nmain is { _ -> { } }");
        let result = ProgramSources::load(&main);
        assert!(result.is_err(), "cycle should be reported");
    }

    #[test]
    fn dedupes_repeated_imports_into_one_load() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "core/io.lake",
            "pub a is { _ -> { } } pub b is { _ -> { } }",
        );
        let main = write_file(
            &dir,
            "main.lake",
            "+core.io.{ a b }\nmain is { _ -> { } }",
        );
        let sources = ProgramSources::load(&main).expect("loads");
        let count = sources
            .files()
            .iter()
            .filter(|f| f.module_path.display() == "core.io")
            .count();
        assert_eq!(count, 1, "core.io should be loaded once");
    }

    #[test]
    fn search_paths_per_lib_root_consumes_first_segment() {
        let dir = TempDir::new().unwrap();
        // STD_PATH points at <dir>/lake-std; module `std.io` resolves to
        // `<dir>/lake-std/io.lake` (the `std` segment is consumed by the
        // env var name).
        write_file(&dir, "lake-std/io.lake", "pub println is { s str -> { } }");
        let main = write_file(
            &dir,
            "main.lake",
            "+std.io.println\nmain is { _ -> { } }",
        );
        let mut search = SearchPaths::default();
        search.lib_roots.insert(
            "std".to_string(),
            dir.path().join("lake-std"),
        );
        let sources = ProgramSources::load_with_search(&main, search).expect("loads");
        assert!(
            sources
                .files()
                .iter()
                .any(|f| f.module_path.display() == "std.io"),
            "should find std.io via lib root"
        );
    }

    #[test]
    fn search_paths_lake_path_keeps_full_path() {
        let dir = TempDir::new().unwrap();
        // LAKE_PATH points at a generic search root; full module path
        // including the first segment is appended.
        write_file(
            &dir,
            "extra/std/io.lake",
            "pub println is { s str -> { } }",
        );
        let main = write_file(
            &dir,
            "main.lake",
            "+std.io.println\nmain is { _ -> { } }",
        );
        let mut search = SearchPaths::default();
        search.lake_paths.push(dir.path().join("extra"));
        let sources = ProgramSources::load_with_search(&main, search).expect("loads");
        assert!(
            sources
                .files()
                .iter()
                .any(|f| f.module_path.display() == "std.io"),
            "should find std.io via LAKE_PATH"
        );
    }

    #[test]
    fn search_paths_project_root_wins_over_env() {
        let dir = TempDir::new().unwrap();
        // Both project root and LAKE_PATH have a `std/io.lake`; the
        // project-root version wins.
        write_file(&dir, "std/io.lake", "pub LOCAL is { _ -> { } }");
        write_file(
            &dir,
            "extra/std/io.lake",
            "pub REMOTE is { _ -> { } }",
        );
        let main = write_file(
            &dir,
            "main.lake",
            "+std.io.LOCAL\nmain is { _ -> { } }",
        );
        let mut search = SearchPaths::default();
        search.lake_paths.push(dir.path().join("extra"));
        // Looking for `LOCAL` — only present in the project-root copy.
        let result = ProgramSources::load_with_search(&main, search);
        assert!(result.is_ok(), "project root copy should resolve");
    }

    // ── #070 Path pre-scan ─────────────────────────────────────────────

    #[test]
    fn prescan_loads_module_referenced_only_by_path_expr() {
        // `main.lake` never writes `+std.bytes.{ set }` but calls
        // `std:bytes:set()` via the Path form — pre-scan must queue
        // `std/bytes.lake` so the resolver can see the item.
        let dir = TempDir::new().unwrap();
        write_file(&dir, "std/bytes.lake", "pub set is { _ -> { } }");
        let main = write_file(
            &dir,
            "main.lake",
            "main is { _ -> { std:bytes:set() } }",
        );
        let sources = ProgramSources::load(&main).expect("loads");
        let mods: Vec<_> = sources
            .files()
            .iter()
            .map(|f| f.module_path.display())
            .collect();
        assert!(
            mods.contains(&"std.bytes".to_string()),
            "pre-scan should load std.bytes via Path expr; got {mods:?}",
        );
    }

    #[test]
    fn prescan_silent_when_path_module_does_not_resolve() {
        // Unknown module referenced via Path must NOT error at load —
        // typeck surfaces M005 with a proper span instead.
        let dir = TempDir::new().unwrap();
        let main = write_file(
            &dir,
            "main.lake",
            "main is { _ -> { ghost:module:fn(0) } }",
        );
        let result = ProgramSources::load(&main);
        assert!(result.is_ok(), "missing Path module should not error at load");
    }

    #[test]
    fn prescan_dedups_with_explicit_import() {
        // `+std.bytes.{ set }` AND `std:bytes:set()` should produce
        // exactly one load of `std.bytes`.
        let dir = TempDir::new().unwrap();
        write_file(&dir, "std/bytes.lake", "pub set is { _ -> { } }");
        let main = write_file(
            &dir,
            "main.lake",
            "+std.bytes.{ set }\nmain is { _ -> { std:bytes:set() } }",
        );
        let sources = ProgramSources::load(&main).expect("loads");
        let count = sources
            .files()
            .iter()
            .filter(|f| f.module_path.display() == "std.bytes")
            .count();
        assert_eq!(count, 1, "std.bytes should load exactly once");
    }

    #[test]
    fn prescan_loads_transitive_path_chain() {
        // main → uses `a:b:run()` (no +import) → a/b.lake itself uses
        // `c:d:helper()` (no +import either).  Both modules land.
        let dir = TempDir::new().unwrap();
        write_file(&dir, "c/d.lake", "pub helper is { _ -> { } }");
        write_file(
            &dir,
            "a/b.lake",
            "pub run is { _ -> { c:d:helper() } }",
        );
        let main = write_file(
            &dir,
            "main.lake",
            "main is { _ -> { a:b:run() } }",
        );
        let sources = ProgramSources::load(&main).expect("loads");
        let mods: Vec<_> = sources
            .files()
            .iter()
            .map(|f| f.module_path.display())
            .collect();
        assert!(mods.contains(&"a.b".to_string()), "got {mods:?}");
        assert!(mods.contains(&"c.d".to_string()), "got {mods:?}");
    }

    #[test]
    fn prescan_build_program_resolves_path_without_import() {
        // End-to-end: build_program over a main that calls
        // `std:bytes:set()` without `+std.bytes.{ set }`.  Pre-scan
        // pulls std.bytes into the load queue; registry must hold
        // the item and no E004 / M005 fires.
        use crate::prelude::build_program;

        let dir = TempDir::new().unwrap();
        write_file(&dir, "std/bytes.lake", "pub set is { _ -> { } }");
        let main = write_file(
            &dir,
            "main.lake",
            "main is { _ -> { std:bytes:set() } }",
        );
        let sources = ProgramSources::load(&main).expect("loads");
        let prog = build_program(&sources).expect("build_program succeeds");
        let std_bytes = ModulePath(vec!["std".to_string(), "bytes".to_string()]);
        let id = prog
            .registry
            .module_id_for_path(&std_bytes)
            .expect("std.bytes registered");
        assert!(
            prog.registry.module(id).machines.contains_key("set"),
            "registry should contain std.bytes.set",
        );
    }

    #[test]
    fn prescan_is_idempotent_under_repeated_load() {
        // Loading the same entry twice must produce identical file sets.
        let dir = TempDir::new().unwrap();
        write_file(&dir, "std/bytes.lake", "pub set is { _ -> { } }");
        let main = write_file(
            &dir,
            "main.lake",
            "main is { _ -> { std:bytes:set() } }",
        );
        let s1 = ProgramSources::load(&main).expect("loads");
        let s2 = ProgramSources::load(&main).expect("loads");
        let mods1: Vec<_> = s1.files().iter().map(|f| f.module_path.display()).collect();
        let mods2: Vec<_> = s2.files().iter().map(|f| f.module_path.display()).collect();
        assert_eq!(mods1, mods2, "repeated load must produce identical module set");
    }

    #[test]
    fn parse_all_returns_one_parsed_module_per_file() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "core/io.lake", "pub println is { s str -> { } }");
        let main = write_file(
            &dir,
            "main.lake",
            "+core.io.println\nmain is { _ -> { } }",
        );
        let sources = ProgramSources::load(&main).expect("loads");
        let parsed = sources.parse_all().expect("parses");
        assert_eq!(parsed.modules.len(), 2);
        assert!(parsed.by_path(&ModulePath::root()).is_some());
        assert!(parsed
            .by_path(&ModulePath(vec!["core".to_string(), "io".to_string()]))
            .is_some());
    }

    /// Bug #126: a parse error in module B must be tagged with B's
    /// path, never A's.  Without the tag the multi-file renderer
    /// would slice B's span into A's bytes and panic on a non-char
    /// boundary.
    #[test]
    fn parse_errors_tagged_with_owning_file() {
        let dir = TempDir::new().unwrap();
        // Module `core.io` has a deliberate syntax error after a
        // comment.  The comment is to validate spans don't drift.
        write_file(
            &dir,
            "core/io.lake",
            "// some comment that takes bytes\npub println is { @@@ }",
        );
        let main = write_file(
            &dir,
            "main.lake",
            "+core.io.println\nmain is { _ -> { } }",
        );
        // Load may surface parse errors itself (it parses each file to
        // discover transitive imports), or parse_all may later.  Either
        // way every error must be tagged with `core/io.lake`.
        let io_path = dir.path().join("core/io.lake");
        let errs = match ProgramSources::load(&main) {
            Ok(sources) => {
                let perr = sources.parse_all().expect_err("parse should fail");
                let mut bag = LakeErrors::default();
                for (_, e) in perr.per_file {
                    bag.extend(e);
                }
                bag
            }
            Err(e) => e,
        };
        assert!(!errs.is_empty(), "must have at least one parse error");
        for e in &errs.0 {
            assert_eq!(
                e.source_path.as_deref(),
                Some(io_path.as_path()),
                "every LakeError tagged with owning source path"
            );
        }
    }
}
