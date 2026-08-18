use serde::{Deserialize, Serialize};

/// Span represents a range in a source file.
/// Only information about position inside a file is stored in it,
/// the file source and location itself should be stored separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Combine two spans to create a single, bigger, one.
    pub fn to(self, end: Span) -> Self {
        Self {
            start: self.start,
            end: end.end,
        }
    }
}
