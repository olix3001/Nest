//! Terminal renderer for [`Diagnostic`]s, drawn by [ariadne]:
//!
//! ```text
//! error: unknown package `deep`
//!    ╭─[ util/src/package.nest:1:1 ]
//!    │
//!  1 │ deep :: import <deep>
//!    │ ──────────┬──────────
//!    │           ╰──────────── not a dependency of `util`
//! ───╯
//! ```
//!
//! **The compiler builds [`Diagnostic`]s and nothing else knows ariadne exists.**
//! This file is the whole of the translation: a severity becomes a report kind,
//! a label becomes a coloured span, a note stays a note. What goes in is
//! unchanged, which is what keeps `--error-format=json` and a language server
//! reading the same data a person reads here.
//!
//! [ariadne]: https://docs.rs/ariadne

use std::fmt::Write as _;

use ariadne::{Cache, CharSet, Color, Config, Fmt, IndexType, Report, ReportKind, Source};

use super::diagnostic::{Diagnostic, Severity};
use super::source::{FileId, SourceMap};

/// Render a diagnostic to a `String` against `sources`, in colour when `color`
/// is set. Never panics: a label in a file the map does not have is dropped, and
/// a diagnostic with no label left is its header and notes alone.
pub fn render(diag: &Diagnostic, sources: &SourceMap, color: bool) -> String {
    let labels: Vec<_> = diag
        .labels
        .iter()
        .filter(|l| sources.file(l.span.file).is_some())
        .collect();
    let anchor = labels
        .iter()
        .find(|l| l.primary)
        .or_else(|| labels.first())
        .copied();
    let Some(anchor) = anchor else {
        return render_bare(diag, color);
    };

    let config = Config::default()
        .with_color(color)
        .with_index_type(IndexType::Byte)
        .with_char_set(CharSet::Unicode);
    let mut report = Report::build(kind(diag.severity), span(anchor.span))
        .with_config(config)
        .with_message(&diag.message);
    if let Some(code) = &diag.code {
        report = report.with_code(code);
    }
    for label in &labels {
        let colour = if label.primary {
            tint(diag.severity)
        } else {
            Color::Blue
        };
        // ariadne draws an underline only for a label with a message, so every
        // label gets one — empty if it had none, and `bare_underline` below
        // takes the arrow to nothing back out.
        let l = ariadne::Label::new(span(label.span))
            .with_color(colour)
            .with_message(&label.message);
        report = report.with_label(l);
    }
    for note in &diag.notes {
        report = report.with_note(note);
    }

    let mut out = Vec::new();
    let cache = MapCache {
        sources,
        loaded: Default::default(),
    };
    if report.finish().write(cache, &mut out).is_err() {
        return render_bare(diag, color);
    }
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if labels.len() == 1 && anchor.message.is_empty() {
        text = bare_underline(&text);
    }
    // ariadne colours a custom report kind's header whatever the config says, so
    // colour that was not asked for is taken back out here.
    if color { text } else { strip_ansi(&text) }
}

/// A lone label with no message, drawn as an underline and nothing else.
///
/// ariadne draws the label as `─┬─` with `╰──` below it pointing at the message,
/// and with no message that is an arrow to nothing. Only the simple shape is
/// rewritten — one `┬` over one arrow — so a span over several lines, which
/// ariadne draws differently, is left as it is.
fn bare_underline(text: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let is_arrow = |line: &str| {
        let plain = strip_ansi(line);
        let body = plain.trim_start().strip_prefix('│').unwrap_or("").trim();
        body.starts_with('╰') && body.chars().skip(1).all(|c| c == '─')
    };
    let arrows: Vec<usize> = (1..lines.len()).filter(|&i| is_arrow(lines[i])).collect();
    let [at] = arrows[..] else {
        return text.to_string();
    };
    if strip_ansi(lines[at - 1]).matches('┬').count() != 1 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (i, line) in lines.iter().enumerate() {
        match i {
            _ if i == at => {}
            _ if i == at - 1 => out.push_str(&line.replace('┬', "─")),
            _ => out.push_str(line),
        }
    }
    out
}

/// `s` without its ANSI escape sequences (`ESC [ ... letter`).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The header and notes, for a diagnostic with nothing in a file to point at: a
/// failure of the compilation rather than one found in the program.
fn render_bare(diag: &Diagnostic, color: bool) -> String {
    let mut out = String::new();
    let word = diag.severity.label();
    let head = match &diag.code {
        Some(code) => format!("{word}[{code}]"),
        None => word.to_string(),
    };
    if color {
        let _ = writeln!(out, "{}: {}", head.fg(tint(diag.severity)), diag.message);
    } else {
        let _ = writeln!(out, "{head}: {}", diag.message);
    }
    for note in &diag.notes {
        let _ = writeln!(out, "  = note: {note}");
    }
    out
}

/// The report kind, spelled the way the rest of the toolchain prints it: `error`,
/// not ariadne's `Error`, so a build tool's own `error:` lines match.
fn kind(severity: Severity) -> ReportKind<'static> {
    ReportKind::Custom(severity.label(), tint(severity))
}

fn tint(severity: Severity) -> Color {
    match severity {
        Severity::Error => Color::Red,
        Severity::Warning => Color::Yellow,
        Severity::Note => Color::Cyan,
        Severity::Help => Color::Green,
    }
}

fn span(s: super::source::FileSpan) -> (FileId, std::ops::Range<usize>) {
    (s.file, s.span.start..s.span.end.max(s.span.start))
}

