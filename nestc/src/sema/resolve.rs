//! Name resolution (§4.6) — the stage that links every use of a name to the
//! unique [`Def`](super::def::Def) it refers to.
//!
//! It walks a file with a scope stack and attaches a [`Resolution`] to each name
//! node: single- and multi-segment [`Path`](NodeKind::Path)s, the namespace hops
//! of a [`FieldAccess`](NodeKind::FieldAccess), and the head of a
//! [`TypePath`](NodeKind::TypePath). Block locals, function parameters, generic
//! parameters, and pattern bindings become [`DefKind::Local`]/[`DefKind::Param`]
//! defs as they come into scope, so they too resolve to a unique id.
//!
//! Lookup order for an unqualified name: innermost local scope outward, then the
//! enclosing namespaces (each with its `import`ed names and globs), then the
//! prelude (builtins + the public members of `core`). The first match wins.

use std::collections::HashMap;

use crate::common::diagnostic::Diagnostic;
use crate::common::source::FileId;
use crate::common::source::FileSpan;
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, NodeId, NodeKind, SliceRest};

use super::def::{DefId, DefKind, DefTable, Visibility};
use super::{DefMeta, PathRes, Resolution};

/// Resolve every name in `file`, whose file namespace is `file_ns`.
#[allow(clippy::too_many_arguments)]
pub fn resolve_file(
    defs: &mut DefTable,
    diags: &mut Vec<Diagnostic>,
    ast: &Ast,
    file: FileId,
    file_ns: DefId,
    prelude_globs: &[DefId],
    builtins: DefId,
) {
    let mut r = Resolver {
        defs,
        diags,
        ast,
        file,
        prelude_globs,
        builtins,
        scopes: Vec::new(),
        ns_stack: vec![file_ns],
        self_ty: Vec::new(),
    };
    if let Some(root) = ast.root() {
        r.resolve_node(root);
    }
}

struct Resolver<'a> {
    defs: &'a mut DefTable,
    diags: &'a mut Vec<Diagnostic>,
    ast: &'a Ast,
    file: FileId,
    prelude_globs: &'a [DefId],
    /// The builtins namespace: fixed primitives plus the width-parameterized
    /// primitives (`i32`, `u7`, `f64`, …) synthesized lazily on first use.
    builtins: DefId,
    /// Transient local frames (params, generics, block/pattern bindings).
    scopes: Vec<HashMap<Symbol, DefId>>,
    /// Enclosing namespace chain; the last entry is the current namespace.
    ns_stack: Vec<DefId>,
    /// `Self` targets for the enclosing `impl`/`trait` bodies.
    self_ty: Vec<DefId>,
}

