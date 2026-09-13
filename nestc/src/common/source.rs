//! Source files, file ids, and byte-offset → line/column resolution.
//!
//! A [`Span`] is only a byte range; to turn it into something a human (or an
//! editor) can act on you need the file's text. [`SourceMap`] owns every loaded
//! file and hands out [`FileId`]s; [`SourceFile`] precomputes line boundaries so
//! that resolving an offset to a [`LineCol`] is a binary search, not a re-scan.
//!
//! The line/column model is chosen to be **LSP-friendly**: [`LineCol`] exposes
//! both a 1-based form (for terminal diagnostics) and a 0-based form (what the
//! Language Server Protocol wants). See [`LineCol::lsp`] for the one caveat a
//! real LSP server will eventually have to close.

use serde::{Deserialize, Serialize};

use super::span::Span;

/// Identifies the source file a node originates from. Because `import` splices
/// members from other files into a namespace, a node's file is tracked
/// independently of the arena it ends up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(pub u32);

/// A [`Span`] paired with the [`FileId`] it lives in — enough to locate a range
/// unambiguously across the whole [`SourceMap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileSpan {
    pub file: FileId,
    pub span: Span,
}

impl FileSpan {
    pub fn new(file: FileId, span: Span) -> Self {
        Self { file, span }
    }
}

/// A resolved position inside a file.
///
/// `line` and `column` are **1-based** and count Unicode scalar values
/// (`char`s), which is what a terminal wants to print. The LSP protocol instead
/// wants 0-based positions; use [`LineCol::lsp`] for that form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column, counted in `char`s from the start of the line.
    pub column: u32,
}

impl LineCol {
    /// The 0-based `(line, character)` pair the Language Server Protocol expects.
    ///
    /// Note: LSP counts `character` in UTF-16 code units by default, while this
    /// counts `char`s. For the bootstrap compiler (and any all-BMP source) the
    /// two agree; a production server negotiating `positionEncoding` will want to
    /// recompute the column against the raw line text.
    pub fn lsp(self) -> (u32, u32) {
        (self.line - 1, self.column - 1)
    }
}

/// A single loaded source file with precomputed line boundaries.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Id assigned by the owning [`SourceMap`].
    pub id: FileId,
    /// Display name (usually a path). Only used for diagnostics.
    pub name: String,
    /// The full source text.
    pub src: String,
    /// Byte offset of the start of each line. Always begins with `0`; strictly
    /// increasing; `line_starts.len()` is the number of lines.
    line_starts: Vec<usize>,
}

impl SourceFile {
    fn new(id: FileId, name: String, src: String) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        Self {
            id,
            name,
            src,
            line_starts,
        }
    }

    /// Number of lines in the file (a trailing newline does not add an empty
    /// final line unless there is text after it).
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// The 0-based index of the line containing byte `offset`.
    fn line_index(&self, offset: usize) -> usize {
        // `partition_point` returns the count of starts `<= offset`; subtract one
        // to land on the line that actually contains the offset.
        self.line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1)
    }

    /// Resolve a byte offset to a 1-based line/column. Offsets past the end clamp
    /// to the last position rather than panicking.
    pub fn line_col(&self, offset: usize) -> LineCol {
        let offset = offset.min(self.src.len());
        let line = self.line_index(offset);
        let line_start = self.line_starts[line];
        let column = self.src[line_start..offset].chars().count() + 1;
        LineCol {
            line: line as u32 + 1,
            column: column as u32,
        }
    }

    /// The text of a 1-based `line`, without its trailing newline. Returns `""`
    /// for out-of-range lines.
    pub fn line_text(&self, line: u32) -> &str {
        let Some(line) = (line as usize).checked_sub(1) else {
            return "";
        };
        let Some(&start) = self.line_starts.get(line) else {
            return "";
        };
        let end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.src.len());
        self.src[start..end].trim_end_matches(['\n', '\r'])
    }
}

/// Owns every loaded [`SourceFile`] and hands out [`FileId`]s. The [`FileId`]
/// index is just the position in `files`, so lookups are `O(1)`.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a file and return its fresh [`FileId`].
    pub fn add(&mut self, name: impl Into<String>, src: impl Into<String>) -> FileId {
        let id = FileId(self.files.len() as u32);
        self.files
            .push(SourceFile::new(id, name.into(), src.into()));
        id
    }

    /// Borrow a loaded file, or `None` if `id` was never added.
    pub fn file(&self, id: FileId) -> Option<&SourceFile> {
        self.files.get(id.0 as usize)
    }

    /// Every file loaded, in the order they were added.
    pub fn files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(src: &str) -> SourceFile {
        SourceFile::new(FileId(0), "test.nest".into(), src.into())
    }

    #[test]
    fn line_col_basics() {
        let f = file("abc\ndef\n\nghi");
        assert_eq!(f.line_col(0), LineCol { line: 1, column: 1 });
        assert_eq!(f.line_col(2), LineCol { line: 1, column: 3 });
        // offset 4 is the 'd' at start of line 2
        assert_eq!(f.line_col(4), LineCol { line: 2, column: 1 });
        // the empty line 3
        assert_eq!(f.line_col(8), LineCol { line: 3, column: 1 });
        assert_eq!(f.line_col(9), LineCol { line: 4, column: 1 });
    }

    #[test]
    fn column_counts_chars_not_bytes() {
        let f = file("héllo");
        // after the two-byte 'é', the 'l' is column 3, not 4
        let byte_of_l = "hé".len();
        assert_eq!(f.line_col(byte_of_l), LineCol { line: 1, column: 3 });
    }

    #[test]
    fn line_text_strips_newline() {
        let f = file("first\r\nsecond\n");
        assert_eq!(f.line_text(1), "first");
        assert_eq!(f.line_text(2), "second");
        assert_eq!(f.line_text(99), "");
    }

    #[test]
    fn offset_past_end_clamps() {
        let f = file("ab");
        assert_eq!(f.line_col(999), LineCol { line: 1, column: 3 });
    }

    #[test]
    fn lsp_form_is_zero_based() {
        assert_eq!(LineCol { line: 3, column: 5 }.lsp(), (2, 4));
    }

    #[test]
    fn source_map_assigns_sequential_ids() {
        let mut map = SourceMap::new();
        let a = map.add("a.nest", "x");
        let b = map.add("b.nest", "y");
        assert_eq!(a, FileId(0));
        assert_eq!(b, FileId(1));
        assert_eq!(map.file(a).unwrap().name, "a.nest");
        assert!(map.file(FileId(2)).is_none());
    }
}
