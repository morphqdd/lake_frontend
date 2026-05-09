use std::path::Path;

use chumsky::{Parser, input::Input, span::Spanned};

use crate::api::ast::Item;
use crate::api::token::Token;
use crate::loader::{ParsedProgram, ProgramSources};
use crate::lowering::lower_program;
use crate::registry::ProgramRegistry;
use crate::resolver::resolve_program;
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
    let parsed = sources.parse_all().map_err(|errs| {
        // Flatten per-file ParseErrors into a single LakeErrors bag so
        // callers handle one shape.
        let mut bag = LakeErrors::default();
        for (_, e) in errs.per_file {
            bag.extend(e);
        }
        bag
    })?;

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

    let resolved = resolve_program(parsed, &mut registry);
    let errors = typecheck_program(&resolved, &registry);
    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(LakeProgram {
        program: resolved,
        registry,
    })
}
