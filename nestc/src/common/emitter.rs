//! Terminal renderer for [`Diagnostic`]s — a small, dependency-free take on the
//! familiar rustc layout:
//!
//! ```text
//! error[E0001]: unexpected token
//!  --> main.nest:3:13
//!   |
//! 3 |     let x = ;
//!   |             ^ expected an expression
//!   |
//!   = note: statements end at a newline
//! ```
//!
//! Rendering is intentionally simple: each label is shown against the first line
//! of its span, with a caret run under the columns it covers. A span crossing
//! several lines underlines from its start to the end of that first line — good
//! enough for a bootstrap compiler, and the [`Diagnostic`] data it consumes is
//! rich enough that a fancier renderer (or an LSP server) can be swapped in
//! without touching the rest of the compiler.

use std::fmt::Write as _;

use super::diagnostic::{Diagnostic, Label};
use super::source::{FileSpan, SourceMap};

/// Render a diagnostic to a `String` against `sources`. Never panics: spans in
/// unknown files, or out-of-range offsets, degrade to a header-only render.
pub fn render(diag: &Diagnostic, sources: &SourceMap) -> String {
    let mut out = String::new();

    // Header: `error[E0001]: message`
    out.push_str(diag.severity.label());
    if let Some(code) = &diag.code {
        let _ = write!(out, "[{code}]");
    }
    let _ = writeln!(out, ": {}", diag.message);

    // Gutter width: the widest line number any label lands on.
    let gutter = gutter_width(diag, sources);
    let pad = " ".repeat(gutter);

    // Location arrow points at the primary label (or the first label).
    let anchor = diag.primary_label().or_else(|| diag.labels.first());
    if let Some(anchor) = anchor {
        if let Some((name, lc)) = locate(anchor.span, sources) {
            let _ = writeln!(out, "{pad}--> {name}:{}:{}", lc.line, lc.column);
        }
    }

    // One snippet block per label.
    for (i, label) in diag.labels.iter().enumerate() {
        render_label(&mut out, label, sources, gutter);
        // Blank gutter line between adjacent snippets and before notes.
        if i + 1 < diag.labels.len() {
            let _ = writeln!(out, "{pad} |");
        }
    }

    // Trailing notes.
    if !diag.notes.is_empty() && !diag.labels.is_empty() {
        let _ = writeln!(out, "{pad} |");
    }
    for note in &diag.notes {
        let _ = writeln!(out, "{pad} = note: {note}");
    }

    out
}

/// Widest line-number string across every label; at least 1.
fn gutter_width(diag: &Diagnostic, sources: &SourceMap) -> usize {
    diag.labels
        .iter()
        .filter_map(|l| locate(l.span, sources).map(|(_, lc)| lc.line))
        .map(|line| line.to_string().len())
        .max()
        .unwrap_or(1)
}

/// Resolve a span's file name and start position, if the file is known.
fn locate(span: FileSpan, sources: &SourceMap) -> Option<(&str, super::source::LineCol)> {
    let file = sources.file(span.file)?;
    Some((&file.name, file.line_col(span.span.start)))
}

/// Emit the `N | line` / `  | ^^^ msg` pair for one label.
fn render_label(out: &mut String, label: &Label, sources: &SourceMap, gutter: usize) {
    let pad = " ".repeat(gutter);
    let Some(file) = sources.file(label.span.file) else {
        return;
    };

    let start = file.line_col(label.span.span.start);
    let end = file.line_col(label.span.span.end.max(label.span.span.start));
    let line_text = file.line_text(start.line);

    // Opening gutter line.
    let _ = writeln!(out, "{pad} |");
    // Source line, right-aligned line number in the gutter.
    let num = start.line.to_string();
    let lead = " ".repeat(gutter - num.len());
    let _ = writeln!(out, "{lead}{num} | {line_text}");

    // Underline. Carets span the label's columns on the start line; a
    // multi-line span underlines to end-of-line.
    let caret = if label.primary { '^' } else { '-' };
    let underline_cols = if end.line == start.line {
        (end.column.saturating_sub(start.column)).max(1)
    } else {
        (line_text.chars().count() as u32 + 1).saturating_sub(start.column)
    };
    let spaces = " ".repeat(start.column.saturating_sub(1) as usize);
    let carets: String = std::iter::repeat(caret)
        .take(underline_cols.max(1) as usize)
        .collect();

    if label.message.is_empty() {
        let _ = writeln!(out, "{pad} | {spaces}{carets}");
    } else {
        let _ = writeln!(out, "{pad} | {spaces}{carets} {}", label.message);
    }
}

