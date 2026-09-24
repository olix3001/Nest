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

use std::collections::{HashMap, HashSet};

use crate::common::diagnostic::Diagnostic;
use crate::common::source::FileId;
use crate::common::source::FileSpan;
use crate::common::symbol::Symbol;
use crate::ir::const_eval::ConstValue;
use crate::parser::ast::{Ast, NodeId, NodeKind, SliceRest};

use super::def::AttrValue;
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
    pkg_of: &HashMap<FileId, String>,
) {
    let mut r = Resolver {
        defs,
        diags,
        ast,
        file,
        pkg_of,
        prelude_globs,
        builtins,
        scopes: Vec::new(),
        ns_stack: vec![file_ns],
        self_ty: Vec::new(),
        unsized_ok: HashSet::new(),
        decl_static: false,
        decl_comptime: false,
        boundaries: Vec::new(),
        owners: Vec::new(),
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
    /// Which package each file belongs to, for `@public(package)` (§4.4). A
    /// file with no entry belongs to no package — the program's own files,
    /// which are one such unit between them.
    pkg_of: &'a HashMap<FileId, String>,
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
    /// Type nodes that sit directly under a pointer, and may therefore name a
    /// **sizeless** type: a `dyn Trait` (§3.4) or an `opaque` (§3.1, §11).
    /// Filled in on the way down, so the type node sees it.
    ///
    /// The two are one rule because they are one situation — a type that has no
    /// size is a type only behind a pointer — and keeping one set means a new
    /// sizeless type gets the position rule by naming it here rather than by
    /// growing a parallel mechanism.
    unsized_ok: HashSet<NodeId>,
    /// Whether the `::` binding currently being walked carries `#static`.
    ///
    /// Set by the enclosing [`NodeKind::Decl`] on the way down. A block-local
    /// `::` is immutable; a `#static` one names a program-lifetime region and is
    /// the only `::` form that is assignable (§2.6), and the binding is
    /// introduced one level below where the directive is written.
    decl_static: bool,
    /// Whether the binding being resolved was written `#comptime` — the
    /// variable an unrolled `#comptime for` binds, which names a compile-time
    /// value rather than a slot.
    decl_comptime: bool,
    /// The function literals and closures being walked, innermost last, each
    /// with the depth of [`Resolver::scopes`] where it starts (§5.5).
    ///
    /// A local bound below a closure's depth and named inside it is one the
    /// closure captures; below a `::` function's, it is one the function cannot
    /// see at all, since a `::` never captures.
    boundaries: Vec<Boundary>,
    /// The canonical path of the function or closure being walked, and how many
    /// closures it has had, so each closure gets a path of its own to be
    /// mangled from.
    owners: Vec<(Vec<Symbol>, u32)>,
}

/// One entry of [`Resolver::boundaries`].
struct Boundary {
    depth: usize,
    /// `None` for a `::` function; the locals it captures, for a closure.
    captures: Option<Vec<DefId>>,
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
                self.resolve_generics(&generics);
                self.resolve_node(ty);
                if let Some(t) = for_ty {
                    self.resolve_node(t);
                }
                // The self type is the `for` target of a trait impl, else the
                // head type of an inherent impl. `Self` and the member host
                // namespace both follow it (never the implemented trait).
                let self_ty = for_ty.unwrap_or(ty);
                let self_def = self.type_head_def(self_ty);
                // The member host is the target type's namespace when the target
                // names one, and otherwise the anonymous `<impl>` namespace
                // collection created for this block — the same choice collection
                // made, so the names it parked there are the names found here.
                let host = self_def
                    .or_else(|| self.def_of(id))
                    .unwrap_or_else(|| self.current_ns());
                // A structural target (`impl <T> []T`) names no type, so
                // collection bound `Self` in that anonymous namespace as an alias
                // for the target's type expression; prefer it over the enclosing
                // namespace, which `Self` has no business meaning.
                // The block's own namespace first: collection puts a `Self`
                // alias there for a target that is structural *or* carries
                // generic arguments, and that alias is the only form that
                // holds the arguments. The head def is the fallback, and it
                // is enough exactly when there are none.
                let own = self.def_of(id).and_then(|d| {
                    self.defs
                        .get(d)
                        .ns
                        .get_direct(&Symbol::new("Self"))
                        .map(|s| self.defs.resolve_alias(s))
                });
                let self_binding = own.or(self_def).or_else(|| {
                    self.defs
                        .get(host)
                        .ns
                        .get_direct(&Symbol::new("Self"))
                        .map(|d| self.defs.resolve_alias(d))
                });
                self.self_ty.push(self_binding.unwrap_or(host));
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
                self.boundaries.push(Boundary {
                    depth: self.scopes.len(),
                    captures: None,
                });
                self.push_scope();
                self.bind_generics(&generics);
                self.resolve_generics(&generics);
                for p in &params {
                    if self.self_ty.is_empty() && self.is_bare_self(*p) {
                        self.report(
                            *p,
                            "a bare `self` is a receiver and has no `Self` outside an `impl` \
                             or a `trait`; write its type",
                        );
                        self.bind_param(*p);
                        continue;
                    }
                    self.resolve_node(*p);
                    self.bind_param(*p);
                }
                if let Some(r) = ret {
                    if matches!(self.ast.node(r).kind, NodeKind::GenericTypeParam { .. }) {
                        self.bind_opaque(r, &generics);
                    } else {
                        self.resolve_node(r);
                    }
                }
                if let Some(b) = body {
                    self.resolve_node(b);
                }
                self.pop_scope();
                self.boundaries.pop();
            }
            NodeKind::Closure {
                captures,
                params,
                ret,
                body,
            } => self.resolve_closure(id, &captures, &params, ret, body),
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
                is_const,
                pattern,
                ty,
                value,
            } => {
                if let Some(t) = ty {
                    self.resolve_node(t);
                }
                self.resolve_node(value);
                // A `let` introduces assignable storage; a `const` does not
                // (§2.3). Everywhere else a binding is immutable unless the
                // pattern wrote `mut`.
                self.bind_pattern(pattern, !is_const);
            }
            // A `::` binding. At namespace scope, in a trait, and in an `impl`,
            // collection has already made a def for this node and put the name
            // in a namespace — there is nothing to introduce and the name is
            // found by path resolution, so only the RHS is walked.
            //
            // Inside a block there is no namespace to collect into, so the name
            // is introduced *here*, as a scoped binding like any local. Without
            // this the pattern reached the generic child walk and was resolved
            // as if it were a use of the name it declares, which reported
            // `cannot resolve name` on the declaration itself.
            NodeKind::ConstBind { pattern, rhs } => {
                let owner = matches!(self.ast.node(rhs).kind, NodeKind::FuncExpr { .. })
                    .then(|| self.owner_path(id, pattern));
                let pushed = owner.is_some();
                if let Some(path) = owner {
                    self.owners.push((path, 0));
                }
                self.resolve_node(rhs);
                if pushed {
                    self.owners.pop();
                }
                // An abstract associated type records what its own bounds came
                // to, on its def — the one place a later file can read them
                // from, having no access to this one's syntax tree. See
                // [`Def::assoc_bounds`].
                if let NodeKind::AssocType { bounds } = self.ast.node(rhs).kind.clone()
                    && let Some(def) = self.def_of(id)
                {
                    let traits = bounds
                        .iter()
                        .filter_map(|&b| self.bound_trait_def(b))
                        .collect();
                    self.defs.get_mut(def).assoc_bounds = Some(traits);
                }
                if self.def_of(id).is_some() {
                    return;
                }
                // `#static` is the one `::` form that names assignable storage
                // (§2.6): a program-lifetime region that outlives the call. The
                // directive sits on the enclosing `Decl`, which is why the walk
                // records it on the way down.
                //
                // A function-local one is a **global** that happens to be named
                // inside a block — C's `static` local. So it is introduced as a
                // `DefKind::Const` region rather than a `Local`, which is what
                // puts it in front of `lower_globals`; only its *visibility* is
                // the block's, and that comes from the scope frame either way.
                if self.decl_static {
                    self.introduce_static(pattern, id);
                    return;
                }
                // Same shape, immutable: a name bound to a compile-time value
                // rather than to a slot.
                if self.decl_comptime {
                    self.introduce_comptime(pattern, id);
                    return;
                }
                self.bind_pattern(pattern, false);
            }
            // A decorated item. The directives are carried by collection, but
            // `#static` changes what the binding underneath *is*, so it has to
            // be visible while that binding is resolved.
            NodeKind::Decl {
                attrs,
                directives,
                item,
            } => {
                // Attributes and directives are compiler vocabulary, never
                // program names, so neither is walked (see below).
                let is_static = directives.iter().any(|&d| self.is_static_directive(d));
                // `#comptime` on the binding an unrolled loop emits for its
                // variable. It has to be a *constant* and not a local, because
                // the point of unrolling is that the body may be typed with it
                // — `[i]u8` is a different type each iteration, and an array
                // length is a compile-time value or nothing.
                let is_comptime = directives.iter().any(|&d| self.is_directive(d, "comptime"));
                let outer = std::mem::replace(&mut self.decl_static, is_static);
                let outer_ct = std::mem::replace(&mut self.decl_comptime, is_comptime);
                self.resolve_node(item);
                self.decl_static = outer;
                self.decl_comptime = outer_ct;
                // An `@attribute` value written on the declaration is recorded
                // on its def, as one written on a field is — after the item, so
                // a block-local binding has its def. The compiler's own
                // attributes resolve to nothing and are left alone.
                for a in attrs {
                    self.resolve_node(a);
                }
            }
            // A `@Name(args)` a program wrote. The compiler's own attributes
            // (`@public`, `@link_name`) are vocabulary and resolve to nothing;
            // a name that *does* resolve is an `@attribute` struct, and its
            // value is recorded on the declaration it decorates so the
            // descriptor can carry it (§9's addition).
            NodeKind::Attribute { name, args } => self.resolve_attribute(id, &name, &args),
            NodeKind::For {
                pattern,
                iter,
                body,
            } => {
                self.resolve_node(iter);
                self.push_scope();
                self.bind_pattern(pattern, false);
                self.resolve_node(body);
                self.pop_scope();
            }
            NodeKind::MatchArm {
                pattern,
                guard,
                body,
            } => {
                self.push_scope();
                self.bind_pattern(pattern, false);
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
                self.bind_pattern(pattern, false);
                self.resolve_node(then);
                self.pop_scope();
                if let Some(e) = els {
                    self.resolve_node(e);
                }
            }

            // ===< type nodes with a position rule >===
            //
            // `dyn Trait` and `opaque` are unsized: each is a type only
            // *behind a pointer*, so `*dyn ToJson` and `*opaque` name one and a
            // bare `dyn ToJson` or `opaque` — as a variable's type, a field, a
            // parameter, a slice element — names nothing that has a size (§3.4,
            // §3.1). The permission is granted on the way down, by the pointer,
            // to exactly its own pointee.
            NodeKind::PtrType { inner, .. } => {
                self.unsized_ok.insert(inner);
                self.resolve_node(inner);
            }
            // `Handle :: distinct opaque` is how a library gets a nominal handle
            // of its own, so that its `*Handle` does not interchange with every
            // other `*opaque`. The `distinct` stands *over* a type rather than
            // holding a value of it, so naming a sizeless one here is not a
            // value position — `Handle` is then as sizeless as what it stands
            // over, and a use of `Handle` by value is refused by this same rule
            // when the layout is asked for.
            NodeKind::DistinctType { inner, .. } => {
                self.unsized_ok.insert(inner);
                self.resolve_node(inner);
            }
            NodeKind::DynType { inner } => {
                if !self.unsized_ok.contains(&id) {
                    self.report(
                        id,
                        "`dyn Trait` is unsized: use it behind a pointer, as `*dyn Trait`",
                    );
                }
                self.resolve_node(inner);
                // Only a trait has a vtable; `dyn i32` is not a trait object.
                if let Some(Resolution::Def(d)) = self.ast.meta::<Resolution>(inner) {
                    let d = self.defs.resolve_alias(d);
                    if self.defs.get(d).kind != DefKind::Trait {
                        let msg = format!(
                            "`{}` is not a trait, so `dyn` does not apply to it",
                            self.defs.canonical_string(d)
                        );
                        self.report(id, msg);
                    }
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
                // And the per-segment resolutions with it. A type position holds
                // the `TypePath`, not the `Path` inside it, so anything that has
                // to know what a name was read *through* — `T.Item`, where the
                // base decides whether this is a projection — would otherwise
                // find nothing here.
                if let Some(res) = self.ast.meta::<PathRes>(path) {
                    self.ast.set_meta(id, res);
                }
                // `opaque` obeys the same position rule as `dyn Trait`: no size,
                // so it is a type only behind a pointer. It is checked here
                // rather than beside the `dyn` because it arrives as an ordinary
                // name — a `DefKind::Primitive` — and not as a node kind of its
                // own.
                //
                // `distinct opaque` is how a library mints a nominal handle, and
                // it is not this: the `distinct` declaration names `opaque` as
                // the type it stands over, which is a use behind the
                // declaration, not a value position. That case is let through by
                // the same permission the pointer grants, extended by `Distinct`
                // on the way down.
                if !self.unsized_ok.contains(&id) && self.names_opaque(path) {
                    self.report(
                        id,
                        "`opaque` has no size: use it behind a pointer, as `*opaque`",
                    );
                }
                for a in generic_args {
                    self.resolve_node(a);
                }
            }
            NodeKind::FieldAccess { base, name } => {
                self.resolve_node(base);
                self.resolve_field(id, base, &name);
            }

            // A directive's arguments are drawn from a fixed compiler
            // vocabulary — `#align(16)`, `#lang("add")` — not from the
            // program's names, so they are read by `collect`, never resolved.
            NodeKind::Directive { .. } => {}

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
                // A member of an import that failed to load is as unknown as
                // the import, which was already reported.
                Resolution::Def(base) if self.is_external(*base) => Resolution::Def(*base),
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
            "self" => match self.lookup_local(name) {
                Some(d) => {
                    self.note_use(_id, name, d);
                    Resolution::Def(d)
                }
                None => Resolution::Error,
            },
            "Self" => self
                .self_ty
                .last()
                .copied()
                .map(Resolution::Def)
                .unwrap_or(Resolution::Error),
            _ => {
                let res = self
                    .lookup_unqualified(name)
                    .or_else(|| self.synth_primitive(name).map(Resolution::Def))
                    .unwrap_or(Resolution::Error);
                if let Resolution::Def(d) = res {
                    self.note_use(_id, name, d);
                }
                res
            }
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
        Some(self.defs.intern_primitive(self.builtins, name))
    }

    /// A `base.name` hop where `base` is a name path: if `base` resolved to a
    /// namespace-like def, resolve `name` as its member.
    fn resolve_field(&mut self, id: NodeId, base: NodeId, name: &Symbol) {
        let Some(Resolution::Def(base_def)) = self.ast.meta::<Resolution>(base) else {
            return; // runtime field access on a value — left for the type checker
        };
        // A type parameter's associated types were put in its namespace
        // (`introduce_bound_projections`), and `Item :: I.Item` in an impl reads
        // one in expression position — a `::` right-hand side is parsed as an
        // expression. Anything else through a parameter is inference's to type.
        let base_def = self.defs.resolve_alias(base_def);
        if self.defs.get(base_def).kind == DefKind::TypeParam {
            if let Some(&d) = self.defs.get(base_def).ns.members.get(name) {
                self.ast.set_meta(id, Resolution::Def(d));
            }
            return;
        }
        if !self
            .defs
            .get(self.defs.resolve_alias(base_def))
            .kind
            .is_namespace_like()
        {
            return;
        }
        if self.is_external(base_def) {
            self.ast.set_meta(id, Resolution::Def(base_def));
            return;
        }
        match self.resolve_member(base_def, name) {
            Some(d) => {
                self.ast.set_meta(id, Resolution::Def(d));
            }
            None => {
                // Marked, so inference knows the access was already reported
                // rather than reading `namespace.member` as a field of a value.
                self.ast.set_meta(id, Resolution::Error);
                self.report(
                    id,
                    format!(
                        "`{name}` is not a public member of `{}`",
                        self.defs
                            .canonical_string(self.defs.resolve_alias(base_def))
                    ),
                )
            }
        }
    }

    // ===< scope search >===

    /// Resolve one `@Name(args)` and, when `Name` is an `@attribute` struct,
    /// record its value on the definition the attribute is written on.
    ///
    /// A name that resolves to nothing is left alone: `@public` is not a
    /// program name, and every attribute the compiler reads is spelled like
    /// one. A name that resolves to something that is *not* an `@attribute` is
    /// the error — the program meant a struct it may not use this way.
    fn resolve_attribute(&mut self, id: NodeId, name: &Symbol, args: &[NodeId]) {
        let Some(Resolution::Def(def)) = self.lookup_unqualified(name) else {
            return;
        };
        let def = self.defs.resolve_alias(def);
        if !self.defs.get(def).attribute {
            let msg = format!(
                "`{name}` is not an `@attribute`; declare it `@attribute {name} :: struct {{ ... }}`"
            );
            self.report(id, msg);
            return;
        }
        // The declaration this attribute sits on. Collection stamped the def
        // onto the binding, and an attribute is a child of it.
        let Some(owner) = self.attr_owner(id) else {
            return;
        };
        let declared = self.defs.get(def).ns.members.len();
        if args.len() != declared {
            let msg = format!(
                "`@{name}` takes {declared} argument(s), not {}: an attribute writes every member",
                args.len()
            );
            self.report(id, msg);
            return;
        }
        let mut values = Vec::new();
        for &a in args {
            let (arg_name, value) = match &self.ast.node(a).kind {
                NodeKind::Arg { name, value } => (name.clone(), *value),
                _ => (None, a),
            };
            if let Some(n) = &arg_name {
                if !self.defs.get(def).ns.members.contains_key(n) {
                    let msg = format!("`{name}` has no member `{n}`");
                    self.report(a, msg);
                    return;
                }
            }
            let Some(v) = attr_literal(self.ast, value) else {
                self.report(a, "an attribute's arguments are literals");
                return;
            };
            values.push((arg_name, v));
        }
        self.defs
            .get_mut(owner)
            .attrs
            .push(AttrValue { def, args: values });
    }

    /// The def an attribute node decorates: the field it sits on, or the
    /// binding the enclosing `Decl` introduces.
    fn attr_owner(&self, attr: NodeId) -> Option<DefId> {
        let mut stack = vec![self.ast.root()?];
        while let Some(n) = stack.pop() {
            let kids = self.ast.children(n);
            if kids.contains(&attr) {
                if let Some(DefMeta(d)) = self.ast.meta::<DefMeta>(n) {
                    return Some(d);
                }
                // A `Decl` carries the attributes and its `item` carries the def.
                if let NodeKind::Decl { item, .. } = &self.ast.node(n).kind {
                    if let Some(DefMeta(d)) = self.ast.meta::<DefMeta>(*item) {
                        return Some(d);
                    }
                }
                return None;
            }
            stack.extend(kids);
        }
        None
    }

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
    /// Whether `def` stands in for an import that could not be loaded.
    fn is_external(&self, def: DefId) -> bool {
        self.defs.get(self.defs.resolve_alias(def)).kind == DefKind::External
    }

    fn resolve_member(&self, base: DefId, name: &Symbol) -> Option<DefId> {
        let base = self.defs.resolve_alias(base);
        let same_file = self.defs.get(base).file == Some(self.file);
        // What a namespace imported without `@public` is its own business:
        // another file reaches its members, never its imports.
        let ns = &self.defs.get(base).ns;
        let d = if same_file {
            ns.get_direct(name)?
        } else {
            *ns.members.get(name)?
        };
        let d = self.defs.resolve_alias(d);
        if same_file || self.reaches(d) {
            Some(d)
        } else {
            None
        }
    }

    /// Whether `d` is exported far enough to be named from the file being
    /// resolved: always for `@public`, and within its own package for
    /// `@public(package)`.
    fn reaches(&self, d: DefId) -> bool {
        let home = self.defs.get(d).file.and_then(|f| self.pkg_of.get(&f));
        let at = self.pkg_of.get(&self.file);
        self.defs
            .get(d)
            .vis
            .reaches(home.map(String::as_str), at.map(String::as_str))
    }

    /// Resolve a generic list's bounds, one parameter at a time, giving each
    /// its associated types before the next is read — so a later bound may name
    /// an earlier parameter's: `<I: Iterator, F: Func(I.Item) -> B>`.
    fn resolve_generics(&mut self, generics: &[NodeId]) {
        for &g in generics {
            self.resolve_node(g);
            self.introduce_bound_projections(&[g]);
        }
    }

    /// Give each generic type parameter the associated types its bounds
    /// declare, as type parameters of their own (§5.4).
    ///
    /// `<T: Holder>` makes `T.Item` writable wherever `T` is. A type parameter
    /// has no namespace to look a member up in — it is not a type yet — so the
    /// name is put *there*: `Item` becomes a member of `T`'s own namespace, and
    /// the ordinary member hop finds it with no special case.
    ///
    /// What it becomes is the point. `T.Item` is not known until a call site
    /// says what `T` is, and something whose value one call site fixes is a
    /// **type parameter**, so that is what is synthesized — one per
    /// `(parameter, associated type)` pair, because two parameters bounded by
    /// the same trait project two different types. The equation that solves it
    /// rides along in [`Def::projection`], and the call site registers it as an
    /// ordinary projection obligation once it has a `T` to project through.
    ///
    /// Runs after the constraints are resolved, which is why it is not part of
    /// `bind_generics` — a bound cannot be read until its trait name has been.
    fn introduce_bound_projections(&mut self, generics: &[NodeId]) {
        self.record_param_bounds(generics);
        for &g in generics {
            let NodeKind::GenericTypeParam {
                name,
                constraint: Some(constraint),
                ..
            } = self.ast.node(g).kind.clone()
            else {
                continue;
            };
            let Some(param) = self.def_of(g) else {
                continue;
            };
            let bounds = match self.ast.node(constraint).kind.clone() {
                NodeKind::Bounds { bounds } => bounds,
                _ => vec![constraint],
            };
            let traits: Vec<(DefId, Option<NodeId>)> = bounds
                .iter()
                .filter_map(|&b| self.bound_trait_def(b).map(|t| (t, Some(b))))
                .collect();
            self.project_bounds(param, name.as_str(), &traits, g, 0);
        }
    }

    /// Record what each generic parameter's bounds resolved to, on the
    /// parameter's own def.
    ///
    /// The bounds go **on the def**, for the same reason an abstract associated
    /// type's do (see [`Def::assoc_bounds`]): a later compilation reading this
    /// parameter out of a library has no syntax tree to read the constraint
    /// from, and impl selection has to know it — `impl <T: Float> Display for T`
    /// applies to a float and to nothing else, and a bound nobody recorded reads
    /// as no bound at all, which is every type in the program.
    ///
    /// Every declaration that takes parameters records them, not functions
    /// alone. An **impl**'s are the ones monomorphization asks about: a blanket
    /// impl's body writes `Self.BITS`, which is the *trait's* declaration, and
    /// the bound is the only thing that says which impl supplies the value.
    fn record_param_bounds(&mut self, generics: &[NodeId]) {
        for &g in generics {
            let NodeKind::GenericTypeParam {
                constraint: Some(constraint),
                ..
            } = self.ast.node(g).kind.clone()
            else {
                continue;
            };
            let Some(param) = self.def_of(g) else {
                continue;
            };
            let bounds = match self.ast.node(constraint).kind.clone() {
                NodeKind::Bounds { bounds } => bounds,
                _ => vec![constraint],
            };
            let traits: Vec<DefId> = bounds
                .iter()
                .filter_map(|&b| self.bound_trait_def(b))
                .collect();
            self.defs.get_mut(param).param_bounds = Some(traits);
        }
    }

    /// Add one parameter per associated type of `traits` to `param`'s namespace,
    /// then do the same to each one it adds.
    ///
    /// The recursion is what makes `T.Item.Item` work: an associated type
    /// declared `Item :: type: Holder` is itself a `Holder`, so it has an `Item`
    /// of its own, and nothing about the second hop differs from the first. The
    /// depth cap is for a trait whose associated type is bounded by the trait
    /// itself — `Item :: type: Holder` inside `Holder` is a legal declaration
    /// and an infinite family of names, so the parameters are minted to a fixed
    /// depth rather than forever. A projection past it is an unresolved name,
    /// which is a diagnostic rather than a hang.
    fn project_bounds(
        &mut self,
        param: DefId,
        path: &str,
        traits: &[(DefId, Option<NodeId>)],
        at: NodeId,
        depth: u32,
    ) {
        if depth > 4 {
            return;
        }
        for &(t, bound) in traits {
            if self.defs.get(t).kind != DefKind::Trait {
                continue;
            }
            let members: Vec<(Symbol, DefId)> = self
                .defs
                .get(t)
                .ns
                .members
                .iter()
                .map(|(n, &d)| (n.clone(), d))
                .collect();
            for (assoc, member) in members {
                let member = self.defs.resolve_alias(member);
                // Only an **abstract** associated type is a parameter: a trait
                // may also declare ordinary aliases, and those already have an
                // answer that needs none.
                let Some(inner) = self.abstract_assoc_bounds(member) else {
                    continue;
                };
                // A name two bounds both declare is bound by the first; the
                // second would be a different type under the same name, and that
                // is a question §5.4 does not answer yet.
                if self.defs.get(param).ns.members.contains_key(&assoc) {
                    continue;
                }
                let name = format!("{path}.{assoc}");
                let mut canonical = self.defs.get(param).canonical.clone();
                canonical.push(assoc.clone());
                let span = self.ast.node(at).span;
                let synth = self.defs.alloc(
                    Symbol::new(&name),
                    DefKind::TypeParam,
                    Visibility::Private,
                    Some(param),
                    Some(self.file),
                    Some(span),
                    Some(at),
                    canonical,
                );
                self.defs.get_mut(synth).projection = Some(super::def::Projection {
                    base: param,
                    trait_def: t,
                    assoc: assoc.clone(),
                    pinned: bound.and_then(|b| self.assoc_binding(b, &assoc)),
                });
                self.defs.get_mut(param).ns.members.insert(assoc, synth);
                let inner: Vec<(DefId, Option<NodeId>)> =
                    inner.into_iter().map(|t| (t, None)).collect();
                self.project_bounds(synth, &name, &inner, at, depth + 1);
            }
        }
    }

    /// What bounds an abstract associated type, or `None` when `member` is not
    /// one.
    ///
    /// Recorded on the def when its trait is resolved; a trait declared
    /// **later in this file** has not been yet, so its declaration is read
    /// directly instead — `<C: FromIterator>` written above `FromIterator` must
    /// still give `C` its `Item`. Its own bounds' names may not be resolved at
    /// that point, and whatever they do not yet name is left out.
    fn abstract_assoc_bounds(&self, member: DefId) -> Option<Vec<DefId>> {
        if let Some(b) = self.defs.get(member).assoc_bounds.clone() {
            return Some(b);
        }
        let d = self.defs.get(member);
        if d.file != Some(self.file) {
            return None;
        }
        let NodeKind::ConstBind { rhs, .. } = self.ast.node(d.node?).kind.clone() else {
            return None;
        };
        let NodeKind::AssocType { bounds } = self.ast.node(rhs).kind.clone() else {
            return None;
        };
        Some(bounds.iter().filter_map(|&b| self.bound_trait_def(b)).collect())
    }

    /// The type node a bound pinned an associated type to: the `i32` of
    /// `Holder.<Item = i32>`.
    fn assoc_binding(&self, bound: NodeId, assoc: &Symbol) -> Option<NodeId> {
        // A bound is written either way round depending on where it stands:
        // `Holder.<Item = i32>` in a generic list parses as a `TypePath` with
        // arguments, and the same thing in expression position as a postfix
        // `GenericApply`.
        let args = match self.ast.node(bound).kind.clone() {
            NodeKind::TypePath { generic_args, .. } => generic_args,
            NodeKind::GenericApply { args, .. } => args,
            _ => return None,
        };
        args.iter().find_map(|&a| match &self.ast.node(a).kind {
            NodeKind::AssocBinding { name, ty } if name == assoc => Some(*ty),
            _ => None,
        })
    }

    /// The trait a bound names, following `Trait.<args>` to its head.
    fn bound_trait_def(&self, bound: NodeId) -> Option<DefId> {
        let head = match self.ast.node(bound).kind.clone() {
            NodeKind::GenericApply { base, .. } => base,
            _ => bound,
        };
        let Resolution::Def(t) = self.ast.meta::<Resolution>(head)? else {
            return None;
        };
        let t = self.defs.resolve_alias(t);
        (self.defs.get(t).kind == DefKind::Trait).then_some(t)
    }

    fn public_member(&self, base: DefId, name: &Symbol) -> Option<DefId> {
        let base = self.defs.resolve_alias(base);
        let d = self
            .defs
            .resolve_alias(*self.defs.get(base).ns.members.get(name)?);
        self.reaches(d).then_some(d)
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

    /// Whether `param` is a `self` written without a type, whose `Self` the
    /// parser supplied at the name's own span.
    fn is_bare_self(&self, param: NodeId) -> bool {
        let node = self.ast.node(param);
        match &node.kind {
            NodeKind::Param {
                name, ty: Some(t), ..
            } => name.as_str() == "self" && self.ast.node(*t).span == node.span,
            _ => false,
        }
    }

    /// A closure (§5.5): its defs, its capture list, and what it shares.
    ///
    /// The capture list is resolved **outside** the closure, since it copies
    /// what those names hold where the closure is made; each name is then bound
    /// again inside, to the copy. Everything else the body names from outside is
    /// shared, and is collected as it is resolved (see [`Resolver::note_use`]).
    fn resolve_closure(
        &mut self,
        id: NodeId,
        captures: &[NodeId],
        params: &[NodeId],
        ret: Option<NodeId>,
        body: NodeId,
    ) {
        for &c in captures {
            let NodeKind::Capture { name } = self.ast.node(c).kind.clone() else {
                continue;
            };
            let res = self.resolve_root(c, &name);
            match res {
                Resolution::Def(d)
                    if matches!(self.defs.get(d).kind, DefKind::Local | DefKind::Param) =>
                {
                    self.ast.set_meta(c, res);
                }
                Resolution::Error => {
                    self.report(c, format!("cannot resolve name `{name}`"));
                }
                _ => self.report(
                    c,
                    format!(
                        "`{name}` is not a local, so there is nothing to copy into the closure"
                    ),
                ),
            }
        }
        let defs = self.closure_defs(id);
        self.ast.set_meta(id, defs);
        let (path, _) = self.owners.last().cloned().unwrap_or_default();
        let mut own = path;
        own.push(self.defs.get(defs.ty).name.clone());
        self.owners.push((own, 0));
        self.boundaries.push(Boundary {
            depth: self.scopes.len(),
            captures: Some(Vec::new()),
        });
        self.push_scope();
        for &c in captures {
            if let NodeKind::Capture { name } = self.ast.node(c).kind.clone() {
                self.introduce(name, DefKind::Local, c);
            }
        }
        for &p in params {
            self.resolve_node(p);
            self.bind_param(p);
        }
        if let Some(r) = ret {
            self.resolve_node(r);
        }
        self.resolve_node(body);
        self.pop_scope();
        let shared = self
            .boundaries
            .pop()
            .and_then(|b| b.captures)
            .unwrap_or_default();
        self.owners.pop();
        self.ast.set_meta(id, crate::sema::Captures(shared));
    }

    /// `-> impl Bounds` (§5.4): a type parameter the parser put in the return
    /// slot, bound like one declared in the generic list and marked as the
    /// body's to decide. It is generic over the function's own type
    /// parameters, which is what [`crate::sema::OpaqueArgs`] records: the type
    /// the body returns may mention them, and a caller's instantiation says
    /// what they are.
    fn bind_opaque(&mut self, ret: NodeId, generics: &[NodeId]) {
        self.bind_generics(&[ret]);
        self.resolve_node(ret);
        self.introduce_bound_projections(&[ret]);
        let Some(def) = self.def_of(ret) else { return };
        self.defs.get_mut(def).opaque = true;
        let args = generics
            .iter()
            .filter(|&&g| matches!(self.ast.node(g).kind, NodeKind::GenericTypeParam { .. }))
            .filter_map(|&g| self.def_of(g))
            .collect();
        self.ast.set_meta(ret, crate::sema::OpaqueArgs(args));
    }

    /// Allocate a closure's three defs: its type, named after its place in the
    /// function that writes it; its `call`; and `call`'s first parameter.
    fn closure_defs(&mut self, id: NodeId) -> crate::sema::ClosureDefs {
        let span = self.ast.node(id).span;
        let scope = self.current_ns();
        let (mut path, n) = match self.owners.last_mut() {
            Some((path, n)) => {
                *n += 1;
                (path.clone(), *n - 1)
            }
            None => (Vec::new(), 0),
        };
        let name = Symbol::new(&format!("{{closure#{n}}}"));
        path.push(name.clone());
        let ty = self.defs.alloc(
            name,
            DefKind::Closure,
            Visibility::Private,
            Some(scope),
            Some(self.file),
            Some(span),
            Some(id),
            path.clone(),
        );
        let call_name = Symbol::new("call");
        let mut call_path = path;
        call_path.push(call_name.clone());
        let call = self.defs.alloc(
            call_name.clone(),
            DefKind::Func,
            Visibility::Private,
            Some(ty),
            Some(self.file),
            Some(span),
            None,
            call_path,
        );
        self.defs.get_mut(ty).ns.members.insert(call_name, call);
        let this_name = Symbol::new("self");
        let this = self.defs.alloc(
            this_name.clone(),
            DefKind::Param,
            Visibility::Private,
            Some(scope),
            Some(self.file),
            Some(span),
            None,
            vec![this_name],
        );
        crate::sema::ClosureDefs { ty, call, this }
    }

    /// The canonical path of the function a `::` binding defines — the one a
    /// closure inside it is named under.
    fn owner_path(&self, bind: NodeId, pattern: NodeId) -> Vec<Symbol> {
        if let Some(d) = self.def_of(bind) {
            return self.defs.get(d).canonical.clone();
        }
        let mut path = self.defs.get(self.current_ns()).canonical.clone();
        if let NodeKind::BindingPat { name, .. } = &self.ast.node(pattern).kind {
            path.push(name.clone());
        }
        path
    }

    /// Record a use of `def` at `at`, if it reaches across a function or a
    /// closure (§5.5): a closure it crosses captures it, and a `::` function it
    /// crosses cannot see it.
    fn note_use(&mut self, at: NodeId, name: &Symbol, def: DefId) {
        if !matches!(self.defs.get(def).kind, DefKind::Local | DefKind::Param) {
            return;
        }
        let Some(frame) = self.scopes.iter().rposition(|f| f.get(name) == Some(&def)) else {
            return;
        };
        let mut crossed_item = false;
        for b in self.boundaries.iter_mut().rev() {
            if frame >= b.depth {
                break;
            }
            match &mut b.captures {
                Some(caps) => {
                    if !caps.contains(&def) {
                        caps.push(def);
                    }
                }
                None => {
                    crossed_item = true;
                    break;
                }
            }
        }
        if crossed_item {
            self.report(
                at,
                format!(
                    "`{name}` belongs to the function around this one, and a `::` function \
                     cannot capture it; bind a closure instead: `const f := {{ x in ... }}`"
                ),
            );
        }
    }

    fn bind_param(&mut self, param: NodeId) {
        if let NodeKind::Param { name, .. } = &self.ast.node(param).kind {
            let name = name.clone();
            self.introduce(name, DefKind::Param, param);
        }
    }

    /// Bind every name a pattern introduces into the current scope frame.
    ///
    /// `mutable` is the binding site's default: `true` under a `let`, `false`
    /// under a `const`, a `match` arm or a `for`. A pattern binding written
    /// `mut` is mutable regardless (§7.1's `[ 'mut' ] identifier`), so the two
    /// combine rather than one overriding the other.
    fn bind_pattern(&mut self, pattern: NodeId, mutable: bool) {
        match self.ast.node(pattern).kind.clone() {
            NodeKind::BindingPat {
                name,
                mutable: wrote_mut,
            } => {
                self.introduce_binding(name, DefKind::Local, pattern, mutable || wrote_mut);
            }
            NodeKind::AtPat {
                name,
                pattern: inner,
            } => {
                self.introduce_binding(name, DefKind::Local, pattern, mutable);
                self.bind_pattern(inner, mutable);
            }
            NodeKind::TuplePat { elems }
            | NodeKind::OrPat {
                alternatives: elems,
            } => {
                for e in elems {
                    self.bind_pattern(e, mutable);
                }
            }
            NodeKind::RefPat { pattern: inner } => self.bind_pattern(inner, mutable),
            NodeKind::StructPat { path, fields, .. } => {
                if let Some(p) = path {
                    self.resolve_node(p);
                }
                for f in fields {
                    self.bind_field_pat(f, mutable);
                }
            }
            NodeKind::TupleStructPat { path, elems, .. } => {
                self.resolve_node(path);
                for e in elems {
                    self.bind_pattern(e, mutable);
                }
            }
            NodeKind::VariantPat { args, .. } => {
                match args {
                    // Tuple payload: each child is a sub-pattern.
                    crate::parser::ast::VariantPatArgs::Tuple(elems) => {
                        for e in elems {
                            self.bind_pattern(e, mutable);
                        }
                    }
                    // Record payload: each child is a `FieldPat` (`{ radius }` /
                    // `{ radius: p }`), bound like a struct pattern's fields.
                    crate::parser::ast::VariantPatArgs::Record { fields, .. } => {
                        for f in fields {
                            self.bind_field_pat(f, mutable);
                        }
                    }
                    crate::parser::ast::VariantPatArgs::None => {}
                }
            }
            NodeKind::SlicePat { elems, rest } => {
                for e in elems {
                    self.bind_pattern(e, mutable);
                }
                if let Some(SliceRest {
                    name: Some(name), ..
                }) = rest
                {
                    self.introduce_binding(name, DefKind::Local, pattern, mutable);
                }
            }
            // Literals, ranges, wildcards, globs bind nothing.
            _ => {}
        }
    }

    fn bind_field_pat(&mut self, field: NodeId, mutable: bool) {
        if let NodeKind::FieldPat { name, pattern, .. } = self.ast.node(field).kind.clone() {
            match pattern {
                Some(p) => self.bind_pattern(p, mutable),
                None => self.introduce_binding(name, DefKind::Local, field, mutable),
            }
        }
    }

    /// Allocate a local-ish def, add it to the current scope frame, and stamp
    /// [`DefMeta`] on its introducing node.
    fn introduce(&mut self, name: Symbol, kind: DefKind, node: NodeId) {
        self.introduce_binding(name, kind, node, false);
    }

    /// [`Resolver::introduce`], recording whether the binding may be assigned to.
    /// Introduce a function-local `#static` as a program-lifetime region.
    ///
    /// The def points at the **binding**, not the pattern, because that is where
    /// its type and initializer are and what every consumer of a global reads.
    fn introduce_static(&mut self, pattern: NodeId, bind: NodeId) {
        let NodeKind::BindingPat { name, .. } = self.ast.node(pattern).kind.clone() else {
            // A destructuring `#static` names no single region; the binding form
            // is reported elsewhere, and treating it as ordinary locals here
            // keeps the rest of the body resolvable.
            self.bind_pattern(pattern, true);
            return;
        };
        self.introduce_binding(name, DefKind::Const, pattern, true);
        if let Some(def) = self.def_of(pattern) {
            self.defs.get_mut(def).node = Some(bind);
        }
    }

    /// A block-local `::` that names a **compile-time value** — the variable an
    /// unrolled `#comptime for` binds. It is a [`DefKind::Const`] for the same
    /// reason `#static` is one: what the name denotes is not a slot in the
    /// frame, and every stage that asks "is this a constant?" asks the def.
    fn introduce_comptime(&mut self, pattern: NodeId, bind: NodeId) {
        let NodeKind::BindingPat { name, .. } = self.ast.node(pattern).kind.clone() else {
            self.bind_pattern(pattern, false);
            return;
        };
        self.introduce_binding(name, DefKind::Const, pattern, false);
        if let Some(def) = self.def_of(pattern) {
            self.defs.get_mut(def).node = Some(bind);
        }
    }

    fn introduce_binding(&mut self, name: Symbol, kind: DefKind, node: NodeId, mutable: bool) {
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
        self.defs.get_mut(id).mutable = mutable;
        self.ast.set_meta(node, DefMeta(id));
        if let Some(frame) = self.scopes.last_mut() {
            frame.insert(name, id);
        }
    }

    // ===< helpers >===

    fn def_of(&self, node: NodeId) -> Option<DefId> {
        self.ast.meta::<DefMeta>(node).map(|m| m.0)
    }

    fn is_static_directive(&self, node: NodeId) -> bool {
        self.is_directive(node, "static")
    }

    fn is_directive(&self, node: NodeId, want: &str) -> bool {
        matches!(&self.ast.node(node).kind,
            NodeKind::Directive { name, .. } if name.as_str() == want)
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
            Resolution::Def(d) => {
                let d = self.defs.resolve_alias(d);
                // `int` / `uint` name a family, not a type, so an `impl
                // <const N: usize> int.<N>` has no named head — it is structural,
                // like `impl <T> []T`. Saying so here is what sends `Self` to the
                // alias collection bound to the whole `int.<N>` expression
                // instead of to the bare constructor, which carries no width.
                (!self.defs.get(d).is_int_family()).then_some(d)
            }
            _ => None,
        }
    }

    /// Whether `path` resolved to the `opaque` primitive.
    ///
    /// Keyed on the definition rather than on the spelling: `opaque` is a
    /// builtin and cannot be shadowed, but `c.void` is an alias for it and an
    /// alias resolves to the same def, so the position rule reaches the name a
    /// C programmer actually writes.
    fn names_opaque(&self, path: NodeId) -> bool {
        let Some(Resolution::Def(d)) = self.ast.meta::<Resolution>(path) else {
            return false;
        };
        let d = self.defs.resolve_alias(d);
        let def = self.defs.get(d);
        def.kind == DefKind::Primitive && def.name.as_str() == "opaque"
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

/// One attribute argument, as a compile-time value.
///
/// The vocabulary is the literals, deliberately: an attribute is data written
/// on a declaration, and a declaration is not a place an expression runs. The
/// same restriction directives have had since §9.
fn attr_literal(ast: &Ast, node: NodeId) -> Option<ConstValue> {
    use crate::parser::ast::Lit;
    match &ast.node(node).kind {
        NodeKind::Lit(Lit::Str(s)) => Some(ConstValue::Str(s.clone())),
        NodeKind::Lit(Lit::Int(n)) => Some(ConstValue::Int(n.clone())),
        NodeKind::Lit(Lit::Float(f)) => Some(ConstValue::Float(*f)),
        NodeKind::Lit(Lit::Bool(b)) => Some(ConstValue::Bool(*b)),
        NodeKind::Lit(Lit::Char(c)) => Some(ConstValue::Char(*c)),
        NodeKind::Lit(Lit::Bytes(b)) => Some(ConstValue::Bytes(b.clone())),
        _ => None,
    }
}
