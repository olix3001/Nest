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
use crate::common::options::Options;
use crate::parser::ast::Ast;
use crate::parser::parse::Parser;

use super::def::{DefId, DefKind, DefTable, LangItems, Visibility};
use super::imports::ImportDecl;

/// Where to find the `core` package when nothing says otherwise.
///
/// `core` is an ordinary package — a directory of `.nest` files rooted at
/// `core.nest` — and the compiler ships *no* copy of it. This default points at
/// the one in the repository, which is what lets the test suite and a dev build
/// run straight from a checkout; a real driver overrides it (see
/// [`Session::with_core_path`]) or sets `NEST_CORE`.
pub fn default_core_path() -> String {
    if let Ok(p) = std::env::var("NEST_CORE") {
        return p;
    }
    // `nestc/` sits next to `packages/` in the repository, which is where
    // every shipped package (`core`, and later `std`, `c`, ...) lives.
    format!("{}/../packages/core/package.nest", env!("CARGO_MANIFEST_DIR"))
}

/// Where to find the `std` package when nothing says otherwise.
///
/// `std` ships **with the compiler and is versioned with it**, the way Rust's
/// does: one `std` per `nestc`, so a program never has to resolve which one it
/// is being built against. This is what that decision costs — a second default
/// path beside `core`'s.
///
/// It is a *fallback* and not a link. Registering the path only makes
/// `import <std/io>` resolvable; a program that does not write that import gets
/// nothing from here, and `-L` or `--package std=<path>` replaces it — which is
/// how `twig` will hand over a `std` it resolved itself.
pub fn default_std_path() -> String {
    if let Ok(p) = std::env::var("NEST_STD") {
        return p;
    }
    format!("{}/../packages/std/package.nest", env!("CARGO_MANIFEST_DIR"))
}

/// The fixed-name primitive types the prelude makes available without an import
/// (§4.6). They have no source definition; each becomes a [`DefKind::Primitive`]
/// def in the builtins scope.
///
/// `never` is here for its *spelling* only: it was always the type of `return`,
/// `break` and a `loop` with no `break`, and this is what lets a signature
/// promise divergence (`abort :: func () -> never`). `string` is **not** here —
/// `str` is an ordinary `distinct []u8` in `core`, found by its `#lang` tag; a
/// stale entry meant `x: string` resolved to a primitive with no `Ty`, silently
/// becoming an error type with no diagnostic.
///
/// The width-parameterized *spellings* are **not** listed here: the signed /
/// unsigned integers `i<N>` / `u<N>` (arbitrary `N` in `1..=65535`, `i1`
/// excluded, `u1` an alias of `bool`) and the floats `f16`/`f32`/`f64`/`f80`/
/// `f128`. There are far too many to pre-register, so the resolver synthesizes
/// each on first use and interns it into the builtins scope (see
/// `sema::resolve`). `isize`/`usize` are the only pointer-sized integers; there
/// is no bare `int`/`uint`.
/// `int` and `uint` are the two integer **families** (§3.1): generic type
/// constructors taking one `const N: u16` width, of which every `i<N>` /
/// `u<N>` is an instance. They are fixed names rather than synthesized ones
/// because they carry no width in the name — `int.<32>` *is* `i32`, written
/// with the width as an argument instead of as spelling.
///
/// `usize` / `isize` are **not** here: they are `distinct` types declared in
/// `core/num.nest` over `uint.<PTR_BITS>` / `int.<PTR_BITS>`, found by `#lang`
/// tag like every other core type the compiler wires to.
/// The one file of `core` the compiler **writes** rather than reads: the build's
/// own settings (see [`Session::target_module_source`]). It is a member of
/// `core` rather than a package of its own so that it can name `core`'s `Os` /
/// `Arch` / `Profile` enums — the coupling is then between two files of one
/// package, not between the compiler and a library's vocabulary.
pub const TARGET_FILE: &str = "target.nest";

pub const PRIMITIVES: &[&str] = &["bool", "char", "int", "never", "uint", "void"];

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
        load_from_fs(from, spec)
    }
}

