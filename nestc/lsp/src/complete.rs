//! Completion: what could be written at a place in a file.
//!
//! The session asked is one analyzed with a placeholder name written at the
//! cursor, so that what is being typed is a node with a place in the tree:
//! `p.` is a member access whose base has a type, `.` in a `match` arm is a
//! variant pattern, and a bare word is a path.
//!
//! ### Offered, and imported on use
//!
//! What is not in scope is offered too, when an `import` can reach it: a
//! function or type of another package, or a method of a trait the file has not
//! imported. Choosing it adds that import. What an import can reach is found by
//! walking every importable package's public members from its root, the
//! shortest path winning; a file of the program being edited that no package
//! reaches is imported by its path from the file being edited.
//!
//! ### Without analyzing again
//!
//! Analyzing takes long enough to be felt between two keys, so a session
//! analyzed a few edits ago answers when it can. The edits since say where the
//! cursor was in the text it analyzed, unless the cursor is inside one of them:
//! the word being typed and a `.` before it are left out of that, and what the
//! `.` follows is the expression that ends there, which has to read the same.
//! Anything else — what is completed edited, a `.` whose meaning is the type
//! expected there — is analyzed with the placeholder.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, Documentation,
    InsertTextFormat, MarkupContent, MarkupKind, Range, TextEdit,
};
use nestc::common::source::FileId;
use nestc::common::span::Span;
use nestc::parser::ast::{Ast, CompositeBody, NodeId, NodeKind};
use nestc::sema::decl::Decls;
use nestc::sema::def::{Def, DefId, DefKind, Visibility};
use nestc::sema::session::Session;
use nestc::sema::ty::Ty;
use nestc::sema::{PathRes, Resolution, builtins};

use crate::analysis;
use crate::ide::{self, contains, target};

const KEYWORDS: &[&str] = &[
    "func",
    "extern",
    "struct",
    "enum",
    "trait",
    "impl",
    "namespace",
    "distinct",
    "let",
    "const",
    "mut",
    "return",
    "defer",
    "match",
    "import",
    "if",
    "else",
    "for",
    "in",
    "while",
    "loop",
    "break",
    "continue",
    "dyn",
    "true",
    "false",
];

/// What could be written at `offset` of `text`, the document as the editor has
/// it, asked of `s`, which analyzed it with `placeholder` written at `offset`.
pub fn complete(
    s: &Session,
    file: FileId,
    text: &str,
    offset: usize,
    placeholder: &str,
    snippets: bool,
) -> Vec<CompletionItem> {
    let Some(ast) = s.asts.get(&file) else {
        return Vec::new();
    };
    // What follows the placeholder is where it would be without it.
    let edits = Edits(vec![Edit {
        at: offset,
        removed: placeholder.len(),
        inserted: 0,
    }]);
    let cx = Cx {
        s,
        file,
        ast,
        text,
        cursor: offset,
        offset,
        edits: &edits,
        placeholder,
        visible: visible(s, file),
        snippets,
    };
    cx.run()
}

/// What could be written at `cursor` of `text`, the document as the editor has
/// it, answered from `s`, which analyzed the text `edits` made it into `text`.
/// `None` when the edits touch what is completed.
pub fn from_analysis(
    s: &Session,
    file: FileId,
    text: &str,
    cursor: usize,
    edits: &Edits,
    snippets: bool,
) -> Option<Vec<CompletionItem>> {
    let ast = s.asts.get(&file)?;
    let analyzed = ide::source(s, file)?;
    let cursor = text.floor_char_boundary(cursor.min(text.len()));
    let before = &text[..cursor];
    let start = before
        .trim_end_matches(|c: char| c == '_' || c.is_alphanumeric())
        .len();
    // After a `.`, what it follows ends where the text before it does, which may
    // be on the line above.
    let base = before[..start]
        .strip_suffix('.')
        .map(|rest| rest.trim_end().len());
    let from = base.unwrap_or(start);
    let offset = edits.back(from)?;

    let cx = Cx {
        s,
        file,
        ast,
        text,
        cursor,
        offset,
        edits,
        placeholder: "",
        visible: visible(s, file),
        snippets,
    };
    let Some(end) = base else {
        return Some(cx.literal_fields().unwrap_or_else(|| cx.scope()));
    };
    // The expression reads the same, and so does the byte before it, so that
    // `p` is not taken for the end of `sop`.
    let same = |start: usize| {
        let len = offset - start + usize::from(start > 0);
        let old = analyzed.as_bytes().get(offset - len..offset);
        old.is_some()
            && old
                == end
                    .checked_sub(len)
                    .and_then(|at| text.as_bytes().get(at..end))
    };
    let mut nodes: Vec<NodeId> = ast
        .ids()
        .filter(|&id| {
            let n = ast.node(id);
            n.file == file && n.span.start < offset && n.span.end == offset && same(n.span.start)
        })
        .collect();
    nodes.sort_by_key(|&id| {
        let span = ast.node(id).span;
        span.end - span.start
    });
    nodes.into_iter().find_map(|id| cx.after_dot(id))
}

/// How the editor's text was made from an analyzed one, oldest edit first.
#[derive(Debug, Clone, Default)]
pub struct Edits(pub Vec<Edit>);

/// A range of a text replaced: `removed` bytes at `at` became `inserted` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit {
    pub at: usize,
    pub removed: usize,
    pub inserted: usize,
}

impl Edit {
    /// The one range `new` replaced in `old`.
    pub fn between(old: &str, new: &str) -> Edit {
        let (a, b) = (old.as_bytes(), new.as_bytes());
        let mut at = a.iter().zip(b).take_while(|(x, y)| x == y).count();
        while !old.is_char_boundary(at) || !new.is_char_boundary(at) {
            at -= 1;
        }
        let most = a.len().min(b.len()) - at;
        let mut same = a
            .iter()
            .rev()
            .zip(b.iter().rev())
            .take(most)
            .take_while(|(x, y)| x == y)
            .count();
        while !old.is_char_boundary(a.len() - same) || !new.is_char_boundary(b.len() - same) {
            same -= 1;
        }
        Edit {
            at,
            removed: a.len() - same - at,
            inserted: b.len() - same - at,
        }
    }
}

