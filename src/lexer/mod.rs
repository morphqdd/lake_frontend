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
            // Parse comments as tokens (will be filtered later)
            just("//")
                .then(any().and_is(just('\n').not()).repeated())
                .to(Token::Comment),
            text::ident().map(|s| match s {
                "is" => Token::Is,
                "when" => Token::When,
                "true" => Token::True,
                "false" => Token::False,
                "pub" => Token::Pub,
                "ret" => Token::Ret,
                "self" => Token::SelfKw,
                "wait" => Token::Wait,
                "let" => Token::Let,
                "as" => Token::As,
                "pin" => Token::Pin,
                s => Token::Ident(s),
            }),
            just("@").to(Token::At),
            just(".").to(Token::Dot),
            just(":").to(Token::Colon),
            just("->").to(Token::Arrow),
            just("+").to(Token::Plus),
            just("-").to(Token::Minus),
            just("*").to(Token::Star),
            just("/").to(Token::Slash),
            just("<=").to(Token::LessEq),
            just(">=").to(Token::GreaterEq),
            just("==").to(Token::EqEq),
            just("=").to(Token::Eq),
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
                .at_least(0)
                .collect::<Vec<_>>()
                .padded()
                .delimited_by(just("("), just(")"))
                .map(Token::Parens),
            token
                .clone()
                .padded()
                .repeated()
                .at_least(0)
                .collect::<Vec<_>>()
                .padded()
                .delimited_by(just("{"), just("}"))
                .map(Token::CurlyBrackets),
            token
                .padded()
                .repeated()
                .at_least(0)
                .collect()
                .delimited_by(just("[").padded(), just("]").padded())
                .map(Token::SquareBrackets),
        ))
        .spanned()
        .padded()
    })
    .repeated()
    .collect()
    .map(|tokens: Vec<Spanned<Token>>| {
        // Filter out comments
        tokens.into_iter()
            .filter(|t| !matches!(t.inner, Token::Comment))
            .collect()
    })
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

    #[test]
    fn wait_keyword_test() {
        let tokens = lexer().parse("wait").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::Wait));
    }

    #[test]
    fn let_keyword_test() {
        let tokens = lexer().parse("let").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::Let));
    }

    #[test]
    fn eq_token_test() {
        let tokens = lexer().parse("=").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::Eq));
    }

    #[test]
    fn eq_vs_eqeq_test() {
        let tokens = lexer().parse("== =").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::EqEq));
        assert!(matches!(tokens[1].inner, Token::Eq));
    }

    #[test]
    fn comment_single_line_test() {
        // Comments should be completely ignored
        let tokens = lexer()
            .parse("n i64 // this is a comment")
            .into_result()
            .expect("should lex");
        assert_eq!(tokens.len(), 2); // only "n" and "i64"
        assert!(matches!(tokens[0].inner, Token::Ident("n")));
        assert!(matches!(tokens[1].inner, Token::Ident("i64")));
    }

    #[test]
    fn comment_full_line_test() {
        let tokens = lexer()
            .parse("// comment line\nn i64")
            .into_result()
            .expect("should lex");
        assert_eq!(tokens.len(), 2);
        assert!(matches!(tokens[0].inner, Token::Ident("n")));
        assert!(matches!(tokens[1].inner, Token::Ident("i64")));
    }

    #[test]
    fn colon_token_test() {
        let tokens = lexer().parse(":").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::Colon));
    }

    #[test]
    fn module_path_lexes_as_ident_colon_chain() {
        // `core:io:writer` → ident colon ident colon ident
        let tokens = lexer()
            .parse("core:io:writer")
            .into_result()
            .expect("should lex");
        assert!(matches!(tokens[0].inner, Token::Ident("core")));
        assert!(matches!(tokens[1].inner, Token::Colon));
        assert!(matches!(tokens[2].inner, Token::Ident("io")));
        assert!(matches!(tokens[3].inner, Token::Colon));
        assert!(matches!(tokens[4].inner, Token::Ident("writer")));
    }

    #[test]
    fn as_keyword_test() {
        let tokens = lexer().parse("as").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::As));
    }

    #[test]
    fn as_is_not_an_ident() {
        let tokens = lexer().parse("as foo").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::As));
        assert!(matches!(tokens[1].inner, Token::Ident("foo")));
    }

    #[test]
    fn comment_multiple_test() {
        let src = r#"
// This is a counter
counter is {
  n i64 -> { // parameter
    self(n-1) // recurse
  }
}
"#;
        let tokens = lexer().parse(src).into_result().expect("should lex");
        // Should have: counter, is, {, n, i64, ->, {, self, (, n, -, 1, ), }, }
        assert!(tokens.len() > 0);
        assert!(matches!(tokens[0].inner, Token::Ident("counter")));
    }
}
