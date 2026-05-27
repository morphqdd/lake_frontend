use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra,
    prelude::{any, choice, just, one_of, recursive},
    span::Spanned,
    text,
};

use crate::api::token::Token;

/// Decode the body of a `'...'` char or `b"..."` byte-string at lex
/// time.  Supports the same escape subset:
///   `\n` `\r` `\t` `\0` `\\` `\'` `\"` `\xHH`.
/// Any other backslash-escape yields a lexer error.
fn decode_escapes(body: &str) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::with_capacity(body.len());
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        // `\` — consume one more byte (or two for `\xHH`).
        if i + 1 >= bytes.len() {
            return Err("trailing backslash".to_string());
        }
        let e = bytes[i + 1];
        match e {
            b'n' => { out.push(b'\n'); i += 2; }
            b'r' => { out.push(b'\r'); i += 2; }
            b't' => { out.push(b'\t'); i += 2; }
            b'0' => { out.push(0); i += 2; }
            b'\\' => { out.push(b'\\'); i += 2; }
            b'\'' => { out.push(b'\''); i += 2; }
            b'"' => { out.push(b'"'); i += 2; }
            b'x' => {
                if i + 3 >= bytes.len() {
                    return Err("incomplete \\xHH escape".to_string());
                }
                let hi = (bytes[i + 2] as char)
                    .to_digit(16)
                    .ok_or_else(|| "invalid hex digit in \\xHH".to_string())?;
                let lo = (bytes[i + 3] as char)
                    .to_digit(16)
                    .ok_or_else(|| "invalid hex digit in \\xHH".to_string())?;
                out.push(((hi << 4) | lo) as u8);
                i += 4;
            }
            other => return Err(format!("unknown escape: \\{}", other as char)),
        }
    }
    Ok(out)
}

