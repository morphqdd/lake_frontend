use miette::{Diagnostic, NamedSource, SourceSpan};
use nom::error::{ErrorKind, ParseError};
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
    IdentParsingError {
        #[allow(dead_code)]
        #[source_code]
        source_code: String,
        #[allow(dead_code)]
        #[label("Something wrong here")]
        bad_bit: SourceSpan,
    },
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
