//! Import resolution (§4.5) — the stage that turns each `import` binding into
//! names in the importing file's scope.
//!
//! Collection (see [`super::collect`]) records a [`RawImport`] per `import`
//! binding without touching the filesystem. The session driver then *loads* each
//! target (parsing sibling files and package roots once), turning every
//! [`RawImport`] into an [`ImportDecl`] whose target is a concrete namespace
//! [`DefId`]. Finally [`wire`] applies the binding **pattern** on the left of the
//! `::` — a whole-namespace bind, a `*` glob, or a `{ ... }` destructuring
//! (optionally nested, optionally re-exported) — populating the importer's
//! [`Namespace`].

use crate::common::span::Span;
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, NodeId, NodeKind};

use super::def::{DefId, DefKind, DefTable, Namespace, Visibility};
use super::session::Session;

/// A target as written, before loading: the operand of `import`.
#[derive(Debug, Clone)]
pub enum RawTarget {
    /// `import <a/b/c>` — a package path.
    Package(Vec<Symbol>),
    /// `import "spec"` — a file path, exactly as written.
    File(String),
}

/// An `import` binding discovered during collection, not yet loaded.
#[derive(Debug, Clone)]
pub struct RawImport {
    /// The binding pattern on the left of `::`.
    pub pattern: NodeId,
    /// The namespace the binding populates (the enclosing scope of the `import`).
    pub scope: DefId,
    /// Whether the binding is `@public` (re-exports what it brings in).
    pub reexport: bool,
    pub target: RawTarget,
    /// A `#lang("tag")` written on the binding, registered against the imported
    /// namespace once it is known. The prelude is the reason this exists (§4.6):
    /// `core.prelude` is a namespace assembled by re-export, so the binding that
    /// names it is the only thing a tag can sit on.
    pub lang: Option<Symbol>,
    pub span: Span,
}

/// A loaded import target. Kept as file references (not a namespace [`DefId`])
/// because a target file may be collected *after* the import is recorded; the
/// concrete namespace is looked up at wiring time.
#[derive(Debug, Clone)]
pub enum ImportTarget {
    /// A loaded sibling file's whole namespace.
    File(crate::common::source::FileId),
    /// A package: its root file plus the public member path to walk
    /// (`<std/io/net>` = root `std`, members `["io", "net"]`).
    PackageMember(crate::common::source::FileId, Vec<Symbol>),
    /// The target could not be loaded/resolved; an error was already reported.
    /// Bindings still create [`DefKind::External`] stubs so later uses resolve.
    Broken,
}

/// A [`RawImport`] after its target has been loaded to a namespace def.
#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub pattern: NodeId,
    pub scope: DefId,
    pub reexport: bool,
    pub target: ImportTarget,
    pub lang: Option<Symbol>,
    pub span: Span,
}

/// Wire every import of `file` into the file namespace `scope`. Runs after all
/// transitively-imported files have been collected, so every target namespace
/// already has its members.
pub fn wire(session: &mut Session, file: crate::common::source::FileId) {
    // Take the imports out so we can borrow the ast and defs freely; they are
    // consumed here and not put back.
    let imports = std::mem::take(&mut session.files.get_mut(&file).unwrap().imports);
    for imp in &imports {
        // Resolve the target to a concrete namespace now that every file is
        // collected. A missed package hop is reported and treated as broken.
        let base = match &imp.target {
            ImportTarget::File(f) => session.files.get(f).map(|m| m.ns),
            ImportTarget::PackageMember(root, members) => {
                let root_ns = session.files.get(root).map(|m| m.ns);
                match root_ns {
                    Some(ns) => match walk_package(&session.defs, ns, members) {
                        Ok(target) => Some(target),
                        Err(seg) => {
                            session.error(
                                file,
                                imp.span,
                                format!("package has no public member `{seg}`"),
                            );
                            None
                        }
                    },
                    None => None,
                }
            }
            ImportTarget::Broken => None,
        };
        let base = base.map(|d| session.defs.resolve_alias(d));
        // A `#lang` tag on the binding names the *target* namespace, not the
        // alias: whoever looks the tag up wants the members, and an alias that
        // mirrors them is one hop of indirection with nothing on the other side.
        if let (Some(tag), Some(base)) = (imp.lang.clone(), base) {
            let in_core = session.pkg_of.get(&file).map(String::as_str) == Some("core");
            match session.lang_items.set(tag.clone(), base, in_core) {
                Some(prev) if prev != base => {
                    session.error(file, imp.span, format!("duplicate `#lang(\"{tag}\")` item"));
                }
                _ => session.defs.get_mut(base).lang = Some(tag),
            }
        }
        // Disjoint field borrows: reading `asts` while mutating `defs`.
        let Session { asts, defs, .. } = &mut *session;
        let ast = &asts[&file];
        bind_pattern(defs, ast, imp.pattern, imp.scope, base, imp.reexport, file);
    }
}

