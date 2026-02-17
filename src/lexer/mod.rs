use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra,
    prelude::{any, choice, just, one_of, recursive},
    span::Spanned,
    text,
};

use crate::api::token::Token;

pub fn lexer<'src>()
-> impl Parser<'src, &'src str, Vec<Spanned<Token<'src>>>, extra::Err<Rich<'src, char>>> {
    recursive(|token| {
        choice((
            text::ident().map(|s| match s {
                "is" => Token::Is,
                "when" => Token::When,
                "true" => Token::True,
                "false" => Token::False,
                "pub" => Token::Pub,
                "ret" => Token::Ret,
                "self" => Token::SelfKw,
                s => Token::Ident(s),
            }),
            just("@").to(Token::At),
            just(".").to(Token::Dot),
            just("->").to(Token::Arrow),
            just("+").to(Token::Plus),
            just("-").to(Token::Minus),
            just("*").to(Token::Star),
            just("/").to(Token::Slash),
            just("<=").to(Token::LessEq),
            just(">=").to(Token::GreaterEq),
            just("==").to(Token::EqEq),
            just("<").to(Token::Less),
            just(">").to(Token::Greater),
            text::int(10)
                .then(just('.').then(text::digits(10)).or_not())
                .to_slice()
                .map(Token::Num),
            any()
                .and_is(one_of("\"").not())
                .repeated()
                .to_slice()
                .delimited_by(just("\""), just("\""))
                .to_slice()
                .map(|s: &'src str| {
                    Token::String(
                        s.strip_prefix("\"")
                            .unwrap_or(s)
                            .strip_suffix("\"")
                            .unwrap_or(s),
                    )
                }),
            token
                .clone()
                .padded()
                .repeated()
                .collect()
                .delimited_by(just("("), just(")"))
                .map(Token::Parens),
            token
                .clone()
                .padded()
                .repeated()
                .collect()
                .delimited_by(just("{"), just("}"))
                .map(Token::CurlyBrackets),
            token
                .padded()
                .repeated()
                .collect()
                .delimited_by(just("[").padded(), just("]").padded())
                .map(Token::SquareBrackets),
        ))
        .spanned()
        .padded()
    })
    .repeated()
    .collect()
}

#[cfg(test)]
mod test {
    use chumsky::{
        Parser,
        span::{SimpleSpan, Span, Spanned},
    };

    use crate::{api::token::Token, lexer::lexer};

    #[test]
    fn string_test() {
        assert_eq!(
            lexer().parse("\"Hello, Jake! I'm 20\"").into_result(),
            Ok(vec![Spanned {
                inner: Token::String("Hello, Jake! I'm 20"),
                span: SimpleSpan::new((), 0..21)
            }])
        )
    }

    #[test]
    fn num_decimal_test() {
        assert_eq!(
            lexer().parse("10").into_result(),
            Ok(vec![Spanned {
                inner: Token::Num("10"),
                span: SimpleSpan::new((), 0..2)
            }])
        );
    }

    #[test]
    fn num_float_test() {
        assert_eq!(
            lexer().parse("10.10").into_result(),
            Ok(vec![Spanned {
                inner: Token::Num("10.10"),
                span: SimpleSpan::new((), 0..5)
            }])
        );
    }

    #[test]
    fn variable_test() {
        assert_eq!(
            lexer().parse("n i32.1").into_result(),
            Ok(vec![
                Spanned {
                    inner: Token::Ident("n"),
                    span: SimpleSpan::new((), 0..1)
                },
                Spanned {
                    inner: Token::Ident("i32"),
                    span: SimpleSpan::new((), 2..5)
                },
                Spanned {
                    inner: Token::Dot,
                    span: SimpleSpan::new((), 5..6)
                },
                Spanned {
                    inner: Token::Num("1"),
                    span: SimpleSpan::new((), 6..7)
                }
            ])
        )
    }

    #[test]
    fn math_expr_test() {
        assert_eq!(
            lexer().parse("2*4/5+2-4").into_result(),
            Ok(vec![
                Spanned {
                    inner: Token::Num("2"),
                    span: SimpleSpan::new((), 0..1)
                },
                Spanned {
                    inner: Token::Star,
                    span: SimpleSpan::new((), 1..2)
                },
                Spanned {
                    inner: Token::Num("4"),
                    span: SimpleSpan::new((), 2..3)
                },
                Spanned {
                    inner: Token::Slash,
                    span: SimpleSpan::new((), 3..4)
                },
                Spanned {
                    inner: Token::Num("5"),
                    span: SimpleSpan::new((), 4..5)
                },
                Spanned {
                    inner: Token::Plus,
                    span: SimpleSpan::new((), 5..6)
                },
                Spanned {
                    inner: Token::Num("2"),
                    span: SimpleSpan::new((), 6..7)
                },
                Spanned {
                    inner: Token::Minus,
                    span: SimpleSpan::new((), 7..8)
                },
                Spanned {
                    inner: Token::Num("4"),
                    span: SimpleSpan::new((), 8..9)
                },
            ])
        )
    }

    #[test]
    fn branch_test() {
        assert_eq!(
            lexer().parse("n i32.1 -> print [ n ]").into_result(),
            Ok(vec![
                Spanned {
                    inner: Token::Ident("n"),
                    span: SimpleSpan::new((), 0..1)
                },
                Spanned {
                    inner: Token::Ident("i32"),
                    span: SimpleSpan::new((), 2..5)
                },
                Spanned {
                    inner: Token::Dot,
                    span: SimpleSpan::new((), 5..6)
                },
                Spanned {
                    inner: Token::Num("1"),
                    span: SimpleSpan::new((), 6..7)
                },
                Spanned {
                    inner: Token::Arrow,
                    span: SimpleSpan::new((), 8..10)
                },
                Spanned {
                    inner: Token::Ident("print"),
                    span: SimpleSpan::new((), 11..16)
                },
                Spanned {
                    inner: Token::SquareBrackets(vec![Spanned {
                        inner: Token::Ident("n"),
                        span: SimpleSpan::new((), 19..20)
                    },]),
                    span: SimpleSpan::new((), 17..22)
                },
            ])
        );
    }

    #[test]
    fn self_keyword_test() {
        assert_eq!(
            lexer().parse("self").into_result(),
            Ok(vec![Spanned {
                inner: Token::SelfKw,
                span: SimpleSpan::new((), 0..4)
            }])
        );
    }

    #[test]
    fn self_is_not_ident_test() {
        let tokens = lexer().parse("self").into_result().expect("should lex");
        assert!(!matches!(tokens[0].inner, Token::Ident(_)));
    }
}
