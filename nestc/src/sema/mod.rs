//! Semantic analysis: everything between parsing and (a future) type checking.
//!
//! The parser hands each file to this layer as a standalone
//! [`Ast`](crate::parser::ast::Ast). `sema` turns that pile of syntax trees into
//! a resolved program by running a fixed pipeline of **stages**, each in its own
//! file, over a shared [`Session`]:
//!
//! 1. **collect** ([`collect`]) — build the global [`DefTable`](def::DefTable):
//!    one [`Def`](def::Def) per namespace-level definition, with visibility and
//!    `#lang` tags. Runs on every file transitively reached by `import`.
//! 2. **imports** ([`imports`]) — load each `import` target once (sibling files
//!    and packages), then bind its names into the importing scope per the binding
//!    pattern (§4.5).
//! 3. **resolve** ([`resolve`]) — walk each file with a scope stack and attach a
//!    [`Resolution`] to every name, linking uses to the unique def they refer to
//!    (§4.6).
//! 4. **desugar** ([`desugar`]) — lower `for` / `.?` / `.!` to their core
//!    `#lang` forms (§6.13).
//!
//! Results live in the [`Session`], not in the AST nodes: definitions in the
//! [`DefTable`], per-node facts in the arena's type-indexed metadata side table
//! (see [`Ast::set_meta`](crate::parser::ast::Ast::set_meta)). This is a library
//! — the `nestc` binary, a language server, and the tests all drive the same
//! [`Session`].

pub mod collect;
pub mod def;
pub mod desugar;
pub mod imports;
pub mod pretty;
pub mod resolve;
pub mod session;

#[cfg(test)]
mod tests;

use crate::common::source::{FileId, FileSpan};
use crate::common::symbol::Symbol;

use def::{DefId, DefKind, Visibility};
use imports::{ImportDecl, ImportTarget, RawImport, RawTarget};
use session::{FileMeta, Session};

// ===< Per-node metadata attached by the stages >===

/// Which [`Def`](def::Def) a name refers to. Attached to name nodes (single- and
/// multi-segment [`Path`](crate::parser::ast::NodeKind::Path)s, the namespace
/// hops of a [`FieldAccess`](crate::parser::ast::NodeKind::FieldAccess), a
/// [`TypePath`](crate::parser::ast::NodeKind::TypePath)) by the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Resolved to a unique definition.
    Def(DefId),
    /// A compiler `$`-intrinsic (no def).
    Intrinsic(Symbol),
    /// Could not be resolved (a diagnostic was reported).
    Error,
}

/// Marks a node that *introduces* a definition, linking it to its [`DefId`] (and
/// thus its canonical name). Attached by collection and by the resolver's local
/// binding introduction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefMeta(pub DefId);

/// The per-segment resolution of a multi-segment [`Path`], so every name in a
/// dotted path (e.g. `Self.Output`) is linked, not just the final one.
#[derive(Debug, Clone)]
pub struct PathRes(pub Vec<Resolution>);

// ===< Pipeline entry points >===

/// Convenience: create a session, register `packages`, load `src` as the entry
/// file named `name`, run the whole pipeline, and return the session.
pub fn analyze_source(name: &str, src: &str, packages: &[(&str, &str)]) -> Session {
    let mut session = Session::new();
    for (pkg, root) in packages {
        session.register_package(pkg, root);
    }
    let file = session.sources.add(name.to_string(), src.to_string());
    // Mirror the parse-once cache and ast store the loader path would populate.
    let (ast, errors) = crate::parser::parse::Parser::parse_file(src, file);
    for err in errors {
        session
            .diagnostics
            .push(crate::common::diagnostic::simple_error(
                file, err.span, err.message,
            ));
    }
    session.asts.insert(file, ast);
    analyze(&mut session, file);
    session
}

