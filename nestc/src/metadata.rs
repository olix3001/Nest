//! A package described as JSON, for a documentation generator (`--emit
//! metadata`, `twig doc`).
//!
//! What is described is what a **user** of the package can name: the root's
//! public members, walked down through public namespaces, with each type's
//! fields, variants and methods and each trait's members beside it. Every
//! description carries the item's `@doc` — its `///` comment — and the other
//! attributes written on it, so a site generated from this has the prose and
//! the shape in one place.
//!
//! A name re-exported in several places is described **once**, where the walk
//! first meets it, and everywhere else as a `reexport` of that path: `std`'s
//! `collections.Vec` and `collections.vec.Vec` are one type, and a generator
//! decides whether to show it twice. The walk is in declaration order, so the
//! place a type is described is stable from one build to the next.
//!
//! The format is versioned by [`FORMAT`]; a field is added without a bump, and
//! one that changes meaning or goes away is a bump.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::common::source::FileId;
use crate::ir::const_eval::ConstValue;
use crate::parser::ast::NodeKind;
use crate::sema::decl::{Decl, Decls};
use crate::sema::def::{Def, DefId, DefKind, Visibility};
use crate::sema::session::Session;
use crate::sema::ty::Ty;

/// The version of the shape written. See the module documentation.
pub const FORMAT: u32 = 1;

/// The package `entry` is the root file of, described. `package` is its name
/// when it was compiled as one (`--package`), and `null` for a program.
pub fn describe(session: &Session, entry: FileId) -> Value {
    let Some(meta) = session.files.get(&entry) else {
        return json!({ "format": FORMAT, "package": null, "root": null });
    };
    let package = session.pkg_of.get(&entry).cloned();
    let mut w = Walker {
        s: session,
        decls: Decls::new(&session.defs, &session.asts, &session.decls),
        seen: HashMap::new(),
        methods: methods_by_type(session),
    };
    let name = package.clone().unwrap_or_else(|| "main".to_string());
    let root = w.namespace(meta.ns, &name, &name);
    json!({ "format": FORMAT, "package": package, "root": root })
}

struct Walker<'a> {
    s: &'a Session,
    decls: Decls<'a>,
    /// Each definition already described, and the public path it is described
    /// under.
    seen: HashMap<DefId, String>,
    /// The trait impls written for each type, from the impl table.
    methods: HashMap<DefId, Vec<(Option<DefId>, Vec<DefId>, Option<Ty>)>>,
}

/// Every impl in the table, grouped by the type it is for: `(trait, members,
/// self type)`.
fn methods_by_type(s: &Session) -> HashMap<DefId, Vec<(Option<DefId>, Vec<DefId>, Option<Ty>)>> {
    let mut out: HashMap<DefId, Vec<_>> = HashMap::new();
    for imp in &s.impls.impls {
        let Some(head) = imp.self_head else { continue };
        let mut members: Vec<DefId> = imp.members.values().copied().collect();
        members.sort_by_key(|d| s.defs.get(*d).span.map(|sp| sp.start));
        out.entry(head).or_default().push((
            imp.trait_def,
            members,
            imp.typed.as_ref().map(|t| t.self_ty.clone()),
        ));
    }
    out
}