/// `path` with its `.` and `..` components folded away, lexically.
///
/// The path is a file's identity, so `json/../encode.nest` and `encode.nest`
/// beside it must spell one key — otherwise a file reached two ways is loaded
/// twice and every type in it is declared twice.
fn normalize(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(out.components().next_back(), Some(Component::Normal(_))) =>
            {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Resolve `spec` against `from`'s directory and read it. Shared by [`FsLoader`]
/// and by [`MemLoader`]'s fallback.
fn load_from_fs(from: &str, spec: &str) -> Result<(String, String), String> {
    use std::path::Path;
    let base = Path::new(from).parent().unwrap_or_else(|| Path::new(""));
    let mut path = normalize(&base.join(spec));
    if path.extension().is_none() {
        path.set_extension("nest");
    }
    let key = path.to_string_lossy().into_owned();
    match std::fs::read_to_string(&path) {
        Ok(src) => Ok((key, src)),
        Err(err) => Err(format!("cannot read import \"{spec}\": {err}")),
    }
}

/// An in-memory loader for tests: `import "spec"` looks `spec` up in a map,
/// after normalizing away a leading `./` and a `.nest` suffix, and falls back to
/// the real filesystem when the map has no entry.
///
/// The fallback is what lets an in-memory program import the on-disk `core`: the
/// overlay-then-disk shape is also what an editor needs, where unsaved buffers
/// shadow the files behind them.
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
    fn load(&self, from: &str, spec: &str) -> Result<(String, String), String> {
        let key = Self::norm(spec);
        if let Some(src) = self.files.get(&key) {
            return Ok((format!("mem:{key}"), src.clone()));
        }
        load_from_fs(from, spec)
            .map_err(|err| format!("no in-memory file for import \"{spec}\", and {err}"))
    }
}

/// A registered package: its name and the path of its root namespace file.
/// `<std>` evaluates to that root; `<std/io>` walks into its public member `io`.
///
/// The path goes through the session's [`FileLoader`], so a package's own files
/// import each other exactly the way user files do — there is no package-internal
/// import mechanism, because a package is just a directory.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub root_path: String,
    /// Whether something *said* where this package is, as opposed to the
    /// compiled-in default for `core`.
    ///
    /// It is the difference between a fact and a fallback. A `-L` directory
    /// holding a `core/` is a person pointing at the `core` they mean, and it
    /// has to beat the path this binary was built with — otherwise a compiler
    /// built from a checkout would keep compiling against that checkout
    /// wherever it was run. An **explicit** registration is the opposite: a
    /// build tool that resolved a package to a path has already done the
    /// searching, and a directory on the command line must not silently
    /// substitute a different copy.
    pub explicit: bool,
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
    /// The IR's id allocator and per-node side table (spans above all), shared
    /// by every lowered file.
    ///
    /// It sits on the session rather than on each
    /// [`Program`](crate::ir::Program) because [`IrId`](crate::ir::IrId)s are
    /// unique across the whole compilation: lowering is per file, but every pass
    /// after it is whole-program and must be able to ask for a node's span
    /// without first working out which file the node came from.
    pub ir_meta: crate::ir::Meta,
    /// Every lowered function in the compilation, merged out of [`Session::ir`]
    /// once lowering is done. This is what the whole-program passes — validation,
    /// monomorphization, LIR lowering — read; the per-file programs above stay
    /// as the record of what lowering produced for each file on its own.
    pub linked: crate::ir::Linked,
    /// The whole-program `impl` index, and each impl's target resolved into
    /// types.
    ///
    /// Inference builds and borrows the table, then drops it; monomorphization
    /// needs the same answers long afterwards — a `Dispatch::Generic` call is a
    /// bound with no impl chosen, and choosing one is exactly a search over
    /// this. The two travel together because the second is indexed by the
    /// first: `impl_targets[i]` is the resolved target of `impls.impls[i]`.
    pub impls: super::impls::ImplTable,
    pub impl_targets: Vec<super::infer::ImplTarget>,
    /// Everything this build decided for the compiler — the target, and the
    /// run-time behaviours a profile chooses (§ [`Options`]).
    ///
    /// Public and plainly assignable: the driver fills it from `-C` pairs, a
    /// test assigns it directly, and every stage that cares reads it from here
    /// rather than assuming an answer of its own.
    pub options: Options,
    /// The synthetic builtins namespace (primitives) that backs the prelude.
    pub builtins: DefId,
    /// Namespaces globbed into every file's outermost scope (the prelude:
    /// builtins + the public members of `core`).
    pub prelude_globs: Vec<DefId>,
    /// Registered packages, by name.
    packages: HashMap<String, Package>,
    /// Directories searched for a package that nothing registered (`-L`).
    ///
    /// A package `foo` is `<dir>/foo/foo.nest` — the layout `packages/` in this
    /// repository already has, and the one a package tool would unpack into.
    /// Searched in the order given, so the first `-L` wins, which is what a
    /// person overriding one library with a local build expects.
    search_paths: Vec<String>,
    /// Which loaded files are package roots, and under what package name — used
    /// to give a package's members a `pkg.member` canonical path.
    pub pkg_of: HashMap<FileId, String>,
    /// Each loaded package's directory, by name: what a file's path inside the
    /// package — and so its canonical path — is measured from.
    pkg_dir: HashMap<String, String>,
    /// Cache: source key → the [`FileId`] it was parsed into (parse-once).
    cache: HashMap<String, FileId>,
    loader: Box<dyn FileLoader>,
}

