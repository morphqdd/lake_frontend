use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra,
    prelude::{any, choice, just, one_of, recursive},
    span::Spanned,
    text,
};

use crate::lexer::token::Token;

pub mod token;

pub fn lexer<'src>()
-> impl Parser<'src, &'src str, Vec<Spanned<Token<'src>>>, extra::Err<Rich<'src, char>>> {
    recursive(|token| {
        choice((
            text::ident().map(|s| match s {
                "is" => Token::Is,
                "when" => Token::When,
                "true" => Token::True,
                "false" => Token::False,
                s => Token::Ident(s),
            }),
            just("+").to(Token::Plus),
            just("-").to(Token::Minus),
            just("*").to(Token::Star),
            just("/").to(Token::Slash),
            text::int(10)
                .then(just('.').then(text::digits(10).or_not()))
                .to_slice()
                .map(|s: &'src str| Token::Num(s.parse().unwrap())),
            any()
                .and_is(one_of("\"").not())
                .repeated()
                .collect::<String>()
                .delimited_by(just("\""), just("\""))
                .to_slice()
                .map(|s: &str| Token::String(s.trim_prefix("\"").trim_suffix("\""))),
            token
                .repeated()
                .collect()
                .delimited_by(just("("), just(")"))
                .labelled("token tree")
                .as_context()
                .map(Token::Parens),
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

    use crate::lexer::{lexer, token::Token};

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
}
