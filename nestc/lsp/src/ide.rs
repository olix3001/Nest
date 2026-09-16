//! Questions about a place in a file, answered from an analyzed session: what is
//! here (hover), where was it defined (go-to-definition), and what could be
//! written here (completion).
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
use std::collections::HashSet;

use lsp_types::{CompletionItem, CompletionItemKind, Documentation, MarkupContent, MarkupKind};
use nestc::common::source::FileId;
use nestc::common::span::Span;
use nestc::parser::ast::{Ast, NodeId, NodeKind};
use nestc::sema::def::{Def, DefId, DefKind, Visibility};
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
        !s.is_foreign_file(id) && s.sources.file(id).is_some_and(|f| std::path::Path::new(&f.name) == path)
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
        let found = |def: DefId, at: Span| Found { def, span: at, node: Some(id) };
        match &kind {
            NodeKind::Path { segments } => {
                let Some((i, at)) = segment_at(&src, span, segments.iter().map(|s| s.as_str()), offset) else {
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
            return Some(Found { def: d, span: at, node: None });
        }
    }
    None
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
    if matches!(d.kind, DefKind::Local | DefKind::Param | DefKind::TypeParam | DefKind::ConstParam) {
        return None;
    }
    let path = &d.canonical;
    (path.len() > 1).then(|| path[..path.len() - 1].iter().map(|s| s.as_str()).collect::<Vec<_>>().join("."))
}

/// How many lines of a declaration a hover shows.
const DECLARATION_LINES: usize = 16;

