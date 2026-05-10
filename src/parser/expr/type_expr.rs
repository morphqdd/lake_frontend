use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    input::Input,
    prelude::{choice, recursive},
    select_ref,
    span::Spanned,
};

use crate::{
    api::{ast::Type, token::Token},
    parser::helpers::{TokenInput, ident_parser},
};

/// Parses a type expression:
/// - `i32`, `str`              → `Type::Named`
/// - `box(T)`, `result({})`   → `Type::Generic`
/// - `{}`                     → `Type::Unit`
/// - `{ i64 usize }`          → `Type::Struct`
pub fn type_expr<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Type<'src>>, Err<Rich<'t, Token<'src>>>> {
    recursive(|ty| {
        // Struct / unit type inside `{ ... }`
        let struct_ty = ty
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span())))
            .map(|types: Vec<Spanned<Type<'src>>>| {
                if types.is_empty() {
                    Type::Unit
                } else {
                    Type::Struct(types)
                }
            })
            .spanned();

        // Simple ident, optionally followed by generic args `(T U)`
        let named = ident_parser()
            .then(
                ty.clone()
                    .repeated()
                    .collect::<Vec<_>>()
                    .nested_in(select_ref!(
                        Token::Parens(ts) = e => ts.split_spanned(e.span()),
                        Token::TightParens(ts) = e => ts.split_spanned(e.span()),
                    ))
                    .or_not(),
            )
            .map(|(name, args)| match args {
                None => Type::Named(name),
                Some(args) => Type::Generic(name, args),
            })
            .spanned();

        choice((struct_ty, named))
    })
}