/// The [`SourceMap`] as ariadne's source cache. Each file is split into lines
/// the first time a label lands in it, and only then.
struct MapCache<'a> {
    sources: &'a SourceMap,
    loaded: std::collections::HashMap<FileId, Source<&'a str>>,
}

impl<'a> Cache<FileId> for MapCache<'a> {
    type Storage = &'a str;

    fn fetch(&mut self, id: &FileId) -> Result<&Source<&'a str>, impl std::fmt::Debug> {
        let sources = self.sources;
        match self.loaded.entry(*id) {
            std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
            std::collections::hash_map::Entry::Vacant(e) => match sources.file(*id) {
                Some(file) => Ok(e.insert(Source::from(file.src.as_str()))),
                None => Err(format!("no source for file {}", id.0)),
            },
        }
    }

    /// The file's name, from the working directory when it is under it: the
    /// path a person would type to open it.
    fn display<'b>(&self, id: &'b FileId) -> Option<impl std::fmt::Display + 'b> {
        let name = &self.sources.file(*id)?.name;
        let relative = std::env::current_dir().ok().and_then(|cwd| {
            std::path::Path::new(name)
                .strip_prefix(cwd)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        });
        Some(relative.unwrap_or_else(|| name.clone()))
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
        rendered: render(diag, sources, false),
    };
    // A diagnostic that cannot be serialized would be a diagnostic lost, so the
    // failure is reported in the one format that cannot fail.
    match serde_json::to_string(&value) {
        Ok(line) => format!("{line}\n"),
        Err(e) => format!(
            "{{\"severity\":\"error\",\"message\":\"cannot serialize a diagnostic: {e}\"}}\n"
        ),
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
        let diag = Diagnostic::error("expected an expression")
            .with_code("E0001")
            .with_primary(
                FileSpan::new(file, Span::new(8, 9)),
                "expected an expression",
            )
            .with_note("statements end at a newline");

        let rendered = render(&diag, &sources, false);
        assert!(
            rendered.starts_with("[E0001] error: expected an expression\n"),
            "got:\n{rendered}"
        );
        assert!(rendered.contains("main.nest:1:9"), "got:\n{rendered}");
        assert!(rendered.contains("1 │ let x = ;"), "got:\n{rendered}");
        assert!(
            rendered.contains("expected an expression"),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains("Note: statements end at a newline"),
            "got:\n{rendered}"
        );
    }

    /// Offsets are **bytes**, which is what a [`Span`] holds; read as characters
    /// they would point past a multi-byte character by one column per extra byte.
    #[test]
    fn spans_are_byte_offsets() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.nest", "é bad\n");
        let diag =
            Diagnostic::error("x").with_primary(FileSpan::new(file, Span::new(3, 6)), "here");
        let rendered = render(&diag, &sources, false);
        assert!(rendered.contains("t.nest:1:3"), "got:\n{rendered}");
    }

    /// A label with nothing to say is an underline, not an arrow to nothing —
    /// with or without colour.
    #[test]
    fn a_label_with_no_message_is_a_bare_underline() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.nest", "ab cd\n");
        let diag = Diagnostic::error("x").with_primary(FileSpan::new(file, Span::new(3, 5)), "");
        for color in [false, true] {
            let rendered = strip_ansi(&render(&diag, &sources, color));
            assert!(
                !rendered.contains('╰') && !rendered.contains('┬'),
                "got:\n{rendered}"
            );
            assert!(rendered.contains("│    ──"), "got:\n{rendered}");
        }
    }

    /// Several labels keep their arrows, and a secondary one is drawn too.
    #[test]
    fn secondary_labels_are_drawn() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.nest", "let a: i32 := b\n");
        let diag = Diagnostic::error("type mismatch")
            .with_primary(FileSpan::new(file, Span::new(14, 15)), "this is a `bool`")
            .with_label(Label::secondary(
                FileSpan::new(file, Span::new(7, 10)),
                "expected because of this",
            ));
        let rendered = render(&diag, &sources, false);
        assert!(rendered.contains("this is a `bool`"), "got:\n{rendered}");
        assert!(
            rendered.contains("expected because of this"),
            "got:\n{rendered}"
        );
    }

    #[test]
    fn colour_is_only_written_when_asked_for() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.nest", "ab cd\n");
        let diag =
            Diagnostic::error("x").with_primary(FileSpan::new(file, Span::new(0, 2)), "here");
        assert!(!render(&diag, &sources, false).contains('\u{1b}'));
        assert!(render(&diag, &sources, true).contains('\u{1b}'));
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
        assert_eq!(
            label["start"],
            serde_json::json!({"offset": 8, "line": 1, "column": 9})
        );
        assert_eq!(
            label["end"],
            serde_json::json!({"offset": 9, "line": 1, "column": 10})
        );

        // A tool that just wants to show the compiler's own text has it, and
        // does not have to reimplement the renderer to stay readable.
        assert_eq!(
            v["rendered"].as_str().expect("a render"),
            render(&diag, &sources, false)
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
        assert!(
            v.get("code").is_none(),
            "an absent code is absent, not null"
        );
    }

    #[test]
    fn unknown_file_degrades_to_header() {
        let sources = SourceMap::new();
        let diag = Diagnostic::error("boom").with_primary(
            FileSpan::new(crate::common::source::FileId(7), Span::new(0, 1)),
            "",
        );
        let rendered = render(&diag, &sources, false);
        assert_eq!(rendered, "error: boom\n");
    }
}
