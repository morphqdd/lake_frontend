use nom::character::complete::multispace0;

use crate::parser::parser_error::Result;

pub fn skip_whitespaces<'a, T>(
    input: &'a str,
    f: fn(&'a str) -> Result<'a, (&'a str, T)>,
) -> Result<'a, (&'a str, T)> {
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