/// Apply an import binding pattern, inserting the resulting names into `scope`.
#[allow(clippy::too_many_arguments)]
fn bind_pattern(
    defs: &mut DefTable,
    ast: &Ast,
    pattern: NodeId,
    scope: DefId,
    base: Option<DefId>,
    reexport: bool,
    file: crate::common::source::FileId,
) {
    let kind = ast.node(pattern).kind.clone();
    match kind {
        // `name :: import ...` — bind the whole namespace under `name`.
        NodeKind::BindingPat { name, .. } => {
            let alias = alias_def(defs, name.clone(), base, scope, file);
            insert(defs, scope, name, alias, reexport);
        }
        // `* :: import ...` — glob every public member into this scope.
        NodeKind::GlobPat => {
            if let Some(base) = base {
                if reexport {
                    // Re-export: the globbed members become public members here.
                    for (name, member) in public_members(defs, base) {
                        insert(defs, scope, name, member, true);
                    }
                } else {
                    defs.get_mut(scope).ns.globs.push(base);
                }
            }
        }
        // `{ a, b: pat, c: * } :: import ...` — selective destructuring.
        NodeKind::StructPat { fields, .. } => {
            for field in fields {
                bind_field(defs, ast, field, scope, base, reexport, file);
            }
        }
        // Anything else in import position is meaningless; ignore (collection
        // already reports odd shapes if needed).
        _ => {}
    }
}

/// Handle one `{ ... }` field: `name`, `name: rename`, `name: { ... }`, `name: *`.
#[allow(clippy::too_many_arguments)]
fn bind_field(
    defs: &mut DefTable,
    ast: &Ast,
    field: NodeId,
    scope: DefId,
    base: Option<DefId>,
    reexport: bool,
    file: crate::common::source::FileId,
) {
    let NodeKind::FieldPat { name, pattern, .. } = ast.node(field).kind.clone() else {
        return;
    };
    // Look the member up in the target namespace.
    let member = base.and_then(|b| lookup_public(defs, b, &name));
    match pattern.map(|p| ast.node(p).kind.clone()) {
        // `{ name }` — bind the member itself under its own name.
        None => {
            let bound = member.unwrap_or_else(|| external(defs, name.clone(), scope, file));
            insert(defs, scope, name, bound, reexport);
        }
        // `{ name: rename }` — bind the member under `rename`.
        Some(NodeKind::BindingPat { name: rename, .. }) => {
            let bound = member.unwrap_or_else(|| external(defs, rename.clone(), scope, file));
            insert(defs, scope, rename, bound, reexport);
        }
        // `{ name: * }` — select sub-namespace `name` and glob its members here.
        Some(NodeKind::GlobPat) => {
            if let Some(member) = member {
                let member = defs.resolve_alias(member);
                if reexport {
                    for (n, m) in public_members(defs, member) {
                        insert(defs, scope, n, m, true);
                    }
                } else {
                    defs.get_mut(scope).ns.globs.push(member);
                }
            }
        }
        // `{ name: { ... } }` — recurse into sub-namespace `name`.
        Some(NodeKind::StructPat { .. }) => {
            let sub = member.map(|m| defs.resolve_alias(m));
            if let Some(p) = pattern {
                bind_pattern(defs, ast, p, scope, sub, reexport, file);
            }
        }
        _ => {}
    }
}

