use std::path::Path;

use chumsky::{Parser, input::Input, span::Spanned};

use crate::{api::expr::Expr, error_handle::LakeErrors, lexer::lexer, parser::program};

pub use crate::resolver::resolve;
pub use crate::typeck::typecheck;

/// Parse a Lake source file.  Returns the AST on success or a collection of
/// errors that can be displayed with [`LakeErrors::display`].
///
/// Lex errors are tagged `L001`; parse errors are tagged `P001`.
pub fn parse<'src, P: AsRef<Path>>(
    _path: P,
    src: &'src str,
) -> Result<Vec<Spanned<Expr<'src>>>, LakeErrors> {
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
pub fn build_ast<'src, P: AsRef<Path>>(
    _path: P,
    src: &'src str,
) -> Result<Vec<Spanned<Expr<'src>>>, LakeErrors> {
    let mut all_errors = LakeErrors::default();

    // Stage 1 — lex
    let (tokens, lex_errs) = lexer().parse(src).into_output_errors();
    all_errors.extend(LakeErrors::from_lex_errs(lex_errs));

    let Some(tokens) = tokens else {
        return Err(all_errors);
    };

    // Stage 2 — parse
    let (ast, parse_errs) = program()
        .parse(tokens[..].split_spanned((0..src.len()).into()))
        .into_output_errors();
    all_errors.extend(LakeErrors::from_parse_errs(parse_errs));

    let Some(ast) = ast else {
        return Err(all_errors);
    };

    // Stage 3 — resolve + typecheck
    let resolved = resolve(ast);
    all_errors.extend(typecheck(resolved.clone()));

    if all_errors.is_empty() {
        Ok(resolved)
    } else {
        Err(all_errors)
    }
}