/// `def` as it was declared: the source of its declaration, up to a function's
/// body; or, for a binding, its name and type.
pub fn declaration(s: &Session, def: DefId, use_ty: Option<Ty>) -> String {
    let d = s.defs.get(def);
    let typed = |ty: Option<Ty>| match ty {
        Some(t) => format!("{}: {}", d.name, t.display(&s.defs)),
        None => d.name.to_string(),
    };
    match d.kind {
        DefKind::Local | DefKind::ConstParam => return typed(use_ty.or_else(|| def_ty(s, def))),
        DefKind::Param => return typed(def_ty(s, def).or(use_ty)),
        DefKind::Primitive | DefKind::External | DefKind::TypeParam => return d.name.to_string(),
        _ => {}
    }
    let text = (|| {
        let (file, node) = (d.file?, d.node?);
        let ast = s.asts.get(&file)?;
        let src = source(s, file)?;
        let span = d.span.unwrap_or(ast.node(node).span);
        let end = body_start(ast, node).unwrap_or(span.end);
        let text = src.get(span.start..end.max(span.start))?.trim_end();
        Some(clip(text))
    })();
    match (text, d.kind) {
        (Some(text), _) if !text.is_empty() => text,
        (_, DefKind::Namespace) => format!("namespace {}", d.canonical.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(".")),
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
        NodeKind::FuncExpr { body: Some(body), .. } => Some(ast.node(*body).span.start),
        _ => None,
    }
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

/// The `///` lines right above `def`, without their slashes.
pub fn docs(s: &Session, def: DefId) -> Option<String> {
    let d = s.defs.get(def);
    let src = source(s, d.file?)?;
    let start = d.span?.start.min(src.len());
    let line_start = src[..start].rfind('\n').map_or(0, |i| i + 1);
    let mut lines: Vec<&str> = Vec::new();
    for line in src[..line_start].lines().rev() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("///") {
            lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if lines.is_empty() && (t.starts_with('@') || t.starts_with('#')) {
            continue;
        } else {
            break;
        }
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

/// What could be written at `offset` in `file`, where the session was analyzed
/// with `placeholder` written there: a member after a `.`, or a name in scope.
pub fn complete(s: &Session, file: FileId, offset: usize, placeholder: &str) -> Vec<CompletionItem> {
    let Some(ast) = s.asts.get(&file) else {
        return Vec::new();
    };
    let at = offset + placeholder.len();
    let mut nodes: Vec<NodeId> = ast
        .ids()
        .filter(|&id| {
            let n = ast.node(id);
            n.file == file && n.span.start <= offset && at <= n.span.end
        })
        .collect();
    nodes.sort_by_key(|&id| {
        let span = ast.node(id).span;
        span.end - span.start
    });
    for id in nodes {
        let kind = ast.node(id).kind.clone();
        match kind {
            NodeKind::FieldAccess { base, name } if name.as_str().ends_with(placeholder) => {
                if let Some(Resolution::Def(d)) = ast.meta::<Resolution>(base) {
                    let d = target(s, d);
                    if s.defs.get(d).kind.is_namespace_like() {
                        return items(s, members(s, d));
                    }
                }
                return match ast.meta::<Ty>(base) {
                    Some(ty) => items(s, ty_members(s, &ty)),
                    None => Vec::new(),
                };
            }
            NodeKind::Path { segments } if segments.last().is_some_and(|l| l.as_str().ends_with(placeholder)) => {
                if segments.len() == 1 {
                    return scope(s, file, offset);
                }
                let before = ast.meta::<PathRes>(id).and_then(|p| p.0.get(segments.len() - 2).cloned());
                return match before {
                    Some(Resolution::Def(d)) => items(s, members(s, target(s, d))),
                    _ => Vec::new(),
                };
            }
            _ => {}
        }
    }
    scope(s, file, offset)
}

/// The members of a namespace or a type that a `.` reaches: every member of a
/// type, and the public ones of a namespace. What a namespace only imported is
/// not reachable through it.
fn members(s: &Session, def: DefId) -> Vec<DefId> {
    let d = s.defs.get(def);
    let namespace = d.kind == DefKind::Namespace;
    let mut out: Vec<DefId> = d
        .ns
        .members
        .values()
        .copied()
        .filter(|&m| !namespace || s.defs.get(m).vis == Visibility::Public)
        .collect();
    out.extend(impl_members(s, def));
    out
}

/// What a value of type `ty` has after a `.`: fields and methods, through any
/// number of pointers.
fn ty_members(s: &Session, ty: &Ty) -> Vec<DefId> {
    match ty {
        Ty::Ptr { inner, .. } => ty_members(s, inner),
        Ty::Nominal { def, .. } => {
            let d = s.defs.get(*def);
            let mut out: Vec<DefId> = d
                .ns
                .members
                .values()
                .copied()
                .filter(|&m| matches!(s.defs.get(m).kind, DefKind::Field | DefKind::Func))
                .collect();
            out.extend(impl_members(s, *def).filter(|&m| s.defs.get(m).kind == DefKind::Func));
            out
        }
        _ => Vec::new(),
    }
}

/// The members of every impl for `def`.
fn impl_members(s: &Session, def: DefId) -> impl Iterator<Item = DefId> + '_ {
    s.impls
        .impls
        .iter()
        .filter(move |i| i.self_head == Some(def))
        .flat_map(|i| i.members.values().copied())
}

/// Whether a local `d` is visible at `offset`: after the statement declaring
/// it, and inside the innermost block around it.
fn in_scope(ast: &Ast, d: &Def, offset: usize) -> bool {
    let (Some(node), Some(span)) = (d.node, d.span) else {
        return true;
    };
    let mut block: Option<Span> = None;
    for id in ast.ids() {
        let n = ast.node(id);
        match &n.kind {
            NodeKind::LocalDecl { pattern, .. } if *pattern == node && offset < n.span.end => return false,
            NodeKind::Block { .. } if n.span.start <= span.start && span.end <= n.span.end => {
                if block.is_none_or(|b| n.span.end - n.span.start < b.end - b.start) {
                    block = Some(n.span);
                }
            }
            _ => {}
        }
    }
    block.is_none_or(|b| contains(b, offset))
}

const KEYWORDS: &[&str] = &[
    "func", "extern", "struct", "enum", "trait", "impl", "namespace", "distinct", "let", "const",
    "mut", "return", "defer", "match", "import", "if", "else", "for", "in", "while", "loop", "break",
    "continue", "dyn", "true", "false",
];

/// Every name visible at `offset`: the function's parameters and the locals
/// declared before it, the file's names, and the prelude's.
fn scope(s: &Session, file: FileId, offset: usize) -> Vec<CompletionItem> {
    let mut defs: Vec<DefId> = Vec::new();
    if let Some(ast) = s.asts.get(&file) {
        let function = ast
            .ids()
            .filter(|&id| {
                let n = ast.node(id);
                matches!(n.kind, NodeKind::FuncExpr { .. }) && n.span.start <= offset && offset <= n.span.end
            })
            .min_by_key(|&id| {
                let span = ast.node(id).span;
                span.end - span.start
            })
            .map(|id| ast.node(id).span);
        if let Some(function) = function {
            let mut locals: Vec<&Def> = s
                .defs
                .iter()
                .filter(|d| matches!(d.kind, DefKind::Local | DefKind::Param | DefKind::TypeParam | DefKind::ConstParam))
                .filter(|d| d.file == Some(file))
                .filter(|d| d.span.is_some_and(|sp| function.start <= sp.start && sp.start < offset))
                .filter(|d| in_scope(ast, d, offset))
                .collect();
            // The nearest of two with one name is the one in scope.
            locals.sort_by_key(|d| std::cmp::Reverse(d.span.map_or(0, |sp| sp.start)));
            defs.extend(locals.iter().map(|d| d.id));
        }
    }
    if let Some(meta) = s.files.get(&file) {
        let ns = &s.defs.get(meta.ns).ns;
        defs.extend(ns.members.values().chain(ns.imported.values()).copied());
        for &glob in &ns.globs {
            defs.extend(members(s, target(s, glob)));
        }
    }
    for &glob in &s.prelude_globs {
        defs.extend(members(s, target(s, glob)));
    }
    let mut out = items(s, defs);
    out.extend(KEYWORDS.iter().map(|k| CompletionItem {
        label: k.to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        ..Default::default()
    }));
    out
}

/// A completion item per name, the first def of each name winning, and nothing
/// the compiler made up.
fn items(s: &Session, defs: impl IntoIterator<Item = DefId>) -> Vec<CompletionItem> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for def in defs {
        let d = s.defs.get(def);
        let name = d.name.as_str();
        let written = name.chars().next().is_some_and(|c| c == '_' || c.is_alphabetic());
        if !written || !seen.insert(name.to_string()) {
            continue;
        }
        let real = target(s, def);
        let kind = match s.defs.get(real).kind {
            DefKind::Namespace | DefKind::External => CompletionItemKind::MODULE,
            DefKind::Struct => CompletionItemKind::STRUCT,
            DefKind::Enum => CompletionItemKind::ENUM,
            DefKind::Trait => CompletionItemKind::INTERFACE,
            DefKind::TypeAlias | DefKind::Primitive => CompletionItemKind::CLASS,
            DefKind::TypeParam => CompletionItemKind::TYPE_PARAMETER,
            DefKind::Const | DefKind::ConstParam => CompletionItemKind::CONSTANT,
            DefKind::Func => CompletionItemKind::FUNCTION,
            DefKind::Field => CompletionItemKind::FIELD,
            DefKind::Variant => CompletionItemKind::ENUM_MEMBER,
            DefKind::Param | DefKind::Local => CompletionItemKind::VARIABLE,
            DefKind::Import => CompletionItemKind::MODULE,
        };
        let declared = declaration(s, real, None);
        let detail = declared.lines().next().map(str::to_string);
        out.push(CompletionItem {
            label: name.to_string(),
            kind: Some(kind),
            detail,
            documentation: docs(s, real).map(|value| {
                Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value })
            }),
            ..Default::default()
        });
    }
    out
}

fn contains(span: Span, offset: usize) -> bool {
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
fn word_in(src: &str, span: Span, word: &str) -> Option<Span> {
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
