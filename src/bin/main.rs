use std::{fs, path::Path};

use chumsky::Parser;
use lake_frontend::{error_handle::parse_failure, lexer::lexer};

fn main() {
    let path = Path::new("./examples/simple.lake");
    let src = fs::read_to_string(&path).unwrap();
    println!(
        "{:?}",
        lexer()
            .parse(&src)
            .into_result()
            .unwrap_or_else(|errs| parse_failure(errs, &src, &path))
    );
}
