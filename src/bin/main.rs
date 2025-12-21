use std::{fs, path::Path};

use chumsky::{Parser, input::Input};
use lake_frontend::{
    error_handle::parse_failure,
    lexer::lexer,
    parser::{branch, pattern},
};

fn main() {
    let path = Path::new("./examples/branch.lake");
    let src = fs::read_to_string(&path).unwrap();
    let tokens = lexer().parse(&src).into_result().unwrap_or_else(|errs| {
        println!("{errs:?}");
        parse_failure(errs, &src, &path)
    });
    println!("{:?}", tokens);
    println!(
        "{:?}",
        branch()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .unwrap_or_else(|errs| {
                println!("{errs:?}");
                parse_failure(errs, &src, &path)
            })
    )
}
