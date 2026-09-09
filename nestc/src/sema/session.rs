//! The analysis session — the library-first entry point every front end (the
//! `nestc` binary, a future LSP server, tests) drives.
//!
//! A [`Session`] owns everything a whole-program analysis touches: the
//! [`SourceMap`], one parsed [`Ast`] per loaded file, the package registry, the
//! global [`DefTable`], the [`LangItems`] registry, and the collected
//! diagnostics. Files are parsed **once** and cached; importing the same file or
//! package twice reuses the first result.
//!
//! The orchestration is a worklist: starting from an entry file, [`Session`]
//! parses and collects a file's definitions, discovers its `import`s, and
//! enqueues their targets — loading sibling files (`import "..."`) through a
//! pluggable [`FileLoader`] and packages (`import <...>`) from the registry. Once
//! the transitive set is collected, the import / resolve / desugar stages run.

use std::collections::HashMap;

use crate::common::diagnostic::Diagnostic;
use crate::common::source::{FileId, FileSpan, SourceMap};
use crate::common::span::Span;
use crate::common::symbol::Symbol;
use crate::parser::ast::Ast;
use crate::parser::parse::Parser;

use super::def::{DefId, DefKind, DefTable, LangItems, Visibility};
use super::imports::ImportDecl;

/// The embedded bootstrap core library source (see `core/prelude.nest`).
pub const CORE_SRC: &str = include_str!("core/prelude.nest");

/// The fixed-name primitive types the prelude makes available without an import
/// (§4.6). They have no source definition; each becomes a [`DefKind::Primitive`]
/// def in the builtins scope.
///
/// The width-parameterized primitives are **not** listed here: the signed /
/// unsigned integers `i<N>` / `u<N>` (arbitrary `N` in `1..=65535`, `i1`
/// excluded, `u1` an alias of `bool`) and the floats `f16`/`f32`/`f64`/`f80`/
/// `f128`. There are far too many to pre-register, so the resolver synthesizes
/// each on first use and interns it into the builtins scope (see
/// `sema::resolve`). `isize`/`usize` are the only pointer-sized integers; there
/// is no bare `int`/`uint`.
pub const PRIMITIVES: &[&str] = &["bool", "char", "string", "isize", "usize", "void"];

/// Resolves `import "spec"` file specifiers to a stable key and source text.
///
/// Injecting this is what lets the analyzer run against an in-memory file set in
/// tests (and, later, an editor's unsaved buffers) instead of only the real
/// filesystem.
pub trait FileLoader {
    /// Resolve `spec` — exactly as written in `import "spec"` — relative to the
    /// importing file `from`. Returns `(key, source)` where `key` is a stable
    /// identity used for the parse-once cache, or an error message.
    fn load(&self, from: &str, spec: &str) -> Result<(String, String), String>;
}

/// The default loader: resolve the spec against the importing file's directory
/// on the real filesystem.
#[derive(Debug, Default)]
pub struct FsLoader;

impl FileLoader for FsLoader {
    fn load(&self, from: &str, spec: &str) -> Result<(String, String), String> {
        use std::path::Path;
        let base = Path::new(from).parent().unwrap_or_else(|| Path::new(""));
        let mut path = base.join(spec);
        if path.extension().is_none() {
            path.set_extension("nest");
        }
        let key = path.to_string_lossy().into_owned();
        match std::fs::read_to_string(&path) {
            Ok(src) => Ok((key, src)),
            Err(err) => Err(format!("cannot read import \"{spec}\": {err}")),
        }
    }
}

/// An in-memory loader for tests: `import "spec"` looks `spec` up in a map,
/// after normalizing away a leading `./` and a `.nest` suffix.
#[derive(Debug, Default)]
pub struct MemLoader {
    files: HashMap<String, String>,
}

impl MemLoader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, name: &str, src: &str) -> Self {
        self.files.insert(Self::norm(name), src.to_string());
        self
    }

    fn norm(spec: &str) -> String {
        spec.trim_start_matches("./")
            .trim_end_matches(".nest")
            .to_string()
    }
}

impl FileLoader for MemLoader {
    fn load(&self, _from: &str, spec: &str) -> Result<(String, String), String> {
        let key = Self::norm(spec);
        match self.files.get(&key) {
            Some(src) => Ok((format!("mem:{key}"), src.clone())),
            None => Err(format!("no in-memory file for import \"{spec}\"")),
        }
    }
}

/// A registered package: its name and the source of its root namespace. `<std>`
/// evaluates to this root; `<std/io>` walks into its public member `io`.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub root_src: String,
}

/// Per-file analysis bookkeeping, created when a file is collected. The parsed
/// [`Ast`] itself lives in [`Session::asts`] (populated earlier, at parse time).
pub struct FileMeta {
    /// The namespace def representing this whole file (§4: a file *is* an
    /// anonymous namespace).
    pub ns: DefId,
    /// Import declarations found in the file, wired up by the imports stage.
    pub imports: Vec<ImportDecl>,
    /// Display name / path, mirrored from the [`SourceMap`] for convenience.
    pub name: String,
}

