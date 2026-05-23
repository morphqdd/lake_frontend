use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::Input,
    prelude::just,
    select_ref,
    span::Spanned,
};

use crate::{
    api::{ast::{RecordDecl, RecordField}, token::Token},
    parser::{
        expr::type_expr::type_expr,
        helpers::{TokenInput, ident_parser},
    },
};

fn record_field<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<RecordField<'src>>, Err<Rich<'t, Token<'src>>>> {
    ident_parser()
        .then(type_expr())
        .map(|(name, ty)| RecordField { name, ty })
        .spanned()
}

/// Parses `[pub] ident is { field* }` where every item is `<ident> <type>`.
///
/// Placed before `machine()` in the top-level `choice` so chumsky can
/// backtrack to `machine()` if the body contains `->` or `@` items.
pub fn record<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<RecordDecl<'src>>, Err<Rich<'t, Token<'src>>>> {
    just(Token::Pub)
        .or_not()
        .map(|p| p.is_some())
        .then(ident_parser())
        .then_ignore(just(Token::Is))
        .then(
            record_field()
                .repeated()
                .at_least(1)
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
        )
        .map(|((vis, ident), fields)| RecordDecl { vis, ident, fields })
        .spanned()
}
