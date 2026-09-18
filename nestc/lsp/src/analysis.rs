//! One analysis: a `nestc` command line run as far as type checking, over the
//! files on disk with the editor's buffers in their place, and what it found in
//! the protocol's terms.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{
    DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString, Position, Range,
    Uri,
};
use nestc::common::diagnostic::{Diagnostic, Severity};
use nestc::common::source::{FileId, FileSpan, SourceMap};
use nestc::driver::Invocation;
use nestc::parser::parse::Parser;
use nestc::sema;
use nestc::sema::session::{FileLoader, FsLoader, Session, resolve_import};

/// The open documents' text, by path.
pub type Buffers = Arc<HashMap<PathBuf, String>>;

/// The filesystem, with every open document read from its buffer instead.
struct Overlay {
    buffers: Buffers,
}

impl FileLoader for Overlay {
    fn load(&self, from: &str, spec: &str) -> Result<(String, String), String> {
        let path = resolve_import(from, spec);
        match self.buffers.get(&path) {
            Some(text) => Ok((path.to_string_lossy().into_owned(), text.clone())),
            None => FsLoader.load(from, spec),
        }
    }
}

/// What one analysis found.
pub struct Outcome {
    /// Every file it read from source, whether or not it had anything to say
    /// about it: the files whose diagnostics it is the authority on.
    pub files: HashSet<PathBuf>,
    pub diagnostics: HashMap<PathBuf, Vec<lsp_types::Diagnostic>>,
    /// The open documents among `files` that parsed without an error. What a
    /// parser could not make sense of is left out of the tree, and nothing in
    /// it has a type.
    pub parsed: HashSet<PathBuf>,
    /// The analyzed session itself, for the questions asked about it later.
    pub session: Session,
}

/// Analyze what `args` compiles. A command line that cannot even be set up — a
/// library that is missing or was built by another compiler — is an error
/// rather than an outcome, because there is no file to pin it to.
pub fn analyze(args: &[String], buffers: Buffers) -> Result<Outcome, String> {
    let inv = Invocation::parse(args.iter().cloned())?;
    let entry = inv.path.clone().ok_or("the command line names no file")?;
    let mut session = inv.session(Box::new(Overlay {
        buffers: buffers.clone(),
    }))?;
    if let Some(file) = session.load_entry(&entry) {
        sema::analyze(&mut session, file);
    }

    let mut files = HashSet::new();
    for i in 0..session.sources.len() {
        let id = FileId(i as u32);
        if session.is_foreign_file(id) {
            continue;
        }
        if let Some(file) = session.sources.file(id) {
            files.insert(PathBuf::from(&file.name));
        }
    }
    let entry = PathBuf::from(entry);
    let mut diagnostics: HashMap<PathBuf, Vec<lsp_types::Diagnostic>> = HashMap::new();
    for diag in &session.diagnostics {
        let (path, lsp) = convert(diag, &session.sources, &entry);
        diagnostics.entry(path).or_default().push(lsp);
    }
    let parsed = buffers
        .iter()
        .filter(|(path, text)| {
            files.contains(*path) && Parser::parse_file(text, FileId(0)).1.is_empty()
        })
        .map(|(path, _)| path.clone())
        .collect();
    Ok(Outcome {
        files,
        diagnostics,
        parsed,
        session,
    })
}

/// `diag` as the protocol has it, and the file it belongs in: its primary
/// label's, and the entry's when it has no label at all.
fn convert(
    diag: &Diagnostic,
    sources: &SourceMap,
    entry: &Path,
) -> (PathBuf, lsp_types::Diagnostic) {
    let located = |span: &FileSpan| {
        let file = sources.file(span.file)?;
        Some((
            PathBuf::from(&file.name),
            range(&file.src, span.span.start, span.span.end),
        ))
    };
    let label = diag.primary_label().or(diag.labels.first());
    let (path, range) = label
        .and_then(|l| located(&l.span))
        .unwrap_or_else(|| (entry.to_path_buf(), Range::default()));

    let mut message = diag.message.clone();
    if let Some(label) = label
        && !label.message.is_empty()
        && label.message != diag.message
    {
        message.push('\n');
        message.push_str(&label.message);
    }
    for note in &diag.notes {
        message.push('\n');
        message.push_str(note);
    }
    let related: Vec<DiagnosticRelatedInformation> = diag
        .labels
        .iter()
        .filter(|l| label.is_none_or(|p| !std::ptr::eq(*l, p)))
        .filter_map(|l| {
            let (path, range) = located(&l.span)?;
            Some(DiagnosticRelatedInformation {
                location: Location::new(path_to_uri(&path)?, range),
                message: l.message.clone(),
            })
        })
        .collect();

    let lsp = lsp_types::Diagnostic {
        range,
        severity: Some(match diag.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Note => DiagnosticSeverity::INFORMATION,
            Severity::Help => DiagnosticSeverity::HINT,
        }),
        code: diag.code.clone().map(NumberOrString::String),
        source: Some("nestc".to_string()),
        message,
        related_information: (!related.is_empty()).then_some(related),
        ..Default::default()
    };
    (path, lsp)
}

/// The range between two byte offsets into `src`.
pub fn range(src: &str, start: usize, end: usize) -> Range {
    Range::new(position(src, start), position(src, end))
}

/// A byte offset into `src` as a line and a column, the column counted in
/// UTF-16 code units, which is what the protocol counts by default.
pub fn position(src: &str, offset: usize) -> Position {
    let offset = src.floor_char_boundary(offset);
    let before = &src[..offset];
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    let character = before[start..].encode_utf16().count();
    Position::new(line as u32, character as u32)
}

/// A line and a UTF-16 column in `src` as a byte offset, clamped to the line's
/// end and to the text's.
pub fn offset(src: &str, position: Position) -> usize {
    let mut start = 0;
    for _ in 0..position.line {
        match src[start..].find('\n') {
            Some(i) => start += i + 1,
            None => return src.len(),
        }
    }
    let end = src[start..].find('\n').map_or(src.len(), |i| start + i);
    let mut units = 0;
    for (i, c) in src[start..end].char_indices() {
        if units >= position.character as usize {
            return start + i;
        }
        units += c.len_utf16();
    }
    end
}

pub fn path_to_uri(path: &Path) -> Option<Uri> {
    let url = url::Url::from_file_path(path).ok()?;
    url.as_str().parse().ok()
}

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    url::Url::parse(uri.as_str()).ok()?.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Columns count UTF-16 code units: `ż` is one, and `𝄞` is two.
    #[test]
    fn a_column_counts_utf16_units() {
        let src = "a\nżb𝄞c";
        assert_eq!(position(src, 0), Position::new(0, 0));
        assert_eq!(position(src, 2), Position::new(1, 0));
        let c = src.find('c').unwrap();
        assert_eq!(position(src, c), Position::new(1, 4));
        // Past the end clamps to it.
        assert_eq!(position(src, 100), Position::new(1, 5));
        // And back.
        assert_eq!(offset(src, Position::new(1, 4)), c);
        assert_eq!(offset(src, Position::new(1, 99)), src.len());
        assert_eq!(offset(src, Position::new(7, 0)), src.len());
    }

    #[test]
    fn a_path_survives_a_uri() {
        let path = PathBuf::from("/tmp/a dir/ż.nest");
        let uri = path_to_uri(&path).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/a%20dir/%C5%BC.nest");
        assert_eq!(uri_to_path(&uri), Some(path));
    }
}