impl Edits {
    /// Where `offset` in the edited text was before the edits, or `None` when an
    /// edit wrote it.
    pub fn back(&self, offset: usize) -> Option<usize> {
        let mut offset = offset;
        for e in self.0.iter().rev() {
            if offset <= e.at {
                continue;
            }
            if offset < e.at + e.inserted {
                return None;
            }
            offset = offset - e.inserted + e.removed;
        }
        Some(offset)
    }

    /// Where `offset` before the edits is after them; inside a range an edit
    /// replaced, its end.
    pub fn forward(&self, offset: usize) -> usize {
        let mut offset = offset;
        for e in &self.0 {
            if offset <= e.at {
                continue;
            }
            offset = if offset < e.at + e.removed {
                e.at + e.inserted
            } else {
                offset + e.inserted - e.removed
            };
        }
        offset
    }
}

struct Cx<'a> {
    s: &'a Session,
    file: FileId,
    ast: &'a Ast,
    /// The document as the editor has it, and the cursor in it.
    text: &'a str,
    cursor: usize,
    /// The cursor in the text the session analyzed.
    offset: usize,
    /// How the analyzed text became `text`.
    edits: &'a Edits,
    placeholder: &'a str,
    /// Every definition the file can name without another import.
    visible: HashSet<DefId>,
    /// Whether the editor understands a snippet, which is what lets a chosen
    /// function be written with its parentheses and the cursor inside them.
    snippets: bool,
}

/// How an import reaches a definition.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Via {
    /// Through a package's public members: the segments to the namespace it
    /// is a member of, package first.
    Package(Vec<String>),
    /// A member of a file, by its path from the file being edited.
    File(String),
}

impl Via {
    fn len(&self) -> usize {
        match self {
            Via::Package(segments) => segments.len(),
            Via::File(_) => usize::MAX,
        }
    }

    /// The `import` that binds `name`, a member of what this reaches, or the
    /// namespace `name` itself.
    fn line(&self, name: &str, namespace: bool) -> String {
        match (self, namespace) {
            (Via::Package(segments), true) => {
                format!("{name} :: import <{}/{name}>", segments.join("/"))
            }
            (Via::Package(segments), false) => {
                format!("{{ {name} }} :: import <{}>", segments.join("/"))
            }
            (Via::File(path), _) => format!("{{ {name} }} :: import \"{path}\""),
        }
    }

    fn describe(&self) -> String {
        match self {
            Via::Package(segments) => segments.join("."),
            Via::File(path) => path.clone(),
        }
    }
}

