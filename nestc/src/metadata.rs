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
//! one that changes meaning or goes away is a bump. The shape is documented for
//! generator authors in `docs/src/content/docs/toolchain/metadata.md`.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::common::source::FileId;
use crate::ir::const_eval::ConstValue;
use crate::parser::ast::NodeKind;
use crate::sema::decl::{Decl, Decls};
use crate::sema::def::{Def, DefId, DefKind, DirectiveArg, Visibility};
use crate::sema::session::Session;
use crate::sema::ty::Ty;

/// The version of the shape written. See the module documentation.
pub const FORMAT: u32 = 2;

/// The package `entry` is the root file of, described. `package` is its name
/// when it was compiled as one (`--package`), and `null` for a program.
pub fn describe(session: &Session, entry: FileId) -> Value {
    let Some(meta) = session.files.get(&entry) else {
        return json!({ "format": FORMAT, "package": null, "root": null });
    };
    let package = session.pkg_of.get(&entry).cloned();
    let entry_dir = session
        .sources
        .file(entry)
        .and_then(|f| std::path::Path::new(&f.name).parent().map(|p| p.to_path_buf()));
    let mut w = Walker {
        s: session,
        decls: Decls::new(&session.defs, &session.asts, &session.decls),
        seen: HashMap::new(),
        methods: methods_by_type(session),
        tags: variant_tags(session),
        pins: pins_by_base(session),
        entry_dir,
    };
    let name = package.clone().unwrap_or_else(|| "main".to_string());
    let root = w.namespace(meta.ns, &name, &name, None);
    // Every impl this package wrote that no type above lists: a blanket
    // `impl <T: Default> Fill for T`, one for a primitive, one for another
    // package's type. Without these, a trait's implementors are incomplete.
    let mut impls = Vec::new();
    for (i, imp) in session.impls.impls.iter().enumerate() {
        let listed = imp.self_head.is_some_and(|h| w.seen.contains_key(&h));
        if listed || imp.trait_def.is_none() || session.pkg_of.get(&imp.file) != package.as_ref() {
            continue;
        }
        impls.push(w.impl_json(i));
    }
    json!({ "format": FORMAT, "package": package, "root": root, "impls": impls })
}

/// Each enum variant's discriminant, by its def.
fn variant_tags(s: &Session) -> HashMap<DefId, i128> {
    let mut out = HashMap::new();
    for ir in s.ir.values() {
        for t in &ir.types {
            if let crate::ir::TypeDefKind::Enum { variants } = &t.kind {
                out.extend(variants.iter().map(|v| (v.def, v.tag)));
            }
        }
    }
    out
}

struct Walker<'a> {
    s: &'a Session,
    decls: Decls<'a>,
    /// Each definition already described, and the public path it is described
    /// under.
    seen: HashMap<DefId, String>,
    /// The impls written for each type, from the impl table: each one's index
    /// there and its members in written order.
    methods: HashMap<DefId, Vec<(usize, Vec<DefId>)>>,
    /// Each enum variant's discriminant.
    tags: HashMap<DefId, i128>,
    /// The associated-type parameters synthesized for each type parameter's
    /// bounds, as `(trait, associated type, the parameter)` — where a pin such
    /// as `Func(i32) -> R`'s `Output = R` is recorded.
    pins: HashMap<DefId, Vec<(DefId, String, DefId)>>,
    /// The directory of the program described, when it is not a package, for
    /// locations to be relative to.
    entry_dir: Option<std::path::PathBuf>,
}

/// The synthesized projection parameters, by the parameter they project
/// through.
fn pins_by_base(s: &Session) -> HashMap<DefId, Vec<(DefId, String, DefId)>> {
    let mut out: HashMap<DefId, Vec<_>> = HashMap::new();
    for (i, d) in s.defs.iter().enumerate() {
        if let Some(p) = &d.projection {
            out.entry(p.base)
                .or_default()
                .push((p.trait_def, p.assoc.to_string(), DefId(i as u32)));
        }
    }
    out
}

/// Every impl in the table, grouped by the type it is for.
fn methods_by_type(s: &Session) -> HashMap<DefId, Vec<(usize, Vec<DefId>)>> {
    let mut out: HashMap<DefId, Vec<_>> = HashMap::new();
    for (i, imp) in s.impls.impls.iter().enumerate() {
        let Some(head) = imp.self_head else { continue };
        let mut members: Vec<DefId> = imp.members.values().copied().collect();
        members.sort_by_key(|d| s.defs.get(*d).span.map(|sp| sp.start));
        out.entry(head).or_default().push((i, members));
    }
    out
}

