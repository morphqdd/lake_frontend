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

/// Parses `[pub] ident[<T, U, ...>] is { field* }` where every item is
/// `<ident> <type>`.  The optional `<T, ...>` clause is the phase-1
/// surface for #142 generics — see docs/state/features/142_generics.md.
///
/// Placed before `machine()` in the top-level `choice` so chumsky can
/// backtrack to `machine()` if the body contains `->` or `@` items.
pub fn record<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<RecordDecl<'src>>, Err<Rich<'t, Token<'src>>>> {
    // agent: generics-142 — optional `<T U ...>` after the record name.
    // Lake has no comma token; type parameters are whitespace-separated
    // like every other list construct in the grammar.
    let type_param_list = just(Token::Less)
        .ignore_then(
            ident_parser()
                .repeated()
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then_ignore(just(Token::Greater));

    just(Token::Pub)
        .or_not()
        .map(|p| p.is_some())
        .then(ident_parser())
        .then(type_param_list.or_not().map(|p| p.unwrap_or_default()))
        .then_ignore(just(Token::Is))
        .then(
            record_field()
                .repeated()
                .at_least(1)
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
        )
        .map(|(((vis, ident), type_params), fields)| RecordDecl {
            vis,
            ident,
            type_params,
            fields,
        })
        .spanned()
}
