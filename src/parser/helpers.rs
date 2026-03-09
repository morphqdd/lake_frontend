use chumsky::{
    Parser,
    error::Rich,
    extra::Err,
    input::MappedInput,
    select_ref,
    span::{SimpleSpan, SpanWrap, Spanned},
};

use crate::api::{ast::Ident, token::Token};

pub fn ident_parser<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Ident<'src>>, Err<Rich<'t, Token<'src>>>> + Clone {
    select_ref!(Token::Ident(n) = e => Ident::new(n).with_span(e.span()))
}

pub type TokenInput<'t, 'src> =
    MappedInput<'t, Token<'src>, SimpleSpan, &'t [Spanned<Token<'src>>]>;
