use chumsky::{
    Parser,
    error::Rich,
    extra::Err,
    prelude::{choice, just},
    select_ref,
    span::{SpanWrap, Spanned},
};

use crate::{
    api::{
        ast::{ConstDecl, Ident, Type},
        expr::Expr,
        token::Token,
    },
    parser::helpers::{TokenInput, ident_parser},
};

/// Parse `[pub] const IDENT = <literal>`.
///
/// RHS is restricted to a literal expression (Num / String / Bool / Atom).
/// The resolver and typeck refuse non-literal RHS — this parser just
/// rejects the cases that can't possibly be reduced at compile time.
pub fn const_item<'t, 'src: 't>()
-> impl Parser<'t, TokenInput<'t, 'src>, Spanned<ConstDecl<'src>>, Err<Rich<'t, Token<'src>>>> {
    let num = select_ref! {
        Token::Num(n) = e => Expr::Num(n, Type::Named(Ident::new("i64").with_span(e.span()))).with_span(e.span()),
    };
    let string = select_ref! {
        Token::String(s) = e => Expr::String(s, Type::Named(Ident::new("str").with_span(e.span()))).with_span(e.span()),
    };
    let bool_true = just(Token::True).map_with(|_, e| Expr::Bool(true).with_span(e.span()));
    let bool_false = just(Token::False).map_with(|_, e| Expr::Bool(false).with_span(e.span()));
    let atom_lit = just(Token::Colon)
        .ignore_then(select_ref! { Token::Ident(n) = e => (n, e.span()) })
        .map(|(n, span)| Expr::Atom(n).with_span(span));

    let literal_rhs = choice((num, string, bool_true, bool_false, atom_lit));

    just(Token::Pub)
        .or_not()
        .map(|p| p.is_some())
        .then_ignore(just(Token::Const))
        .then(ident_parser())
        .then_ignore(just(Token::Eq))
        .then(literal_rhs)
        .map_with(|((vis, ident), value), e| {
            ConstDecl::new(vis, ident, value).with_span(e.span())
        })
}
