use std::path::Path;

use chumsky::{Parser, input::Input, span::Spanned};

use crate::api::ast::Item;
use crate::api::token::Token;
use crate::cfg_filter::{CfgTarget, filter_program};
use crate::loader::{ParsedProgram, ProgramSources};
use crate::lowering::{lower_program, mangle_program, relift_program};
use crate::mono::monomorphise_program;
use crate::registry::ProgramRegistry;
use crate::resolver::resolve_program_with_errors;
use crate::typeck::typecheck_program;
use crate::{error_handle::LakeErrors, lexer::lexer, parser::program};

pub use crate::resolver::resolve;
pub use crate::semantic::analyze;
pub use crate::typeck::typecheck;

/// Parse a Lake source file.  Returns the AST (a flat list of top-level
/// items) on success or a collection of errors that can be displayed with
/// [`LakeErrors::display`].
///
/// Lex errors are tagged `L001`; parse errors are tagged `P001`.
pub fn parse<'src, P: AsRef<Path>>(
    _path: P,
    src: &'src str,
) -> Result<Vec<Spanned<Item<'src>>>, LakeErrors> {
    let tokens = lexer()
        .parse(src)
        .into_result()
        .map_err(LakeErrors::from_lex_errs)?;

    program()
        .parse(tokens[..].split_spanned((0..src.len()).into()))
        .into_result()
        .map_err(LakeErrors::from_parse_errs)
}

/// Run the full frontend pipeline: **lex → parse → resolve → typecheck**.
///
/// Unlike [`parse`], this function continues past recoverable lex/parse errors
/// and accumulates diagnostics from every stage before returning.  Use this as
/// the single entry point for compilation.
#[allow(clippy::type_complexity)]
pub fn build_ast<'src, P: AsRef<Path>>(
    _path: P,
    src: &'src str,
) -> Result<
    (Vec<Spanned<Token<'src>>>, Vec<Spanned<Item<'src>>>),
    (Vec<Spanned<Token<'src>>>, LakeErrors),
> {
    let mut all_errors = LakeErrors::default();

    let (tokens, lex_errs) = lexer().parse(src).into_output_errors();
    all_errors.extend(LakeErrors::from_lex_errs(lex_errs));

    let Some(tokens) = tokens else {
        return Err((vec![], all_errors));
    };

    let (ast, parse_errs) = program()
        .parse(tokens[..].split_spanned((0..src.len()).into()))
        .into_output_errors();
    all_errors.extend(LakeErrors::from_parse_errs(parse_errs));

    let Some(ast) = ast else {
        return Err((tokens, all_errors));
    };

    let resolved = resolve(ast);
    all_errors.extend(typecheck(resolved.clone()));

    if all_errors.is_empty() {
        Ok((tokens, resolved))
    } else {
        Err((tokens, all_errors))
    }
}

/// The fully-cooked compilation context: every loaded source file plus the
/// resolved, type-checked, registry-aware AST.  Backends pull machines and
/// directives out of `program.modules`, signature lookups out of `registry`.
///
/// Lifetime parameter `'src` ties [`ParsedProgram`] and [`ProgramRegistry`]
/// to the underlying [`ProgramSources`].  In normal usage the caller owns
/// the [`ProgramSources`] and produces this struct from
/// [`build_program`].
pub struct LakeProgram<'src> {
    pub program: ParsedProgram<'src>,
    pub registry: ProgramRegistry<'src>,
}

/// Multi-file entry point: load the entry source plus all its transitive
/// `+import` dependencies, parse them, populate the [`ProgramRegistry`],
/// resolve types, and run typeck.  Returns owned [`ProgramSources`]
/// alongside the borrowed [`LakeProgram`] so the caller can keep both
/// alive for the rest of the compilation.
///
/// Errors from any stage are accumulated and surfaced together.  The
/// `ProgramSources` is returned even on error so callers can render
/// diagnostics with the original source text.
#[allow(clippy::type_complexity)]
pub fn load_and_build<P: AsRef<Path>>(
    entry: P,
) -> Result<ProgramSources, LakeErrors> {
    ProgramSources::load(entry)
}

/// Borrow phase of the multi-file pipeline.  Given owned
/// [`ProgramSources`], parse every file, build the registry, resolve
/// types, run typeck, and return the resulting [`LakeProgram`].
///
/// Errors from any stage are accumulated.  Lex / parse errors are
/// tagged per-file; resolver / typeck diagnostics already carry their
/// own codes.
pub fn build_program<'src>(
    sources: &'src ProgramSources,
) -> Result<LakeProgram<'src>, LakeErrors> {
    build_program_for_target(sources, host_arch_str())
}