impl Walker<'_> {
    /// A namespace and everything public in it.
    fn namespace(&mut self, ns: DefId, name: &str, path: &str) -> Value {
        let ns = self.s.defs.resolve_alias(ns);
        self.seen.insert(ns, path.to_string());
        let mut item = self.header(ns, name, path, "namespace");
        let members = self.members(ns, path);
        item.insert("members".into(), Value::Array(members));
        Value::Object(item)
    }

    /// The public members of a namespace-like def, in declaration order.
    fn members(&mut self, ns: DefId, path: &str) -> Vec<Value> {
        let mut named: Vec<(String, DefId)> = self
            .s
            .defs
            .get(ns)
            .ns
            .members
            .iter()
            .map(|(n, d)| (n.to_string(), *d))
            .filter(|(_, d)| self.s.defs.get(*d).vis == Visibility::Public)
            .collect();
        let written = self.written_order(ns);
        named.sort_by_key(|(n, d)| {
            let at = written.get(n.as_str()).copied().unwrap_or(usize::MAX);
            (at, self.order(*d), n.clone())
        });
        named
            .into_iter()
            .filter_map(|(n, d)| self.item(d, &n, &format!("{path}.{n}")))
            .collect()
    }

    /// Each name the namespace `ns` binds, by the position of the item that
    /// binds it — which is the only order a re-export has, since the name it
    /// brings in was declared somewhere else.
    fn written_order(&self, ns: DefId) -> HashMap<String, usize> {
        let mut out = HashMap::new();
        let file_root = self
            .s
            .files
            .iter()
            .find(|(_, m)| m.ns == ns)
            .and_then(|(f, _)| Some((*f, self.s.asts.get(f)?.root()?)));
        let def = self.s.defs.get(ns);
        let found = file_root.or_else(|| Some((def.file?, def.node?)));
        let Some((file, node)) = found else {
            return out;
        };
        let Some(ast) = self.s.asts.get(&file) else {
            return out;
        };
        let items = match &ast.node(node).kind {
            NodeKind::File { items } => items.clone(),
            NodeKind::ConstBind { rhs, .. } => match &ast.node(*rhs).kind {
                NodeKind::NamespaceExpr { items, .. } => items.clone(),
                _ => return out,
            },
            _ => return out,
        };
        for (i, item) in items.iter().enumerate() {
            let NodeKind::ConstBind { pattern, .. } = &ast.node(ast.decl_item(*item)).kind else {
                continue;
            };
            let mut stack = vec![*pattern];
            while let Some(n) = stack.pop() {
                match &ast.node(n).kind {
                    NodeKind::BindingPat { name, .. } | NodeKind::FieldPat { name, .. } => {
                        out.entry(name.to_string()).or_insert(i);
                    }
                    _ => {}
                }
                stack.extend(ast.children(n));
            }
        }
        out
    }

    /// Where `d` was written, for ordering: its file's position and its offset.
    ///
    /// An import binding has no span of its own, so its node's is read: a
    /// root that re-exports is ordered the way it was written.
    fn order(&self, d: DefId) -> (u32, usize) {
        let d = self.s.defs.get(d);
        let at = d.span.map(|s| s.start).or_else(|| {
            let ast = self.s.asts.get(&d.file?)?;
            Some(ast.node(d.node?).span.start)
        });
        (d.file.map_or(u32::MAX, |f| f.0), at.unwrap_or(usize::MAX))
    }

    /// One member named `name` at `path`: described, or a reference to the
    /// place it already was.
    fn item(&mut self, d: DefId, name: &str, path: &str) -> Option<Value> {
        let target = self.s.defs.resolve_alias(d);
        if let Some(first) = self.seen.get(&target) {
            return Some(json!({ "name": name, "path": path, "reexport": first }));
        }
        let t = self.s.defs.get(target);
        match t.kind {
            DefKind::Namespace => Some(self.namespace(target, name, path)),
            DefKind::Struct | DefKind::Enum | DefKind::Trait | DefKind::TypeAlias => {
                Some(self.ty(target, name, path))
            }
            DefKind::Func => Some(self.func(target, name, path)),
            DefKind::Overload => {
                self.seen.insert(target, path.to_string());
                let mut item = self.header(target, name, path, "overload");
                if let Some(Decl::Overload(fs)) = self.decls.get(target) {
                    let names: Vec<Value> = fs
                        .iter()
                        .map(|f| Value::String(self.s.defs.canonical_string(*f)))
                        .collect();
                    item.insert("functions".into(), Value::Array(names));
                }
                Some(Value::Object(item))
            }
            // `Res :: Result.<usize, Error>` names a type, whatever the
            // collector filed it under.
            DefKind::Const if matches!(self.decls.get(target), Some(Decl::Alias(_))) => {
                Some(self.ty(target, name, path))
            }
            DefKind::Const => {
                self.seen.insert(target, path.to_string());
                let mut item = self.header(target, name, path, "const");
                if let Some(Decl::Const(c)) = self.decls.get(target) {
                    if let crate::sema::decl::ConstTy::Settled(ty) = &c.ty {
                        item.insert("type".into(), self.show(ty).into());
                    }
                    if let Some(v) = &c.value {
                        item.insert("value".into(), const_json(v));
                    }
                }
                Some(Value::Object(item))
            }
            // Primitives, externals and the rest are not a package's own.
            _ => None,
        }
    }

    /// A struct, an enum, a trait, or a type alias.
    fn ty(&mut self, d: DefId, name: &str, path: &str) -> Value {
        self.seen.insert(d, path.to_string());
        let def = self.s.defs.get(d);
        let kind = match def.kind {
            DefKind::Struct => "struct",
            DefKind::Enum => "enum",
            DefKind::Trait => "trait",
            _ => "type",
        };
        let mut item = self.header(d, name, path, kind);
        if let Some(Decl::Type(t)) = self.decls.get(d) {
            let generics: Vec<Value> = t
                .generics
                .iter()
                .filter_map(|g| g.def)
                .map(|g| Value::String(self.s.defs.get(g).name.to_string()))
                .collect();
            if !generics.is_empty() {
                item.insert("generics".into(), Value::Array(generics));
            }
        }
        if let Some(Decl::Alias(a)) = self.decls.get(d)
            && let Some(repr) = &a.repr
        {
            item.insert("repr".into(), self.show(repr).into());
        }
        let mut children: Vec<DefId> = def.ns.members.values().copied().collect();
        children.sort_by_key(|c| self.order(*c));
        let (mut fields, mut variants, mut members) = (Vec::new(), Vec::new(), Vec::new());
        for c in children {
            let cd = self.s.defs.get(c);
            match cd.kind {
                // A private field is the type's own business, and no reader
                // of its documentation can name it.
                DefKind::Field if cd.vis == Visibility::Private => {}
                DefKind::Field => {
                    let mut f = Map::new();
                    f.insert("name".into(), cd.name.to_string().into());
                    if let Some(Decl::Field(ty)) = self.decls.get(c) {
                        f.insert("type".into(), self.show(ty).into());
                    }
                    f.insert("visibility".into(), vis(cd.vis).into());
                    self.notes(c, &mut f);
                    fields.push(Value::Object(f));
                }
                DefKind::Variant => {
                    let mut v = Map::new();
                    v.insert("name".into(), cd.name.to_string().into());
                    if let Some(Decl::Variant(payload)) = self.decls.get(c) {
                        let p: Vec<Value> = payload
                            .iter()
                            .map(|(n, ty)| json!({ "name": n.as_ref().map(|n| n.to_string()), "type": self.show(ty) }))
                            .collect();
                        if !p.is_empty() {
                            v.insert("payload".into(), Value::Array(p));
                        }
                    }
                    self.notes(c, &mut v);
                    variants.push(Value::Object(v));
                }
                // A trait's own members: its methods, associated types and
                // constants, public by being the trait's.
                _ if def.kind == DefKind::Trait => {
                    let n = cd.name.to_string();
                    if let Some(m) = self.member(c, &n, &format!("{path}.{n}")) {
                        members.push(m);
                    }
                }
                _ => {}
            }
        }
        if !fields.is_empty() {
            item.insert("fields".into(), Value::Array(fields));
        }
        if !variants.is_empty() {
            item.insert("variants".into(), Value::Array(variants));
        }
        if !members.is_empty() {
            item.insert("members".into(), Value::Array(members));
        }
        let (methods, impls) = self.impls(d, path);
        if !methods.is_empty() {
            item.insert("methods".into(), Value::Array(methods));
        }
        if !impls.is_empty() {
            item.insert("impls".into(), Value::Array(impls));
        }
        Value::Object(item)
    }

    /// A trait member: a method, or an associated type or constant.
    fn member(&mut self, d: DefId, name: &str, path: &str) -> Option<Value> {
        let def = self.s.defs.get(d);
        match def.kind {
            DefKind::Func => Some(self.func(d, name, path)),
            _ => {
                let kind = match self.decls.get(d) {
                    Some(Decl::Assoc(a)) if a.ty.is_none() && a.value.is_none() => "assoc",
                    _ if def.kind == DefKind::TypeAlias => "assoc_type",
                    _ => "assoc_const",
                };
                Some(Value::Object(self.header(d, name, path, kind)))
            }
        }
    }

    /// The public methods of `ty`'s inherent impls, and the traits it
    /// implements.
    fn impls(&mut self, ty: DefId, path: &str) -> (Vec<Value>, Vec<Value>) {
        let (mut methods, mut impls) = (Vec::new(), Vec::new());
        let list = self.methods.get(&ty).cloned().unwrap_or_default();
        for (trait_def, members, self_ty) in list {
            match trait_def {
                None => {
                    for m in members {
                        let md = self.s.defs.get(m);
                        if md.kind != DefKind::Func || md.vis != Visibility::Public {
                            continue;
                        }
                        let n = md.name.to_string();
                        methods.push(self.func(m, &n, &format!("{path}.{n}")));
                    }
                }
                Some(t) => {
                    let mut i = Map::new();
                    i.insert("trait".into(), self.s.defs.canonical_string(t).into());
                    if let Some(st) = self_ty {
                        i.insert("for".into(), self.show(&st).into());
                    }
                    impls.push(Value::Object(i));
                }
            }
        }
        (methods, impls)
    }

    /// A function: its parameters, what it returns, and whether it is a method.
    fn func(&mut self, d: DefId, name: &str, path: &str) -> Value {
        self.seen.entry(d).or_insert_with(|| path.to_string());
        let mut item = self.header(d, name, path, "func");
        if let Some(Decl::Func(f)) = self.decls.get(d) {
            item.insert("method".into(), f.recv.into());
            let generics: Vec<Value> = f
                .generics
                .iter()
                .filter_map(|g| g.def)
                .map(|g| Value::String(self.s.defs.get(g).name.to_string()))
                .collect();
            if !generics.is_empty() {
                item.insert("generics".into(), Value::Array(generics));
            }
            if let Some(Ty::Func { params, ret, .. }) = &f.sig {
                let tys = &params[usize::from(f.recv).min(params.len())..];
                let ps: Vec<Value> = f
                    .params
                    .iter()
                    .zip(tys)
                    .map(|(p, t)| json!({ "name": p.name.to_string(), "type": self.show(t), "default": p.default }))
                    .collect();
                item.insert("params".into(), Value::Array(ps));
                if !matches!(**ret, Ty::Void) {
                    item.insert("returns".into(), self.show(ret).into());
                }
            }
        }
        Value::Object(item)
    }

    /// What every description starts with.
    fn header(&self, d: DefId, name: &str, path: &str, kind: &str) -> Map<String, Value> {
        let def = self.s.defs.get(d);
        let mut item = Map::new();
        item.insert("name".into(), name.into());
        item.insert("path".into(), path.into());
        item.insert("kind".into(), kind.into());
        item.insert("visibility".into(), vis(def.vis).into());
        let canonical = self.s.defs.canonical_string(d);
        if canonical != path {
            item.insert("defined_at".into(), canonical.into());
        }
        if let Some(text) = self.declaration(def) {
            item.insert("declaration".into(), text.into());
        }
        if let Some(loc) = self.location(def) {
            item.insert("location".into(), loc);
        }
        self.notes(d, &mut item);
        item
    }

    /// The doc and the other attributes written on `d`.
    fn notes(&self, d: DefId, item: &mut Map<String, Value>) {
        if let Some(doc) = self.s.doc_of(d) {
            // The first paragraph, for an index page or a member list.
            let summary = doc.split("\n\n").next().unwrap_or("").replace('\n', " ");
            item.insert("summary".into(), summary.trim().into());
            item.insert("doc".into(), doc.into());
        }
        let doc = self
            .s
            .lang_items
            .get("doc")
            .map(|x| self.s.defs.resolve_alias(x));
        let attrs: Vec<Value> = self
            .s
            .defs
            .get(d)
            .attrs
            .iter()
            .filter(|a| Some(self.s.defs.resolve_alias(a.def)) != doc)
            .map(|a| {
                let args: Vec<Value> = a
                    .args
                    .iter()
                    .map(|(n, v)| json!({ "name": n.as_ref().map(|n| n.to_string()), "value": const_json(v) }))
                    .collect();
                json!({ "name": self.s.defs.canonical_string(a.def), "args": args })
            })
            .collect();
        if !attrs.is_empty() {
            item.insert("attributes".into(), Value::Array(attrs));
        }
    }

    /// The source of `def`'s declaration up to its body: a function's
    /// signature, a type's header line.
    fn declaration(&self, def: &Def) -> Option<String> {
        let file = def.file?;
        let src = &self.s.sources.file(file)?.src;
        let span = def.span?;
        let mut end = span.end.min(src.len());
        if let (Some(ast), Some(node)) = (self.s.asts.get(&file), def.node)
            && let NodeKind::ConstBind { rhs, .. } = &ast.node(node).kind
            && let NodeKind::FuncExpr {
                body: Some(body), ..
            } = &ast.node(*rhs).kind
        {
            end = ast.node(*body).span.start.min(end);
        } else if let Some(brace) = src[span.start..end].find(['{', '\n']) {
            end = span.start + brace;
        }
        let text = src.get(span.start..end)?.trim_end();
        (!text.is_empty()).then(|| text.to_string())
    }

    fn location(&self, def: &Def) -> Option<Value> {
        let file = self.s.sources.file(def.file?)?;
        let at = file.line_col(def.span?.start);
        Some(json!({ "file": file.name, "line": at.line, "column": at.column }))
    }

    fn show(&self, ty: &Ty) -> String {
        ty.display(&self.s.defs)
    }
}

