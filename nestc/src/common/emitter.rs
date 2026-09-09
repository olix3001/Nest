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
