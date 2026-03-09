use std::{fmt, path::Path};

use ariadne::{Color, Label, Report, ReportKind, sources};
use chumsky::{error::Rich, span::SimpleSpan};
use thiserror::Error;

/// A secondary source span with an explanatory message (shown in yellow).
#[derive(Debug, Clone)]
pub struct SecondaryLabel {
    pub span: SimpleSpan,
    pub message: String,
}

impl SecondaryLabel {
    pub fn new(span: SimpleSpan, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

/// A compile-time error with a source span, optional error code, secondary
/// labels, notes, and a help suggestion — rendered via `ariadne`.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct LakeError {
    /// Short error code shown before the message, e.g. `"E001"`.
    pub code: Option<String>,
    /// One-line summary of the problem.
    pub message: String,
    /// The primary (red) source span and its inline label.
    pub span: SimpleSpan,
    pub label: String,
    /// Additional (yellow) labelled spans.
    pub secondary: Vec<SecondaryLabel>,
    /// Free-form notes appended below the snippet.
    pub notes: Vec<String>,
    /// A `help:` suggestion appended below the snippet.
    pub help: Option<String>,
}

impl LakeError {
    /// Create a new error at `span` with `message` used as both the report
    /// headline and the inline span label.
    pub fn new(message: impl Into<String>, span: SimpleSpan) -> Self {
        let message = message.into();
        Self {
            code: None,
            label: message.clone(),
            message,
            span,
            secondary: vec![],
            notes: vec![],
            help: None,
        }
    }

    /// Create a new error at `span` with separate headline and inline label.
    pub fn with_label_msg(
        message: impl Into<String>,
        span: SimpleSpan,
        label: impl Into<String>,
    ) -> Self {
        Self {
            code: None,
            message: message.into(),
            span,
            label: label.into(),
            secondary: vec![],
            notes: vec![],
            help: None,
        }
    }

    /// Attach an error code (e.g. `"E001"`).
    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// Add a secondary (yellow) labelled span.
    pub fn secondary(mut self, span: SimpleSpan, msg: impl Into<String>) -> Self {
        self.secondary.push(SecondaryLabel::new(span, msg));
        self
    }

    /// Append a free-form note shown below the code snippet.
    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// Set the `help:` line shown below the code snippet.
    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Convert a chumsky `Rich` parse error into a `LakeError`.
    pub fn from_rich<T: fmt::Display>(err: &Rich<T>) -> Self {
        let mut e = Self::new(err.reason().to_string(), *err.span());
        for (ctx_label, ctx_span) in err.contexts() {
            e = e.secondary(*ctx_span, format!("while parsing this {ctx_label}"));
        }
        e
    }

    /// Render this diagnostic to stderr with full source context.
    pub fn display<P: AsRef<Path>>(&self, src: &str, path: P) {
        let fname: &'static str = path.as_ref().display().to_string().leak();

        let mut builder = Report::build(ReportKind::Error, (fname, self.span.into_range()))
            .with_config(ariadne::Config::new().with_index_type(ariadne::IndexType::Byte))
            .with_message(&self.message)
            .with_label(
                Label::new((fname, self.span.into_range()))
                    .with_message(&self.label)
                    .with_color(Color::Red),
            );

        if let Some(code) = &self.code {
            builder = builder.with_code(code);
        }

        for sec in &self.secondary {
            builder = builder.with_label(
                Label::new((fname, sec.span.into_range()))
                    .with_message(&sec.message)
                    .with_color(Color::Yellow),
            );
        }

        for note in &self.notes {
            builder = builder.with_note(note);
        }

        if let Some(help) = &self.help {
            builder = builder.with_help(help);
        }

        let result = builder.finish().print(sources([(fname, src)]));

        if let Err(io_err) = result {
            eprintln!("error while printing diagnostic: {io_err}");
        }
    }
}

/// A collection of `LakeError`s from a single compilation unit.
#[derive(Debug, Default)]
pub struct LakeErrors(pub Vec<LakeError>);

impl LakeErrors {
    pub fn new(errors: Vec<LakeError>) -> Self {
        Self(errors)
    }

    pub fn push(&mut self, err: LakeError) {
        self.0.push(err);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn extend(&mut self, other: LakeErrors) {
        self.0.extend(other.0);
    }

    /// Display all diagnostics to stderr with source context.
    pub fn display<P: AsRef<Path>>(&self, src: &str, path: P) {
        for err in &self.0 {
            err.display(src, &path);
        }
    }

    pub fn from_rich_vec<T: fmt::Display>(errs: Vec<Rich<T>>) -> Self {
        Self(errs.iter().map(LakeError::from_rich).collect())
    }

    /// Convert lexer errors into `LakeErrors`, tagging each with code `L001`.
    pub fn from_lex_errs<T: fmt::Display>(errs: Vec<Rich<T>>) -> Self {
        Self(
            errs.iter()
                .map(|e| LakeError::from_rich(e).code("L001"))
                .collect(),
        )
    }

    /// Convert parser errors into `LakeErrors`, tagging each with code `P001`.
    pub fn from_parse_errs<T: fmt::Display>(errs: Vec<Rich<T>>) -> Self {
        Self(
            errs.iter()
                .map(|e| LakeError::from_rich(e).code("P001"))
                .collect(),
        )
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

impl IntoIterator for LakeErrors {
    type Item = LakeError;
    type IntoIter = std::vec::IntoIter<LakeError>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Display errors and exit the process.  Only for use in binaries.
pub fn parse_failure<P: AsRef<Path>>(errs: Vec<Rich<impl fmt::Display>>, src: &str, path: P) -> ! {
    let errors = LakeErrors::from_rich_vec(errs);
    errors.display(src, path);
    std::process::exit(1)
}