impl Cx<'_> {
    fn run(&self) -> Vec<CompletionItem> {
        let ast = self.ast;
        let ends = |name: &str| name.ends_with(self.placeholder);
        // A variant's span is only its `.`, so it is looked for by name.
        for id in ast.ids() {
            let n = ast.node(id);
            if n.file != self.file {
                continue;
            }
            match &n.kind {
                NodeKind::VariantLit { name, .. } if ends(name.as_str()) => {
                    return self.variants(ast.meta::<Ty>(id));
                }
                NodeKind::VariantPat { name, .. } if ends(name.as_str()) => {
                    return self.variants(self.scrutinee(id));
                }
                _ => {}
            }
        }

        // Inside a `Point { ... }`, where another field's name goes.
        if let Some(items) = self.literal_fields() {
            return items;
        }

        let end = self.offset + self.placeholder.len();
        let mut nodes: Vec<NodeId> = ast
            .ids()
            .filter(|&id| {
                let n = ast.node(id);
                n.file == self.file && n.span.start <= self.offset && end <= n.span.end
            })
            .collect();
        nodes.sort_by_key(|&id| {
            let span = ast.node(id).span;
            span.end - span.start
        });
        for id in nodes {
            let kind = ast.node(id).kind.clone();
            match kind {
                NodeKind::FieldAccess { base, name } if ends(name.as_str()) => {
                    return self.after_dot(base).unwrap_or_default();
                }
                NodeKind::Path { segments }
                    if segments.last().is_some_and(|l| ends(l.as_str())) =>
                {
                    if segments.len() == 1 {
                        return self.scope();
                    }
                    let before = ast
                        .meta::<PathRes>(id)
                        .and_then(|p| p.0.get(segments.len() - 2).cloned());
                    return match before {
                        Some(Resolution::Def(d)) => self.plain(members(self.s, target(self.s, d))),
                        _ => Vec::new(),
                    };
                }
                _ => {}
            }
        }
        self.scope()
    }

    /// The fields a composite literal has not written yet, when the cursor is
    /// where another field's *name* goes.
    ///
    /// Where that is comes from the text — after the `{` or after a `,`, past
    /// whatever word is typed so far — because it is the same question in both
    /// entries to this file and one of them has no placeholder to look at. What
    /// the literal *is* comes from the tree: a `{` is also a block, and only a
    /// composite literal has fields.
    fn literal_fields(&self) -> Option<Vec<CompletionItem>> {
        let before = self.text.get(..self.cursor)?;
        let word = before.trim_end_matches(|c: char| c == '_' || c.is_alphanumeric());
        if !matches!(word.trim_end().chars().next_back(), Some('{' | ',')) {
            return None;
        }

        let ast = self.ast;
        // The innermost literal the cursor is in, in the analyzed text's own
        // offsets.
        let mut lits: Vec<NodeId> = ast
            .ids()
            .filter(|&id| {
                let n = ast.node(id);
                n.file == self.file
                    && matches!(n.kind, NodeKind::CompositeLit { .. })
                    && n.span.start <= self.offset
                    && self.offset <= n.span.end
            })
            .collect();
        lits.sort_by_key(|&id| {
            let span = ast.node(id).span;
            span.end - span.start
        });
        let lit = *lits.first()?;
        let NodeKind::CompositeLit { ty, body } = ast.node(lit).kind.clone() else {
            return None;
        };

        // The literal's own type is asked for first and its written type
        // second: a literal with a half-typed field in it may not have inferred
        // at all, and `Point { ... }` still says `Point`.
        let def = match ast.meta::<Ty>(lit) {
            Some(Ty::Nominal { def, .. }) => def,
            _ => match ty.and_then(|ty| ast.meta::<Resolution>(ty)) {
                Some(Resolution::Def(d)) => target(self.s, d),
                _ => return None,
            },
        };
        // A `[_]u8 { 1, 2 }` is a composite literal too, and its entries are
        // values rather than names.
        let fields: Vec<DefId> = members(self.s, def)
            .into_iter()
            .filter(|&m| self.s.defs.get(m).kind == DefKind::Field)
            .collect();
        if fields.is_empty() {
            return None;
        }
        // What the literal already names is not offered again.
        let written: HashSet<String> = entries(&body)
            .iter()
            .filter_map(|&e| match &ast.node(e).kind {
                NodeKind::FieldInit { name, .. } => Some(name.to_string()),
                _ => None,
            })
            .collect();
        Some(
            fields
                .into_iter()
                .filter(|&f| !written.contains(self.s.defs.get(f).name.as_str()))
                .map(|f| {
                    let mut it = item(self.s, f, self.snippets);
                    // A field in a literal is written `name: value`, so the `:`
                    // comes with the name and the cursor lands after it.
                    it.insert_text = Some(format!("{}: ", self.s.defs.get(f).name));
                    it
                })
                .collect(),
        )
    }

    /// What a `.` after `base` reaches: a namespace's or a type's members when it
    /// names one, and otherwise what its value's type has. `None` when it is
    /// neither, or its type is not known.
    fn after_dot(&self, base: NodeId) -> Option<Vec<CompletionItem>> {
        let ast = self.ast;
        if let Some(Resolution::Def(d)) = ast.meta::<Resolution>(base) {
            let d = target(self.s, d);
            if self.s.defs.get(d).kind.is_namespace_like() {
                return Some(self.plain(members(self.s, d)));
            }
        }
        match ast.meta::<Ty>(base)? {
            Ty::Error | Ty::Var(_) => None,
            ty => Some(self.methods(&ty)),
        }
    }

    /// The type a variant pattern at `pat` is matched against.
    fn scrutinee(&self, pat: NodeId) -> Option<Ty> {
        let ast = self.ast;
        for id in ast.ids() {
            match &ast.node(id).kind {
                NodeKind::MatchExpr { scrutinee, arms } => {
                    let on = arms.iter().any(|&arm| match ast.node(arm).kind {
                        NodeKind::MatchArm { pattern, .. } => pattern == pat,
                        _ => false,
                    });
                    if on {
                        return ast.meta::<Ty>(*scrutinee);
                    }
                }
                NodeKind::IfMatch { pattern, value, .. } if *pattern == pat => {
                    return ast.meta::<Ty>(*value);
                }
                _ => {}
            }
        }
        None
    }

    /// The variants of the enum `ty` is, through pointers.
    fn variants(&self, ty: Option<Ty>) -> Vec<CompletionItem> {
        let mut ty = ty;
        while let Some(Ty::Ptr { inner, .. }) = ty {
            ty = Some(*inner);
        }
        let Some(Ty::Nominal { def, .. }) = ty else {
            return Vec::new();
        };
        let d = self.s.defs.get(def);
        let variants =
            d.ns.members
                .values()
                .copied()
                .filter(|&m| self.s.defs.get(m).kind == DefKind::Variant);
        self.plain(variants)
    }

    /// What a value of type `ty` has after a `.`: its fields, the methods its
    /// bounds declare when it is a generic parameter, and the methods of every
    /// impl for it, through any number of pointers and through a
    /// `distinct` type to what it stands over. A method of a trait the file has
    /// not imported imports it.
    fn methods(&self, ty: &Ty) -> Vec<CompletionItem> {
        let s = self.s;
        let mut found: Vec<(DefId, Option<DefId>)> = Vec::new();
        let mut items: Vec<CompletionItem> = Vec::new();
        let mut ty = self.concrete(ty.clone());
        loop {
            // A tuple's members are positions, not definitions: `t.0` is a
            // `TupleIndex` and there is no `Def` anywhere to offer, so the items
            // are built here from the type itself.
            if let Ty::Tuple(elems) = &ty {
                items.extend(elems.iter().enumerate().map(|(i, e)| CompletionItem {
                    label: i.to_string(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(format!("{i}: {}", e.display(&s.defs))),
                    // Digits sort after letters by label, and a tuple's own
                    // members are what a `.` on one is most likely reaching for.
                    sort_text: Some(format!("0{i:03}")),
                    ..Default::default()
                }));
            }
            if let Ty::Nominal { def, .. } = &ty {
                let fields = s.defs.get(*def).ns.members.values().copied();
                found.extend(
                    fields
                        .filter(|&m| s.defs.get(m).kind == DefKind::Field)
                        .map(|m| (m, None)),
                );
                // A generic parameter has what its bounds declare.
                if s.defs.get(*def).kind == DefKind::TypeParam {
                    for t in bounds(s, *def) {
                        let declared = s.defs.get(t).ns.members.values().copied();
                        found.extend(
                            declared
                                .filter(|&m| {
                                    s.defs.get(m).kind == DefKind::Func && takes_self(s, m)
                                })
                                .map(|m| (m, Some(t))),
                        );
                    }
                }
            }
            for (i, imp) in s.impls.impls.iter().enumerate() {
                if !self.applies(i, &ty, 0) {
                    continue;
                }
                for &m in imp.members.values() {
                    if s.defs.get(m).kind == DefKind::Func && takes_self(s, m) {
                        found.push((m, imp.trait_def));
                    }
                }
            }
            ty = match ty {
                Ty::Ptr { inner, .. } => *inner,
                Ty::Nominal { def, .. } => match representation(s, def) {
                    Some(inner) => inner,
                    None => break,
                },
                _ => break,
            };
        }
        // An inherent method before a trait's of the same name.
        found.sort_by_key(|(_, t)| t.is_some());

        let importable = importable(s, self.file);
        let mut seen = HashSet::new();
        let mut out = items;
        for (m, trait_def) in found {
            if !seen.insert(s.defs.get(m).name.to_string()) {
                continue;
            }
            let import = trait_def
                .filter(|t| !self.visible.contains(t))
                .and_then(|t| {
                    let (via, name) = importable.get(&t)?;
                    Some((
                        self.import_edit(&via.line(name, false)),
                        via.describe() + "." + name,
                    ))
                });
            let mut it = item(s, m, self.snippets);
            if let Some((edit, from)) = import {
                it.additional_text_edits = Some(vec![edit]);
                it.label_details = Some(CompletionItemLabelDetails {
                    detail: None,
                    description: Some(from),
                });
            }
            out.push(it);
        }
        out
    }

    /// `ty` with a literal's type made the type it would become.
    fn concrete(&self, ty: Ty) -> Ty {
        match ty {
            Ty::ComptimeInt => Ty::int(32, true),
            Ty::ComptimeStr => match self.s.lang_items.get("str") {
                Some(def) => Ty::Nominal {
                    def,
                    args: Vec::new(),
                },
                None => ty,
            },
            other => other,
        }
    }

    /// Whether impl `i` applies to `ty`: its self type has `ty`'s shape, and
    /// what matching binds its generics to meets their bounds.
    fn applies(&self, i: usize, ty: &Ty, depth: usize) -> bool {
        let s = self.s;
        let (Some(imp), Some(target)) = (s.impls.impls.get(i), s.impl_targets.get(i)) else {
            return false;
        };
        let mut bound: HashMap<DefId, Ty> = HashMap::new();
        if !self.bind(&target.self_ty, ty, &mut bound) {
            return false;
        }
        imp.generics.iter().all(|g| match bound.get(g) {
            Some(arg) => bounds(s, *g)
                .into_iter()
                .all(|t| self.implements(arg, t, depth + 1)),
            None => true,
        })
    }

    /// Match an impl's self type `imp` against `ty`, binding the impl's type
    /// parameters. What is not known yet matches anything.
    fn bind(&self, imp: &Ty, ty: &Ty, map: &mut HashMap<DefId, Ty>) -> bool {
        let all = |a: &[Ty], b: &[Ty], map: &mut HashMap<DefId, Ty>| {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| self.bind(x, y, map))
        };
        match (imp, ty) {
            (_, Ty::Var(_) | Ty::Error) => true,
            (Ty::Nominal { def, .. }, _) if self.s.defs.get(*def).kind == DefKind::TypeParam => {
                map.entry(*def).or_insert_with(|| ty.clone());
                true
            }
            (Ty::Nominal { def: a, args: x }, Ty::Nominal { def: b, args: y }) => {
                a == b && all(x, y, map)
            }
            (Ty::Int { signed: a, .. }, Ty::Int { signed: b, .. }) => a == b,
            (Ty::Float(a), Ty::Float(b)) => a == b,
            (Ty::Bool, Ty::Bool) | (Ty::Char, Ty::Char) | (Ty::Void, Ty::Void) => true,
            (
                Ty::Slice { inner: a, .. },
                Ty::Slice { inner: b, .. } | Ty::Array { inner: b, .. },
            )
            | (Ty::Array { inner: a, .. }, Ty::Array { inner: b, .. })
            | (Ty::Ptr { inner: a, .. }, Ty::Ptr { inner: b, .. }) => self.bind(a, b, map),
            (Ty::Tuple(a), Ty::Tuple(b)) => all(a, b, map),
            (Ty::Func { params: a, ret: r }, Ty::Func { params: b, ret: q }) => {
                all(a, b, map) && self.bind(r, q, map)
            }
            (Ty::Dyn(a), Ty::Dyn(b)) => a == b,
            _ => false,
        }
    }

    /// Whether `ty` implements `trait_def`: an operator the compiler provides
    /// for a primitive, an impl that applies, or an impl for what a `distinct`
    /// type stands over. Past a few levels of bounds on bounds, and for a type
    /// not known yet, the answer is yes, so nothing is hidden for a guess.
    fn implements(&self, ty: &Ty, trait_def: DefId, depth: usize) -> bool {
        const DEPTH: usize = 4;
        let s = self.s;
        let ty = self.concrete(ty.clone());
        if depth > DEPTH || matches!(ty, Ty::Var(_) | Ty::Error) {
            return true;
        }
        // A generic parameter of the code being written: its bounds say what it
        // implements, and they are checked where it is instantiated.
        if let Ty::Nominal { def, .. } = &ty
            && s.defs.get(*def).kind == DefKind::TypeParam
        {
            return true;
        }
        let builtin = s
            .defs
            .get(trait_def)
            .lang
            .as_ref()
            .and_then(|l| builtins::row_for_lang(l.as_str()));
        if builtin.is_some_and(|row| row.applies.matches(&ty)) {
            return true;
        }
        let direct = (0..s.impls.impls.len())
            .any(|i| s.impls.impls[i].trait_def == Some(trait_def) && self.applies(i, &ty, depth));
        direct
            || match &ty {
                Ty::Nominal { def, .. } => representation(s, *def)
                    .is_some_and(|r| self.implements(&r, trait_def, depth + 1)),
                _ => false,
            }
    }

    /// Names in scope, then what an import would bring in that starts with what
    /// is typed, then keywords.
    fn scope(&self) -> Vec<CompletionItem> {
        let s = self.s;
        let mut defs: Vec<DefId> = self.locals();
        if let Some(meta) = s.files.get(&self.file) {
            let ns = &s.defs.get(meta.ns).ns;
            defs.extend(ns.members.values().chain(ns.imported.values()).copied());
            for &glob in &ns.globs {
                defs.extend(members(s, target(s, glob)));
            }
        }
        for &glob in &s.prelude_globs {
            defs.extend(members(s, target(s, glob)));
        }
        let mut out = self.plain(defs);

        let typed = self.typed();
        if !typed.is_empty() {
            let mut offers: Vec<(DefId, Via, String)> = importable(s, self.file)
                .into_iter()
                .filter(|(def, (_, name))| !self.visible.contains(def) && starts_with(name, &typed))
                .map(|(def, (via, name))| (def, via, name))
                .collect();
            // Nearest the surface first, and alphabetical for the same depth.
            offers.sort_by(|a, b| {
                (a.1.len(), &a.2, a.1.describe()).cmp(&(b.1.len(), &b.2, b.1.describe()))
            });
            for (def, via, name) in offers {
                let namespace = s.defs.get(def).kind == DefKind::Namespace;
                let mut it = item(s, def, self.snippets);
                it.label = name.clone();
                it.additional_text_edits =
                    Some(vec![self.import_edit(&via.line(&name, namespace))]);
                it.label_details = Some(CompletionItemLabelDetails {
                    detail: None,
                    description: Some(via.describe()),
                });
                out.push(it);
            }
        }

        out.extend(KEYWORDS.iter().map(|k| CompletionItem {
            label: k.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        }));
        out
    }

    /// The identifier typed before the cursor.
    fn typed(&self) -> String {
        let before = &self.text[..self.cursor.min(self.text.len())];
        let start = before
            .char_indices()
            .rev()
            .take_while(|(_, c)| *c == '_' || c.is_alphanumeric())
            .last()
            .map_or(before.len(), |(i, _)| i);
        before[start..].to_string()
    }

    /// The function's parameters and the locals declared before the cursor,
    /// nearest first.
    fn locals(&self) -> Vec<DefId> {
        let (s, ast, offset) = (self.s, self.ast, self.offset);
        let function = ast
            .ids()
            .filter(|&id| {
                let n = ast.node(id);
                matches!(n.kind, NodeKind::FuncExpr { .. })
                    && n.span.start <= offset
                    && offset <= n.span.end
            })
            .min_by_key(|&id| {
                let span = ast.node(id).span;
                span.end - span.start
            })
            .map(|id| ast.node(id).span);
        let Some(function) = function else {
            return Vec::new();
        };
        let mut locals: Vec<&Def> = s
            .defs
            .iter()
            .filter(|d| {
                matches!(
                    d.kind,
                    DefKind::Local | DefKind::Param | DefKind::TypeParam | DefKind::ConstParam
                )
            })
            .filter(|d| d.file == Some(self.file))
            .filter(|d| {
                d.span
                    .is_some_and(|sp| function.start <= sp.start && sp.start < offset)
            })
            .filter(|d| in_scope(ast, d, offset))
            .collect();
        locals.sort_by_key(|d| std::cmp::Reverse(d.span.map_or(0, |sp| sp.start)));
        locals.iter().map(|d| d.id).collect()
    }

    /// An item per name, the first of each name winning.
    fn plain(&self, defs: impl IntoIterator<Item = DefId>) -> Vec<CompletionItem> {
        let mut seen: HashSet<String> = HashSet::new();
        defs.into_iter()
            .filter(|&d| {
                let name = self.s.defs.get(d).name.as_str();
                written(name) && seen.insert(name.to_string())
            })
            .map(|d| item(self.s, d, self.snippets))
            .collect()
    }

    /// The edit that writes `line` with the file's imports: after the last of
    /// them, or after the comments the file starts with.
    fn import_edit(&self, line: &str) -> TextEdit {
        let ast = self.ast;
        let last = ast
            .ids()
            .filter(|&id| ast.node(id).file == self.file)
            .filter_map(|id| match ast.node(id).kind {
                NodeKind::ConstBind { rhs, .. }
                    if matches!(ast.node(rhs).kind, NodeKind::Import { .. }) =>
                {
                    Some(ast.node(id).span.end)
                }
                _ => None,
            })
            .max();
        // Offsets into the analyzed text, which is not quite the editor's.
        let analyzed = ide::source(self.s, self.file).unwrap_or_default();
        let at = match last {
            Some(end) => analyzed[end..]
                .find('\n')
                .map_or(analyzed.len(), |i| end + i + 1),
            None => {
                let mut at = 0;
                for l in analyzed.split_inclusive('\n') {
                    if !l.trim_start().starts_with("//") {
                        break;
                    }
                    at += l.len();
                }
                at
            }
        };
        // Into the editor's text, which the analyzed one may reach past: the
        // edits since can have made it shorter.
        let at = self.edits.forward(at).min(self.text.len());
        let at = (0..=at)
            .rev()
            .find(|&i| self.text.is_char_boundary(i))
            .unwrap_or(0);
        let position = analysis::position(self.text, at);
        let prefix = if at > 0 && !self.text[..at].ends_with('\n') {
            "\n"
        } else {
            ""
        };
        TextEdit::new(Range::new(position, position), format!("{prefix}{line}\n"))
    }
}

