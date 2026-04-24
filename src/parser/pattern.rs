use chumsky::{
    Parser,
    error::Rich,
    extra::Err,
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

/// Parses a branch parameter: `n i32`, `_`, `0 i64`, or `"hello" str`.
///
/// `_` is the only pattern that may omit the type annotation.
/// Numeric literals (`0 i64`) and string literals (`"hello" str`) are literal
/// guards: the branch only matches when the argument equals that value.
pub fn pattern<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<Pattern<'src>>, Err<Rich<'t, Token<'src>>>> {
    // String literal guard: `"hello" str`
    let string_guard = select_ref!(
        Token::String(n) = e => Ident::new(n).with_span(e.span()),
    )
    .then(type_expr())
    .map(|(ident, ty)| Pattern::new_string_guard(ident, ty))
    .spanned();

    // Normal patterns: ident or numeric literal + optional type.
    let normal = select_ref!(
        Token::Ident(n) = e => Ident::new(n).with_span(e.span()),
        Token::Num(n)   = e => Ident::new(n).with_span(e.span()),
    )
    .then(type_expr().or_not())
    .map(|(ident, opt_ty)| {
        let ty = opt_ty.unwrap_or_else(|| Type::Unit.with_span(ident.span));
        Pattern::new(ident, ty)
    })
    .spanned();

    string_guard.or(normal)
}
