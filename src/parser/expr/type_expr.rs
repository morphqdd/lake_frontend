use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra::Err,
    prelude::{choice, just, recursive},
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
/// - `Vec<T>`, `Map<K V>`     → `Type::NamedGeneric` (agent: generics-142)
/// - `{}`                     → `Type::Unit`
/// - `{ i64 usize }`          → `Type::Struct`
pub fn type_expr<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Type<'src>>, Err<Rich<'t, Token<'src>>>> {
    recursive(|ty| {
        use chumsky::input::Input;
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

        // Simple ident, optionally followed by:
        //   * `(T U)` — historical paren-form generic (Type::Generic)
        //   * `<T U>` — agent: generics-142 angle-bracket form
        //                (Type::NamedGeneric)
        let paren_args = ty
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .nested_in(select_ref!(
                Token::Parens(ts) = e => ts.split_spanned(e.span()),
                Token::TightParens(ts) = e => ts.split_spanned(e.span()),
            ));

        let angle_args = just(Token::Less)
            .ignore_then(
                ty.clone()
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .then_ignore(just(Token::Greater));

        let named = ident_parser()
            .then(choice((
                paren_args.map(|args| Some((false, args))),
                angle_args.map(|args| Some((true, args))),
                chumsky::prelude::empty().to(None),
            )))
            .map(|(name, args)| match args {
                None => Type::Named(name),
                Some((false, args)) => Type::Generic(name, args),
                Some((true, args)) => Type::NamedGeneric { name, args },
            })
            .spanned();

        choice((struct_ty, named))
    })
}
