use nom::{
    FindSubstring, IResult, Parser,
    bytes::complete::{tag, take_till},
    combinator::recognize,
    sequence::pair,
};

use crate::{
    api::ast::Literal,
    parser::parser_error::{ParserError, Result},
};

#[allow(dead_code)]
pub fn string(input: &str) -> Result<(&str, Literal)> {
    let res: IResult<&str, &str> = recognize(pair(
        tag(r#"""#),
        pair(take_till(|c: char| c == '"'), tag(r#"""#)),
    ))
    .parse(input);
    let (next, str) = res.map_err(|err| match err {
        nom::Err::Incomplete(_needed) => todo!(),
        nom::Err::Error(err) => match input.find_substring(err.input) {
            Some(start) => ParserError::StringParsingError(
                input.into(),
                (start, start + err.input.len()).into(),
            ),
            None => ParserError::BadSubstring(err.input.into()),
        },
        nom::Err::Failure(_) => todo!(),
    })?;

    Ok((next, Literal::String(str.into())))
}

#[cfg(test)]
mod tests {
    use crate::{
        api::ast::Literal,
        parser::{literal::string::string, parser_error::Result},
    };

    #[test]
    fn simple_string_test() -> Result<()> {
        assert_eq!(
            string(r#""Hello, world!""#),
            Ok(("", Literal::String(r#""Hello, world!""#.into())))
        );
        Ok(())
    }

    #[test]
    fn multiline_string_test() -> Result<()> {
        assert_eq!(
            string(
                r#""
                Hello, world!
                Bye, world!
                ""#
            ),
            Ok((
                "",
                Literal::String(
                    r#""
                Hello, world!
                Bye, world!
                ""#
                    .into()
                )
            ))
        );
        Ok(())
    }
}
