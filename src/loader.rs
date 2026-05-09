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

use crate::api::ast::{Import, Item};
use crate::api::expr::Expr;
use crate::error_handle::{LakeError, LakeErrors};
use crate::lexer::lexer;
use crate::parser::program;
use crate::registry::ModulePath;

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
    /// becomes the project root used for resolving `+core.io.writer`-style
    /// imports to filesystem paths.
    pub fn load<P: AsRef<Path>>(entry_path: P) -> Result<Self, LakeErrors> {
        let entry_path = entry_path.as_ref().to_path_buf();
        let project_root = entry_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let mut sources = ProgramSources::default();
        let mut in_progress: HashSet<ModulePath> = HashSet::new();

        sources.load_recursive(
            ModulePath::root(),
            entry_path,
            &project_root,
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
                    errors.push(&f.source_path, LakeErrors::from_lex_errs(errs));
                    continue;
                }
            };
            let ast = match program()
                .parse(tokens[..].split_spanned((0..f.src.len()).into()))
                .into_result()
            {
                Ok(a) => a,
                Err(errs) => {
                    errors.push(&f.source_path, LakeErrors::from_parse_errs(errs));
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
        project_root: &Path,
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

        // Extract imports via a temporary parse — drops at end of scope so
        // we can mutate `self.files` later without holding a borrow.
        let imports = {
            let last = self
                .files
                .last()
                .expect("just pushed");
            let tokens = match lexer().parse(last.src.as_str()).into_result() {
                Ok(t) => t,
                Err(errs) => {
                    in_progress.remove(&module_path);
                    return Err(LakeErrors::from_lex_errs(errs));
                }
            };
            let ast = match program()
                .parse(tokens[..].split_spanned((0..last.src.len()).into()))
                .into_result()
            {
                Ok(a) => a,
                Err(errs) => {
                    in_progress.remove(&module_path);
                    return Err(LakeErrors::from_parse_errs(errs));
                }
            };
            collect_referenced_modules(&ast)
        };

        for sub in imports {
            let sub_path = resolve_module_file(&sub, project_root)
                .ok_or_else(|| LakeErrors::new(vec![module_not_found(&sub, project_root)]))?;
            self.load_recursive(sub, sub_path, project_root, in_progress)?;
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
/// deduplicated list — multiple `+core.io.{ a b }` entries that share the
/// same target file produce one entry.
fn collect_referenced_modules(ast: &[Spanned<Item<'_>>]) -> Vec<ModulePath> {
    let mut out = HashSet::<ModulePath>::new();
    for item in ast {
        if let Item::Import(rc) = &item.inner {
            collect_from_import(&rc.inner, &[], &mut out);
        }
    }
    let mut v: Vec<_> = out.into_iter().collect();
    // Stable order so error messages and load sequence are deterministic.
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn collect_from_import(
    node: &Rc<RefCell<Import<'_>>>,
    prefix: &[String],
    out: &mut HashSet<ModulePath>,
) {
    match &*node.borrow() {
        Import::Import(ident, Some(next), _alias) => {
            let mut new_prefix = prefix.to_vec();
            new_prefix.push(ident.inner.0.to_string());
            collect_from_import(&next.inner, &new_prefix, out);
        }
        Import::Import(_ident, None, _alias) => {
            // Leaf reached.  `prefix` is the module path; the leaf's ident
            // is the item NAME within that module — not part of the file
            // path.  An import like `+core.io.writer` therefore refers to
            // file `core/io.lake` (path ["core", "io"]) and item `writer`.
            if !prefix.is_empty() {
                out.insert(ModulePath(prefix.to_vec()));
            }
        }
        Import::MultiImport(entries) => {
            for entry in entries {
                collect_from_import(&entry.inner, prefix, out);
            }
        }
    }
}

/// Resolve a [`ModulePath`] to an absolute filesystem path inside
/// `project_root`.  Currently only the project root is searched; richer
/// search paths land with #29.
fn resolve_module_file(module: &ModulePath, project_root: &Path) -> Option<PathBuf> {
    let candidate = project_root.join(module.to_filesystem());
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
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

fn module_not_found(module: &ModulePath, project_root: &Path) -> LakeError {
    LakeError::new(
        format!(
            "module `{}` not found (looked for `{}` under `{}`)",
            module.display(),
            module.to_filesystem().display(),
            project_root.display()
        ),
        (0..0).into(),
    )
    .code("M002")
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
}
