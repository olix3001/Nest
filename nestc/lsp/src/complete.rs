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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, Documentation, MarkupContent,
    MarkupKind, Range, TextEdit,
};
use nestc::common::source::FileId;
use nestc::common::span::Span;
use nestc::parser::ast::{Ast, NodeId, NodeKind};
use nestc::sema::def::{Def, DefId, DefKind, Visibility};
use nestc::sema::session::Session;
use nestc::sema::ty::Ty;
use nestc::sema::{PathRes, Resolution};

use crate::analysis;
use crate::ide::{self, contains, target};

const KEYWORDS: &[&str] = &[
    "func", "extern", "struct", "enum", "trait", "impl", "namespace", "distinct", "let", "const",
    "mut", "return", "defer", "match", "import", "if", "else", "for", "in", "while", "loop", "break",
    "continue", "dyn", "true", "false",
];

/// What could be written at `offset` of `text`, the document as the editor has
/// it, asked of `s`, which analyzed it with `placeholder` written at `offset`.
pub fn complete(s: &Session, file: FileId, text: &str, offset: usize, placeholder: &str) -> Vec<CompletionItem> {
    let Some(ast) = s.asts.get(&file) else {
        return Vec::new();
    };
    let cx = Cx { s, file, ast, text, offset, placeholder, visible: visible(s, file) };
    cx.run()
}

