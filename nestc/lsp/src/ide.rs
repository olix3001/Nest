//! Questions about a place in a file, answered from an analyzed session: what is
//! here (hover), where was it defined (go-to-definition), and what could be
//! written here (completion, in [`crate::complete`]).
//!
//! ### What is at an offset
//!
//! Every pass leaves its answer on the tree, so the question is which node to
//! ask. The nodes containing the offset are tried smallest first, and the first
//! that names something wins: a use resolved to a definition, a method call, or
//! a definition itself — but only on its **name**, since a definition's span is
//! its whole declaration, and a cursor in a function's body is not on the
//! function.
//!
//! ### Documentation
//!
//! A definition's documentation is the `///` lines right above it, attributes
//! and directives in between allowed.

use std::borrow::Cow;

use nestc::common::source::FileId;
use nestc::common::span::Span;
use nestc::parser::ast::{Ast, NodeId, NodeKind};
use nestc::sema::def::{Def, DefId, DefKind};
use nestc::sema::infer::MethodRes;
use nestc::sema::session::Session;
use nestc::sema::ty::Ty;
use nestc::sema::{DefMeta, PathRes, Resolution};

/// What an offset is on.
#[derive(Debug, Clone, Copy)]
pub struct Found {
    pub def: DefId,
    /// The name the offset is on, in the file asked about.
    pub span: Span,
    /// The node that uses the definition there, or `None` on the definition's
    /// own name.
    pub node: Option<NodeId>,
}

/// The file a session read from `path`, and not from a library.
pub fn file_of(s: &Session, path: &std::path::Path) -> Option<FileId> {
    (0..s.sources.len() as u32).map(FileId).find(|&id| {
        !s.is_foreign_file(id)
            && s.sources
                .file(id)
                .is_some_and(|f| std::path::Path::new(&f.name) == path)
    })
}

/// A file's text: as the session read it, or from disk for a library that did
/// not carry it.
pub fn source(s: &Session, file: FileId) -> Option<Cow<'_, str>> {
    let f = s.sources.file(file)?;
    if !f.src.is_empty() {
        return Some(Cow::Borrowed(&f.src));
    }
    std::fs::read_to_string(&f.name).ok().map(Cow::Owned)
}

/// What `offset` in `file` is on.
pub fn find(s: &Session, file: FileId, offset: usize) -> Option<Found> {
    let ast = s.asts.get(&file)?;
    let src = source(s, file)?;
    let mut nodes: Vec<NodeId> = ast
        .ids()
        .filter(|&id| {
            let n = ast.node(id);
            n.file == file && n.span.start <= offset && offset <= n.span.end
        })
        .collect();
    nodes.sort_by_key(|&id| {
        let span = ast.node(id).span;
        span.end - span.start
    });

    for id in nodes {
        let (span, kind) = {
            let n = ast.node(id);
            (n.span, n.kind.clone())
        };
        let found = |def: DefId, at: Span| Found {
            def,
            span: at,
            node: Some(id),
        };
        match &kind {
            NodeKind::Path { segments } => {
                let Some((i, at)) =
                    segment_at(&src, span, segments.iter().map(|s| s.as_str()), offset)
                else {
                    continue;
                };
                let res = if i + 1 == segments.len() {
                    ast.meta::<Resolution>(id)
                } else {
                    ast.meta::<PathRes>(id).and_then(|p| p.0.get(i).cloned())
                };
                if let Some(Resolution::Def(d)) = res {
                    return Some(found(d, at));
                }
                continue;
            }
            NodeKind::FieldAccess { name, .. } => {
                let at = Span::new(span.end.saturating_sub(name.as_str().len()), span.end);
                if !contains(at, offset) || src.get(at.start..at.end) != Some(name.as_str()) {
                    continue;
                }
                if let Some(Resolution::Def(d)) = ast.meta::<Resolution>(id) {
                    return Some(found(d, at));
                }
                if let Some(m) = ast.meta::<MethodRes>(id) {
                    return Some(found(m.method, at));
                }
                continue;
            }
            _ => {}
        }
        if let Some(Resolution::Def(d)) = ast.meta::<Resolution>(id)
            && let Some(at) = word_in(&src, span, s.defs.get(d).name.as_str())
            && contains(at, offset)
        {
            return Some(found(d, at));
        }
        if let Some(DefMeta(d)) = ast.meta::<DefMeta>(id)
            && let Some(at) = name_span(s, d)
            && contains(at, offset)
        {
            return Some(Found {
                def: d,
                span: at,
                node: None,
            });
        }
    }
    None
}

