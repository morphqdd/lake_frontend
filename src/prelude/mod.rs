use std::path::Path;

use chumsky::{Parser, input::Input, span::Spanned};

use crate::{api::ast::Process, error_handle::parse_failure, lexer::lexer, parser::process};

pub fn parse<'src, P: AsRef<Path>>(path: P, src: &'src str) -> Spanned<Process<'src>> {
    let tokens = lexer()
        .parse(&src)
        .into_result()
        .unwrap_or_else(|errs| parse_failure(errs, &src, &path));
    process()
        .parse(tokens[..].split_spanned((0..src.len()).into()))
        .into_result()
        .unwrap_or_else(|errs| parse_failure(errs, &src, &path))
}
