use nom::{bytes::complete::tag, character::complete::multispace0};

use crate::{
    api::ast::{Path, Variable},
    parser::{ident::ident, literal::literal, parser_error::Result},
};

#[allow(dead_code)]
pub fn variable(input: &str) -> Result<'_, (&str, Variable)> {
    let (next, var_ident) = ident(input)?;
    let next = match multispace0::<_, nom::error::Error<_>>(next) {
        Ok((next, _)) => next,
        Err(err) => match err {
            nom::Err::Incomplete(_needed) => todo!(),
            nom::Err::Error(_) => todo!(),
            nom::Err::Failure(_) => todo!(),
        },
    };
    let (next, ty_ident) = ident(next)?;
    let res = tag::<_, _, nom::error::Error<_>>(".")(next);
    let (next, default_val) = match res {
        Ok((next, _)) => {
            let (next, default_val) = literal(next)?;
            (next, Some(default_val))
        }
        Err(err) => match err {
            nom::Err::Incomplete(_needed) => todo!(),
            nom::Err::Error(_) => (next, None),
            nom::Err::Failure(_) => todo!(),
        },
    };
    Ok((
        next,
        Variable::new(var_ident, Path::Type(ty_ident), default_val),
    ))
}

#[cfg(test)]
mod tests {
    use crate::{
        api::ast::{Ident, Literal, Path, Variable},
        parser::{parser_error::Result, variable::variable},
    };

    #[test]
    fn simple_variable_with_default_value_test<'a>() -> Result<'a, ()> {
        assert_eq!(
            variable("n i32.1"),
            Ok((
                "",
                Variable::new(
                    Ident::new("n"),
                    Path::Type(Ident::new("i32")),
                    Some(Literal::Number("1".into()))
                )
            ))
        );
        Ok(())
    }

    #[test]
    fn simple_variable_with_default_value_as_str_test<'a>() -> Result<'a, ()> {
        assert_eq!(
            variable("n string.\"Jake\""),
            Ok((
                "",
                Variable::new(
                    Ident::new("n"),
                    Path::Type(Ident::new("string")),
                    Some(Literal::String("\"Jake\"".into()))
                )
            ))
        );
        Ok(())
    }

    #[test]
    fn simple_variable_test<'a>() -> Result<'a, ()> {
        assert_eq!(
            variable("n i32"),
            Ok((
                "",
                Variable::new(Ident::new("n"), Path::Type(Ident::new("i32")), None)
            ))
        );
        Ok(())
    }
}
