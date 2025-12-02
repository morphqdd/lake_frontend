use nom::{
    FindSubstring, IResult, Parser, character::complete::multispace0, multi::many0, sequence::pair,
};

use crate::{
    api::ast::{Branch, BranchType},
    parser::{
        parser_error::{ParserError, Result},
        variable::variable,
    },
};

pub fn branch<'a>(input: &'a str) -> Result<'a, (&'a str, Branch)> {
    let res = many0(pair(
        |input: &'a str| {
            variable(input).map_err(|err| {
                println!("{}", err);
                nom::Err::<nom::error::Error<&str>>::Error(err.into())
            })
        },
        multispace0,
    ))
    .parse(input);

    let (next, vars) = res.map_err(|err| match err {
        nom::Err::Error(err) => match input.find_substring(err.input) {
            Some(start) => ParserError::BranchParsingError(
                input.into(),
                (start, start + err.input.len()).into(),
            ),
            None => ParserError::BadSubstring(err.input.into()),
        },
        _ => todo!(),
    })?;

    Ok((
        "",
        Branch::new(
            BranchType::Action,
            vars.into_iter().map(|(var, _)| var).collect(),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use crate::{
        api::ast::{Branch, BranchType, Ident, Literal, Path, Variable},
        parser::{branch::branch, parser_error::Result},
    };

    #[test]
    fn simple_branch_test<'a>() -> Result<'a, ()> {
        assert_eq!(
            branch("n i32.10 str string"),
            Ok((
                "",
                Branch::new(
                    BranchType::Action,
                    vec![
                        Variable::new(
                            Ident::new("n"),
                            Path::Type(Ident::new("i32")),
                            Some(Literal::Number("10".into()))
                        ),
                        Variable::new(Ident::new("str"), Path::Type(Ident::new("string")), None)
                    ]
                )
            ))
        );
        Ok(())
    }
}
