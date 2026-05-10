use chumsky::{
    IterParser, Parser, error::Rich, extra::Err, input::Input, prelude::just, select_ref,
    span::Spanned,
};

use crate::{
    api::{ast::Directive, token::Token},
    parser::{
        expr::type_expr::type_expr,
        helpers::{TokenInput, ident_parser},
    },
};

/// Parses a built-in macro attribute: `@rt_st(...)`, `@ffi(...)`.
/// Arguments inside the parens are parsed as a sequence of type expressions.
pub fn directive<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Directive<'src>>, Err<Rich<'t, Token<'src>>>> {
    just(Token::At)
        .ignore_then(ident_parser())
        .then(
            type_expr()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(
                    Token::Parens(ts) = e => ts.split_spanned(e.span()),
                    Token::TightParens(ts) = e => ts.split_spanned(e.span()),
                ))
                .or_not()
                .map(|args| args.unwrap_or_default()),
        )
        .map(|(name, args)| Directive::new(name, args))
        .spanned()
}