/// What `offset` is on when it is not a name: a tuple's member, which is a
/// position rather than a definition (`t.0`), so there is no `Found` for it.
///
/// The rendering and the span the editor should highlight, or `None` when the
/// offset is on something else.
pub fn tuple_member(s: &Session, file: FileId, offset: usize) -> Option<(String, Span)> {
    let ast = s.asts.get(&file)?;
    let src = source(s, file)?;
    let mut nodes: Vec<NodeId> = ast
        .ids()
        .filter(|&id| {
            let n = ast.node(id);
            n.file == file && n.span.start <= offset && offset <= n.span.end
        })
        .collect();
    nodes.sort_by_key(|&id| {
        let span = ast.node(id).span;
        span.end - span.start
    });
    for id in nodes {
        let n = ast.node(id);
        let NodeKind::TupleIndex { base, index } = &n.kind else {
            continue;
        };
        // The index is what is hovered, not the whole expression: `t.0` with
        // the cursor on `t` is the local, which `find` answers.
        let digits = index.to_string();
        let at = Span::new(n.span.end.saturating_sub(digits.len()), n.span.end);
        if !contains(at, offset) || src.get(at.start..at.end) != Some(digits.as_str()) {
            continue;
        }
        let of = match ast.meta::<Ty>(*base) {
            Some(Ty::Tuple(elems)) => elems,
            _ => continue,
        };
        let ty = of.get(*index as usize)?;
        let whole = Ty::Tuple(of.clone());
        return Some((
            format!(
                "```nest\n{}\n```\n\n```nest\n{digits}: {}\n```",
                show(s, &whole),
                show(s, ty)
            ),
            at,
        ));
    }
    None
}

/// `ty` as a hover prints it: with the associated types an `impl` return
/// type's bounds pinned, which only the declaration table knows.
pub fn show(s: &Session, ty: &Ty) -> String {
    let decls = nestc::sema::decl::Decls::new(&s.defs, &s.asts, &s.decls);
    ty.display_with(&s.defs, &|d| decls.param_pinned(d))
}

/// Where `def`'s name is written, in its own file.
pub fn name_span(s: &Session, def: DefId) -> Option<Span> {
    let d = s.defs.get(def);
    let (file, span) = (d.file?, d.span?);
    word_in(&source(s, file)?, span, d.name.as_str())
}

/// The definition an import names, or `def` itself.
pub fn target(s: &Session, def: DefId) -> DefId {
    s.defs.resolve_alias(def)
}

/// Hover text for `found`, as Markdown.
pub fn hover(s: &Session, file: FileId, found: Found) -> String {
    let def = target(s, found.def);
    let d = s.defs.get(def);
    let use_ty = found
        .node
        .and_then(|n| s.asts.get(&file)?.meta::<Ty>(n))
        .filter(|t| !matches!(t, Ty::Error | Ty::Var(_)));
    let mut out = String::new();
    if let Some(container) = container(d) {
        out.push_str(&format!("```nest\n{container}\n```\n\n"));
    }
    out.push_str(&format!("```nest\n{}\n```", declaration(s, def, use_ty)));
    if let Some(docs) = docs(s, def) {
        out.push_str("\n\n---\n\n");
        out.push_str(&docs);
    }
    out
}

/// Where `d` lives, when that says more than its name: `std.io` for
/// `std.io.print`.
fn container(d: &Def) -> Option<String> {
    if matches!(
        d.kind,
        DefKind::Local | DefKind::Param | DefKind::TypeParam | DefKind::ConstParam
    ) {
        return None;
    }
    let path = &d.canonical;
    (path.len() > 1).then(|| {
        path[..path.len() - 1]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(".")
    })
}

/// How many lines of a declaration a hover shows.
const DECLARATION_LINES: usize = 16;

/// `def` as it was declared: the source of its declaration, up to a function's
/// body; or, for a binding, its name and type.
pub fn declaration(s: &Session, def: DefId, use_ty: Option<Ty>) -> String {
    let d = s.defs.get(def);
    let typed = |ty: Option<Ty>| match ty {
        Some(t) => format!("{}: {}", d.name, show(s, &t)),
        None => d.name.to_string(),
    };
    match d.kind {
        DefKind::Local | DefKind::ConstParam => return typed(use_ty.or_else(|| def_ty(s, def))),
        DefKind::Param => return typed(def_ty(s, def).or(use_ty)),
        DefKind::Primitive | DefKind::External | DefKind::TypeParam => return d.name.to_string(),
        _ => {}
    }
    let text = (|| {
        // The **span** travels with the def and the tree does not, and
        // `source` already falls back to the file on disk — so a library's
        // declaration is quotable here without anything this compilation
        // parsed. The tree is used only to find where a function's body
        // starts; without one, the first brace serves.
        let file = d.file?;
        let ast = s.asts.get(&file);
        let src = source(s, file)?;
        let span = d.span.or_else(|| Some(ast?.node(d.node?).span))?;
        let end = match (ast, d.node) {
            (Some(ast), Some(node)) => body_start(ast, node).unwrap_or(span.end),
            _ => text_body_start(&src, span.start).unwrap_or(span.end),
        };
        let text = src.get(span.start..end.max(span.start))?.trim_end();
        Some(clip(text))
    })();
    match (text, d.kind) {
        (Some(text), _) if !text.is_empty() => text,
        (_, DefKind::Namespace) => format!(
            "namespace {}",
            d.canonical
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(".")
        ),
        _ => typed(use_ty),
    }
}