/// Whether a namespace's member `def` is reachable from another file: what it
/// names is public.
/// The nodes a composite literal's body holds.
fn entries(body: &CompositeBody) -> Vec<NodeId> {
    match body {
        CompositeBody::Named { fields, spread } => fields.iter().copied().chain(*spread).collect(),
        CompositeBody::Positional(entries) => entries.clone(),
        CompositeBody::Repeat { value, count } => vec![*value, *count],
    }
}

fn public(s: &Session, def: DefId) -> bool {
    s.defs.get(target(s, def)).vis == Visibility::Public
}

/// Whether `name` is one a program wrote, rather than one the compiler made.
fn written(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c == '_' || c.is_alphabetic())
}

fn starts_with(name: &str, typed: &str) -> bool {
    name.to_lowercase().starts_with(&typed.to_lowercase())
}

/// The traits the generic parameter `param` is bounded by.
fn bounds(s: &Session, param: DefId) -> Vec<DefId> {
    let d = s.defs.get(param);
    // Name resolution wrote these down on the parameter itself, which is what
    // makes them readable for a parameter that came out of a library — there is
    // no constraint node here to walk.
    if let Some(recorded) = d.param_bounds.clone() {
        return recorded;
    }
    let (Some(file), Some(node)) = (d.file, d.node) else {
        return Vec::new();
    };
    let Some(ast) = s.asts.get(&file) else {
        return Vec::new();
    };
    let NodeKind::GenericTypeParam {
        constraint: Some(constraint),
        ..
    } = ast.node(node).kind
    else {
        return Vec::new();
    };
    let nodes = match &ast.node(constraint).kind {
        NodeKind::Bounds { bounds } => bounds.clone(),
        _ => vec![constraint],
    };
    nodes
        .into_iter()
        .filter_map(|n| trait_of(ast, n))
        .map(|t| target(s, t))
        .collect()
}

