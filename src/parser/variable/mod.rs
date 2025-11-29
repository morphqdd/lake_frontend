use nom::{
    IResult, Parser,
    bytes::complete::tag,
    character::complete::{multispace0, one_of},
    multi::{many, many0},
};

use crate::{
    api::ast::{Ident, Path, Variable},
    parser::{ident::ident, literal::number::number, parser_error::Result},
};

pub fn variable(input: &str) -> Result<(&str, Variable)> {
    let (next, var_ident) = ident(input)?;
    let next = match multispace0::<_, nom::error::Error<_>>(next) {
        Ok((next, _)) => next,
        Err(err) => match err {
            nom::Err::Incomplete(needed) => todo!(),
            nom::Err::Error(_) => todo!(),
            nom::Err::Failure(_) => todo!(),
        },
    };
    let (next, ty_ident) = ident(next)?;
    let res = tag::<_, _, nom::error::Error<_>>(".")(next);
    let (next, default_val) = match res {
        Ok((next, _)) => number(next)?,
        Err(err) => match err {
            nom::Err::Incomplete(needed) => todo!(),
            nom::Err::Error(_) => todo!(),
            nom::Err::Failure(_) => todo!(),
        },
    };
    Ok((
        next,
        Variable::new(var_ident, Path::Type(ty_ident), Some(default_val)),
    ))
}

#[cfg(test)]
mod tests {
    use crate::{
        api::ast::{Ident, Literal, Path, Variable},
        parser::{parser_error::Result, variable::variable},
    };

    #[test]
    fn simple_variable_test() -> Result<()> {
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
}
