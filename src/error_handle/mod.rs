use core::fmt;
use std::path::Path;

use ariadne::{Color, Label, Report, ReportKind, sources};
use chumsky::{error::Rich, span::SimpleSpan};

fn failure<P: AsRef<Path>>(
    msg: String,
    label: (String, SimpleSpan),
    extra_labels: impl IntoIterator<Item = (String, SimpleSpan)>,
    src: &str,
    path: P,
) {
    let fname: &'static str = path.as_ref().display().to_string().leak();
    Report::build(ReportKind::Error, (fname, label.1.into_range()))
        .with_config(ariadne::Config::new().with_index_type(ariadne::IndexType::Byte))
        .with_message(&msg)
        .with_label(
            Label::new((fname, label.1.into_range()))
                .with_message(label.0)
                .with_color(Color::Red),
        )
        .with_labels(extra_labels.into_iter().map(|label2| {
            Label::new((fname, label2.1.into_range()))
                .with_message(label2.0)
                .with_color(Color::Yellow)
        }))
        .finish()
        .print(sources([(fname, src)]))
        .unwrap();
}

pub fn parse_failure<P: AsRef<Path>>(errs: Vec<Rich<impl fmt::Display>>, src: &str, path: P) -> ! {
    errs.iter().for_each(|err| {
        failure(
            err.reason().to_string(),
            (
                err.found()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "end of input".to_string()),
                *err.span(),
            ),
            err.contexts()
                .map(|(l, s)| (format!("while parsing this {l}"), *s)),
            src,
            &path,
        )
    });
    std::process::exit(1)
}