/// The trait a bound's type node names: `Eq`, `Add.<f64>`, `core.cmp.Eq`.
fn trait_of(ast: &Ast, node: NodeId) -> Option<DefId> {
    if let Some(Resolution::Def(d)) = ast.meta::<Resolution>(node) {
        return Some(d);
    }
    match ast.node(node).kind {
        NodeKind::TypePath { path, .. } => trait_of(ast, path),
        NodeKind::GenericApply { base, .. } => trait_of(ast, base),
        _ => None,
    }
}

/// What the `distinct` type `def` stands over.
fn representation(s: &Session, def: DefId) -> Option<Ty> {
    let d = s.defs.get(def);
    let ast = s.asts.get(&d.file?)?;
    let node = d.node?;
    let rhs = match ast.node(node).kind {
        NodeKind::ConstBind { rhs, .. } => rhs,
        _ => node,
    };
    match ast.node(rhs).kind {
        NodeKind::DistinctType { inner, .. } => ast.meta::<Ty>(inner),
        _ => None,
    }
}

/// Whether the function `def` is a method: its first parameter is `self`.
fn takes_self(s: &Session, def: DefId) -> bool {
    // The declaration table, which a function out of a library has and a tree
    // it does not: every `core` and `std` function used to answer "yes" here,
    // so every one of them was offered after a `.`.
    nestc::sema::decl::Decls::new(&s.defs, &s.asts, &s.decls).takes_receiver(def)
}

