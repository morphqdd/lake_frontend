use nom::character::complete::multispace0;

use crate::{
    api::ast::Literal,
    parser::{
        literal::{number::number, string::string},
        parser_error::Result,
    },
};

pub mod number;
pub mod string;

fn skip_whitespaces(
    input: &str,
    f: fn(&str) -> Result<(&str, Literal)>,
) -> Result<'_, (&str, Literal)> {
    let next = match multispace0::<_, nom::error::Error<_>>(input) {
        Ok((next, _)) => next,
        Err(err) => match err {
            nom::Err::Incomplete(_needed) => todo!(),
            nom::Err::Error(_) => todo!(),
            nom::Err::Failure(_) => todo!(),
        },
    };
    f(next)
}

#[allow(dead_code)]
pub fn literal(input: &str) -> Result<'_, (&str, Literal)> {
    match skip_whitespaces(input, string) {
        Ok(res) => Ok(res),
        Err(_) => Ok(skip_whitespaces(input, number)?),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        api::ast::Literal,
        parser::{literal::literal, parser_error::Result},
    };

    #[test]
    fn lit_number_test<'a>() -> Result<'a, ()> {
        assert_eq!(literal("10"), Ok(("", Literal::Number("10".into()))));
        Ok(())
    }

    #[test]
    fn lit_string_test<'a>() -> Result<'a, ()> {
        assert_eq!(
            literal("\"Hello, world\""),
            Ok(("", Literal::String("\"Hello, world\"".into())))
        );
        Ok(())
    }

    #[test]
    fn multi_test<'a>() -> Result<'a, ()> {
        let (next, lit_str) = literal("\"Hello, world\" 10")?;
        let (_, lit_num) = literal(next)?;
        assert_eq!(lit_str, Literal::String("\"Hello, world\"".into()));
        assert_eq!(lit_num, Literal::Number("10".into()));
        Ok(())
    }
}
