use std::{fs, path::Path, process};

use lake_frontend::prelude::build_ast;

fn parse_file(path: &Path) {
    let src = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("error reading {}: {e}", path.display());
        process::exit(1);
    });
    match build_ast(path, &src) {
        Ok((tokens, ast)) => {
            fs::write(
                path.parent().unwrap().join(format!(
                    "{}.{}.laketokens",
                    chrono::prelude::Utc::now(),
                    path.file_name().unwrap().to_str().unwrap()
                )),
                format!("{tokens:#?}"),
            )
            .unwrap();

            fs::write(
                path.parent().unwrap().join(format!(
                    "{}.{}.lakeast",
                    chrono::prelude::Utc::now(),
                    path.file_name().unwrap().to_str().unwrap()
                )),
                format!("{ast:#?}"),
            )
            .unwrap();
        }
        Err((tokens, errs)) => {
            fs::write(
                path.parent().unwrap().join(format!(
                    "{}.{}.laketokens",
                    chrono::prelude::Utc::now(),
                    path.file_name().unwrap().to_str().unwrap()
                )),
                format!("{tokens:#?}"),
            )
            .unwrap();

            errs.display(&src, path);
            process::exit(1);
        }
    }
}

fn main() {
    let examples = [
        "examples/simple/simple.lake",
        "examples/simple/core/error.lake",
        "examples/simple/core/result.lake",
        "examples/simple/core/box.lake",
        "examples/simple/core/io.lake",
    ];

    for path in examples {
        parse_file(Path::new(path));
    }
}