/// The members of a namespace or a type that a `.` reaches: every member of a
/// type, and the public ones of a namespace. What a namespace only imported is
/// not reachable through it; what it re-exports is public when what it names
/// is, the way `imports::lookup_public` judges.
fn members(s: &Session, def: DefId) -> Vec<DefId> {
    let d = s.defs.get(def);
    let namespace = d.kind == DefKind::Namespace;
    let mut out: Vec<DefId> =
        d.ns.members
            .values()
            .copied()
            .filter(|&m| !namespace || public(s, m))
            .collect();
    out.extend(
        s.impls
            .impls
            .iter()
            .filter(|i| i.self_head == Some(def))
            .flat_map(|i| i.members.values().copied()),
    );
    out
}

/// Every definition `file` names without another import: its own members and
/// imports, what it globs, and the prelude.
fn visible(s: &Session, file: FileId) -> HashSet<DefId> {
    let mut out = HashSet::new();
    let add = |d: DefId, out: &mut HashSet<DefId>| {
        out.insert(d);
        out.insert(target(s, d));
    };
    if let Some(meta) = s.files.get(&file) {
        let ns = &s.defs.get(meta.ns).ns;
        for &d in ns.members.values().chain(ns.imported.values()) {
            add(d, &mut out);
        }
        for &glob in &ns.globs {
            for d in members(s, target(s, glob)) {
                add(d, &mut out);
            }
        }
    }
    for &glob in &s.prelude_globs {
        for d in members(s, target(s, glob)) {
            add(d, &mut out);
        }
    }
    out
}

/// Every definition an import in `file` can reach, how, and by what name.
fn importable(s: &Session, file: FileId) -> HashMap<DefId, (Via, String)> {
    let mut roots: Vec<(String, DefId)> = Vec::new();
    for lib in s.libraries.iter().filter(|l| l.importable) {
        if let Some(meta) = s.files.get(&lib.root) {
            roots.push((lib.name.clone(), meta.ns));
        }
    }
    let mut from_source: Vec<(&FileId, &String)> = s
        .pkg_of
        .iter()
        .filter(|(f, _)| !s.is_foreign_file(**f))
        .collect();
    from_source.sort();
    for (f, name) in from_source {
        if let Some(meta) = s.files.get(f)
            && s.defs.get(meta.ns).canonical.len() == 1
            && !roots.iter().any(|(n, _)| n == name)
        {
            roots.push((name.clone(), meta.ns));
        }
    }
    let here = s.files.get(&file).map(|m| m.ns);

    let mut out: HashMap<DefId, (Via, String)> = HashMap::new();
    let mut reached: HashSet<DefId> = HashSet::new();
    for (name, root) in roots {
        let mut walked: Vec<(DefId, Via, String)> = Vec::new();
        let mut seen: HashSet<DefId> = HashSet::from([root]);
        let mut queue = VecDeque::from([(root, vec![name])]);
        let mut own = false;
        while let Some((ns, segments)) = queue.pop_front() {
            own |= Some(ns) == here;
            let mut members: Vec<(&nestc::common::symbol::Symbol, &DefId)> =
                s.defs.get(ns).ns.members.iter().collect();
            members.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            for (member, &id) in members {
                if !public(s, id) || !written(member.as_str()) {
                    continue;
                }
                let t = target(s, id);
                walked.push((t, Via::Package(segments.clone()), member.to_string()));
                if s.defs.get(t).kind == DefKind::Namespace && seen.insert(t) {
                    let mut deeper = segments.clone();
                    deeper.push(member.to_string());
                    queue.push_back((t, deeper));
                }
            }
        }
        // The package the file is in reaches its own files by path instead.
        if own {
            continue;
        }
        reached.extend(seen);
        for (t, via, member) in walked {
            let shorter = out.get(&t).is_none_or(|(v, _)| via.len() < v.len());
            if shorter {
                out.insert(t, (via, member));
            }
        }
    }

    let here_path = s.sources.file(file).map(|f| PathBuf::from(&f.name));
    let mut files: Vec<(&FileId, &nestc::sema::session::FileMeta)> = s.files.iter().collect();
    files.sort_by_key(|(f, _)| **f);
    for (&f, meta) in files {
        if f == file || s.is_foreign_file(f) || reached.contains(&meta.ns) {
            continue;
        }
        let (Some(here_path), Some(there)) = (&here_path, s.sources.file(f)) else {
            continue;
        };
        let Some(path) = relative(here_path, Path::new(&there.name)) else {
            continue;
        };
        for (member, &id) in &s.defs.get(meta.ns).ns.members {
            let t = target(s, id);
            if !public(s, id)
                || !written(member.as_str())
                || s.defs.get(t).kind == DefKind::Namespace
            {
                continue;
            }
            out.entry(t)
                .or_insert_with(|| (Via::File(path.clone()), member.to_string()));
        }
    }
    out
}