/// Where a function's body starts, when `node` binds one.
fn body_start(ast: &Ast, node: NodeId) -> Option<usize> {
    let rhs = match &ast.node(node).kind {
        NodeKind::ConstBind { rhs, .. } => *rhs,
        _ => node,
    };
    match &ast.node(rhs).kind {
        NodeKind::FuncExpr {
            body: Some(body), ..
        } => Some(ast.node(*body).span.start),
        _ => None,
    }
}

/// Where a declaration's body starts, found in the text rather than in a tree.
///
/// For a def read out of a library, which has no tree here. The first `{` after
/// the declaration's start opens a function's body — a parameter list and a
/// return type cannot contain one, because every type that could is written
/// with `[`, `(` or `.<`. A declaration with no brace at all (an `extern`
/// function, a constant) has no body and answers `None`, which leaves the whole
/// span quoted, as it should be.
fn text_body_start(src: &str, from: usize) -> Option<usize> {
    src.get(from..)?.find('{').map(|i| from + i)
}

/// `text`, cut after [`DECLARATION_LINES`] lines.
fn clip(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= DECLARATION_LINES {
        return text.to_string();
    }
    format!("{}\n  …", lines[..DECLARATION_LINES].join("\n"))
}

/// The type of a value definition, from its own node or from a use of it.
fn def_ty(s: &Session, def: DefId) -> Option<Ty> {
    let d = s.defs.get(def);
    // What the declaration table knows first: a field's type, a function's
    // signature, an associated constant's — all of which travel, and none of
    // which the tree below can answer for a definition out of a library.
    let decls = nestc::sema::decl::Decls::new(&s.defs, &s.asts, &s.decls);
    let recorded = match d.kind {
        DefKind::Func => decls.signature(def),
        DefKind::Field => decls.field_ty(def),
        DefKind::Const => decls.assoc_const_ty(def),
        _ => None,
    };
    if let Some(t) = recorded.filter(|t| !matches!(t, Ty::Error | Ty::Var(_))) {
        return Some(t);
    }
    let ast = s.asts.get(&d.file?)?;
    let usable = |t: &Ty| !matches!(t, Ty::Error | Ty::Var(_));
    if let Some(node) = d.node {
        if let Some(t) = ast.meta::<Ty>(node).filter(usable) {
            return Some(t);
        }
        // A `let` binding's type is its value's.
        for id in ast.ids() {
            if let NodeKind::LocalDecl { pattern, value, .. } = ast.node(id).kind
                && pattern == node
            {
                if let Some(t) = ast.meta::<Ty>(value).filter(usable) {
                    return Some(t);
                }
            }
        }
    }
    ast.ids().find_map(|id| match ast.meta::<Resolution>(id) {
        Some(Resolution::Def(r)) if r == def => ast.meta::<Ty>(id).filter(usable),
        _ => None,
    })
}

/// `def`'s doc comment: the `@doc` that `///` lines above it are sugar for.
pub fn docs(s: &Session, def: DefId) -> Option<String> {
    s.doc_of(def)
}

pub(crate) fn contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset <= span.end
}

/// Which of a path's segments, written in `span` of `src`, `offset` is on.
fn segment_at<'a>(
    src: &str,
    span: Span,
    segments: impl Iterator<Item = &'a str>,
    offset: usize,
) -> Option<(usize, Span)> {
    let mut from = span.start;
    for (i, seg) in segments.enumerate() {
        let at = word_in(src, Span::new(from, span.end), seg)?;
        if contains(at, offset) {
            return Some((i, at));
        }
        from = at.end;
    }
    None
}

/// The first place `word` is written in `span` of `src` as a whole identifier.
pub(crate) fn word_in(src: &str, span: Span, word: &str) -> Option<Span> {
    let text = src.get(span.start..span.end.min(src.len()))?;
    let ident = |c: char| c == '_' || c.is_alphanumeric();
    let mut from = 0;
    while let Some(i) = text[from..].find(word) {
        let start = from + i;
        let end = start + word.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        if !before.is_some_and(ident) && !after.is_some_and(ident) {
            return Some(Span::new(span.start + start, span.start + end));
        }
        from = end;
    }
    None
}