impl Resolver<'_> {
    fn current_ns(&self) -> DefId {
        *self.ns_stack.last().unwrap()
    }

    fn resolve_node(&mut self, id: NodeId) {
        let kind = self.ast.node(id).kind.clone();
        match kind {
            // ===< scoping constructs >===
            NodeKind::NamespaceExpr { items, .. } => {
                let ns = self.def_of(id).unwrap_or_else(|| self.current_ns());
                self.ns_stack.push(ns);
                for item in items {
                    self.resolve_node(item);
                }
                self.ns_stack.pop();
            }
            NodeKind::StructType {
                generics, kind: sk, ..
            } => {
                let ns = self.def_of(id).unwrap_or_else(|| self.current_ns());
                self.ns_stack.push(ns);
                self.push_scope();
                self.bind_generics(&generics);
                for g in &generics {
                    self.resolve_node(*g);
                }
                for child in struct_kind_children(&sk) {
                    self.resolve_node(child);
                }
                self.pop_scope();
                self.ns_stack.pop();
            }
            NodeKind::EnumType {
                generics, variants, ..
            } => {
                let ns = self.def_of(id).unwrap_or_else(|| self.current_ns());
                self.ns_stack.push(ns);
                self.push_scope();
                self.bind_generics(&generics);
                for g in &generics {
                    self.resolve_node(*g);
                }
                for v in variants {
                    self.resolve_node(v);
                }
                self.pop_scope();
                self.ns_stack.pop();
            }
            NodeKind::TraitType {
                generics, members, ..
            } => {
                let def = self.def_of(id).unwrap_or_else(|| self.current_ns());
                self.ns_stack.push(def);
                self.self_ty.push(def);
                self.push_scope();
                self.bind_generics(&generics);
                for g in &generics {
                    self.resolve_node(*g);
                }
                for m in members {
                    self.resolve_node(m);
                }
                self.pop_scope();
                self.self_ty.pop();
                self.ns_stack.pop();
            }
            NodeKind::ImplBlock {
                generics,
                ty,
                for_ty,
                items,
            } => {
                self.push_scope();
                self.bind_generics(&generics);
                for g in &generics {
                    self.resolve_node(*g);
                }
                self.resolve_node(ty);
                if let Some(t) = for_ty {
                    self.resolve_node(t);
                }
                // The self type is the `for` target of a trait impl, else the
                // head type of an inherent impl. `Self` and the member host
                // namespace both follow it (never the implemented trait).
                let self_ty = for_ty.unwrap_or(ty);
                let self_def = self.type_head_def(self_ty);
                let host = self_def.unwrap_or_else(|| self.current_ns());
                if let Some(sd) = self_def {
                    self.self_ty.push(sd);
                } else {
                    self.self_ty.push(host);
                }
                self.ns_stack.push(host);
                for item in items {
                    self.resolve_node(item);
                }
                self.ns_stack.pop();
                self.self_ty.pop();
                self.pop_scope();
            }
            NodeKind::FuncExpr {
                generics,
                params,
                ret,
                body,
                ..
            } => {
                self.push_scope();
                self.bind_generics(&generics);
                for g in &generics {
                    self.resolve_node(*g);
                }
                for p in &params {
                    self.resolve_node(*p);
                    self.bind_param(*p);
                }
                if let Some(r) = ret {
                    self.resolve_node(r);
                }
                if let Some(b) = body {
                    self.resolve_node(b);
                }
                self.pop_scope();
            }
            NodeKind::Block { stmts, tail } => {
                self.push_scope();
                for s in stmts {
                    self.resolve_node(s);
                }
                if let Some(t) = tail {
                    self.resolve_node(t);
                }
                self.pop_scope();
            }
            NodeKind::LocalDecl {
                pattern, ty, value, ..
            } => {
                if let Some(t) = ty {
                    self.resolve_node(t);
                }
                self.resolve_node(value);
                self.bind_pattern(pattern);
            }
            NodeKind::For {
                pattern,
                iter,
                body,
            } => {
                self.resolve_node(iter);
                self.push_scope();
                self.bind_pattern(pattern);
                self.resolve_node(body);
                self.pop_scope();
            }
            NodeKind::MatchArm {
                pattern,
                guard,
                body,
            } => {
                self.push_scope();
                self.bind_pattern(pattern);
                if let Some(g) = guard {
                    self.resolve_node(g);
                }
                self.resolve_node(body);
                self.pop_scope();
            }
            NodeKind::IfMatch {
                pattern,
                value,
                then,
                els,
            } => {
                self.resolve_node(value);
                self.push_scope();
                self.bind_pattern(pattern);
                self.resolve_node(then);
                self.pop_scope();
                if let Some(e) = els {
                    self.resolve_node(e);
                }
            }

            // ===< name nodes >===
            NodeKind::Path { segments } => self.resolve_path(id, &segments),
            NodeKind::TypePath { path, generic_args } => {
                self.resolve_node(path);
                // Mirror the path's resolution onto the TypePath for convenience.
                if let Some(res) = self.ast.meta::<Resolution>(path) {
                    self.ast.set_meta(id, res);
                }
                for a in generic_args {
                    self.resolve_node(a);
                }
            }
            NodeKind::FieldAccess { base, name } => {
                self.resolve_node(base);
                self.resolve_field(id, base, &name);
            }

            // An attribute's / directive's arguments are drawn from a fixed
            // compiler vocabulary — `@public(all)`, `#align(16)` — not from the
            // program's names, so they are read by `collect`, never resolved.
            NodeKind::Attribute { .. } | NodeKind::Directive { .. } => {}

            // ===< everything else: structural recursion >===
            _ => {
                let children = self.ast.node(id).kind.children();
                for child in children {
                    self.resolve_node(child);
                }
            }
        }
    }

    // ===< path / field resolution >===

    fn resolve_path(&mut self, id: NodeId, segments: &[Symbol]) {
        if segments.is_empty() {
            return;
        }
        let mut per_seg = Vec::with_capacity(segments.len());
        // Root segment: reserved names first, then the scope search.
        let mut cur = self.resolve_root(id, &segments[0].clone());
        per_seg.push(cur.clone());
        // Subsequent segments: member hops.
        for seg in &segments[1..] {
            cur = match &cur {
                Resolution::Def(base) => self
                    .resolve_member(*base, seg)
                    .map(Resolution::Def)
                    .unwrap_or(Resolution::Error),
                _ => Resolution::Error,
            };
            per_seg.push(cur.clone());
        }
        // The whole-path resolution is its final segment.
        self.ast.set_meta(id, cur.clone());
        if segments.len() > 1 {
            self.ast.set_meta(id, PathRes(per_seg));
        }
        if matches!(cur, Resolution::Error) {
            let dotted = segments
                .iter()
                .map(Symbol::as_str)
                .collect::<Vec<_>>()
                .join(".");
            self.report(id, format!("cannot resolve name `{dotted}`"));
        }
    }

    fn resolve_root(&mut self, _id: NodeId, name: &Symbol) -> Resolution {
        match name.as_str() {
            "self" => self
                .lookup_local(name)
                .map(Resolution::Def)
                .unwrap_or(Resolution::Error),
            "Self" => self
                .self_ty
                .last()
                .copied()
                .map(Resolution::Def)
                .unwrap_or(Resolution::Error),
            _ => self
                .lookup_unqualified(name)
                .or_else(|| self.synth_primitive(name).map(Resolution::Def))
                .unwrap_or(Resolution::Error),
        }
    }

    /// A name that resolved nowhere may still be a width-parameterized primitive
    /// (`i32`, `u7`, `f64`, …). Parse it; if it is a valid one, intern a
    /// [`DefKind::Primitive`] into the builtins scope (so later uses — in this
    /// file and others — find it through the normal prelude glob) and return it.
    /// `u1` aliases the fixed `bool` primitive (§3.1). Names that merely *look*
    /// like a primitive but carry an invalid width (`i1`, `f100`, `u70000`)
    /// return `None`, falling through to the ordinary "cannot resolve" error.
    fn synth_primitive(&mut self, name: &Symbol) -> Option<DefId> {
        let s = name.as_str();
        let (prefix, digits) = if let Some(d) = s.strip_prefix('i') {
            ('i', d)
        } else if let Some(d) = s.strip_prefix('u') {
            ('u', d)
        } else if let Some(d) = s.strip_prefix('f') {
            ('f', d)
        } else {
            return None;
        };
        // No leading zeros, digits only, fits the width range.
        if digits.is_empty()
            || (digits.len() > 1 && digits.starts_with('0'))
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let width: u32 = digits.parse().ok()?;
        match prefix {
            // `bool` is an alias for `u1`, so `u1` resolves to the same def (§3.1).
            'u' if width == 1 => {
                return self
                    .defs
                    .get(self.builtins)
                    .ns
                    .members
                    .get(&Symbol::new("bool"))
                    .copied();
            }
            // 1-bit signed integers are not a type (§3.1); every other width up to
            // 65535 is legal.
            'i' if width == 1 => return None,
            'i' | 'u' if (1..=65535).contains(&width) => {}
            'f' if matches!(width, 16 | 32 | 64 | 80 | 128) => {}
            _ => return None,
        }
        // Intern once: a second use of `i32` must resolve to the same def so the
        // two are the same nominal type.
        if let Some(&existing) = self.defs.get(self.builtins).ns.members.get(name) {
            return Some(existing);
        }
        let id = self.defs.alloc(
            name.clone(),
            DefKind::Primitive,
            Visibility::Public,
            Some(self.builtins),
            None,
            None,
            None,
            vec![name.clone()],
        );
        self.defs
            .get_mut(self.builtins)
            .ns
            .members
            .insert(name.clone(), id);
        Some(id)
    }

    /// A `base.name` hop where `base` is a name path: if `base` resolved to a
    /// namespace-like def, resolve `name` as its member.
    fn resolve_field(&mut self, id: NodeId, base: NodeId, name: &Symbol) {
        let Some(Resolution::Def(base_def)) = self.ast.meta::<Resolution>(base) else {
            return; // runtime field access on a value — left for the type checker
        };
        if !self
            .defs
            .get(self.defs.resolve_alias(base_def))
            .kind
            .is_namespace_like()
        {
            return;
        }
        match self.resolve_member(base_def, name) {
            Some(d) => {
                self.ast.set_meta(id, Resolution::Def(d));
            }
            None => self.report(
                id,
                format!(
                    "`{name}` is not a public member of `{}`",
                    self.defs
                        .canonical_string(self.defs.resolve_alias(base_def))
                ),
            ),
        }
    }

    // ===< scope search >===

    fn lookup_local(&self, name: &Symbol) -> Option<DefId> {
        self.scopes
            .iter()
            .rev()
            .find_map(|frame| frame.get(name).copied())
    }

    fn lookup_unqualified(&self, name: &Symbol) -> Option<Resolution> {
        if let Some(d) = self.lookup_local(name) {
            return Some(Resolution::Def(d));
        }
        // Enclosing namespaces, innermost outward.
        //
        // Every frame on the stack is searched, not just the innermost, because
        // an `impl` block pushes the *host type's* namespace — which for an impl
        // on a foreign type (`impl Trait for core.Result.<T, MyError>`) is not a
        // descendant of the file at all. Walking only its parent chain would
        // reach `core` and stop, losing every name the impl is lexically written
        // among. Each frame contributes its own parent chain, innermost first,
        // and a namespace already searched is not searched again.
        let mut seen: Vec<DefId> = Vec::new();
        for &frame in self.ns_stack.iter().rev() {
            let mut ns = Some(frame);
            while let Some(n) = ns {
                if seen.contains(&n) {
                    break;
                }
                seen.push(n);
                if let Some(d) = self.defs.get(n).ns.get_direct(name) {
                    return Some(Resolution::Def(self.defs.resolve_alias(d)));
                }
                for &g in &self.defs.get(n).ns.globs {
                    if let Some(d) = self.public_member(g, name) {
                        return Some(Resolution::Def(d));
                    }
                }
                ns = self.defs.get(n).parent;
            }
        }
        // Prelude (builtins + core globs).
        for &g in self.prelude_globs {
            if let Some(d) = self.public_member(g, name) {
                return Some(Resolution::Def(d));
            }
        }
        None
    }

    /// Resolve `name` as a member of `base`, enforcing visibility across files.
    fn resolve_member(&self, base: DefId, name: &Symbol) -> Option<DefId> {
        let base = self.defs.resolve_alias(base);
        let same_file = self.defs.get(base).file == Some(self.file);
        let d = self.defs.get(base).ns.get_direct(name)?;
        let d = self.defs.resolve_alias(d);
        if same_file || self.defs.get(d).vis.is_public() {
            Some(d)
        } else {
            None
        }
    }

    fn public_member(&self, base: DefId, name: &Symbol) -> Option<DefId> {
        let base = self.defs.resolve_alias(base);
        let d = self
            .defs
            .resolve_alias(*self.defs.get(base).ns.members.get(name)?);
        self.defs.get(d).vis.is_public().then_some(d)
    }

    // ===< binding introduction >===

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind_generics(&mut self, generics: &[NodeId]) {
        for &g in generics {
            // A `<const N: T>` parameter is a compile-time *value*, not a
            // type, so it gets its own kind: `N` may be used where a value is
            // expected and as the length in `[N]T`, but never as a type.
            let (name, kind) = match &self.ast.node(g).kind {
                NodeKind::GenericTypeParam { name, .. } => (name.clone(), DefKind::TypeParam),
                NodeKind::GenericConstParam { name, .. } => (name.clone(), DefKind::ConstParam),
                _ => continue,
            };
            self.introduce(name, kind, g);
        }
    }

    fn bind_param(&mut self, param: NodeId) {
        if let NodeKind::Param { name, .. } = &self.ast.node(param).kind {
            let name = name.clone();
            self.introduce(name, DefKind::Param, param);
        }
    }

    /// Bind every name a pattern introduces into the current scope frame.
    fn bind_pattern(&mut self, pattern: NodeId) {
        match self.ast.node(pattern).kind.clone() {
            NodeKind::BindingPat { name, .. } => {
                self.introduce(name, DefKind::Local, pattern);
            }
            NodeKind::AtPat {
                name,
                pattern: inner,
            } => {
                self.introduce(name, DefKind::Local, pattern);
                self.bind_pattern(inner);
            }
            NodeKind::TuplePat { elems }
            | NodeKind::OrPat {
                alternatives: elems,
            } => {
                for e in elems {
                    self.bind_pattern(e);
                }
            }
            NodeKind::RefPat { pattern: inner } => self.bind_pattern(inner),
            NodeKind::StructPat { path, fields, .. } => {
                if let Some(p) = path {
                    self.resolve_node(p);
                }
                for f in fields {
                    self.bind_field_pat(f);
                }
            }
            NodeKind::TupleStructPat { path, elems, .. } => {
                self.resolve_node(path);
                for e in elems {
                    self.bind_pattern(e);
                }
            }
            NodeKind::VariantPat { args, .. } => {
                match args {
                    // Tuple payload: each child is a sub-pattern.
                    crate::parser::ast::VariantPatArgs::Tuple(elems) => {
                        for e in elems {
                            self.bind_pattern(e);
                        }
                    }
                    // Record payload: each child is a `FieldPat` (`{ radius }` /
                    // `{ radius: p }`), bound like a struct pattern's fields.
                    crate::parser::ast::VariantPatArgs::Record { fields, .. } => {
                        for f in fields {
                            self.bind_field_pat(f);
                        }
                    }
                    crate::parser::ast::VariantPatArgs::None => {}
                }
            }
            NodeKind::SlicePat { elems, rest } => {
                for e in elems {
                    self.bind_pattern(e);
                }
                if let Some(SliceRest { name: Some(name), .. }) = rest {
                    self.introduce(name, DefKind::Local, pattern);
                }
            }
            // Literals, ranges, wildcards, globs bind nothing.
            _ => {}
        }
    }

    fn bind_field_pat(&mut self, field: NodeId) {
        if let NodeKind::FieldPat { name, pattern, .. } = self.ast.node(field).kind.clone() {
            match pattern {
                Some(p) => self.bind_pattern(p),
                None => self.introduce(name, DefKind::Local, field),
            }
        }
    }

    /// Allocate a local-ish def, add it to the current scope frame, and stamp
    /// [`DefMeta`] on its introducing node.
    fn introduce(&mut self, name: Symbol, kind: DefKind, node: NodeId) {
        let scope = self.current_ns();
        let span = self.ast.node(node).span;
        let id = self.defs.alloc(
            name.clone(),
            kind,
            Visibility::Private,
            Some(scope),
            Some(self.file),
            Some(span),
            Some(node),
            vec![name.clone()],
        );
        self.ast.set_meta(node, DefMeta(id));
        if let Some(frame) = self.scopes.last_mut() {
            frame.insert(name, id);
        }
    }

    // ===< helpers >===

    fn def_of(&self, node: NodeId) -> Option<DefId> {
        self.ast.meta::<DefMeta>(node).map(|m| m.0)
    }

    /// The def a type expression's head names, resolved through the current
    /// scope (used for `impl` target / `Self`).
    fn type_head_def(&mut self, ty: NodeId) -> Option<DefId> {
        let path = match &self.ast.node(ty).kind {
            NodeKind::TypePath { path, .. } => *path,
            NodeKind::Path { .. } => ty,
            _ => return None,
        };
        let seg = match &self.ast.node(path).kind {
            NodeKind::Path { segments } => segments.first().cloned()?,
            _ => return None,
        };
        match self.lookup_unqualified(&seg)? {
            Resolution::Def(d) => Some(self.defs.resolve_alias(d)),
            _ => None,
        }
    }

    fn report(&mut self, node: NodeId, message: impl Into<String>) {
        let span = self.ast.node(node).span;
        self.diags
            .push(Diagnostic::error(message).with_primary(FileSpan::new(self.file, span), ""));
    }
}

// ===< small AST helpers >===

fn struct_kind_children(kind: &crate::parser::ast::StructKind) -> Vec<NodeId> {
    use crate::parser::ast::StructKind::*;
    match kind {
        Record(ids) | Tuple(ids) => ids.clone(),
        Unit => Vec::new(),
    }
}
