use std::path::Path;

use chumsky::{Parser, input::Input, span::Spanned};

use crate::{api::ast::Machine, error_handle::parse_failure, lexer::lexer, parser::program};

pub fn parse<'src, P: AsRef<Path>>(path: P, src: &'src str) -> Vec<Spanned<Machine<'src>>> {
    let tokens = lexer()
        .parse(&src)
        .into_result()
        .unwrap_or_else(|errs| parse_failure(errs, &src, &path));
    program()
        .parse(tokens[..].split_spanned((0..src.len()).into()))
        .into_result()
        .unwrap_or_else(|errs| parse_failure(errs, &src, &path))
}
