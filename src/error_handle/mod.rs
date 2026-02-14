use std::{fmt, path::Path};

use ariadne::{Color, Label, Report, ReportKind, sources};
use chumsky::{error::Rich, span::SimpleSpan};
use thiserror::Error;

// ─── Error type ───────────────────────────────────────────────────────────────

/// A single compile-time error with source location and context.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct LakeError {
    pub message: String,
    pub span: SimpleSpan,
    pub contexts: Vec<(String, SimpleSpan)>,
}

impl LakeError {
    pub fn new(
        message: impl Into<String>,
        span: SimpleSpan,
        contexts: Vec<(String, SimpleSpan)>,
    ) -> Self {
        Self {
            message: message.into(),
            span,
            contexts,
        }
    }

    /// Convert a chumsky `Rich` error into a `LakeError`.
    pub fn from_rich<T: fmt::Display>(err: &Rich<T>) -> Self {
        Self {
            message: err.reason().to_string(),
            span: *err.span(),
            contexts: err
                .contexts()
                .map(|(label, span)| (format!("while parsing this {label}"), *span))
                .collect(),
        }
    }

    /// Print this error to stderr using ariadne.
    pub fn display<P: AsRef<Path>>(&self, src: &str, path: P) {
        let fname: &'static str = path.as_ref().display().to_string().leak();

        let result = Report::build(ReportKind::Error, (fname, self.span.into_range()))
            .with_config(ariadne::Config::new().with_index_type(ariadne::IndexType::Byte))
            .with_message(&self.message)
            .with_label(
                Label::new((fname, self.span.into_range()))
                    .with_message(&self.message)
                    .with_color(Color::Red),
            )
            .with_labels(self.contexts.iter().map(|(msg, span)| {
                Label::new((fname, span.into_range()))
                    .with_message(msg)
                    .with_color(Color::Yellow)
            }))
            .finish()
            .print(sources([(fname, src)]));

        if let Err(io_err) = result {
            eprintln!("error while printing diagnostic: {io_err}");
        }
    }
}

// ─── Error collection ─────────────────────────────────────────────────────────

/// A collection of `LakeError`s from a single compilation unit.
#[derive(Debug, Default)]
pub struct LakeErrors(pub Vec<LakeError>);

impl LakeErrors {
    pub fn new(errors: Vec<LakeError>) -> Self {
        Self(errors)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Display all errors and return the count.
    pub fn display<P: AsRef<Path>>(&self, src: &str, path: P) {
        for err in &self.0 {
            err.display(src, &path);
        }
    }

    pub fn from_rich_vec<T: fmt::Display>(errs: Vec<Rich<T>>) -> Self {
        Self(errs.iter().map(LakeError::from_rich).collect())
    }
}

impl fmt::Display for LakeErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for err in &self.0 {
            writeln!(f, "{err}")?;
        }
        Ok(())
    }
}

impl std::error::Error for LakeErrors {}

// ─── Legacy helper (used by bin/main.rs) ──────────────────────────────────────

/// Display errors and exit the process.  Only for use in binaries, not in
/// library code.
pub fn parse_failure<P: AsRef<Path>>(errs: Vec<Rich<impl fmt::Display>>, src: &str, path: P) -> ! {
    let errors = LakeErrors::from_rich_vec(errs);
    errors.display(src, path);
    std::process::exit(1)
}
