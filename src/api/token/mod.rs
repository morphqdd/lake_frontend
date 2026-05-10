use std::fmt::Display;

use chumsky::span::Spanned;

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub enum Token<'src> {
    Ident(&'src str),
    Num(&'src str),
    String(&'src str),
    Parens(Vec<Spanned<Self>>),
    SquareBrackets(Vec<Spanned<Self>>),
    CurlyBrackets(Vec<Spanned<Self>>),

    At,
    Arrow,
    Dot,
    /// `:` — module path qualifier (`core:io:writer`).
    Colon,
    Plus,
    Minus,
    Star,
    Slash,
    LessEq,
    GreaterEq,
    EqEq,
    Less,
    Greater,

    Eq,

    // Keywords
    Is,
    When,
    False,
    True,
    Pub,
    Ret,
    SelfKw,
    Wait,
    Let,
    /// `as` — import alias (`+core.io.writer as foo`).
    As,
    /// `pin` — sync sugar for a ret-machine call.  `pin M(args)`
    /// blocks the current actor until M's reply arrives and discards
    /// the value (the captured binding is anonymous and hidden).
    Pin,
    /// `const` — module-level compile-time constant.
    /// Inlined at every use-site; restricted to literal RHS.
    Const,

    // Comments (filtered out after lexing)
    Comment,
}

impl<'src> Display for Token<'src> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Ident(src) => write!(f, "{src}"),
            Token::Num(src) => write!(f, "{src}"),
            Token::String(src) => write!(f, "\"{src}\""),
            Token::Arrow => write!(f, "->"),
            Token::Dot => write!(f, "."),
            Token::Colon => write!(f, ":"),
            Token::As => write!(f, "as"),
            Token::Pin => write!(f, "pin"),
            Token::Const => write!(f, "const"),
            Token::Plus => write!(f, "+"),
            Token::Minus => write!(f, "-"),
            Token::Star => write!(f, "*"),
            Token::Is => write!(f, "is"),
            Token::When => write!(f, "when"),
            Token::False => write!(f, "false"),
            Token::True => write!(f, "true"),
            Token::Pub => write!(f, "pub"),
            Token::Ret => write!(f, "ret"),
            Token::SelfKw => write!(f, "self"),
            Token::Slash => write!(f, "/"),
            Token::LessEq => write!(f, "<="),
            Token::GreaterEq => write!(f, ">="),
            Token::EqEq => write!(f, "=="),
            Token::Less => write!(f, "<"),
            Token::Greater => write!(f, ">"),
            Token::Parens(_) => write!(f, "( ... )"),
            Token::SquareBrackets(_) => write!(f, "[ ... ]"),
            Token::CurlyBrackets(_) => write!(f, "{{ ... }}"),
            Token::At => write!(f, "@"),
            Token::Eq => write!(f, "="),
            Token::Wait => write!(f, "wait"),
            Token::Let => write!(f, "let"),
            Token::Comment => write!(f, "// ..."),
        }
    }
}
