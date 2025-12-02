use miette::{Diagnostic, SourceSpan};
use nom::error::ErrorKind;
use thiserror::Error;

#[allow(dead_code)]
pub type Result<'a, T> = std::result::Result<T, ParserError<'a>>;

#[allow(dead_code)]
#[derive(Error, Diagnostic, Debug, PartialEq, PartialOrd)]
pub enum ParserError<'a> {
    #[error("Ident parsing error")]
    #[diagnostic(
        code(parser::error::identParsing),
        help(
            "You must only use identifiers that begin with letters or underscores and use only letters, numbers, and underscores."
        )
    )]
    IdentParsingError(
        #[source_code] &'a str,
        #[label("Wrong ident here")] SourceSpan,
    ),

    #[error("Substring not found error")]
    #[diagnostic(
        code(parser::error::substringNotFound),
        help(
            "This is a compiler bug! Report about it here: (https://github.com/morphqdd/lake_frontend)"
        )
    )]
    BadSubstring(&'a str),

    #[error("Number parsing error")]
    #[diagnostic(
        code(parser::error::numberParsingError),
        help("Expected a integer consisting only of digits (0–9).")
    )]
    NumberParsingError(
        #[source_code] &'a str,
        #[label("Wrong number here")] SourceSpan,
    ),

    #[error("String parsing error")]
    #[diagnostic(
        code(parser::error::stringParsingError),
        help("Expected a valid string literal enclosed in quotes.")
    )]
    StringParsingError(
        #[source_code] &'a str,
        #[label("Wrong string here")] SourceSpan,
    ),

    #[error("Branch parsing error")]
    #[diagnostic(
        code(parser::error::branchParsingError),
        help("Expected a valid branch.")
    )]
    BranchParsingError(
        #[source_code] &'a str,
        #[label("Wrong branch here")] SourceSpan,
    ),
}

impl<'a> From<ParserError<'a>> for nom::error::Error<&'a str> {
    fn from(value: ParserError<'a>) -> Self {
        let src = match value {
            ParserError::IdentParsingError(src, _) => src,
            ParserError::BadSubstring(src) => src,
            ParserError::NumberParsingError(src, _) => src,
            ParserError::StringParsingError(src, _) => src,
            ParserError::BranchParsingError(src, _) => src,
        };

        Self::new(src, ErrorKind::Eof)
    }
}
