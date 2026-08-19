//! The compiler's diagnostic model: a severity, a message, a primary location,
//! and any number of secondary labels and trailing notes.
//!
//! This is deliberately a *data* type with no rendering baked in. The terminal
//! renderer lives in [`super::emitter`]; an LSP server would consume the same
//! [`Diagnostic`] and translate each [`Label`] into an
//! `lsp_types::Diagnostic` using [`Severity::lsp_code`] and
//! [`SourceFile::line_col`](super::source::SourceFile::line_col).

use super::source::{FileId, FileSpan};
use super::span::Span;

/// How serious a [`Diagnostic`] is. Ordered least-to-most severe is not implied;
/// use the variants directly. The numeric values match the LSP
/// `DiagnosticSeverity` enum via [`Severity::lsp_code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
    Help,
}

impl Severity {
    /// The word printed in the terminal header (`error`, `warning`, …).
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
            Severity::Help => "help",
        }
    }

    /// The LSP `DiagnosticSeverity` code: 1 = Error, 2 = Warning, 3 =
    /// Information, 4 = Hint.
    pub fn lsp_code(self) -> u8 {
        match self {
            Severity::Error => 1,
            Severity::Warning => 2,
            Severity::Note => 3,
            Severity::Help => 4,
        }
    }
}

/// A span annotation: a message pinned to a range of source. The `primary` label
/// is the one the diagnostic is really "about"; secondary labels add context
/// (e.g. "expected because of this").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub span: FileSpan,
    pub message: String,
    pub primary: bool,
}

impl Label {
    pub fn primary(span: FileSpan, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            primary: true,
        }
    }

    pub fn secondary(span: FileSpan, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            primary: false,
        }
    }
}

/// A single diagnostic. Build one with [`Diagnostic::error`] (or the other
/// severity constructors) and attach detail with the `with_*` methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Optional short code such as `E0001`, shown as `error[E0001]`.
    pub code: Option<String>,
    /// The top-line summary.
    pub message: String,
    /// Span annotations. At least one should be `primary` for a useful render.
    pub labels: Vec<Label>,
    /// Free-form footnotes printed after the snippet (`= note: …`).
    pub notes: Vec<String>,
}

impl Diagnostic {
    fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self {
            severity,
            code: None,
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(Severity::Error, message)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, message)
    }

    /// Attach a machine-readable code (rendered as `severity[code]`).
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// Add the primary span, optionally with an inline message.
    pub fn with_primary(mut self, span: FileSpan, message: impl Into<String>) -> Self {
        self.labels.push(Label::primary(span, message));
        self
    }

    /// Add a secondary (context) span.
    pub fn with_label(mut self, label: Label) -> Self {
        self.labels.push(label);
        self
    }

    /// Add a trailing note line.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// The primary label, if one was attached.
    pub fn primary_label(&self) -> Option<&Label> {
        self.labels.iter().find(|l| l.primary)
    }
}

/// Convenience: turn a bare `(file, span, message)` error — the shape the parser
/// records — into a full primary-only [`Diagnostic`].
pub fn simple_error(file: FileId, span: Span, message: impl Into<String>) -> Diagnostic {
    let message = message.into();
    Diagnostic::error(message).with_primary(FileSpan::new(file, span), "")
}