/// `to` written from the directory `from` is in.
fn relative(from: &Path, to: &Path) -> Option<String> {
    let base: Vec<Component> = from.parent()?.components().collect();
    let target: Vec<Component> = to.components().collect();
    let common = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return None;
    }
    let mut parts: Vec<String> = vec!["..".to_string(); base.len() - common];
    parts.extend(
        target[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    Some(parts.join("/"))
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
            NodeKind::LocalDecl { pattern, .. } if *pattern == node && offset < n.span.end => {
                return false;
            }
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

/// The completion item for `def`: its kind, the first line of its declaration,
/// and its documentation.
fn item(s: &Session, def: DefId, snippets: bool) -> CompletionItem {
    let real = target(s, def);
    let kind = match s.defs.get(real).kind {
        DefKind::Namespace | DefKind::External | DefKind::Import => CompletionItemKind::MODULE,
        DefKind::Struct => CompletionItemKind::STRUCT,
        DefKind::Enum => CompletionItemKind::ENUM,
        DefKind::Trait => CompletionItemKind::INTERFACE,
        DefKind::TypeAlias | DefKind::Primitive => CompletionItemKind::CLASS,
        DefKind::TypeParam => CompletionItemKind::TYPE_PARAMETER,
        DefKind::Const | DefKind::ConstParam => CompletionItemKind::CONSTANT,
        DefKind::Func => CompletionItemKind::FUNCTION,
        // An overload set is called like a function and offered like one.
        DefKind::Overload => CompletionItemKind::FUNCTION,
        DefKind::Field => CompletionItemKind::FIELD,
        DefKind::Variant => CompletionItemKind::ENUM_MEMBER,
        DefKind::Param | DefKind::Local => CompletionItemKind::VARIABLE,
    };
    let declared = ide::declaration(s, real, None);
    let name = s.defs.get(def).name.to_string();
    let mut it = CompletionItem {
        label: name.clone(),
        kind: Some(kind),
        detail: declared.lines().next().map(str::to_string),
        documentation: ide::docs(s, real).map(|value| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            })
        }),
        ..Default::default()
    };
    // A function is called, so choosing one writes the call: its parentheses,
    // with the cursor between them where the arguments go. An editor that
    // understands snippets puts the cursor there; one that does not gets the
    // parentheses only when there is nothing to type between them, since
    // landing *after* a `)` the caller still has to go back through would be
    // worse than not writing it.
    if matches!(s.defs.get(real).kind, DefKind::Func | DefKind::Overload) {
        let takes_args = match s.defs.get(real).kind {
            DefKind::Overload => true,
            _ => params_of(s, real) != Some(0),
        };
        if snippets {
            it.insert_text = Some(format!("{name}($0)"));
            it.insert_text_format = Some(InsertTextFormat::SNIPPET);
        } else if !takes_args {
            it.insert_text = Some(format!("{name}()"));
        }
    }
    it
}