pub fn lexer<'src>()
-> impl Parser<'src, &'src str, Vec<Spanned<Token<'src>>>, extra::Err<Rich<'src, char>>> {
    recursive(|token| {
        let punct = choice((
            just("@").to(Token::At),
            just(".").to(Token::Dot),
            just(":").to(Token::Colon),
            just("->").to(Token::Arrow),
            just("+").to(Token::Plus),
            just("-").to(Token::Minus),
            just("*").to(Token::Star),
            just("/").to(Token::Slash),
            just("<<").to(Token::Shl),
            just(">>").to(Token::Shr),
            just("<=").to(Token::LessEq),
            just(">=").to(Token::GreaterEq),
            just("==").to(Token::EqEq),
            just("=").to(Token::Eq),
            just("<").to(Token::Less),
            just(">").to(Token::Greater),
            just("&").to(Token::BitAnd),
            just("|").to(Token::BitOr),
            just("^").to(Token::BitXor),
        ));

        // Hex / binary literals share the Token::Num shape but are
        // tried before the decimal / float rule because both prefixes
        // start with `0` and would otherwise be sliced as a leading
        // zero plus an ident.
        let nums = choice((
            just("0x")
                .then(any().filter(|c: &char| c.is_ascii_hexdigit()).repeated().at_least(1))
                .to_slice()
                .map(Token::Num),
            just("0b")
                .then(any().filter(|c: &char| *c == '0' || *c == '1').repeated().at_least(1))
                .to_slice()
                .map(Token::Num),
            text::int(10)
                .then(just('.').then(text::digits(10)).or_not())
                .to_slice()
                .map(Token::Num),
        ));

        // Byte-string literal `b"..."`.  Same escape grammar as
        // `String` — body bytes are kept un-decoded; the parser /
        // codegen path runs `unescape` later.  Must come before the
        // ident rule, otherwise the lexer would slice `b` as an
        // identifier and the trailing `"..."` would lex as a separate
        // plain string.
        let byte_string = just("b").ignore_then(
            choice((
                just('\\').then(any()).to_slice(),
                any().and_is(one_of("\"\\").not()).to_slice(),
            ))
                .repeated()
                .to_slice()
                .delimited_by(just("\""), just("\""))
                .to_slice()
                .map(|s: &'src str| {
                    Token::ByteStr(
                        s.strip_prefix("\"")
                            .unwrap_or(s)
                            .strip_suffix("\"")
                            .unwrap_or(s),
                    )
                }),
        );

        // Char literal `'X'` — single byte value with the same escape
        // subset as byte-strings, decoded at lex time so the rest of
        // the pipeline sees a plain `i64`.  Empty (`''`) and
        // multi-char bodies fall through to the generic Rich error.
        let char_lit = choice((
            just('\\').then(any()).to_slice(),
            any().and_is(one_of("'\\").not()).to_slice(),
        ))
            .repeated()
            .to_slice()
            .delimited_by(just("'"), just("'"))
            .try_map(|s: &'src str, span| {
                decode_escapes(s)
                    .map_err(|m| Rich::custom(span, m))
                    .and_then(|bs| {
                        if bs.len() == 1 {
                            Ok(Token::Char(bs[0]))
                        } else {
                            Err(Rich::custom(
                                span,
                                format!("char literal must decode to exactly one byte, got {}", bs.len()),
                            ))
                        }
                    })
            });

        choice((
            // Parse comments as tokens (will be filtered later)
            just("//")
                .then(any().and_is(just('\n').not()).repeated())
                .to(Token::Comment),
            byte_string,
            char_lit,
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
                "const" => Token::Const,
                s => Token::Ident(s),
            }),
            punct,
            nums,
            // String body: any char except an unescaped `"`.  Backslash
            // escapes (`\"`, `\\`, `\n`, …) are accepted at lex time —
            // the byte-level translation happens later in
            // `string_expr::unescape` once the literal reaches codegen.
            // Without the explicit `\\.` alternative the lexer would
            // close the string on the first `"` after a backslash and
            // refuse to tokenise embedded double-quote escapes.
            choice((
                just('\\').then(any()).to_slice(),
                any().and_is(one_of("\"\\").not()).to_slice(),
            ))
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
            // Container tokens collect their inner tokens via this
            // recursion, but the top-level `Comment`-filter only fires
            // on the outermost stream — so any `// ...` inside
            // `(...)`, `{...}`, or `[...]` would survive the filter
            // and surface to the parser as a stray `Token::Comment`,
            // breaking branch / call grammars.  Each container
            // therefore filters comments out of its own inner Vec
            // before tagging the wrapping token.
            token
                .clone()
                .padded()
                .repeated()
                .at_least(0)
                .collect::<Vec<_>>()
                .padded()
                .delimited_by(just("("), just(")"))
                .map(|toks: Vec<Spanned<Token>>| {
                    Token::Parens(toks.into_iter().filter(|t| !matches!(t.inner, Token::Comment)).collect())
                }),
            token
                .clone()
                .padded()
                .repeated()
                .at_least(0)
                .collect::<Vec<_>>()
                .padded()
                .delimited_by(just("{"), just("}"))
                .map(|toks: Vec<Spanned<Token>>| {
                    Token::CurlyBrackets(toks.into_iter().filter(|t| !matches!(t.inner, Token::Comment)).collect())
                }),
            token
                .padded()
                .repeated()
                .at_least(0)
                .collect::<Vec<_>>()
                .delimited_by(just("[").padded(), just("]").padded())
                .map(|toks: Vec<Spanned<Token>>| {
                    Token::SquareBrackets(toks.into_iter().filter(|t| !matches!(t.inner, Token::Comment)).collect())
                }),
        ))
        .spanned()
        .padded()
    })
    .repeated()
    .collect()
    .map(|tokens: Vec<Spanned<Token>>| {
        // Filter out comments, then tag each `Parens` as tight or
        // loose based on adjacency to the previous token.  A `(` whose
        // byte-position is exactly the previous token's end means
        // no whitespace separated them — Lake's call grammar reads
        // this as `f(args)`.  A space before the `(` flips the token
        // to plain `Parens`, which the parser treats as a value-level
        // paren-group (never as a postfix call).
        let filtered: Vec<Spanned<Token>> = tokens
            .into_iter()
            .filter(|t| !matches!(t.inner, Token::Comment))
            .collect();
        tag_paren_adjacency(filtered)
    })
}

