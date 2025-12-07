use std::fmt::Display;

use chumsky::span::Spanned;

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub enum Token<'src> {
    Ident(&'src str),
    Num(f64),
    String(&'src str),
    Parens(Vec<Spanned<Self>>),
    SquareBrackets(Vec<Spanned<Self>>),
    CurlyBrackets(Vec<Spanned<Self>>),

    Arrow,
    Dot,
    Plus,
    Minus,
    Star,
    Slash,

    // Keywords
    Is,
    When,
    False,
    True,
}

impl<'src> Display for Token<'src> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Ident(src) => write!(f, "{src}"),
            Token::Num(src) => write!(f, "{src}"),
            Token::String(src) => write!(f, "\"{src}\""),
            Token::Arrow => write!(f, "->"),
            Token::Dot => write!(f, "."),
            Token::Plus => write!(f, "+"),
            Token::Minus => write!(f, "-"),
            Token::Star => write!(f, "*"),
            Token::Is => write!(f, "is"),
            Token::When => write!(f, "when"),
            Token::False => write!(f, "false"),
            Token::True => write!(f, "true"),
            Token::Slash => write!(f, "/"),
            Token::Parens(_) => write!(f, "( ... )"),
            Token::SquareBrackets(_) => write!(f, "[ ... ]"),
            Token::CurlyBrackets(_) => write!(f, "{{ ... }}"),
        }
    }
}