/// Host architecture as the canonical `@cfg(arch="...")` string.
/// Defaults the target when the backend does not pass `--target`.
pub fn host_arch_str() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        // Unknown host: hand back the raw arch so `@cfg` simply never
        // matches x86_64/aarch64 predicates rather than panicking here.
        other => other,
    }
}

/// Like [`build_program`] but filters `@cfg(arch="...")` items against an
/// explicit target arch string (`"x86_64"` / `"aarch64"`).  The backend
/// threads its `--target` choice in here so stdlib's per-arch syscall
/// constants resolve to the right value before registry / typeck / codegen
/// ever run.  See docs/state/features/054_cfg.md.
pub fn build_program_for_target<'src>(
    sources: &'src ProgramSources,
    target_arch: &str,
) -> Result<LakeProgram<'src>, LakeErrors> {
    let parsed = sources.parse_all().map_err(|errs| {
        // Flatten per-file ParseErrors into a single LakeErrors bag,
        // tagging each error with the file its span belongs to so the
        // multi-file renderer routes it correctly (bug #126).
        let mut bag = LakeErrors::default();
        for (path, e) in errs.per_file {
            bag.extend(e.tag_source_path(&path));
        }
        bag
    })?;

    // #054 — drop `@cfg(arch=...)` items whose arch != target BEFORE the
    // registry sees them, so two per-arch items with the same name never
    // collide.  Malformed `@cfg` surfaces as E041.
    let (parsed, cfg_errors) = filter_program(parsed, CfgTarget::new(target_arch));
    if !cfg_errors.is_empty() {
        let mut bag = LakeErrors::default();
        for e in cfg_errors {
            bag.push(e);
        }
        return Err(bag);
    }

    let mut registry: ProgramRegistry<'src> = ProgramRegistry::with_rt();
    if let Err(e) = registry.populate_from(&parsed) {
        return Err(e);
    }

    // #142 phases 2-3 — monomorphise generic records / machines into
    // concrete copies.  Runs before `lower_program` so the lowering
    // pass sees only ordinary (non-generic) records.  Re-populate the
    // registry afterwards so freshly-emitted mono'd items are visible
    // to downstream passes.
    let (parsed, mono_errors) = monomorphise_program(parsed, &registry);
    if !mono_errors.is_empty() {
        let mut bag = LakeErrors::default();
        for e in mono_errors {
            bag.push(e);
        }
        return Err(bag);
    }
    let mut registry: ProgramRegistry<'src> = ProgramRegistry::with_rt();
    if let Err(e) = registry.populate_from(&parsed) {
        return Err(e);
    }

    // Desugar `ret`-typed machines and their callers using the registry
    // we just built.  Then re-populate so machine signatures reflect the
    // newly-prepended `__caller` parameter on every ret-branch.
    let parsed = lower_program(parsed, &registry);
    if let Err(e) = registry.populate_from(&parsed) {
        return Err(e);
    }

    let (resolved, resolver_errors) = resolve_program_with_errors(parsed, &mut registry);
    // #145 phase 4 — surface resolver diagnostics
    // (E030/E031/E032) emitted by the enum-aware rewrites.  Without
    // this the pipeline silently dropped the non-exhaustive `when`
    // warning along with constructor arity / unknown-variant errors.
    if !resolver_errors.is_empty() {
        let mut bag = LakeErrors::default();
        for e in resolver_errors {
            bag.push(e);
        }
        return Err(bag);
    }
    // #145 phase 4/5 — resolver rewrites variant patterns
    // (`Ok(n)` → tagged tuple) and constructor calls after
    // `lower_program` has already run, so re-fire the two
    // resolver-aware transforms (`lower_when_tuple_patterns` +
    // `lift_nested_calls`) over the freshly-emitted shapes.
    // See docs/state/features/145_enums.md.
    let resolved = relift_program(resolved, &registry);
    let errors = typecheck_program(&resolved, &registry);
    if !errors.is_empty() {
        return Err(errors);
    }

    // Final pass — rewrite machine definitions + call sites to canonical
    // module-qualified symbols so the backend's flat symbol table sees no
    // collisions and re-exports collapse to one predeclare (#097, #102).
    let mangled = mangle_program(resolved, &registry);

    Ok(LakeProgram {
        program: mangled,
        registry,
    })
}
