use nom::{
    Parser,
    character::{
        complete::{alpha1, alphanumeric0},
        one_of,
    },
    combinator::all_consuming,
};

use crate::{api::ast::Ident, parser::parser_error::ParserError};

pub mod ident;
pub mod parser_error;