impl Walker<'_> {
    /// A namespace and everything public in it. `binding` is the `::` that
    /// named it, whose `///` documents a namespace a file is.
    fn namespace(&mut self, ns: DefId, name: &str, path: &str, binding: Option<DefId>) -> Value {
        let ns = self.s.defs.resolve_alias(ns);
        self.seen.insert(ns, path.to_string());
        let mut item = self.header(ns, name, path, "namespace");
        if let Some(b) = binding
            && !item.contains_key("doc")
        {
            self.notes(b, &mut item);
        }
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
            DefKind::Namespace => Some(self.namespace(target, name, path, Some(d))),
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
        let distinct = matches!(self.decls.get(d), Some(Decl::Alias(a)) if a.repr.is_some());
        let mut item = self.header(d, name, path, if distinct { "distinct" } else { kind });
        if let Some(Decl::Type(t)) = self.decls.get(d) {
            let generics = self.generics(&t.generics);
            if !generics.is_empty() {
                item.insert("generics".into(), Value::Array(generics));
            }
        }
        if let Some(Decl::Alias(a)) = self.decls.get(d) {
            if let Some(repr) = &a.repr {
                item.insert("repr".into(), self.show(repr).into());
            }
            if let Some(to) = &a.expands_to {
                item.insert("expands_to".into(), self.show(to).into());
            }
        }
        // Discriminants are listed only where one was not simply the
        // variant's position — where the program wrote them.
        let positional = {
            let mut vs: Vec<DefId> = def
                .ns
                .members
                .values()
                .copied()
                .filter(|c| self.s.defs.get(*c).kind == DefKind::Variant)
                .collect();
            vs.sort_by_key(|c| self.order(*c));
            vs.iter()
                .enumerate()
                .all(|(i, v)| self.tags.get(v).is_none_or(|t| *t == i as i128))
        };
        let show_tags = !positional;
        let mut children: Vec<DefId> = def.ns.members.values().copied().collect();
        children.sort_by_key(|c| self.order(*c));
        let (mut fields, mut variants, mut members) = (Vec::new(), Vec::new(), Vec::new());
        for c in children {
            let cd = self.s.defs.get(c);
            match cd.kind {
                // A field that is not public is the package's own business,
                // and no reader of its documentation can name it.
                DefKind::Field if cd.vis != Visibility::Public => {}
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
                    if show_tags && let Some(t) = self.tags.get(&c) {
                        v.insert("value".into(), big_int(*t));
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
        if def.kind == DefKind::Func {
            let mut f = self.func(d, name, path);
            // A method with no body is one every impl must write; one with a
            // body is a default an impl may inherit.
            if let (Value::Object(m), Some(Decl::Func(fd))) = (&mut f, self.decls.get(d)) {
                m.insert("required".into(), (!fd.has_body).into());
            }
            return Some(f);
        }
        let Some(Decl::Assoc(a)) = self.decls.get(d) else {
            return Some(Value::Object(self.header(d, name, path, "assoc_type")));
        };
        let kind = match a.kind {
            crate::sema::decl::Requirement::AssocConst => "assoc_const",
            _ => "assoc_type",
        };
        let mut item = self.header(d, name, path, kind);
        if let Some(ty) = &a.ty {
            item.insert("type".into(), self.show(ty).into());
        }
        if let Some(v) = &a.value {
            item.insert("value".into(), const_json(v));
        }
        item.insert("required".into(), (!a.answered).into());
        let bounds = self.bounds(d);
        if !bounds.is_empty() {
            item.insert("bounds".into(), Value::Array(bounds));
        }
        Some(Value::Object(item))
    }

    /// The public methods of `ty`'s inherent impls, and the traits it
    /// implements.
    fn impls(&mut self, ty: DefId, path: &str) -> (Vec<Value>, Vec<Value>) {
        let (mut methods, mut impls) = (Vec::new(), Vec::new());
        let list = self.methods.get(&ty).cloned().unwrap_or_default();
        for (index, members) in list {
            if self.s.impls.impls[index].trait_def.is_some() {
                impls.push(self.impl_json(index));
                continue;
            }
            for m in members {
                let md = self.s.defs.get(m);
                if md.kind != DefKind::Func || md.vis != Visibility::Public {
                    continue;
                }
                let n = md.name.to_string();
                methods.push(self.func(m, &n, &format!("{path}.{n}")));
            }
        }
        (methods, impls)
    }

    /// A trait impl: which trait, for what, over which generics, and where it
    /// was written — the package matters, since a type's impls include the
    /// ones other packages wrote for it.
    fn impl_json(&self, index: usize) -> Value {
        let imp = &self.s.impls.impls[index];
        let mut i = Map::new();
        if let Some(t) = imp.trait_def {
            i.insert("trait".into(), self.s.defs.canonical_string(t).into());
        }
        if let Some(typed) = &imp.typed {
            i.insert("for".into(), self.show(&typed.self_ty).into());
            if !typed.trait_args.is_empty() {
                let args: Vec<Value> = typed.trait_args.iter().map(|t| self.show(t).into()).collect();
                i.insert("trait_args".into(), Value::Array(args));
            }
            if !typed.assoc.is_empty() {
                let mut assoc: Vec<(String, String)> = typed
                    .assoc
                    .iter()
                    .map(|(n, t)| (n.to_string(), self.show(t)))
                    .collect();
                assoc.sort();
                let assoc: Map<String, Value> = assoc.into_iter().map(|(n, t)| (n, t.into())).collect();
                i.insert("assoc".into(), Value::Object(assoc));
            }
        }
        let generics: Vec<Value> = imp
            .generics
            .iter()
            .filter(|g| self.s.defs.get(**g).projection.is_none())
            .map(|g| self.generic(*g))
            .collect();
        if !generics.is_empty() {
            i.insert("generics".into(), Value::Array(generics));
        }
        i.insert("package".into(), self.s.pkg_of.get(&imp.file).cloned().into());
        // Where the `for` target is written: an impl has no span of its own.
        let start = imp
            .syntax
            .as_ref()
            .and_then(|syn| Some(self.s.asts.get(&imp.file)?.node(syn.self_node).span.start));
        if let Some(loc) = start.and_then(|at| self.at(imp.file, at)) {
            i.insert("location".into(), loc);
        }
        Value::Object(i)
    }

    /// A declaration's generic parameters, with their bounds.
    fn generics(&self, list: &[crate::sema::decl::GenericParam]) -> Vec<Value> {
        list.iter()
            .filter_map(|g| Some(self.generic(g.def?)))
            .collect()
    }

    fn generic(&self, g: DefId) -> Value {
        let mut out = Map::new();
        let def = self.s.defs.get(g);
        out.insert("name".into(), def.name.to_string().into());
        if def.kind == DefKind::ConstParam {
            out.insert("const".into(), true.into());
        }
        let bounds = self.bounds(g);
        if !bounds.is_empty() {
            out.insert("bounds".into(), Value::Array(bounds));
        }
        Value::Object(out)
    }

    /// What bounds a type parameter or an associated type, each written the way
    /// a type is: `core.cmp.Ord`, `core.ops.Func.<Args = (i32), Output = T>`.
    fn bounds(&self, g: DefId) -> Vec<Value> {
        let written: Vec<(DefId, Vec<Ty>)> = match self.decls.get(g) {
            Some(Decl::Param(p)) => p.bounds.clone(),
            _ => self
                .s
                .defs
                .get(g)
                .param_bounds
                .iter()
                .flatten()
                .map(|t| (*t, Vec::new()))
                .collect(),
        };
        written
            .iter()
            .map(|(t, args)| {
                let mut shown: Vec<String> = args.iter().map(|a| self.show(a)).collect();
                for (trait_def, assoc, param) in self.pins.get(&g).into_iter().flatten() {
                    if trait_def != t {
                        continue;
                    }
                    if let Some(Decl::Param(p)) = self.decls.get(*param)
                        && let Some(pinned) = &p.pinned
                    {
                        shown.push(format!("{assoc} = {}", self.show(pinned)));
                    }
                }
                // By name: the table's order is the order the parameters were
                // synthesized in, which is not stable from one build to the next.
                shown[args.len()..].sort();
                let name = self.s.defs.canonical_string(*t);
                if shown.is_empty() {
                    name.into()
                } else {
                    format!("{name}.<{}>", shown.join(", ")).into()
                }
            })
            .collect()
    }

    /// A function: its parameters, what it returns, and whether it is a method.
    fn func(&mut self, d: DefId, name: &str, path: &str) -> Value {
        self.seen.entry(d).or_insert_with(|| path.to_string());
        let mut item = self.header(d, name, path, "func");
        if let Some(Decl::Func(f)) = self.decls.get(d) {
            item.insert("method".into(), f.recv.into());
            let generics = self.generics(&f.generics);
            if !generics.is_empty() {
                item.insert("generics".into(), Value::Array(generics));
            }
            // `<Self: Sized, Self.Item: Ord>`, written the way a bound is.
            let mut self_bounds: Vec<Value> = Vec::new();
            if f.sized_self {
                let sized = self.s.lang_items.get("sized");
                let bound = sized.map_or_else(|| "Sized".to_string(), |d| self.s.defs.canonical_string(d));
                self_bounds.push(json!({ "on": "Self", "bound": bound }));
            }
            for (assoc, t) in &f.self_assoc_bounds {
                let bound = self.s.defs.canonical_string(*t);
                self_bounds.push(json!({ "on": format!("Self.{assoc}"), "bound": bound }));
            }
            if !self_bounds.is_empty() {
                item.insert("self_bounds".into(), Value::Array(self_bounds));
            }
            if let Some(Ty::Func { params, ret, .. }) = &f.sig {
                if f.recv
                    && let Some(r) = params.first()
                {
                    item.insert("receiver".into(), self.show(r).into());
                }
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
        let directives: Vec<Value> = def
            .directives
            .iter()
            .map(|dir| {
                let args: Vec<Value> = dir
                    .args
                    .iter()
                    .map(|a| match a {
                        DirectiveArg::Int(n) => big_int(*n),
                        DirectiveArg::Str(s) => s.to_string().into(),
                        DirectiveArg::Name(s) => json!({ "name": s.to_string() }),
                        DirectiveArg::Other => Value::Null,
                    })
                    .collect();
                json!({ "name": dir.name.to_string(), "args": args })
            })
            .collect();
        if !directives.is_empty() {
            item.insert("directives".into(), Value::Array(directives));
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
        self.at(def.file?, def.span?.start)
    }

    /// A place in the source: the package it is in and the file's path
    /// relative to that package's directory (the program's, for a program), so
    /// a description does not depend on where it was built.
    fn at(&self, file: FileId, offset: usize) -> Option<Value> {
        let src = self.s.sources.file(file)?;
        let at = src.line_col(offset);
        let package = self.s.pkg_of.get(&file);
        let dir = match package {
            Some(p) => self.s.package_dir(p).map(|d| d.to_path_buf()),
            None => self.entry_dir.clone(),
        };
        let path = std::path::Path::new(&src.name);
        let name = dir
            .and_then(|d| path.strip_prefix(d).ok().map(|p| p.to_string_lossy().into_owned()))
            .unwrap_or_else(|| src.name.clone());
        Some(json!({ "package": package, "file": name, "line": at.line, "column": at.column }))
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

/// An integer as a JSON number where one holds it exactly, and as a string
/// where it does not.
fn big_int(n: i128) -> Value {
    i64::try_from(n).map_or_else(|_| n.to_string().into(), Value::from)
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

    /// What format 2 added for a generator: bounds and their pins, receivers,
    /// required trait members, directives, discriminants, `distinct` types,
    /// the impls no listed type carries, and a namespace's doc on its binding.
    #[test]
    fn a_program_is_described_for_a_generator() {
        let src = "\
@public Show :: trait {
    Out :: type
    /// Required.
    show :: func (self: *Self) -> i32
    /// Provided.
    twice :: func (self: *Self) -> i32 { return self.show() * 2 }
}

@public Id :: distinct u32

@public Code :: enum { ok = 0, bad = 7 }
@public Plain :: enum { a, b }

@public(all)
Box :: struct {
    value: i32,
    @public(package) inner: i32,
}

impl Box {
    @public get :: func (self: *mut Self) -> i32 { return self.value }
}

impl <T: Default> Show for T {
    Out :: i32
    show :: func (self: *Self) -> i32 { return 0 }
}

/// Things.
@public things :: namespace {
    @public x :: 1
}

@public apply :: #inline func <A, F: Func(A) -> i32> (f: F, a: A) -> i32 { return f(a) }

{ Default } :: import <core/default>
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
        let members = v["root"]["members"].as_array().unwrap();
        let named = |n: &str| members.iter().find(|m| m["name"] == n).unwrap().clone();

        let show = named("Show");
        let m = |n: &str| {
            show["members"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["name"] == n)
                .unwrap()
                .clone()
        };
        assert_eq!(m("Out")["kind"], "assoc_type");
        assert_eq!(m("show")["required"], true);
        assert_eq!(m("twice")["required"], false);

        let id = named("Id");
        assert_eq!(id["kind"], "distinct");
        assert_eq!(id["repr"], "u32");

        assert_eq!(named("Code")["variants"][1]["value"], 7);
        assert!(named("Plain")["variants"][1].get("value").is_none());

        let bx = named("Box");
        let fields = bx["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 1, "a package field is not documented: {fields:#?}");
        assert_eq!(bx["methods"][0]["receiver"], "*mut Box");

        let apply = named("apply");
        assert_eq!(apply["directives"][0]["name"], "inline");
        assert_eq!(apply["generics"][0], json!({ "name": "A" }));
        let bound = apply["generics"][1]["bounds"][0].as_str().unwrap();
        assert!(bound.ends_with("Func.<Args = (A), Output = i32>"), "{bound}");

        assert_eq!(named("things")["doc"], "Things.");

        let impls = v["impls"].as_array().unwrap();
        let blanket = impls.iter().find(|i| i["for"] == "T").expect("the blanket impl");
        assert!(blanket["trait"].as_str().unwrap().ends_with("Show"));
        assert!(blanket["generics"][0]["bounds"][0].as_str().unwrap().ends_with("Default"));
        assert_eq!(blanket["location"]["line"], 24);
    }
}