/// Walk a flat token stream and promote each `Parens` whose opening
/// `(` byte-position equals the previous token's end byte-position to
/// `TightParens`.  Recurses into nested container tokens so an `(a)`
/// inside another `(b ...)` is also tagged correctly.
fn tag_paren_adjacency<'src>(
    tokens: Vec<Spanned<Token<'src>>>,
) -> Vec<Spanned<Token<'src>>> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut prev_end: Option<usize> = None;
    for tok in tokens {
        let start: usize = tok.span.start;
        let end: usize = tok.span.end;
        let tight = prev_end == Some(start);
        let new_inner = match tok.inner {
            Token::Parens(inner) => {
                let inner = tag_paren_adjacency(inner);
                if tight {
                    Token::TightParens(inner)
                } else {
                    Token::Parens(inner)
                }
            }
            Token::CurlyBrackets(inner) => {
                Token::CurlyBrackets(tag_paren_adjacency(inner))
            }
            Token::SquareBrackets(inner) => {
                let inner = tag_paren_adjacency(inner);
                if tight {
                    Token::TightSquareBrackets(inner)
                } else {
                    Token::SquareBrackets(inner)
                }
            }
            other => other,
        };
        out.push(Spanned {
            inner: new_inner,
            span: tok.span,
        });
        prev_end = Some(end);
    }
    out
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
    fn char_lit_basic() {
        let tokens = lexer().parse("'A'").into_result().expect("should lex");
        assert_eq!(tokens.len(), 1);
        assert!(matches!(tokens[0].inner, Token::Char(65)));
    }

    #[test]
    fn char_lit_newline_escape() {
        let tokens = lexer().parse("'\\n'").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::Char(10)));
    }

    #[test]
    fn char_lit_hex_escape() {
        let tokens = lexer().parse("'\\xff'").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::Char(255)));
    }

    #[test]
    fn char_lit_single_quote_escape() {
        let tokens = lexer().parse("'\\''").into_result().expect("should lex");
        assert!(matches!(tokens[0].inner, Token::Char(39)));
    }

    #[test]
    fn char_lit_unterminated_errors() {
        let res = lexer().parse("'A").into_result();
        assert!(res.is_err(), "unterminated char must error");
    }

    #[test]
    fn byte_string_basic() {
        let tokens = lexer().parse("b\"abc\"").into_result().expect("should lex");
        assert_eq!(tokens.len(), 1);
        assert!(matches!(tokens[0].inner, Token::ByteStr("abc")));
    }

    #[test]
    fn byte_string_empty() {
        let tokens = lexer().parse("b\"\"").into_result().expect("should lex");
        assert_eq!(tokens.len(), 1);
        assert!(matches!(tokens[0].inner, Token::ByteStr("")));
    }

    #[test]
    fn byte_string_with_double_quote_escape() {
        let tokens = lexer().parse("b\"a\\\"b\"").into_result().expect("should lex");
        match &tokens[0].inner {
            Token::ByteStr(s) => assert_eq!(*s, "a\\\"b"),
            other => panic!("expected ByteStr, got {other:?}"),
        }
    }

    #[test]
    fn byte_string_distinct_from_string() {
        // `b"x"` must NOT lex as ident `b` + string "x".
        let tokens = lexer().parse("b\"x\"").into_result().expect("should lex");
        assert_eq!(tokens.len(), 1);
        assert!(matches!(tokens[0].inner, Token::ByteStr("x")));
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

#[cfg(test)]
mod bracket_comment_test {
    use chumsky::Parser;
    use crate::lexer::lexer;

    #[test]
    fn brackets_in_comment_top_level() {
        let result = lexer().parse("x i64 // has [brackets]\ny i64").into_result();
        assert!(result.is_ok(), "lexer rejected brackets in comment: {:?}", result);
    }

    #[test]
    fn brackets_in_comment_inside_braces() {
        let src = "f is { x i64 -> {\n    // [brackets]\n    ret x\n  }\n}";
        let result = lexer().parse(src).into_result();
        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn comment_inside_curly_block() {
        // Body of a branch is wrapped in `{}` — comments inside that
        // block must not pollute the inner token stream.  Reproduces
        // the sha256.lake parse failure where `// W[0..15]` was
        // tokenised through the SquareBrackets path inside curlies.
        use crate::api::token::Token;
        let src = "f is {\n  x i64 -> ret i64 {\n    // W[0..15] note\n    ret x + 1\n  }\n}";
        let result = lexer().parse(src).into_result();
        assert!(result.is_ok(), "lexer rejected comment-with-brackets inside curly body: {:?}", result);
    }
}