impl Session {
    /// A session using the real filesystem loader, with `core` at
    /// [`default_core_path`].
    pub fn new() -> Self {
        Self::with_loader(Box::new(FsLoader))
    }

    /// A session with a custom [`FileLoader`] (e.g. [`MemLoader`] for tests).
    pub fn with_loader(loader: Box<dyn FileLoader>) -> Self {
        Self::with_loader_and_core(loader, &default_core_path())
    }

    /// A session whose `core` package root is `core_path`. The path is resolved
    /// by the session's loader, so an in-memory `core` is as valid as an on-disk
    /// one.
    pub fn with_core_path(core_path: &str) -> Self {
        Self::with_loader_and_core(Box::new(FsLoader), core_path)
    }

    fn with_loader_and_core(loader: Box<dyn FileLoader>, core_path: &str) -> Self {
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
            defs.get_mut(builtins)
                .ns
                .members
                .insert(Symbol::new(prim), id);
        }
        let mut session = Self {
            sources: SourceMap::new(),
            defs,
            lang_items: LangItems::new(),
            diagnostics: Vec::new(),
            asts: HashMap::new(),
            files: HashMap::new(),
            ir: HashMap::new(),
            ir_meta: crate::ir::Meta::new(),
            linked: crate::ir::Linked::default(),
            impls: super::impls::ImplTable::default(),
            impl_targets: Vec::new(),
            options: Options::default(),
            builtins,
            prelude_globs: vec![builtins],
            packages: HashMap::new(),
            search_paths: Vec::new(),
            pkg_of: HashMap::new(),
            pkg_dir: HashMap::new(),
            cache: HashMap::new(),
            loader,
        };
        // `core` starts on the compiled-in path, as a **fallback**: a `-L`
        // directory with a `core/` in it replaces this, and an explicit
        // registration always does.
        session.packages.insert(
            "core".to_string(),
            Package {
                name: "core".to_string(),
                root_path: core_path.to_string(),
                explicit: false,
            },
        );
        // `std` the same way, and for the same reason it is a *fallback*: it
        // ships with this compiler, so `import <std/io>` resolves out of the
        // box, and a `-L` directory or a `--package std=` replaces it. Unlike
        // `core` it is not globbed into anything — nothing reaches it without
        // an import naming it.
        session.packages.insert(
            "std".to_string(),
            Package {
                name: "std".to_string(),
                root_path: default_std_path(),
                explicit: false,
            },
        );
        session
    }

    /// Register a package by name and root *path*. Call before [`Session::analyze`]
    /// to make `import <name/...>` resolvable; the package manager passes the set
    /// of linked libraries this way. `core` is registered automatically.
    pub fn register_package(&mut self, name: &str, root_path: &str) {
        self.packages.insert(
            name.to_string(),
            Package {
                name: name.to_string(),
                root_path: root_path.to_string(),
                explicit: true,
            },
        );
    }

    /// Add a directory to search for packages nothing registered (`-L`).
    ///
    /// Call before analysis: a package is found the first time it is imported,
    /// and a path added afterwards is a path that arrives too late to matter.
    pub fn add_search_path(&mut self, dir: &str) {
        self.search_paths.push(dir.to_string());
    }

    /// Where `name`'s root file is, by the rules a `-L` directory sets up.
    ///
    /// A package is a **directory named after itself** holding a root file of
    /// the same name: `foo` is `<dir>/foo/foo.nest`. One convention, checked on
    /// the filesystem rather than guessed at, so that "unknown package" means
    /// the directories were searched and it was not in any of them.
    fn search_for_package(&self, name: &str) -> Option<String> {
        use std::path::Path;
        self.search_paths.iter().find_map(|dir| {
            let root = Path::new(dir).join(name).join("package.nest");
            root.exists().then(|| root.to_string_lossy().into_owned())
        })
    }

    /// The package `name` resolves to: an explicit registration, then the search
    /// paths, then a non-explicit default.
    fn package(&self, name: &str) -> Option<Package> {
        match self.packages.get(name) {
            Some(p) if p.explicit => Some(p.clone()),
            registered => self
                .search_for_package(name)
                .map(|root_path| Package {
                    name: name.to_string(),
                    root_path,
                    explicit: false,
                })
                .or_else(|| registered.cloned()),
        }
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
                    file,
                    err.span,
                    err.message,
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
        let pkg = self.package(name)?;
        // The root is loaded through the loader so that the *file name* it is
        // parsed under is its real path: that name is what a relative
        // `import "sibling.nest"` inside the package resolves against.
        let (path, src) = match self.loader.load("", &pkg.root_path) {
            Ok(pair) => pair,
            Err(msg) => {
                self.diagnostics.push(Diagnostic::error(format!(
                    "cannot load package `{name}`: {msg}"
                )));
                return None;
            }
        };
        let file = self.parse_cached(&key, &path, &src);
        self.pkg_of.insert(file, pkg.name.clone());
        if let Some(dir) = parent_of(&path) {
            self.pkg_dir.insert(pkg.name.clone(), dir.to_string());
        }
        Some(file)
    }

    /// Where a file of package `pkg` sits inside it, as namespace segments —
    /// the part of its canonical path after the package's own name.
    ///
    /// Every file is a namespace of its own, the way a Rust module is, so the
    /// path is the file's: `std/serialize/json/writer.nest` is
    /// `std.serialize.json.writer`. The root `package.nest` is the package
    /// itself, and a directory's file of the same name (`serialize/serialize.nest`)
    /// is the directory, so neither adds a segment.
    pub fn module_path(&self, pkg: &str, file_name: &str) -> Vec<Symbol> {
        let Some(rel) = self
            .pkg_dir
            .get(pkg)
            .and_then(|dir| file_name.strip_prefix(dir.as_str()))
            .map(|r| r.trim_start_matches('/'))
        else {
            return Vec::new();
        };
        let mut segments: Vec<&str> = rel.split('/').collect();
        if let Some(last) = segments.pop() {
            let stem = last.strip_suffix(".nest").unwrap_or(last);
            let is_dir_root = segments.last() == Some(&stem);
            if !(segments.is_empty() && stem == "package") && !is_dir_root {
                segments.push(stem);
            }
        }
        segments.into_iter().map(Symbol::new).collect()
    }

    /// Whether `from` is a file of the `core` package, so a `target.nest` beside
    /// it is *the* generated one rather than some other package's file of the
    /// same name.
    fn is_core_sibling(&self, from: &str) -> bool {
        // Through `package`, not the map: a `-L` core has a different parent
        // directory from the compiled-in one, and it is the one being read.
        let Some(core) = self.package("core") else {
            return false;
        };
        let root = normalize(std::path::Path::new(&core.root_path));
        match (parent_of(&root.to_string_lossy()), parent_of(from)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// The source of the generated `target` file.
    ///
    /// Everything the build decided that a *program* may reasonably ask about,
    /// as ordinary public constants. It is a real Nest file, parsed and checked
    /// like any other, because that is what keeps it honest: `PTR_BITS` is a
    /// `u16` constant here for the same reason an integer width is a `u16`
    /// everywhere else (§3.1), and `core` reads it with an ordinary import.
    ///
    /// `PTR_BITS` in particular is what lets `usize` be *defined* rather than
    /// built in — `usize :: distinct uint.<PTR_BITS>` in `core` — so the one
    /// target-dependent number in the type system enters through a declaration a
    /// reader can look at.
    fn target_module_source(&self) -> String {
        let o = &self.options;
        format!(
            "// Generated by the compiler. Describes the build in progress; the\n\
             // types it names are declared in `os.nest` beside it.\n\
             {{ Os, Arch, Profile }} :: import \"os.nest\"\n\
             \n\
             @public PTR_BITS: u16 :: {bits}\n\
             @public OS: Os :: .{os}\n\
             @public ARCH: Arch :: .{arch}\n\
             @public PROFILE: Profile :: .{profile}\n\
             \n\
             // The two C types whose width or signedness the target decides.\n\
             // `core/c` names them; they are here because this is the file that\n\
             // knows what the build is, and because `c.nest` cannot spell them\n\
             // itself — it declares `int`, which shadows the family there.\n\
             @public C_LONG  :: {clong}\n\
             @public C_ULONG :: {culong}\n\
             @public C_CHAR  :: {cchar}\n",
            bits = o.target.pointer_bits,
            os = variant(o.target.os),
            arch = variant(o.target.arch),
            profile = variant(o.profile),
            clong = format!("i{}", c_long_bits(&o.target)),
            culong = format!("u{}", c_long_bits(&o.target)),
            cchar = if c_char_signed(&o.target) { "i8" } else { "u8" },
        )
    }

    /// Resolve and load a sibling file spec relative to `from`. Reports the
    /// loader error (pinned at `span`) and returns `None` on failure.
    pub fn load_file(&mut self, from: &str, spec: &str, at: FileSpan) -> Option<FileId> {
        // `core`'s `target.nest` is generated, not read: it describes the build,
        // which lives in `Options` and not on disk. It is intercepted here, at
        // the sibling-import hop, so that everything else about it is ordinary —
        // it parses, collects, resolves and type-checks like any core file.
        if spec == TARGET_FILE && self.is_core_sibling(from) {
            let key = format!("gen:{TARGET_FILE}");
            let src = self.target_module_source();
            // Named with `core`'s real directory, not a placeholder: the file's
            // name is what its own relative imports resolve against, and it
            // imports `os.nest` beside it.
            let name = match parent_of(from) {
                Some(dir) => format!("{dir}/{TARGET_FILE}"),
                None => TARGET_FILE.to_string(),
            };
            let file = self.parse_cached(&key, &name, &src);
            self.pkg_of.insert(file, "core".to_string());
            return Some(file);
        }
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

/// The directory part of a path key, or `None` if it has no separator.
fn parent_of(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

/// The width of a C `long`, in bits.
///
/// Two data models are in play and the split is by operating system, not by
/// architecture: Windows is LLP64, where a `long` stays 32 bits however wide a
/// pointer is, and everything else here is LP64 (or ILP32), where a `long` is
/// exactly a pointer wide.
fn c_long_bits(target: &crate::common::options::Target) -> u32 {
    match target.os {
        "windows" => 32,
        _ => target.pointer_bits,
    }
}

/// Whether a C `char` is signed.
///
/// A plain `char` is a third type distinct from `signed char` and `unsigned
/// char`, and which one it matches is the ABI's choice. It is signed on x86 and
/// on Apple's and Microsoft's ARM64, and unsigned on the ARM and RISC-V
/// psABIs — which is the rule below, and the reason `c.char` is generated
/// rather than written down once in `c.nest`.
fn c_char_signed(target: &crate::common::options::Target) -> bool {
    match target.arch {
        "aarch64" | "riscv64" => matches!(target.os, "macos" | "windows"),
        _ => true,
    }
}

/// A setting's spelling as the enum variant `core/os.nest` declares for it:
/// `x86_64` is `X86_64`, `macos` is `MacOs`, `freebsd` is `FreeBsd`.
///
/// Underscore-separated words keep their underscores and each word is
/// capitalized, which is the one rule that produces every variant in that file.
fn variant(setting: &str) -> String {
    setting
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("_")
}
