use std::fmt::Display;

use chumsky::span::Spanned;

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub enum Token<'src> {
    Ident(&'src str),
    Num(&'src str),
    String(&'src str),
    /// `b"..."` — byte-string literal.  Carries the raw inner slice
    /// (escapes left un-processed, same shape as `String`).  Lowers to
    /// `Expr::String` typed as `buf` instead of `str`; the backend's
    /// existing string-literal codegen path handles both equivalently
    /// (`unescape` + rodata + fat-ptr).
    ByteStr(&'src str),
    /// `'X'` — char literal.  Carries the decoded byte value
    /// (`'A'` → 65, `'\n'` → 10, `'\xff'` → 255).  Lowers to
    /// `Expr::Num` typed as `i64` — no dedicated char type exists yet.
    Char(u8),
    /// `(...)` with at least one whitespace between the opening `(`
    /// and the previous non-comment token.  Use sites:
    ///   * paren-group (`(a + b) * c`).
    ///   * fresh argument in an arg-list (`f(x (y - 1))`).
    /// Never matched by the postfix-call rule — a space before `(` is
    /// the syntactic signal that `(` opens a new value, not a call.
    Parens(Vec<Spanned<Self>>),
    /// `(...)` whose opening `(` byte-position is exactly the end of
    /// the previous non-comment token.  Lake's call grammar is
    /// `f(args)` with no whitespace; this variant exists so the
    /// parser can distinguish `f(x)` (call) from `f (x)` (Var + value)
    /// without comma separators.
    TightParens(Vec<Spanned<Self>>),
    SquareBrackets(Vec<Spanned<Self>>),
    /// `[...]` whose opening `[` is adjacent to the previous token (no
    /// whitespace).  Used for postfix indexing (`buf[i]`); the parser
    /// only matches indexing on this variant so `wait [pid]` etc. stay
    /// as space-separated filter syntax.
    TightSquareBrackets(Vec<Spanned<Self>>),
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
    /// `&` — bitwise AND.
    BitAnd,
    /// `|` — bitwise OR.
    BitOr,
    /// `^` — bitwise XOR.
    BitXor,
    /// `<<` — left shift (logical).
    Shl,
    /// `>>` — right shift (logical / unsigned).  Lake's i64 is treated
    /// as a bit pattern for these ops; an arithmetic right shift would
    /// be a separate token (`>>>`) if we ever need it.
    Shr,

    // Comments (filtered out after lexing)
    Comment,
}

impl<'src> Display for Token<'src> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Ident(src) => write!(f, "{src}"),
            Token::Num(src) => write!(f, "{src}"),
            Token::String(src) => write!(f, "\"{src}\""),
            Token::ByteStr(src) => write!(f, "b\"{src}\""),
            Token::Char(b) => write!(f, "'\\x{b:02x}'"),
            Token::Arrow => write!(f, "->"),
            Token::Dot => write!(f, "."),
            Token::Colon => write!(f, ":"),
            Token::As => write!(f, "as"),
            Token::Pin => write!(f, "pin"),
            Token::Const => write!(f, "const"),
            Token::BitAnd => write!(f, "&"),
            Token::BitOr => write!(f, "|"),
            Token::BitXor => write!(f, "^"),
            Token::Shl => write!(f, "<<"),
            Token::Shr => write!(f, ">>"),
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
            Token::TightParens(_) => write!(f, "( ... )"),
            Token::SquareBrackets(_) => write!(f, "[ ... ]"),
            Token::TightSquareBrackets(_) => write!(f, "[ ... ]"),
            Token::CurlyBrackets(_) => write!(f, "{{ ... }}"),
            Token::At => write!(f, "@"),
            Token::Eq => write!(f, "="),
            Token::Wait => write!(f, "wait"),
            Token::Let => write!(f, "let"),
            Token::Comment => write!(f, "// ..."),
        }
    }
}
