use miette::{Diagnostic, SourceSpan};
use thiserror::Error;

#[allow(dead_code)]
pub type Result<T> = std::result::Result<T, ParserError>;

#[allow(dead_code)]
#[derive(Error, Diagnostic, Debug, PartialEq, PartialOrd)]
pub enum ParserError {
    #[error("Ident parsing error")]
    #[diagnostic(
        code(parser::error::identParsing),
        help(
            "You must only use identifiers that begin with letters or underscores and use only letters, numbers, and underscores."
        )
    )]
    IdentParsingError(
        #[source_code] String,
        #[label("Something wrong here")] SourceSpan,
    ),

    #[error("Substring not found error")]
    #[diagnostic(
        code(parser::error::substringNotFound),
        help(
            "This is a compiler bug! Report about it here: (https://github.com/morphqdd/lake_frontend)"
        )
    )]
    BadSubsting(String),
}

// impl<I> ParseError<I> for ParserError
// where
//     I: nom::Offset + ToString,
// {
//     fn from_error_kind(input: I, kind: nom::error::ErrorKind) -> Self {
//         match kind {
//             _ => unimplemented!("{kind:?}"),
//         }
//     }
//
//     fn append(_: I, _: nom::error::ErrorKind, other: Self) -> Self {
//         other
//     }
// }
