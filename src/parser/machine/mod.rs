use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::Input,
    prelude::{choice, just},
    select_ref,
    span::Spanned,
};

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

/// Parses `[pub] ident[(generics)] is { item* }`
pub fn machine<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Machine<'src>>, Err<Rich<'t, Token<'src>>>> {
    just(Token::Pub)
        .or_not()
        .map(|p| p.is_some())
        .then(ident_parser())
        .then(
            ident_parser()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(
                    Token::Parens(ts) = e => ts.split_spanned(e.span()),
                    Token::TightParens(ts) = e => ts.split_spanned(e.span()),
                ))
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