/// The whole-compilation analysis database.
pub struct Session {
    pub sources: SourceMap,
    pub defs: DefTable,
    pub lang_items: LangItems,
    pub diagnostics: Vec<Diagnostic>,
    /// Parsed syntax trees, keyed by [`FileId`] (populated at parse time).
    pub asts: HashMap<FileId, Ast>,
    /// Per-file analysis metadata, keyed by [`FileId`] (populated at collection).
    pub files: HashMap<FileId, FileMeta>,
    /// The lowered IR of each file, keyed by [`FileId`] (populated by the lower
    /// stage after inference).
    pub ir: HashMap<FileId, crate::ir::Program>,
    /// The synthetic builtins namespace (primitives) that backs the prelude.
    pub builtins: DefId,
    /// Namespaces globbed into every file's outermost scope (the prelude:
    /// builtins + the public members of `core`).
    pub prelude_globs: Vec<DefId>,
    /// Registered packages, by name.
    packages: HashMap<String, Package>,
    /// Which loaded files are package roots, and under what package name — used
    /// to give a package's members a `pkg.member` canonical path.
    pub pkg_of: HashMap<FileId, String>,
    /// Cache: source key → the [`FileId`] it was parsed into (parse-once).
    cache: HashMap<String, FileId>,
    loader: Box<dyn FileLoader>,
}

impl Session {
    /// A session using the real filesystem loader and only the embedded `core`
    /// package registered.
    pub fn new() -> Self {
        Self::with_loader(Box::new(FsLoader))
    }

    /// A session with a custom [`FileLoader`] (e.g. [`MemLoader`] for tests).
    pub fn with_loader(loader: Box<dyn FileLoader>) -> Self {
        let mut defs = DefTable::new();
        // The builtins namespace holds the primitive types.
        let builtins = defs.alloc(
            Symbol::new("<builtins>"),
            DefKind::Namespace,
            Visibility::Public,
            None,
            None,
            None,
            None,
            Vec::new(),
        );
        for prim in PRIMITIVES {
            let id = defs.alloc(
                Symbol::new(prim),
                DefKind::Primitive,
                Visibility::Public,
                Some(builtins),
                None,
                None,
                None,
                vec![Symbol::new(prim)],
            );
            defs.get_mut(builtins).ns.members.insert(Symbol::new(prim), id);
        }
        let mut session = Self {
            sources: SourceMap::new(),
            defs,
            lang_items: LangItems::new(),
            diagnostics: Vec::new(),
            asts: HashMap::new(),
            files: HashMap::new(),
            ir: HashMap::new(),
            builtins,
            prelude_globs: vec![builtins],
            packages: HashMap::new(),
            pkg_of: HashMap::new(),
            cache: HashMap::new(),
            loader,
        };
        session.register_package("core", CORE_SRC);
        session
    }

    /// Register a package by name and root source. Call before [`Session::analyze`]
    /// to make `import <name/...>` resolvable; the package manager passes the set
    /// of linked libraries this way. `core` is registered automatically.
    pub fn register_package(&mut self, name: &str, root_src: &str) {
        self.packages.insert(
            name.to_string(),
            Package {
                name: name.to_string(),
                root_src: root_src.to_string(),
            },
        );
    }

    // ===< Parse-once loading >===

    /// Parse `src` named `name` into a fresh file, or return the cached
    /// [`FileId`] if `key` was already loaded. Parse errors are folded into the
    /// session diagnostics. Does not collect defs — the caller drives that.
    fn parse_cached(&mut self, key: &str, name: &str, src: &str) -> FileId {
        if let Some(&id) = self.cache.get(key) {
            return id;
        }
        let file = self.sources.add(name.to_string(), src.to_string());
        let (ast, errors) = Parser::parse_file(src, file);
        for err in errors {
            self.diagnostics
                .push(crate::common::diagnostic::simple_error(
                    file, err.span, err.message,
                ));
        }
        self.asts.insert(file, ast);
        self.cache.insert(key.to_string(), file);
        file
    }

    /// Load a package's root namespace, returning its file. Reports an error and
    /// returns `None` for an unregistered package.
    pub fn load_package(&mut self, name: &str) -> Option<FileId> {
        let key = format!("pkg:{name}");
        if let Some(&id) = self.cache.get(&key) {
            return Some(id);
        }
        let pkg = self.packages.get(name)?.clone();
        let file = self.parse_cached(&key, &format!("<{}>", pkg.name), &pkg.root_src);
        self.pkg_of.insert(file, pkg.name.clone());
        Some(file)
    }

    /// Resolve and load a sibling file spec relative to `from`. Reports the
    /// loader error (pinned at `span`) and returns `None` on failure.
    pub fn load_file(&mut self, from: &str, spec: &str, at: FileSpan) -> Option<FileId> {
        match self.loader.load(from, spec) {
            Ok((key, src)) => Some(self.parse_cached(&key, &key, &src)),
            Err(msg) => {
                self.diagnostics
                    .push(Diagnostic::error(msg).with_primary(at, ""));
                None
            }
        }
    }

    /// Load the entry file through the loader (no importer). Handy for tests and
    /// for a driver that wants the [`MemLoader`] / [`FsLoader`] to own the entry
    /// too, rather than adding it to the [`SourceMap`] by hand.
    pub fn load_entry(&mut self, spec: &str) -> Option<FileId> {
        match self.loader.load("", spec) {
            Ok((key, src)) => Some(self.parse_cached(&key, &key, &src)),
            Err(msg) => {
                self.diagnostics.push(Diagnostic::error(msg));
                None
            }
        }
    }

    /// Whether a file has already been collected (has a [`FileMeta`]).
    pub fn is_collected(&self, file: FileId) -> bool {
        self.files.contains_key(&file)
    }

    // ===< Diagnostics helpers >===

    pub fn error(&mut self, file: FileId, span: Span, message: impl Into<String>) {
        self.diagnostics
            .push(crate::common::diagnostic::simple_error(file, span, message));
    }

    /// Whether any error-severity diagnostic was recorded.
    pub fn has_errors(&self) -> bool {
        use crate::common::diagnostic::Severity;
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