/// Create the [`DefKind::Import`] alias def for a whole-namespace binding.
fn alias_def(
    defs: &mut DefTable,
    name: Symbol,
    base: Option<DefId>,
    scope: DefId,
    file: crate::common::source::FileId,
) -> DefId {
    match base {
        Some(base) => {
            let canonical = child_path(defs, scope, &name);
            let id = defs.alloc(
                name,
                DefKind::Import,
                Visibility::Private,
                Some(scope),
                Some(file),
                None,
                None,
                canonical,
            );
            defs.get_mut(id).alias = Some(base);
            // Mirror the target's members so a `.` hop through the alias works.
            let members = defs.get(base).ns.clone();
            defs.get_mut(id).ns = members;
            id
        }
        None => external(defs, name, scope, file),
    }
}

/// A stand-in def for an import that could not be loaded.
fn external(
    defs: &mut DefTable,
    name: Symbol,
    scope: DefId,
    file: crate::common::source::FileId,
) -> DefId {
    let canonical = child_path(defs, scope, &name);
    defs.alloc(
        name,
        DefKind::External,
        Visibility::Public,
        Some(scope),
        Some(file),
        None,
        None,
        canonical,
    )
}

/// Insert `def` into `scope` under `name`: as a public member when re-exported,
/// otherwise as a (file-private) imported name.
fn insert(defs: &mut DefTable, scope: DefId, name: Symbol, def: DefId, reexport: bool) {
    let ns = &mut defs.get_mut(scope).ns;
    if reexport {
        ns.members.insert(name, def);
    } else {
        ns.imported.insert(name, def);
    }
}

/// Public member `name` of namespace-like `base`, following alias chains.
pub fn lookup_public(defs: &DefTable, base: DefId, name: &Symbol) -> Option<DefId> {
    let base = defs.resolve_alias(base);
    let member = defs.get(base).ns.members.get(name).copied()?;
    let member = defs.resolve_alias(member);
    defs.get(member).vis.is_public().then_some(member)
}

/// All public members of `base` (following aliases), as `(name, def)` pairs.
fn public_members(defs: &DefTable, base: DefId) -> Vec<(Symbol, DefId)> {
    let base = defs.resolve_alias(base);
    defs.get(base)
        .ns
        .members
        .iter()
        .filter_map(|(n, &d)| {
            let d = defs.resolve_alias(d);
            defs.get(d).vis.is_public().then(|| (n.clone(), d))
        })
        .collect()
}

/// Build a canonical path for a new child `name` of `scope`.
fn child_path(defs: &DefTable, scope: DefId, name: &Symbol) -> Vec<Symbol> {
    let mut path = defs.get(scope).canonical.clone();
    path.push(name.clone());
    path
}

/// Walk a package member path from a package root namespace, honoring visibility.
/// The `/` separator only descends into **namespaces**: `<std/math>` is the
/// public `math` namespace of `std` (itself possibly `math :: import "..."`,
/// since [`lookup_public`] follows the alias to the namespace it names). A
/// segment that names a non-namespace member — e.g. a function in
/// `<std/math/fibonacci>` — is an error: `import` always yields a namespace.
/// Returns the final namespace def, or the offending segment on a miss.
pub fn walk_package(defs: &DefTable, root: DefId, members: &[Symbol]) -> Result<DefId, Symbol> {
    let mut cur = defs.resolve_alias(root);
    for seg in members {
        match lookup_public(defs, cur, seg) {
            Some(next) if defs.get(next).kind.is_namespace_like() => cur = next,
            _ => return Err(seg.clone()),
        }
    }
    Ok(cur)
}

impl Namespace {
    /// Whether this namespace has any glob imports (used by resolution).
    pub fn has_globs(&self) -> bool {
        !self.globs.is_empty()
    }
}
