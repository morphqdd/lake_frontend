use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::Input,
    prelude::{just, recursive},
    select_ref,
    span::{SpanWrap, Spanned},
};

use crate::{
    api::{
        ast::{Ident, Pattern, Type},
        token::Token,
    },
    parser::{
        expr::type_expr::type_expr,
        helpers::TokenInput,
    },
};

/// Parses a bare (type-annotation-free) sub-pattern for use inside `{ }`.
/// Grammar: atom-guard | nested-tuple | ident/num/wildcard (no type).
pub(super) fn bare_pattern<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Pattern<'src>>, Err<Rich<'t, Token<'src>>>> {
    recursive(|bare| {
        // Atom guard: `:ok`, `:err`
        let atom = just(Token::Colon)
            .ignore_then(select_ref!(Token::Ident(n) = e => (*n, e.span())))
            .map(|(name, span)| Pattern::new_atom_guard(name, span))
            .spanned()
            .boxed();

        // Nested tuple: `{ elem+ }` — recursive
        let nested = bare
            .repeated()
            .at_least(1)
            .collect::<Vec<_>>()
            .nested_in(
                select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())),
            )
            .map_with(|elems, extra| Pattern::new_tuple(elems, extra.span()))
            .spanned()
            .boxed();

        // Bare ident / wildcard / numeric guard — NO type annotation.
        let bare_ident = select_ref!(
            Token::Ident(n) = e => Ident::new(*n).with_span(e.span()),
            Token::Num(n)   = e => Ident::new(*n).with_span(e.span()),
        )
        .map(|ident: Spanned<Ident<'src>>| {
            let span = ident.span;
            Pattern::new(ident, Type::Unit.with_span(span))
        })
        .spanned()
        .boxed();

        atom.or(nested).or(bare_ident)
    })
}

/// Parses a branch parameter: `n i32`, `_`, `0 i64`, `"hello" str`,
/// `:ok` (atom guard), or `{ a b _ }` (tuple destructure, recursive).
///
/// `_` is the only pattern that may omit the type annotation.
/// Numeric literals (`0 i64`) and string literals (`"hello" str`) are literal
/// guards: the branch only matches when the argument equals that value.
/// Atom guards (`:ok`) match when the first tuple field equals that atom.
/// Tuple patterns (`{ a b }`) destructure an anonymous tuple positionally.
/// Sub-elements of a tuple pattern carry no type annotation; the boundary
/// between elements is positional.
pub fn pattern<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Pattern<'src>>, Err<Rich<'t, Token<'src>>>> {
    // Outer tuple pattern: `{ bare_elem+ }` — uses bare_pattern inside.
    let tuple_pat = bare_pattern()
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .nested_in(
            select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())),
        )
        .map_with(|elems, extra| Pattern::new_tuple(elems, extra.span()))
        .spanned()
        .boxed();

    // String literal guard: `"hello" str`
    let string_guard = select_ref!(
        Token::String(n) = e => Ident::new(n).with_span(e.span()),
    )
    .then(type_expr())
    .map(|(ident, ty)| Pattern::new_string_guard(ident, ty))
    .spanned()
    .boxed();

    // Atom guard: `:ok`, `:err` (top-level, no type annotation)
    let atom_guard = just(Token::Colon)
        .ignore_then(select_ref!(Token::Ident(n) = e => (*n, e.span())))
        .map(|(name, span)| Pattern::new_atom_guard(name, span))
        .spanned()
        .boxed();

    // Normal patterns: ident or numeric literal + optional type.
    let normal = select_ref!(
        Token::Ident(n) = e => Ident::new(n).with_span(e.span()),
        Token::Num(n)   = e => Ident::new(n).with_span(e.span()),
    )
    .then(type_expr().or_not())
    .map(|(ident, opt_ty): (Spanned<Ident<'src>>, _)| {
        let ty = opt_ty.unwrap_or_else(|| Type::Unit.with_span(ident.span));
        Pattern::new(ident, ty)
    })
    .spanned()
    .boxed();

    string_guard.or(atom_guard).or(tuple_pat).or(normal)
}
