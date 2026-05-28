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
        ast::{Branch, Field, Machine, MachineItem},
        token::Token,
    },
    parser::{
        branch::branch,
        helpers::{TokenInput, ident_parser},
        machine::field_decl::field_decl,
    },
};

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
    // that predates the bracket syntax.
    let paren_generics = ident_parser()
        .repeated()
        .collect::<Vec<_>>()
        .nested_in(select_ref!(
            Token::Parens(ts) = e => ts.split_spanned(e.span()),
            Token::TightParens(ts) = e => ts.split_spanned(e.span()),
        ));

    // agent: generics-142 — `[T U ...]` form.  Both spaced and tight
    // bracket tokens accepted (e.g. `id [T]` and `id[T]`).
    let bracket_generics = ident_parser()
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .nested_in(select_ref!(
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
                .map(|g| g.unwrap_or_default()),
        )
        .then_ignore(just(Token::Is))
        .then(
            machine_item()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
        )
        .map(|(((vis, ident), generics), items)| Machine::new(vec![], vis, ident, generics, items))
        .spanned()
}