struct Cx<'a> {
    s: &'a Session,
    file: FileId,
    ast: &'a Ast,
    text: &'a str,
    offset: usize,
    placeholder: &'a str,
    /// Every definition the file can name without another import.
    visible: HashSet<DefId>,
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
            (Via::Package(segments), true) => format!("{name} :: import <{}/{name}>", segments.join("/")),
            (Via::Package(segments), false) => format!("{{ {name} }} :: import <{}>", segments.join("/")),
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
                    if let Some(Resolution::Def(d)) = ast.meta::<Resolution>(base) {
                        let d = target(self.s, d);
                        if self.s.defs.get(d).kind.is_namespace_like() {
                            return self.plain(members(self.s, d));
                        }
                    }
                    return match ast.meta::<Ty>(base) {
                        Some(ty) => self.methods(&ty),
                        None => Vec::new(),
                    };
                }
                NodeKind::Path { segments } if segments.last().is_some_and(|l| ends(l.as_str())) => {
                    if segments.len() == 1 {
                        return self.scope();
                    }
                    let before = ast.meta::<PathRes>(id).and_then(|p| p.0.get(segments.len() - 2).cloned());
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
                NodeKind::IfMatch { pattern, value, .. } if *pattern == pat => return ast.meta::<Ty>(*value),
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
        let variants = d.ns.members.values().copied().filter(|&m| self.s.defs.get(m).kind == DefKind::Variant);
        self.plain(variants)
    }

    /// What a value of type `ty` has after a `.`: its fields, and the methods of
    /// every impl for it, through any number of pointers and through a
    /// `distinct` type to what it stands over. A method of a trait the file has
    /// not imported imports it.
    fn methods(&self, ty: &Ty) -> Vec<CompletionItem> {
        let s = self.s;
        let mut found: Vec<(DefId, Option<DefId>)> = Vec::new();
        let mut ty = self.concrete(ty.clone());
        loop {
            if let Ty::Nominal { def, .. } = &ty {
                let fields = s.defs.get(*def).ns.members.values().copied();
                found.extend(fields.filter(|&m| s.defs.get(m).kind == DefKind::Field).map(|m| (m, None)));
            }
            for (imp, t) in s.impls.impls.iter().zip(&s.impl_targets) {
                if !self.same_head(&t.self_ty, &ty) {
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
        let mut out = Vec::new();
        for (m, trait_def) in found {
            if !seen.insert(s.defs.get(m).name.to_string()) {
                continue;
            }
            let import = trait_def.filter(|t| !self.visible.contains(t)).and_then(|t| {
                let (via, name) = importable.get(&t)?;
                Some((self.import_edit(&via.line(name, false)), via.describe() + "." + name))
            });
            let mut it = item(s, m);
            if let Some((edit, from)) = import {
                it.additional_text_edits = Some(vec![edit]);
                it.label_details = Some(CompletionItemLabelDetails { detail: None, description: Some(from) });
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
                Some(def) => Ty::Nominal { def, args: Vec::new() },
                None => ty,
            },
            other => other,
        }
    }

    /// Whether an impl for `imp` can apply to `ty`, judged by what kind of type
    /// each is; its generics could be anything.
    fn same_head(&self, imp: &Ty, ty: &Ty) -> bool {
        match (imp, ty) {
            (Ty::Nominal { def, .. }, _) if self.s.defs.get(*def).kind == DefKind::TypeParam => true,
            (Ty::Nominal { def: a, .. }, Ty::Nominal { def: b, .. }) => a == b,
            (Ty::Int { signed: a, .. }, Ty::Int { signed: b, .. }) => a == b,
            (Ty::Float(a), Ty::Float(b)) => a == b,
            (Ty::Slice { .. }, Ty::Slice { .. } | Ty::Array { .. }) | (Ty::Array { .. }, Ty::Array { .. }) => true,
            (Ty::Bool, Ty::Bool) | (Ty::Char, Ty::Char) | (Ty::Void, Ty::Void) => true,
            (Ty::Tuple(a), Ty::Tuple(b)) => a.len() == b.len(),
            (Ty::Ptr { inner: a, .. }, Ty::Ptr { inner: b, .. }) => self.same_head(a, b),
            (Ty::Dyn(a), Ty::Dyn(b)) => a == b,
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
            offers.sort_by(|a, b| (a.1.len(), &a.2, a.1.describe()).cmp(&(b.1.len(), &b.2, b.1.describe())));
            for (def, via, name) in offers {
                let namespace = s.defs.get(def).kind == DefKind::Namespace;
                let mut it = item(s, def);
                it.label = name.clone();
                it.additional_text_edits = Some(vec![self.import_edit(&via.line(&name, namespace))]);
                it.label_details = Some(CompletionItemLabelDetails { detail: None, description: Some(via.describe()) });
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
        let before = &self.text[..self.offset.min(self.text.len())];
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
                matches!(n.kind, NodeKind::FuncExpr { .. }) && n.span.start <= offset && offset <= n.span.end
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
            .filter(|d| matches!(d.kind, DefKind::Local | DefKind::Param | DefKind::TypeParam | DefKind::ConstParam))
            .filter(|d| d.file == Some(self.file))
            .filter(|d| d.span.is_some_and(|sp| function.start <= sp.start && sp.start < offset))
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
            .map(|d| item(self.s, d))
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
                NodeKind::ConstBind { rhs, .. } if matches!(ast.node(rhs).kind, NodeKind::Import { .. }) => {
                    Some(ast.node(id).span.end)
                }
                _ => None,
            })
            .max();
        // Offsets into the analyzed text, which has the placeholder in it.
        let analyzed = ide::source(self.s, self.file).unwrap_or_default();
        let at = match last {
            Some(end) => analyzed[end..].find('\n').map_or(analyzed.len(), |i| end + i + 1),
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
        let at = if at > self.offset { at - self.placeholder.len() } else { at };
        let position = analysis::position(self.text, at);
        let prefix = if at > 0 && !self.text[..at].ends_with('\n') { "\n" } else { "" };
        TextEdit::new(Range::new(position, position), format!("{prefix}{line}\n"))
    }
}

/// Whether a namespace's member `def` is reachable from another file: what it
/// names is public.
fn public(s: &Session, def: DefId) -> bool {
    s.defs.get(target(s, def)).vis == Visibility::Public
}

/// Whether `name` is one a program wrote, rather than one the compiler made.
fn written(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c == '_' || c.is_alphabetic())
}

fn starts_with(name: &str, typed: &str) -> bool {
    name.to_lowercase().starts_with(&typed.to_lowercase())
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
    let d = s.defs.get(def);
    let (Some(file), Some(node)) = (d.file, d.node) else {
        return true;
    };
    let Some(ast) = s.asts.get(&file) else {
        return true;
    };
    let rhs = match ast.node(node).kind {
        NodeKind::ConstBind { rhs, .. } => rhs,
        _ => node,
    };
    let NodeKind::FuncExpr { params, .. } = &ast.node(rhs).kind else {
        return true;
    };
    params.first().is_some_and(|&p| matches!(&ast.node(p).kind, NodeKind::Param { name, .. } if name.as_str() == "self"))
}

/// The members of a namespace or a type that a `.` reaches: every member of a
/// type, and the public ones of a namespace. What a namespace only imported is
/// not reachable through it; what it re-exports is public when what it names
/// is, the way `imports::lookup_public` judges.
fn members(s: &Session, def: DefId) -> Vec<DefId> {
    let d = s.defs.get(def);
    let namespace = d.kind == DefKind::Namespace;
    let mut out: Vec<DefId> = d
        .ns
        .members
        .values()
        .copied()
        .filter(|&m| !namespace || public(s, m))
        .collect();
    out.extend(s.impls.impls.iter().filter(|i| i.self_head == Some(def)).flat_map(|i| i.members.values().copied()));
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
    let mut from_source: Vec<(&FileId, &String)> = s.pkg_of.iter().filter(|(f, _)| !s.is_foreign_file(**f)).collect();
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
            let mut members: Vec<(&nestc::common::symbol::Symbol, &DefId)> = s.defs.get(ns).ns.members.iter().collect();
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
        let (Some(here_path), Some(there)) = (&here_path, s.sources.file(f)) else { continue };
        let Some(path) = relative(here_path, Path::new(&there.name)) else { continue };
        for (member, &id) in &s.defs.get(meta.ns).ns.members {
            let t = target(s, id);
            if !public(s, id)
                || !written(member.as_str())
                || s.defs.get(t).kind == DefKind::Namespace
            {
                continue;
            }
            out.entry(t).or_insert_with(|| (Via::File(path.clone()), member.to_string()));
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
    parts.extend(target[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
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

/// The completion item for `def`: its kind, the first line of its declaration,
/// and its documentation.
fn item(s: &Session, def: DefId) -> CompletionItem {
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
        DefKind::Field => CompletionItemKind::FIELD,
        DefKind::Variant => CompletionItemKind::ENUM_MEMBER,
        DefKind::Param | DefKind::Local => CompletionItemKind::VARIABLE,
    };
    let declared = ide::declaration(s, real, None);
    CompletionItem {
        label: s.defs.get(def).name.to_string(),
        kind: Some(kind),
        detail: declared.lines().next().map(str::to_string),
        documentation: ide::docs(s, real)
            .map(|value| Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value })),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_climbs_to_the_common_directory() {
        let from = Path::new("/w/app/src/main.nest");
        assert_eq!(relative(from, Path::new("/w/app/src/build.nest")).as_deref(), Some("build.nest"));
        assert_eq!(relative(from, Path::new("/w/app/src/cli/args.nest")).as_deref(), Some("cli/args.nest"));
        assert_eq!(relative(from, Path::new("/w/util/lib.nest")).as_deref(), Some("../../util/lib.nest"));
    }

    #[test]
    fn an_import_line_names_what_it_binds() {
        let std = Via::Package(vec!["std".to_string(), "collections".to_string()]);
        assert_eq!(std.line("HashMap", false), "{ HashMap } :: import <std/collections>");
        let io = Via::Package(vec!["std".to_string()]);
        assert_eq!(io.line("io", true), "io :: import <std/io>");
        assert_eq!(Via::File("build.nest".to_string()).line("Profile", false), "{ Profile } :: import \"build.nest\"");
    }
}
