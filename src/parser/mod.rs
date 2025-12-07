use chumsky::{
    Parser,
    error::Rich,
    extra::Err,
    input::{Input, MappedInput},
    pratt::{Operator, infix, left},
    prelude::{choice, empty, just, recursive},
    select_ref,
    span::{SimpleSpan, SpanWrap, Spanned},
};

use crate::api::{expr::Expr, token::Token};

pub fn parser<'tokens, 'src: 'tokens>() -> impl Parser<
    'tokens,
    MappedInput<'tokens, Token<'src>, SimpleSpan, &'tokens [Spanned<Token<'src>>]>,
    Spanned<Expr<'src>>,
    Err<Rich<'tokens, Token<'src>>>,
> {
    recursive(|expr| {
        let ident = select_ref! { Token::Ident(n) => *n };
        let atom = choice((
            select_ref! { Token::Num(n) => Expr::Num(*n) },
            select_ref! { Token::String(n) => Expr::String(*n) },
            just(Token::False).to(Expr::Bool(false)),
            just(Token::True).to(Expr::Bool(true)),
            ident.map(Expr::Var),
        ));
        let expr = choice((
            atom.spanned(),
            expr.nested_in(
                select_ref! { Token::SquareBrackets(ts) = e => ts.split_spanned(e.span())},
            ),
        ))
        .pratt(vec![
            infix(left(10), just(Token::Star), |x, _, y, e| {
                Expr::Mul(Box::new(x), Box::new(y)).with_span(e.span())
            })
            .boxed(),
            infix(left(9), just(Token::Plus), |x, _, y, e| {
                Expr::Add(Box::new(x), Box::new(y)).with_span(e.span())
            })
            .boxed(),
            infix(left(1), empty(), |x, _, y, e| {
                Expr::Jump {
                    ident: Box::new(x),
                    args: vec![y],
                }
                .with_span(e.span())
            })
            .boxed(),
        ]);
        expr
    })
}
