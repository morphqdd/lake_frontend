use chumsky::{
    IterParser, Parser, error::Rich, extra::Err, input::Input, prelude::just, select_ref,
    span::Spanned,
};

use crate::{
    api::{ast::Branch, token::Token},
    parser::{
        expr::{expr, type_expr::type_expr},
        helpers::{TokenInput, ident_parser},
        pattern::pattern,
    },
};

/// Parses a process branch: `[@label] pattern+ -> [ret type] { body }`
pub fn branch<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Branch<'src>>, Err<Rich<'t, Token<'src>>>> {
    just(Token::At)
        .ignore_then(ident_parser())
        .or_not()
        .then(pattern().repeated().at_least(1).collect::<Vec<_>>())
        .then_ignore(just(Token::Arrow))
        .then(just(Token::Ret).ignore_then(type_expr()).or_not())
        .then(
            expr()
                .repeated()
                .collect::<Vec<_>>()
                .nested_in(select_ref!(Token::CurlyBrackets(ts) = e => ts.split_spanned(e.span()))),
        )
        .map(|(((label, patterns), ret_ty), body)| Branch::new(label, patterns, ret_ty, body))
        .spanned()
}
