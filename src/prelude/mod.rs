use std::path::Path;

use chumsky::{Parser, input::Input, span::Spanned};

use crate::{api::expr::Expr, error_handle::LakeErrors, lexer::lexer, parser::program};

/// Parse a Lake source file.  Returns the AST on success or a collection of
/// errors that can be displayed with [`LakeErrors::display`].
pub fn parse<'src, P: AsRef<Path>>(
    _path: P,
    src: &'src str,
) -> Result<Vec<Spanned<Expr<'src>>>, LakeErrors> {
    let tokens = lexer()
        .parse(src)
        .into_result()
        .map_err(LakeErrors::from_rich_vec)?;

    program()
        .parse(tokens[..].split_spanned((0..src.len()).into()))
        .into_result()
        .map_err(LakeErrors::from_rich_vec)
}
