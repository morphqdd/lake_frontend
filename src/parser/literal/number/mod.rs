use nom::{AsChar, FindSubstring, IResult, Parser, bytes::complete::take_while1};

use crate::{
    api::ast::Literal,
    parser::parser_error::{ParserError, Result},
};

#[allow(dead_code)]
pub fn number(input: &str) -> Result<(&str, Literal)> {
    let res: IResult<&str, &str> = take_while1(|ch: char| ch.is_dec_digit()).parse(input);
    let (next, raw) = res.map_err(|err| match err {
        nom::Err::Incomplete(needed) => todo!("{needed:?}"),
        nom::Err::Error(error) => match input.find_substring(error.input) {
            Some(start) => ParserError::NumberParsingError(
                error.input.into(),
                (start, error.input.len()).into(),
            ),
            None => ParserError::BadSubsting(error.input.into()),
        },
        nom::Err::Failure(remaining) => todo!("Failure: {remaining}"),
    })?;
    Ok((next, Literal::Number(raw.to_string())))
}

#[cfg(test)]
mod tests {
    use crate::{
        api::ast::Literal,
        parser::{
            literal::number::number,
            parser_error::{ParserError, Result},
        },
    };

    #[test]
    fn simple_number_test() -> Result<()> {
        assert_eq!(number("123"), Ok(("", Literal::Number("123".into()))));
        Ok(())
    }

    #[test]
    fn not_only_number_test() -> Result<()> {
        assert_eq!(number("40abs"), Ok(("abs", Literal::Number("40".into()))));
        Ok(())
    }

    #[test]
    fn not_number_test() -> Result<()> {
        assert_eq!(
            number("abs"),
            Err(ParserError::NumberParsingError("abs".into(), (0, 3).into()))
        );
        Ok(())
    }
}