// ===< The machine-readable form >===

/// One diagnostic, as a build tool reads it.
///
/// **The shape mirrors [`Diagnostic`] rather than inventing a wire format**, and
/// adds the two things only the [`SourceMap`] can answer: the file's name and
/// each span's line and column. A consumer that wants to point at source has
/// them; one that wants to print the compiler's own text has `rendered`, so a
/// tool forwarding a message never has to reimplement the terminal renderer to
/// stay readable.
///
/// Byte offsets travel **beside** the line and column, not instead of them. An
/// editor works in one and a person works in the other, and computing either
/// from the other needs the file — which is exactly what the consumer does not
/// have.
#[derive(serde::Serialize)]
pub struct JsonDiagnostic<'a> {
    /// `error`, `warning`, `note`, `help` — the word the terminal prints.
    pub severity: &'static str,
    /// The LSP `DiagnosticSeverity` code, so a language server needs no table.
    pub severity_code: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'a str>,
    pub message: &'a str,
    pub labels: Vec<JsonLabel<'a>>,
    pub notes: &'a [String],
    /// The human render, exactly as `--error-format=human` would have printed
    /// it, newline and all.
    pub rendered: String,
}

/// One span annotation, resolved against the source it points into.
#[derive(serde::Serialize)]
pub struct JsonLabel<'a> {
    /// The file's display name, which is the path it was read from.
    pub file: &'a str,
    /// Whether this is what the diagnostic is *about*, as opposed to context.
    pub primary: bool,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub message: &'a str,
    pub start: JsonPos,
    pub end: JsonPos,
}

/// A position: the byte offset, and the line and column it falls on.
#[derive(serde::Serialize)]
pub struct JsonPos {
    pub offset: usize,
    /// 1-based.
    pub line: u32,
    /// 1-based, counted in `char`s.
    pub column: u32,
}