/// How many **written** arguments a function takes — `self` excluded, since a
/// method call does not write it. `None` when nothing recorded its parameters.
fn params_of(s: &Session, def: DefId) -> Option<usize> {
    Decls::new(&s.defs, &s.asts, &s.decls)
        .param_names(def)
        .map(|p| p.len())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    const PROGRAM: &str = "\
Reader :: struct { at: i32 }

Decode :: trait {
  decode :: func (self: *mut Self, r: *mut Reader) -> void
}

Point :: struct { x: i32, y: i32 }

impl Point {
  sum :: func (self: *Self) -> i32 { return self.x + self.y }
}

Color :: enum { red, green }

geo :: namespace {
  @public origin :: func () -> Point { return Point { x: 0, y: 0 } }
}

from :: func <T: Decode> (slot: *mut T, r: *mut Reader) -> void {
  slot.decode(r)
}

add :: func (a: i32, b: i32) -> i32 { return a + b }

main :: func () -> i32 {
  let p: Point := geo.origin()
  let q: i32 := p.x
  return add(q, p
    .sum())
}
";

    /// `PROGRAM` analyzed, then edited into each of `after` in turn, each with a
    /// `‸` where the cursor is: what completion at the cursor in the last says
    /// from that one analysis, sorted.
    fn stale(after: &[&str]) -> Option<Vec<String>> {
        let path = std::env::temp_dir()
            .join("nest-lsp-stale")
            .join("main.nest");
        let buffers = Arc::new(HashMap::from([(path.clone(), PROGRAM.to_string())]));
        let o = analysis::analyze(&[path.display().to_string()], buffers).unwrap();
        assert!(o.diagnostics.is_empty(), "{:#?}", o.diagnostics);
        let file = ide::file_of(&o.session, &path).unwrap();

        let mut texts = vec![PROGRAM.to_string()];
        texts.extend(after.iter().map(|t| t.replace('‸', "")));
        let edits = Edits(
            texts
                .windows(2)
                .map(|w| Edit::between(&w[0], &w[1]))
                .collect(),
        );
        let cursor = after.last()?.find('‸')?;
        let items = from_analysis(&o.session, file, texts.last()?, cursor, &edits, false)?;
        let mut labels: Vec<String> = items.into_iter().map(|i| i.label).collect();
        labels.sort();
        Some(labels)
    }

    fn has(labels: &Option<Vec<String>>, want: &[&str]) {
        let labels = labels.as_ref().expect("answered from the analysis");
        for w in want {
            assert!(labels.iter().any(|l| l == w), "`{w}` in {labels:?}");
        }
    }

    /// The editor's scenario: `.decode` deleted a key at a time, which stops
    /// parsing on the way, then a `.` typed after `slot`.
    #[test]
    fn a_method_is_offered_after_its_name_was_deleted() {
        let mut steps: Vec<String> = Vec::new();
        for n in (0..".decode".len()).rev() {
            steps.push(PROGRAM.replace("slot.decode(r)", &format!("slot{}(r)", &".decode"[..n])));
        }
        steps.push(PROGRAM.replace("slot.decode(r)", "slot.‸(r)"));
        let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
        let labels = stale(&steps);
        has(&labels, &["decode"]);
        assert!(
            !labels.unwrap().contains(&"sum".to_string()),
            "a `T` is not a `Point`"
        );
    }

    /// Lines added and removed elsewhere move the cursor, not what it completes.
    #[test]
    fn edits_elsewhere_are_looked_through() {
        let above = PROGRAM.replace("main :: func", "// żółw, a comment\n\nmain :: func");
        let below = above.clone() + "\nlater :: func () -> void {}\n";
        let typed = below.replace("let q: i32 := p.x", "let q: i32 := p.‸");
        has(&stale(&[&above, &below, &typed]), &["x", "y", "sum"]);

        // What was typed after the `.` so far does not matter either.
        let word = below.replace("let q: i32 := p.x", "let q: i32 := p.s‸");
        has(&stale(&[&above, &below, &word]), &["sum"]);
    }

    /// A `.` at the start of a line continues the expression on the line above.
    #[test]
    fn a_dot_on_the_next_line_completes_the_line_above() {
        has(
            &stale(&[&PROGRAM.replace("    .sum())", "    .‸)")]),
            &["x", "sum"],
        );
    }

    #[test]
    fn a_namespace_offers_its_public_members() {
        has(
            &stale(&[&PROGRAM.replace("geo.origin()", "geo.‸()")]),
            &["origin"],
        );
    }

    /// A word typed where a name goes: what is in scope there.
    #[test]
    fn a_name_is_offered_from_the_analysis() {
        let labels = stale(&[&PROGRAM.replace("  return add(q, p", "  ad‸\n  return add(q, p")]);
        has(&labels, &["add", "p", "q", "Point", "main", "return"]);
        assert!(
            !labels.unwrap().contains(&"a".to_string()),
            "`a` is `add`'s parameter"
        );
    }

    /// When what is completed was itself edited, or a `.` has nothing before it,
    /// the analysis cannot say, and the caller analyzes again.
    #[test]
    fn what_the_analysis_cannot_say_is_left_to_analyzing() {
        // `p` became `pp`, which the analysis never saw.
        assert_eq!(
            stale(&[&PROGRAM.replace("let q: i32 := p.x", "let q: i32 := pp.‸")]),
            None
        );
        // A variant, whose enum is what the context expects.
        assert_eq!(
            stale(&[&PROGRAM.replace("let q: i32 := p.x", "let c: Color := .‸")]),
            None
        );
        // A name that only ends the way one the analysis had does.
        let longer = PROGRAM.replace("return add(q, p", "return add(q, sop");
        assert_eq!(
            stale(&[&longer, &longer.replace("add(q, sop", "add(q, sop.‸")]),
            None
        );
    }

    #[test]
    fn an_offset_maps_back_through_the_edits_around_it() {
        // `slot.decode(r)` → `slot(r)` → `slot.(r)`, and a line added above.
        let texts = [
            "x\nslot.decode(r)",
            "x\nslot(r)",
            "x\nslot.(r)",
            "x\ny\nslot.(r)",
        ];
        let edits = Edits(
            texts
                .windows(2)
                .map(|w| Edit::between(w[0], w[1]))
                .collect(),
        );
        assert_eq!(
            edits.0[0],
            Edit {
                at: 6,
                removed: 7,
                inserted: 0
            }
        );
        assert_eq!(
            edits.0[2],
            Edit {
                at: 2,
                removed: 0,
                inserted: 2
            }
        );
        // The end of `slot` is where it was; inside the added line is nowhere.
        assert_eq!(edits.back(8), Some(6));
        assert_eq!(edits.back(3), None);
        // `r` came from the analyzed text, after `decode`, and goes back there.
        assert_eq!(edits.back(10), Some(14));
        assert_eq!(edits.forward(14), 10);
        assert_eq!(edits.forward(0), 0);
        // Inside what was removed is where the removal ended.
        assert_eq!(edits.forward(9), 8);
    }

    #[test]
    fn a_relative_path_climbs_to_the_common_directory() {
        let from = Path::new("/w/app/src/main.nest");
        assert_eq!(
            relative(from, Path::new("/w/app/src/build.nest")).as_deref(),
            Some("build.nest")
        );
        assert_eq!(
            relative(from, Path::new("/w/app/src/cli/args.nest")).as_deref(),
            Some("cli/args.nest")
        );
        assert_eq!(
            relative(from, Path::new("/w/util/lib.nest")).as_deref(),
            Some("../../util/lib.nest")
        );
    }

    #[test]
    fn an_import_line_names_what_it_binds() {
        let std = Via::Package(vec!["std".to_string(), "collections".to_string()]);
        assert_eq!(
            std.line("HashMap", false),
            "{ HashMap } :: import <std/collections>"
        );
        let io = Via::Package(vec!["std".to_string()]);
        assert_eq!(io.line("io", true), "io :: import <std/io>");
        assert_eq!(
            Via::File("build.nest".to_string()).line("Profile", false),
            "{ Profile } :: import \"build.nest\""
        );
    }
}