fn vis(v: Visibility) -> &'static str {
    match v {
        Visibility::Public => "public",
        Visibility::Package => "package",
        Visibility::Private => "private",
    }
}

fn const_json(v: &ConstValue) -> Value {
    match v {
        ConstValue::Str(s) => s.clone().into(),
        ConstValue::Bool(b) => (*b).into(),
        ConstValue::Int(n) => n
            .to_string()
            .parse::<i64>()
            .map_or_else(|_| n.to_string().into(), Value::from),
        ConstValue::Float(f) => json!(f),
        ConstValue::Char(c) => c.to_string().into(),
        other => format!("{other:?}").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The description a docs site is generated from: docs and summaries,
    /// fields and variants, and methods.
    #[test]
    fn a_program_is_described_with_its_docs() {
        let src = "\
/// A point.
///
/// More about it.
@public(all)
Point :: struct {
    /// Across.
    x: i32,
    hidden: i32,
}

impl Point {
    /// Its length.
    @public len :: func (self: *Self) -> i32 { return self.x }
}

@public shapes :: namespace {
    /// A colour.
    @public Color :: enum { red, blue }
}

/// Doubles.
@public twice :: func (n: i32) -> i32 { return n * 2 }
";
        let session = crate::sema::analyze_source("main", src, &[]);
        assert!(!session.has_errors(), "{:#?}", session.diagnostics);
        let entry = session
            .files
            .keys()
            .copied()
            .find(|f| !session.pkg_of.contains_key(f))
            .unwrap();
        let v = describe(&session, entry);
        assert_eq!(v["format"], FORMAT);
        let members = v["root"]["members"].as_array().unwrap();
        let named = |n: &str| members.iter().find(|m| m["name"] == n).unwrap().clone();
        let point = named("Point");
        assert_eq!(point["kind"], "struct");
        assert_eq!(point["summary"], "A point.");
        assert_eq!(point["doc"], "A point.\n\nMore about it.");
        assert_eq!(point["fields"][0]["name"], "x");
        assert_eq!(point["fields"][0]["doc"], "Across.");
        assert_eq!(point["fields"][0]["type"], "i32");
        assert_eq!(point["methods"][0]["name"], "len");
        assert_eq!(point["methods"][0]["doc"], "Its length.");
        assert_eq!(point["methods"][0]["returns"], "i32");
        let twice = named("twice");
        assert_eq!(twice["params"][0]["name"], "n");
        assert_eq!(twice["declaration"], "twice :: func (n: i32) -> i32");
        let shapes = named("shapes");
        assert_eq!(shapes["members"][0]["kind"], "enum");
        assert_eq!(shapes["members"][0]["variants"][1]["name"], "blue");
    }
}