/// Render a diagnostic as one line of JSON.
///
/// **One object per line** (JSON Lines), because a build tool reads the
/// compiler's stderr as a stream and a top-level array could not be parsed until
/// the compiler exited. A label whose file is not in the map is dropped rather
/// than guessed at — the same degradation the terminal renderer makes.
pub fn render_json(diag: &Diagnostic, sources: &SourceMap) -> String {
    let labels = diag
        .labels
        .iter()
        .filter_map(|l| {
            let file = sources.file(l.span.file)?;
            let pos = |offset: usize| {
                let lc = file.line_col(offset);
                JsonPos {
                    offset,
                    line: lc.line,
                    column: lc.column,
                }
            };
            Some(JsonLabel {
                file: &file.name,
                primary: l.primary,
                message: &l.message,
                start: pos(l.span.span.start),
                end: pos(l.span.span.end),
            })
        })
        .collect();

    let value = JsonDiagnostic {
        severity: diag.severity.label(),
        severity_code: diag.severity.lsp_code(),
        code: diag.code.as_deref(),
        message: &diag.message,
        labels,
        notes: &diag.notes,
        rendered: render(diag, sources),
    };
    // A diagnostic that cannot be serialized would be a diagnostic lost, so the
    // failure is reported in the one format that cannot fail.
    match serde_json::to_string(&value) {
        Ok(line) => format!("{line}\n"),
        Err(e) => format!("{{\"severity\":\"error\",\"message\":\"cannot serialize a diagnostic: {e}\"}}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::diagnostic::{Diagnostic, Label};
    use crate::common::source::{FileSpan, SourceMap};
    use crate::common::span::Span;

    #[test]
    fn renders_single_line_error() {
        let mut sources = SourceMap::new();
        let file = sources.add("main.nest", "let x = ;\n");
        // caret under the `;` at column 9 (0-based byte 8)
        let diag = Diagnostic::error("expected an expression")
            .with_code("E0001")
            .with_primary(
                FileSpan::new(file, Span::new(8, 9)),
                "expected an expression",
            )
            .with_note("statements end at a newline");

        let rendered = render(&diag, &sources);
        assert!(rendered.starts_with("error[E0001]: expected an expression\n"));
        assert!(rendered.contains("--> main.nest:1:9"));
        assert!(rendered.contains("1 | let x = ;"));
        assert!(rendered.contains("^ expected an expression"));
        assert!(rendered.contains("= note: statements end at a newline"));
    }

    #[test]
    fn caret_offset_matches_column() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.nest", "  bad\n");
        let diag = Diagnostic::error("x").with_primary(FileSpan::new(file, Span::new(2, 5)), "");
        let rendered = render(&diag, &sources);
        // two leading spaces before the caret run of length 3
        assert!(rendered.contains("\n  |   ^^^\n"), "got:\n{rendered}");
    }

    #[test]
    fn secondary_label_uses_dashes() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.nest", "ab cd\n");
        let diag = Diagnostic::error("x").with_label(Label::secondary(
            FileSpan::new(file, Span::new(0, 2)),
            "here",
        ));
        let rendered = render(&diag, &sources);
        assert!(rendered.contains("-- here"), "got:\n{rendered}");
    }

    /// The machine-readable form carries what the human one shows *plus* the
    /// two things a consumer cannot compute without the source: the file's name
    /// and the line and column of every span.
    #[test]
    fn json_carries_the_resolved_positions_and_the_render() {
        let mut sources = SourceMap::new();
        let file = sources.add("main.nest", "let x = ;\n");
        let diag = Diagnostic::error("expected an expression")
            .with_code("E0001")
            .with_primary(FileSpan::new(file, Span::new(8, 9)), "here")
            .with_note("statements end at a newline");

        let line = render_json(&diag, &sources);
        assert!(line.ends_with('\n'), "one object per line");
        assert_eq!(line.matches('\n').count(), 1, "and only one line: {line}");

        let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(v["severity"], "error");
        assert_eq!(v["severity_code"], 1);
        assert_eq!(v["code"], "E0001");
        assert_eq!(v["message"], "expected an expression");
        assert_eq!(v["notes"][0], "statements end at a newline");

        let label = &v["labels"][0];
        assert_eq!(label["file"], "main.nest");
        assert_eq!(label["primary"], true);
        assert_eq!(label["message"], "here");
        assert_eq!(label["start"], serde_json::json!({"offset": 8, "line": 1, "column": 9}));
        assert_eq!(label["end"], serde_json::json!({"offset": 9, "line": 1, "column": 10}));

        // A tool that just wants to show the compiler's own text has it, and
        // does not have to reimplement the renderer to stay readable.
        assert_eq!(
            v["rendered"].as_str().expect("a render"),
            render(&diag, &sources)
        );
    }

    /// A label pointing into a file the map does not have is dropped rather
    /// than guessed at — the same degradation the terminal renderer makes, and
    /// the diagnostic itself still arrives.
    #[test]
    fn json_drops_a_label_with_no_source() {
        let sources = SourceMap::new();
        let diag = Diagnostic::error("boom").with_primary(
            FileSpan::new(crate::common::source::FileId(7), Span::new(0, 1)),
            "",
        );
        let v: serde_json::Value =
            serde_json::from_str(&render_json(&diag, &sources)).expect("valid JSON");
        assert_eq!(v["message"], "boom");
        assert_eq!(v["labels"].as_array().expect("an array").len(), 0);
        assert!(v.get("code").is_none(), "an absent code is absent, not null");
    }

    #[test]
    fn unknown_file_degrades_to_header() {
        let sources = SourceMap::new();
        let diag = Diagnostic::error("boom").with_primary(
            FileSpan::new(crate::common::source::FileId(7), Span::new(0, 1)),
            "",
        );
        let rendered = render(&diag, &sources);
        assert_eq!(rendered, "error: boom\n");
    }
}
