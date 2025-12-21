use chumsky::{
    IterParser, Parser,
    container::Container,
    error::Rich,
    extra::Err,
    input::MappedInput,
    pratt::{Operator, infix, left},
    prelude::{choice, empty, just, recursive},
    select_ref,
    span::{SimpleSpan, SpanWrap, Spanned},
};

use crate::api::{
    ast::{Branch, Ident, Pattern, Type},
    expr::{Expr, Path},
    token::Token,
};

// pub fn parser<'tokens, 'src: 'tokens>() -> impl Parser<
//     'tokens,
//     MappedInput<'tokens, Token<'src>, SimpleSpan, &'tokens [Spanned<Token<'src>>]>,
//     Spanned<Expr<'src>>,
//     Err<Rich<'tokens, Token<'src>>>,
// > {
//     recursive(|expr| {
//         let ident = select_ref! { Token::Ident(n) => *n };
//         let path = select_ref! { Token::Ident(n) => Expr::Path(Path::Type(*n)) };
//         let atom = choice((
//             select_ref! { Token::Num(n) => Expr::Num(*n) },
//             select_ref! { Token::String(n) => Expr::String(*n) },
//             just(Token::False).to(Expr::Bool(false)),
//             just(Token::True).to(Expr::Bool(true)),
//             ident.map(Expr::Var),
//         ));
//         let var_pattern = ident
//             .spanned()
//             .then(path.spanned().map(Box::new))
//             .then(just(Token::Dot).then(atom.clone()).or_not());
//         let branch = var_pattern
//             .repeated()
//             .then_ignore(just(Token::Arrow))
//             .then(atom.clone())
//             .map(|(a, b)| {
//                 println!("{a:?} {b:?}");
//                 Expr::Bool(false)
//             });
//
//         choice((
//             choice([branch]).spanned(),
//             expr.nested_in(
//                 select_ref! { Token::SquareBrackets(ts) = e => ts.split_spanned(e.span())},
//             ),
//         ))
//         .pratt(vec![
//             infix(left(10), just(Token::Star), |x, _, y, e| {
//                 Expr::Mul(Box::new(x), Box::new(y)).with_span(e.span())
//             })
//             .boxed(),
//             infix(left(9), just(Token::Plus), |x, _, y, e| {
//                 Expr::Add(Box::new(x), Box::new(y)).with_span(e.span())
//             })
//             .boxed(),
//             infix(left(1), empty(), |x, _, y, e| {
//                 Expr::Jump {
//                     ident: Box::new(x),
//                     args: vec![y],
//                 }
//                 .with_span(e.span())
//             })
//             .boxed(),
//         ])
//     })
// }
//

fn expr<'t, 'src: 't>() -> impl Parser<
    't,
    MappedInput<'t, Token<'src>, SimpleSpan, &'t [Spanned<Token<'src>>]>,
    Spanned<Expr<'src>>,
    Err<Rich<'t, Token<'src>>>,
> {
    use chumsky::input::Input;
    recursive(|expr| {
        let ident = select_ref! { Token::Ident(n) => *n }.map(Expr::Var);
        let atom = choice((
            select_ref! { Token::Num(n) => Expr::Num(*n) },
            select_ref! { Token::String(n) => Expr::String(*n) },
            just(Token::False).to(Expr::Bool(false)),
            just(Token::True).to(Expr::Bool(true)),
            ident,
        ));

        let call = choice((
            atom.clone()
                .spanned()
                .then(expr.repeated().collect::<Vec<_>>().nested_in(
                    select_ref!(Token::SquareBrackets(ts) = e => ts.split_spanned(e.span())),
                ))
                .map(|(ident, args)| Expr::Jump {
                    ident: Box::new(ident),
                    args: args,
                }),
            atom.clone(),
        ));

        call.spanned().pratt(vec![
            infix(left(10), just(Token::Star), |x, _, y, e| {
                Expr::Mul(Box::new(x), Box::new(y)).with_span(e.span())
            })
            .boxed(),
            infix(left(9), just(Token::Plus), |x, _, y, e| {
                Expr::Add(Box::new(x), Box::new(y)).with_span(e.span())
            })
            .boxed(),
        ])
    })
}

pub fn pattern<'t, 'src: 't>() -> impl Parser<
    't,
    MappedInput<'t, Token<'src>, SimpleSpan, &'t [Spanned<Token<'src>>]>,
    Spanned<Pattern<'src>>,
    Err<Rich<'t, Token<'src>>>,
> {
    select_ref!(Token::Ident(n) => Ident::new(n))
        .spanned()
        .then(
            select_ref!(Token::Ident(n) = e => Type::Type(Ident::new(n).with_span(e.span())))
                .spanned()
                .then(just(Token::Dot).ignored().then(expr()).or_not()),
        )
        .map(|(ident, (ty, default))| {
            let default = default.map(|(_, expr)| expr);
            Pattern::new(ident, ty, default)
        })
        .spanned()
}

pub fn branch<'t, 'src: 't>() -> impl Parser<
    't,
    MappedInput<'t, Token<'src>, SimpleSpan, &'t [Spanned<Token<'src>>]>,
    Spanned<Branch<'src>>,
    Err<Rich<'t, Token<'src>>>,
> {
    pattern()
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .then_ignore(just(Token::Arrow))
        .then(expr().repeated().collect::<Vec<_>>())
        .map(|(patterns, body)| Branch::new(patterns, body))
        .spanned()
}

#[cfg(test)]
mod test {
    use chumsky::{Parser, input::Input, span::Spanned};

    use crate::{
        api::{
            ast::{Ident, Pattern, Type},
            expr::Expr,
        },
        lexer::lexer,
        parser::pattern,
    };

    #[test]
    fn parse_pattern_test() {
        let input = "n i32.10";
        let tokens = lexer().parse(input).into_result();
        assert!(tokens.is_ok());
        let tokens = tokens.unwrap();
        let ast = pattern()
            .parse(tokens[..].split_spanned((0..input.len()).into()))
            .into_result();
        assert!(ast.is_ok());
        assert_eq!(
            ast.unwrap(),
            Spanned {
                inner: Pattern::new(
                    Spanned {
                        inner: Ident::new("n"),
                        span: (0..1).into()
                    },
                    Spanned {
                        inner: Type::Type(Spanned {
                            inner: Ident::new("i32"),
                            span: (2..5).into()
                        }),
                        span: (2..5).into()
                    },
                    Some(Spanned {
                        inner: Expr::Num("10".into()),
                        span: (6..8).into()
                    })
                ),
                span: (0..8).into()
            }
        )
    }
}
