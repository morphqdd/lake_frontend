use chumsky::{
    Parser,
    error::Rich,
    extra::Err,
    prelude::just,
    span::{SpanWrap, Spanned},
};

use crate::{
    api::{
        ast::{Pattern, Type},
        token::Token,
    },
    parser::{
        expr::{expr, type_expr::type_expr},
        helpers::{TokenInput, ident_parser},
    },
};

/// Parses a branch parameter: `n i32`, `n str."hello"`, `_`
///
/// `_` is the only pattern that may omit the type annotation.
pub fn pattern<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Pattern<'src>>, Err<Rich<'t, Token<'src>>>> {
    ident_parser()
        .then(
            type_expr()
                .then(just(Token::Dot).ignore_then(expr()).or_not())
                .or_not(),
        )
        .map(|(ident, opt)| {
            let (ty, default) = match opt {
                Some((ty, default)) => (ty, default),
                None => (Type::Unit.with_span(ident.span), None),
            };
            Pattern::new(ident, ty, default)
        })
        .spanned()
}
