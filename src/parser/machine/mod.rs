use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::Input,
    prelude::{choice, just},
    select_ref,
    span::Spanned,
};
// agent: generics-142 — see docs/state/features/142_generics.md

use crate::{
    api::{
        ast::{Branch, Field, Ident, Machine, MachineItem},
        token::Token,
    },
    parser::{
        branch::branch,
        helpers::{TokenInput, ident_parser, type_param_bounds_body},
        machine::field_decl::field_decl,
    },
};

/// Type-parameter clause shared shape: ordered names + parallel bounds.
type Generics<'src> = (
    Vec<chumsky::span::Spanned<Ident<'src>>>,
    Vec<(
        chumsky::span::Spanned<Ident<'src>>,
        Vec<chumsky::span::Spanned<Ident<'src>>>,
    )>,
);

mod field_decl;

/// Tries `field_decl` first (has `.` after `@label`), then falls back to `branch`.
fn machine_item<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<MachineItem<'src>>, Err<Rich<'t, Token<'src>>>> {
    choice((
        field_decl()
            .map(|f: Spanned<Field>| MachineItem::Field(f.inner))
            .spanned(),
        branch()
            .map(|b: Spanned<Branch>| MachineItem::Branch(b.inner))
            .spanned(),
    ))
}

/// Parses `[pub] ident[(generics) | [generics]] is { item* }`.
///
/// Two equivalent generic-parameter syntaxes are accepted:
///   * `name(T U)` — historical paren-form, predates #142.
///   * `name[T U]` — bracket form introduced by #142 to align records
///     and machines.  See docs/state/features/142_generics.md.
pub fn machine<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Machine<'src>>, Err<Rich<'t, Token<'src>>>> {
    // Historical `(T U)` form — kept for back-compat with stdlib code
    // that predates the bracket syntax.  No bounds in paren form.
    let paren_generics = ident_parser()
        .repeated()
        .collect::<Vec<_>>()
        .nested_in(select_ref!(
            Token::Parens(ts) = e => ts.split_spanned(e.span()),
            Token::TightParens(ts) = e => ts.split_spanned(e.span()),
        ))
        .map(|g: Vec<_>| -> Generics { (g, Vec::new()) });

    // agent: generics-142 / protocols-146 — `[T U ...]` / `[T: Eq Ord]`
    // form.  Both spaced and tight bracket tokens accepted.  The body
    // parser also captures proto bounds (`T: Eq`).
    let bracket_generics = type_param_bounds_body().nested_in(select_ref!(
        Token::SquareBrackets(ts) = e => ts.split_spanned(e.span()),
        Token::TightSquareBrackets(ts) = e => ts.split_spanned(e.span()),
    ));

    just(Token::Pub)
        .or_not()
        .map(|p| p.is_some())
        .then(ident_parser())
        .then(
            choice((bracket_generics, paren_generics))
                .or_not()
                .map(|g: Option<Generics>| g.unwrap_or_default()),
        )
        .then_ignore(just(Token::Is))
        .then(
            machine_item()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
        )
        .map(|(((vis, ident), (generics, bounds)), items)| {
            Machine::with_bounds(vec![], vis, ident, generics, bounds, items)
        })
        .spanned()
}
