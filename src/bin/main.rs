use std::{fs, path::Path};

use chumsky::{Parser, input::Input};
use lake_frontend::{
    error_handle::parse_failure,
    lexer::lexer,
    parser::process,
};

fn main() {
    let path = Path::new("./examples/simple.lake");
    let src = fs::read_to_string(path).unwrap();
    let tokens = lexer().parse(&src).into_result().unwrap_or_else(|errs| {
        println!("{errs:?}");
        parse_failure(errs, &src, path)
    });
    println!("{:?}", tokens);
    println!(
        "{:?}",
        process()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .unwrap_or_else(|errs| {
                println!("{errs:?}");
                parse_failure(errs, &src, path)
            })
    )
}
