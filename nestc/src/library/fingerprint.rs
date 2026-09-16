//! Whether a library is up to date, decided by content rather than by time.
//!
//! A library's header carries a **fingerprint**: a hash of everything its
//! compilation read — the compiler, the target and settings, each of its own
//! files, and the fingerprint of each library it was compiled against. Asking
//! whether it is stale is computing the same hash from what is on disk now and
//! comparing, which needs no clock and no `stat`: a file touched and left
//! unchanged is not a change, and a dependency rebuilt from the same inputs is
//! the same dependency.

/// FNV-1a, 64 bits. Not for anything adversarial — a fingerprint only has to
/// notice an edit — and stable across builds of this compiler, which is the
/// one property `std::hash` does not promise.
#[derive(Clone, Copy)]
pub struct Hasher(u64);

impl Default for Hasher {
    fn default() -> Self {
        Hasher(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher {
    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // The length after the bytes, so `"ab" + "c"` and `"a" + "bc"` differ.
        let len = bytes.len() as u64;
        for b in len.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self
    }

    pub fn u64(&mut self, n: u64) -> &mut Self {
        self.bytes(&n.to_le_bytes())
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// What a compilation read, in the order it is hashed.
pub struct Inputs<'a> {
    pub compiler: &'a str,
    pub target: &'a str,
    pub settings: &'a str,
    pub package: &'a str,
    /// Each file, by name, with its contents.
    pub files: Vec<(&'a str, &'a [u8])>,
    /// Each library, by name, with its fingerprint and whether it was importable.
    pub libraries: Vec<(&'a str, u64, bool)>,
}

pub fn compute(inputs: &Inputs) -> u64 {
    let mut h = Hasher::default();
    h.bytes(inputs.compiler.as_bytes())
        .bytes(inputs.target.as_bytes())
        .bytes(inputs.settings.as_bytes())
        .bytes(inputs.package.as_bytes());
    let mut files = inputs.files.clone();
    files.sort_by(|a, b| a.0.cmp(b.0));
    h.u64(files.len() as u64);
    for (name, contents) in files {
        h.bytes(name.as_bytes()).bytes(contents);
    }
    let mut libraries = inputs.libraries.clone();
    libraries.sort_by(|a, b| a.0.cmp(b.0));
    h.u64(libraries.len() as u64);
    for (name, fingerprint, importable) in libraries {
        h.bytes(name.as_bytes()).u64(fingerprint).u64(u64::from(importable));
    }
    h.finish()
}
