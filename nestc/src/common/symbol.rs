use std::fmt;

use serde::{Deserialize, Serialize};

/// An interned identifier name.
/// At this moment as a simple wrapper around [`Box<str>`],
/// but will be replaced with intering later on.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Symbol(Box<str>);

impl Symbol {
    pub fn new(text: &str) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Symbol {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for Symbol {
    fn from(text: String) -> Self {
        Self(text.into_boxed_str())
    }
}

impl AsRef<str> for Symbol {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Symbol({:?})", self.as_str())
    }
}
