use std::{fmt, path::{Path, PathBuf}};

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
    /// Path to the source file this span belongs to.  When `Some`,
    /// the multi-file renderer routes the diagnostic to the matching
    /// source — never against an unrelated module (see bug #126).
    pub source_path: Option<PathBuf>,
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
            source_path: None,
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
            source_path: None,
        }
    }

    /// Tag this error with the source file its span refers to.  Used
    /// by multi-file callers (loader / typeck / populate) so the
    /// renderer can route the diagnostic to the right source — see
    /// docs/state/bugs/126_comment_span_offset_drift.md.
    pub fn with_source_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.source_path = Some(path.as_ref().to_path_buf());
        self
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
        // Bug #126 defensive: if this error is tagged with a file
        // other than the one we're being rendered against, skip — a
        // span valid for module B would otherwise be sliced into
        // module A's bytes, panicking on a mid-multibyte boundary.
        if let Some(tag) = &self.source_path {
            if tag != path.as_ref() {
                return;
            }
        }
        let fname: &'static str = path.as_ref().display().to_string().leak();

        let primary = clamp_span(self.span, src);
        let mut builder = Report::build(ReportKind::Error, (fname, primary.clone()))
            .with_config(ariadne::Config::new().with_index_type(ariadne::IndexType::Byte))
            .with_message(&self.message)
            .with_label(
                Label::new((fname, primary))
                    .with_message(&self.label)
                    .with_color(Color::Red),
            );

        if let Some(code) = &self.code {
            builder = builder.with_code(code);
        }

        for sec in &self.secondary {
            builder = builder.with_label(
                Label::new((fname, clamp_span(sec.span, src)))
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

/// Snap a byte span to valid char boundaries within `src` and clamp it
/// to the source length.  Prevents ariadne panics ("end byte index is
/// not a char boundary") when a span drifts past the end of the file
/// or lands inside a multi-byte UTF-8 sequence.  See bug #126.
fn clamp_span(span: SimpleSpan, src: &str) -> std::ops::Range<usize> {
    let len = src.len();
    let mut start = span.start.min(len);
    let mut end = span.end.min(len);
    if end < start {
        end = start;
    }
    while start < len && !src.is_char_boundary(start) {
        start += 1;
    }
    while end < len && !src.is_char_boundary(end) {
        end += 1;
    }
    if end == len {
        // Already at file end — guaranteed boundary.
    }
    start..end
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

    /// Tag every untagged error with `path` so later multi-file
    /// rendering routes them to the correct source.  Errors already
    /// carrying a `source_path` are left alone.
    pub fn tag_source_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        let p = path.as_ref();
        for e in &mut self.0 {
            if e.source_path.is_none() {
                e.source_path = Some(p.to_path_buf());
            }
        }
        self
    }

    /// Render every diagnostic against the file its `source_path`
    /// points at.  `files` maps file paths to their source text.
    /// Errors with no tag — or tagged with a path not in `files` —
    /// are rendered against the first file as a fallback.  Bug #126.
    pub fn display_multi<P: AsRef<Path>>(&self, files: &[(P, &str)]) {
        for err in &self.0 {
            let (path, src) = match &err.source_path {
                Some(tag) => files
                    .iter()
                    .find(|(p, _)| p.as_ref() == tag.as_path())
                    .map(|(p, s)| (p.as_ref(), *s))
                    .or_else(|| files.first().map(|(p, s)| (p.as_ref(), *s)))
                    .unwrap_or_else(|| (Path::new("<unknown>"), "")),
                None => files
                    .first()
                    .map(|(p, s)| (p.as_ref(), *s))
                    .unwrap_or_else(|| (Path::new("<unknown>"), "")),
            };
            err.display(src, path);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug #126: a span past EOF must not panic ariadne — clamp_span
    /// must snap end to len.  Also catches mid-multibyte-char drift.
    #[test]
    fn clamp_span_past_eof_does_not_panic() {
        let src = "abc";
        let span = SimpleSpan::from(5..47);
        let r = clamp_span(span, src);
        assert!(r.end <= src.len());
        assert!(r.start <= r.end);
    }

    /// Bug #126: a span landing inside a multi-byte char (e.g. the
    /// box-drawing `─` glyph from ariadne output reused as source)
    /// must snap to the nearest char boundary, never panic.
    #[test]
    fn clamp_span_snaps_to_char_boundary() {
        // `─` is 3 bytes (E2 94 80).  Span ending at byte 1 lands
        // inside the codepoint.
        let src = "a─b"; // bytes: 61 E2 94 80 62
        assert_eq!(src.len(), 5);
        let span = SimpleSpan::from(0..2); // mid-`─`
        let r = clamp_span(span, src);
        assert!(src.is_char_boundary(r.start));
        assert!(src.is_char_boundary(r.end));
    }

    /// Bug #126: display() against an unrelated file is a no-op when
    /// the error is tagged with a different source path — ariadne is
    /// never handed an out-of-range span to slice.
    #[test]
    fn display_skips_unrelated_source() {
        // We can't easily intercept ariadne's writer here, but we can
        // at least exercise the early-return path without panicking
        // when the error span is far past the unrelated source's len.
        let err = LakeError::new("oops", SimpleSpan::from(5000..5010))
            .with_source_path("/tmp/file_b.lake");
        // Pass file A's source — much shorter than the span.  Without
        // routing this would have panicked.  With it, display returns
        // before touching the bytes.
        err.display("short source", "/tmp/file_a.lake");
    }
}
