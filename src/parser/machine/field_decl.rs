use chumsky::{Parser, error::Rich, extra::Err, prelude::just, span::Spanned};

use crate::{
    api::{ast::Field, token::Token},
    parser::{
        expr::type_expr::type_expr,
        helpers::{TokenInput, ident_parser},
    },
};

/// Parses a field declaration: `@ok.T`, `@.raw_box(T)`, `@.{ str }`
pub fn field_decl<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Field<'src>>, Err<Rich<'t, Token<'src>>>> {
    just(Token::At)
        .ignore_then(ident_parser().or_not())
        .then_ignore(just(Token::Dot))
        .then(type_expr())
        .map(|(label, ty)| Field::new(label, ty))
        .spanned()
}