/// Run the full pipeline over `entry` (already parsed into `session.asts`) and
/// every file it transitively imports.
pub fn analyze(session: &mut Session, entry: FileId) {
    // The prelude globs `core`'s public members into every scope, so `core` must
    // be collected before anything resolves against it.
    if let Some(core_root) = session.load_package("core") {
        collect_reachable(session, vec![core_root]);
        let core_ns = session.files[&core_root].ns;
        session.prelude_globs.push(core_ns);
    }

    // Collect the entry file and its transitive imports.
    collect_reachable(session, vec![entry]);

    // The remaining stages run over every collected file (core included).
    let files: Vec<FileId> = session.files.keys().copied().collect();
    for &file in &files {
        imports::wire(session, file);
    }
    for &file in &files {
        resolve_one(session, file);
    }
    for &file in &files {
        desugar_one(session, file);
    }
}

/// Drain a worklist of files, parsing (already done by the loader), creating each
/// file's namespace def, collecting its definitions, and enqueuing the targets of
/// its imports. Files are processed at most once.
fn collect_reachable(session: &mut Session, mut queue: Vec<FileId>) {
    while let Some(file) = queue.pop() {
        if session.is_collected(file) {
            continue;
        }
        let name = session
            .sources
            .file(file)
            .map(|f| f.name.clone())
            .unwrap_or_default();
        let canonical = session
            .pkg_of
            .get(&file)
            .map(|n| vec![Symbol::new(n)])
            .unwrap_or_default();
        let ns_name = canonical
            .first()
            .cloned()
            .unwrap_or_else(|| Symbol::new("<file>"));
        let ns = session.defs.alloc(
            ns_name,
            DefKind::Namespace,
            Visibility::Public,
            None,
            Some(file),
            None,
            None,
            canonical,
        );
        session.files.insert(
            file,
            FileMeta {
                ns,
                imports: Vec::new(),
                name: name.clone(),
            },
        );

        // Collect definitions (disjoint field borrows while reading the ast).
        let raw_imports = {
            let Session {
                asts,
                defs,
                lang_items,
                diagnostics,
                ..
            } = &mut *session;
            let ast = &asts[&file];
            collect::collect_file(defs, lang_items, diagnostics, ast, file, ns)
        };

        // Load each import target and enqueue it for collection.
        let mut decls = Vec::with_capacity(raw_imports.len());
        for raw in raw_imports {
            let (target, enqueue) = load_target(session, file, &name, &raw);
            if let Some(f) = enqueue {
                queue.push(f);
            }
            decls.push(ImportDecl {
                pattern: raw.pattern,
                scope: raw.scope,
                reexport: raw.reexport,
                target,
                span: raw.span,
            });
        }
        session.files.get_mut(&file).unwrap().imports = decls;
    }
}

/// Load a [`RawImport`]'s target once, returning the resolved [`ImportTarget`]
/// and the file (if any) to enqueue for collection.
fn load_target(
    session: &mut Session,
    from: FileId,
    from_name: &str,
    raw: &RawImport,
) -> (ImportTarget, Option<FileId>) {
    match &raw.target {
        RawTarget::File(spec) => {
            match session.load_file(from_name, spec, FileSpan::new(from, raw.span)) {
                Some(f) => (ImportTarget::File(f), Some(f)),
                None => (ImportTarget::Broken, None),
            }
        }
        RawTarget::Package(segs) => {
            let pkg = segs[0].as_str();
            match session.load_package(pkg) {
                Some(root) => (
                    ImportTarget::PackageMember(root, segs[1..].to_vec()),
                    Some(root),
                ),
                None => {
                    session.error(from, raw.span, format!("unknown package `{pkg}`"));
                    (ImportTarget::Broken, None)
                }
            }
        }
    }
}

fn resolve_one(session: &mut Session, file: FileId) {
    let ns = session.files[&file].ns;
    let globs = session.prelude_globs.clone();
    let Session {
        asts,
        defs,
        diagnostics,
        ..
    } = &mut *session;
    let ast = &asts[&file];
    resolve::resolve_file(defs, diagnostics, ast, file, ns, &globs);
}

fn desugar_one(session: &mut Session, file: FileId) {
    let ns = session.files[&file].ns;
    let Session {
        asts,
        defs,
        lang_items,
        diagnostics,
        ..
    } = &mut *session;
    let ast = asts.get_mut(&file).unwrap();
    desugar::desugar_file(ast, defs, lang_items, diagnostics, file, ns);
}
