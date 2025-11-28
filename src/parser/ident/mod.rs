use nom::{
    FindSubstring, IResult, Parser, bytes::complete::take_while1, combinator::recognize,
    sequence::pair,
};

use crate::{
    api::ast::Ident,
    parser::parser_error::{ParserError, Result},
};

#[allow(dead_code)]
pub fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

#[allow(dead_code)]
pub fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[allow(dead_code)]
pub fn ident(input: &str) -> Result<(&str, Ident)> {
    let res: IResult<&str, &str> = recognize(pair(
        take_while1(is_ident_start),
        take_while1(is_ident_char).or(|i| Ok((i, ""))),
    ))
    .parse(input);
    let (next, raw) = res.map_err(|err| match err {
        nom::Err::Incomplete(needed) => todo!("{needed:?}"),
        nom::Err::Error(error) => match input.find_substring(error.input) {
            Some(start) => ParserError::IdentParsingError(
                error.input.into(),
                (start, error.input.len()).into(),
            ),
            None => ParserError::BadSubsting(error.input.into()),
        },
        nom::Err::Failure(remaining) => todo!("Failure: {remaining}"),
    })?;

    Ok((next, Ident::new(raw)))
}

#[cfg(test)]
mod tests {
    use miette::Result;

    use crate::{
        api::ast::Ident,
        parser::{ident::ident, parser_error::ParserError},
    };

    #[test]
    fn simple_ident_test() -> Result<()> {
        assert_eq!(ident("ident"), Ok(("", Ident::new("ident"))));
        Ok(())
    }

    #[test]
    fn parse_ident_error() -> Result<()> {
        assert_eq!(
            ident("123"),
            Err(ParserError::IdentParsingError("123".into(), (0, 3).into()))
        );
        Ok(())
    }
}
