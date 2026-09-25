//! Definition collection — the pass that populates the [`DefTable`] with every
//! namespace-level definition, before any name is resolved.
//!
//! It walks a file's items (recursing into inline `namespace`s and `impl`
//! blocks) and, for each binding, allocates a [`Def`](super::def::Def) in the
//! enclosing scope's [`Namespace`]. It reads `@public` for visibility (§4.4) and
//! records each `#lang("tag")` in the [`LangItems`] registry (§9.3). `import`
//! bindings are not resolved here — they are recorded as [`RawImport`]s for the
//! import stage.
//!
//! Function-local names (params, generics, block locals) are **not** collected
//! here; they are handled by the resolver's scope stack, where order and
//! shadowing matter.

use crate::common::diagnostic::Diagnostic;
use crate::common::source::FileId;
use crate::common::source::FileSpan;
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, ImportPath, NodeId, NodeKind, StructKind};

use super::DefMeta;
use super::def::{DefId, DefKind, DefTable, Directive, DirectiveArg, LangItems, Visibility};
use super::imports::{RawImport, RawTarget};

/// Collect every namespace-level definition of `file` into `file_ns`, returning
/// the file's (unloaded) import bindings.
pub fn collect_file(
    defs: &mut DefTable,
    lang: &mut LangItems,
    diags: &mut Vec<Diagnostic>,
    ast: &Ast,
    file: FileId,
    file_ns: DefId,
    in_core: bool,
) -> Vec<RawImport> {
    let mut cx = Collector {
        defs,
        lang,
        diags,
        ast,
        file,
        in_core,
        imports: Vec::new(),
        in_impl: false,
        pending: Vec::new(),
        pending_attribute: false,
        anon_impls: 0,
    };
    if let Some(root) = ast.root() {
        if let NodeKind::File { items } = &ast.node(root).kind {
            let items = items.clone();
            cx.collect_items(&items, file_ns);
        }
    }
    cx.imports
}

struct Collector<'a> {
    defs: &'a mut DefTable,
    lang: &'a mut LangItems,
    diags: &'a mut Vec<Diagnostic>,
    ast: &'a Ast,
    file: FileId,
    /// Whether this file belongs to the `core` package. A `#lang` claim from
    /// `core` is a **default** the program may answer over (see [`LangItems`]).
    in_core: bool,
    imports: Vec<RawImport>,
    /// Whether collection is inside an `impl` body, where a name may repeat
    /// across impls that share a host namespace.
    in_impl: bool,
    /// Directives written on the enclosing [`NodeKind::Decl`], waiting for the
    /// binding inside it to claim them.
    pending: Vec<Directive>,
    /// Whether the binding being collected was written `@attribute`.
    pending_attribute: bool,
    /// How many anonymous `impl` namespaces this file has needed so far; the
    /// count names them apart (`<impl 1>`, `<impl 2>`, …) so two impls on
    /// structural targets stay distinguishable in a def dump.
    anon_impls: usize,
}

/// One argument of `@public`: a level for the item, or for its members.
#[derive(Clone, Copy)]
enum VisArg {
    Item(Visibility),
    Fields(Visibility),
}

/// Visibility gathered from an item's attributes.
#[derive(Clone, Copy)]
struct Vis {
    /// What the item itself is: `@public`, `@public(package)`, or neither.
    level: Visibility,
    /// What its **members** are, when it is an aggregate and said so:
    /// `@public(all)` is `fields: public`, and `@public(fields: package)` says
    /// it in full. `None` leaves them private, which is the default (§3.1).
    fields: Option<Visibility>,
}

impl Vis {
    const PRIVATE: Vis = Vis {
        level: Visibility::Private,
        fields: None,
    };

    fn level(self) -> Visibility {
        self.level
    }

    /// What a member of this item is, unless it says otherwise itself.
    fn member_level(self) -> Visibility {
        self.fields.unwrap_or(Visibility::Private)
    }
}

impl Collector<'_> {
    fn collect_items(&mut self, items: &[NodeId], scope: DefId) {
        // Two passes so `impl` targets can be types declared later in the block.
        let mut impls = Vec::new();
        for &item in items {
            if matches!(self.ast.node(item).kind, NodeKind::ImplBlock { .. }) {
                impls.push(item);
            } else {
                self.collect_item(item, scope);
            }
        }
        for item in impls {
            self.collect_impl(item, scope);
        }
    }

    fn collect_item(&mut self, node: NodeId, scope: DefId) {
        match self.ast.node(node).kind.clone() {
            NodeKind::Decl {
                attrs,
                directives,
                item,
            } => {
                let vis = self.visibility(&attrs);
                // `@attribute` marks a struct as one a program may *write* on a
                // declaration. It is recorded here rather than read where it is
                // used because the use is in another file as often as not, and
                // a def is what crosses that boundary.
                let is_attr = self.has_attr(&attrs, "attribute");
                let was_attr = std::mem::replace(&mut self.pending_attribute, is_attr);
                self.check_namespace_binding(item, &directives);
                // Directives written *before* the binding (`#inline\nf :: func …`)
                // and directives written on the RHS form (`f :: #inline func …`)
                // mean the same thing, so the binding collects both.
                let mut here = self.directives(&directives);
                // `@link_name("x")` is an **attribute** the compiler acts on
                // (§9), and what acts on it is the mangler, which reads the
                // def's directives. So it is recorded as one: it says the same
                // kind of thing they say — how this declaration is to be
                // emitted — and putting it anywhere else would mean a second
                // lookup for one attribute.
                here.extend(self.link_name(&attrs));
                here.extend(self.no_mangle(item, &attrs));
                here.extend(self.test(&attrs));
                let outer = std::mem::replace(&mut self.pending, here);
                self.collect_binding(item, vis, scope);
                self.pending = outer;
                self.pending_attribute = was_attr;
            }
            _ => {
                self.check_namespace_binding(node, &[]);
                self.collect_binding(node, Vis::PRIVATE, scope)
            }
        }
    }

    /// A `let` / `const` at namespace scope is always an error (§13.1).
    ///
    /// There is nothing for it to live in: `let` binds a name to a stack slot
    /// that belongs to an enclosing call, and at namespace scope there is no
    /// call. Every namespace binding is `::` — immutable by default, and a
    /// program-lifetime **mutable region** when `#static` decorates it
    /// (`#static count: uint := 0`, §2.6).
    fn check_namespace_binding(&mut self, item: NodeId, _directives: &[NodeId]) {
        if !matches!(self.ast.node(item).kind, NodeKind::LocalDecl { .. }) {
            return;
        }
        self.report(
            item,
            "a `let` / `const` has no meaning at namespace scope; use `::` for a constant, \
             or `#static name: T := value` for a mutable region",
        );
    }

    /// Collect a bare binding node (a `ConstBind`, `LocalDecl`, comptime item).
    fn collect_binding(&mut self, node: NodeId, vis: Vis, scope: DefId) {
        match self.ast.node(node).kind.clone() {
            NodeKind::ConstBind { pattern, rhs } => {
                if let NodeKind::Import { path } = &self.ast.node(rhs).kind {
                    self.record_import(pattern, path.clone(), vis, scope, node);
                } else {
                    self.collect_const_bind(node, pattern, rhs, vis, scope);
                }
            }
            NodeKind::LocalDecl { pattern, .. } => {
                if let Some(name) = self.binding_name(pattern) {
                    self.define(name, DefKind::Const, vis.level(), scope, node, None);
                }
            }
            // Comptime items ($assert, etc.) define no name.
            _ => {}
        }
    }

    fn collect_const_bind(
        &mut self,
        bind: NodeId,
        pattern: NodeId,
        rhs: NodeId,
        vis: Vis,
        scope: DefId,
    ) {
        let Some(name) = self.binding_name(pattern) else {
            return;
        };
        let rhs_kind = self.ast.node(rhs).kind.clone();
        let kind = def_kind_of(&rhs_kind);
        let lang = self.lang_tag(&rhs_kind);
        let def = self.define(name, kind, vis.level(), scope, bind, lang);
        // §9: directives are carried, not acted on, by this stage. Recording
        // them on the def is what lets the IR reach them — every IR node names a
        // `DefId`, so a `#soa` on a struct or an `#inline` on a function is
        // available wherever that definition turns up, without the later stages
        // re-walking the AST.
        if std::mem::take(&mut self.pending_attribute) {
            if kind == DefKind::Struct {
                self.defs.get_mut(def).attribute = true;
            } else {
                self.report(bind, "`@attribute` may only be written on a `struct`");
            }
        }
        let mut directives = std::mem::take(&mut self.pending);
        directives.extend(self.rhs_directives(&rhs_kind));
        // A `#static` binding names a mutable region, which is the whole point
        // of it (§2.6): it is the only way to declare mutable state that is not
        // a local. Every other `::` stays immutable, so the write check reads
        // this flag and needs no notion of "is this one special".
        let is_static = directives.iter().any(|d| d.name.as_str() == "static");
        self.defs.get_mut(def).directives = directives;
        self.check_bodyless(rhs, &rhs_kind, def);
        if is_static {
            self.defs.get_mut(def).mutable = true;
        }

        // A namespace-like RHS is where the resolver looks for this def when it
        // descends into the body (to set the current namespace / `Self`), so mark
        // the RHS node too — collection's own mark sits on the outer binding.
        if matches!(
            rhs_kind,
            NodeKind::NamespaceExpr { .. }
                | NodeKind::StructType { .. }
                | NodeKind::EnumType { .. }
                | NodeKind::TraitType { .. }
        ) {
            self.ast.set_meta(rhs, DefMeta(def));
        }

        // A `const` generic on a *type* has nowhere to live: a nominal type's
        // identity is `(def, type-args)` (§3.8), with no slot for a value. They
        // are supported on functions and `impl` blocks, where the signature is
        // structural and the value can sit in the type it parameterizes.
        if let NodeKind::StructType { generics, .. }
        | NodeKind::EnumType { generics, .. }
        | NodeKind::TraitType { generics, .. } = &rhs_kind
        {
            for &g in generics {
                if matches!(self.ast.node(g).kind, NodeKind::GenericConstParam { .. }) {
                    self.report(
                        g,
                        "a `const` generic parameter is not supported on a type declaration yet —                          put it on the function or `impl` that uses it",
                    );
                }
            }
        }

        // Recurse into members of namespace-like RHS forms.
        match rhs_kind {
            NodeKind::NamespaceExpr { items, .. } => self.collect_items(&items, def),
            NodeKind::StructType { kind, .. } => {
                self.collect_struct(kind, def, vis);
            }
            NodeKind::EnumType { variants, .. } => {
                // A variant is named wherever its enum is: an enum whose
                // variants are private is an enum nothing can match on.
                let member_vis = vis.level();
                for v in variants {
                    if let NodeKind::Variant { name, .. } = self.ast.node(v).kind.clone() {
                        self.define(name, DefKind::Variant, member_vis, def, v, None);
                    }
                }
            }
            NodeKind::TraitType { members, .. } => {
                // Trait members are always visible where the trait is.
                for m in members {
                    self.collect_trait_member(m, def);
                }
            }
            _ => {}
        }
    }

    /// A bodyless `func` stands without an implementation, and §9 gives exactly
    /// three ways that is allowed: `#intrinsic` (the compiler supplies the
    /// body), `extern` (another object file does), and a trait requirement (an
    /// impl does). This path sees the first two — a trait's members are
    /// collected by [`Collector::collect_trait_member`], which never reaches
    /// here — so anything else bodyless is an error.
    ///
    /// Until now the shape was silently accepted and meant nothing, which is the
    /// worst of both: a declaration that looks like it does something and does
    /// not.
    fn check_bodyless(&mut self, rhs: NodeId, rhs_kind: &NodeKind, def: DefId) {
        let NodeKind::FuncExpr {
            body,
            extern_abi,
            params,
            generics,
            ..
        } = rhs_kind
        else {
            return;
        };
        self.check_c_vararg(rhs, def, body, extern_abi, params, generics);
        let tag = self.defs.get(def).intrinsic_tag();
        // An `#intrinsic` **must** be bodyless: the compiler is going to supply
        // the body, so a written one would be dead code with no way to tell.
        if let Some(tag) = &tag {
            if body.is_some() {
                self.report(rhs, "an `#intrinsic` function may not have a body");
            }
            // The compiler must recognize it here, at the declaration. Deferring
            // to the first call would report a missing body far from the line
            // that promised one.
            if super::intrinsics::lookup(tag.as_str()).is_none() {
                let msg = format!("unknown intrinsic `{tag}`");
                self.report(rhs, msg);
            }
            // `member_dyn` takes its trait from the result type, so a result
            // that names none leaves it nothing to build.
            if tag.as_str() == "member_dyn" {
                let dyn_result = match rhs_kind {
                    NodeKind::FuncExpr { ret: Some(r), .. } => matches!(
                        &self.ast.node(*r).kind,
                        NodeKind::PtrType { inner, .. }
                            if matches!(self.ast.node(*inner).kind, NodeKind::DynType { .. })
                    ),
                    _ => false,
                };
                if !dyn_result || generics.len() != 1 || params.len() != 2 {
                    self.report(
                        rhs,
                        "`member_dyn` is declared `func <T> (v: *T, m: Member) -> *dyn Trait`, \
                         and the trait object it returns is what names the trait",
                    );
                }
            }
        }
        if body.is_none() && tag.is_none() && extern_abi.is_none() {
            self.report(
                rhs,
                "a function with no body must be `#intrinsic`, `extern`, or a trait requirement",
            );
        }
    }

    /// What `#c_vararg` may be written on (§9).
    ///
    /// The directive says the declared parameters are a C function's **fixed**
    /// ones and that a call may pass a tail beyond them. Everything refused here
    /// is refused because the tail would have no meaning, not because it is hard:
    ///
    /// - **It must be `extern`.** Accepting the tail is one thing and *reading*
    ///   it is another — that is `va_list`, whose layout differs per target and
    ///   which nothing in this compiler emits. A declaration never reads it.
    /// - **It must have a fixed parameter.** C has no way to start a variadic
    ///   tail with nothing before it, because `va_start` names the last fixed
    ///   parameter.
    /// - **No defaults, and no generics.** Both decide arguments by a rule of
    ///   this language, and the tail is decided by C's — a defaulted parameter
    ///   and a tail argument would compete for the same position.
    fn check_c_vararg(
        &mut self,
        rhs: NodeId,
        def: DefId,
        body: &Option<NodeId>,
        extern_abi: &Option<Symbol>,
        params: &[NodeId],
        generics: &[NodeId],
    ) {
        if !self.defs.get(def).is_c_variadic() {
            return;
        }
        if extern_abi.is_none() || body.is_some() {
            self.report(
                rhs,
                "only an `extern` declaration may be `#c_vararg`: reading a variadic tail needs \
                 `va_list`, which this compiler does not emit",
            );
            return;
        }
        if params.is_empty() {
            self.report(
                rhs,
                "a `#c_vararg` function needs at least one fixed parameter, because `va_start` \
                 names the last one",
            );
        }
        if !generics.is_empty() {
            self.report(rhs, "a `#c_vararg` function may not be generic");
        }
        for &p in params {
            if let NodeKind::Param {
                default: Some(_), ..
            } = self.ast.node(p).kind
            {
                self.report(
                    p,
                    "a `#c_vararg` function may not have a default argument: the default and the \
                     variadic tail would claim the same position",
                );
            }
        }
    }

    /// Collect the fields of a struct as [`DefKind::Field`] members. Fields are
    /// public only under `@public(all)`, and a field's own `@private` re-hides it.
    fn collect_struct(&mut self, kind: StructKind, ty: DefId, vis: Vis) {
        let fields = match kind {
            StructKind::Record(fields) => fields,
            // A tuple struct's members are positional, and are collected under
            // the names their positions give them — `p.0`, `p.1`. That is the
            // spelling the surface language uses (§3.3), so making them real
            // field defs means tuple structs need no separate machinery: field
            // typing, `Pair(1, 2)`'s argument check, and `.0` all go through the
            // same path a record's named field does.
            StructKind::Tuple(types) => {
                let member_vis = vis.member_level();
                for (i, t) in types.iter().enumerate() {
                    self.define(
                        Symbol::new(&i.to_string()),
                        DefKind::Field,
                        member_vis,
                        ty,
                        *t,
                        None,
                    );
                }
                return;
            }
            StructKind::Unit => return,
        };
        // At most one field per struct may be `@using` (§3.10); the first one
        // wins and any further one is an error.
        let mut upcast_seen = false;
        for f in fields {
            if let NodeKind::Field {
                attrs,
                directives,
                name,
                ..
            } = self.ast.node(f).kind.clone()
            {
                // A field says it for itself when it carries an attribute of
                // its own — `@public`, `@public(package)`, `@private` — and
                // otherwise takes what the aggregate said its fields are.
                let own = self.visibility(&attrs);
                let member_vis = if attrs.iter().any(|&a| self.is_attr(a, "private")) {
                    Visibility::Private
                } else if attrs.iter().any(|&a| self.is_attr(a, "public")) {
                    own.level()
                } else {
                    vis.member_level()
                };
                let def = self.define(name, DefKind::Field, member_vis, ty, f, None);
                self.defs.get_mut(def).directives = self.directives(&directives);
                if attrs.iter().any(|&a| self.is_attr(a, "using")) {
                    if upcast_seen {
                        self.report(f, "a struct may have at most one `@using` field");
                    } else {
                        upcast_seen = true;
                        self.defs.get_mut(def).using = true;
                    }
                }
            }
        }
    }

    fn is_attr(&self, attr: NodeId, want: &str) -> bool {
        matches!(&self.ast.node(attr).kind, NodeKind::Attribute { name, .. } if name.as_str() == want)
    }

    fn collect_trait_member(&mut self, member: NodeId, trait_def: DefId) {
        let member = self.ast.decl_item(member);
        if let NodeKind::ConstBind { pattern, rhs } = self.ast.node(member).kind.clone() {
            if let Some(name) = self.binding_name(pattern) {
                let kind = match self.ast.node(rhs).kind {
                    NodeKind::AssocType { .. } => DefKind::TypeAlias,
                    NodeKind::FuncExpr { .. } => DefKind::Func,
                    NodeKind::OverloadSet { .. } => DefKind::Overload,
                    _ => DefKind::Const,
                };
                let def = self.define(name, kind, Visibility::Public, trait_def, member, None);
                // An abstract associated type is known to be one from its
                // syntax alone. Its bounds are resolved with its file, which a
                // file importing it cyclically may be resolved before; with no
                // bounds there is nothing to wait for, so the answer is
                // recorded now (see [`Def::assoc_bounds`]).
                if let NodeKind::AssocType { bounds } = &self.ast.node(rhs).kind
                    && bounds.is_empty()
                {
                    self.defs.get_mut(def).assoc_bounds = Some(Vec::new());
                }
            }
        }
    }

    /// `impl [<g>] Type [for Target] { items }` — attach items to the self
    /// type's namespace when it names a type collected in `scope`; otherwise
    /// park them under a fresh anonymous impl namespace so their names still
    /// exist. The self type is the `for` target of a trait impl (`impl Trait for
    /// Self`), or the head type of an inherent impl (`impl Self`).
    fn collect_impl(&mut self, node: NodeId, scope: DefId) {
        let NodeKind::ImplBlock {
            ty, for_ty, items, ..
        } = self.ast.node(node).kind.clone()
        else {
            return;
        };
        let self_ty = for_ty.unwrap_or(ty);
        let target = self
            .type_head_name(self_ty)
            .and_then(|name| self.defs.get(scope).ns.members.get(&name).copied())
            .filter(|&d| self.defs.get(d).kind.is_namespace_like());
        // A target that carries **generic arguments** — `impl Draw for
        // Box.<i32>` — names a type, so its members belong to that type's
        // namespace, but `Self` must not: the head def is `Box`, and `Self`
        // meaning `Box.<?>` leaves the arguments to inference, which then has
        // nothing to solve them from in a member that never mentions `self`.
        // So such an impl gets a namespace of its own for the `Self` binding
        // alone, exactly as a structural target does.
        let parameterized = matches!(
            &self.ast.node(self_ty).kind,
            NodeKind::TypePath { generic_args, .. } if !generic_args.is_empty()
        );
        let host = match target {
            Some(t) => t,
            // The target names no type collected here — a structural `impl <T>
            // []T`, or an impl for a type another package owns. The members
            // still need somewhere to live, so give the block a namespace of its
            // own. It is deliberately *not* a member of `scope`: nothing may
            // name it, and inserting it would collide with the next such impl.
            None => {
                self.anon_impls += 1;
                let name = Symbol::new(&format!("<impl {}>", self.target_label(self_ty)));
                let mut canonical = self.defs.get(scope).canonical.clone();
                canonical.push(name.clone());
                let span = self.ast.node(node).span;
                let id = self.defs.alloc(
                    name,
                    DefKind::Namespace,
                    Visibility::Private,
                    Some(scope),
                    Some(self.file),
                    Some(span),
                    Some(node),
                    canonical,
                );
                self.ast.set_meta(node, DefMeta(id));
                id
            }
        };
        // An impl whose target is **structural** — `impl <T> []T`, `impl Iterator
        // for Range.<T>` reaching a type the core library does not own — has no
        // named type for `Self` to point at. Bind `Self` in the impl's own
        // namespace as an alias for the target's type expression, so `self: *Self`
        // means `*[]T` there exactly as it means `*Vec3` in `impl Vec3`.
        if target.is_none() || parameterized {
            // Where the members went and where `Self` goes are two questions:
            // for a parameterized target the members are the type's and the
            // binding is the block's own.
            let owner = match target {
                Some(_) if parameterized => {
                    self.anon_impls += 1;
                    let name = Symbol::new(&format!("<impl {}>", self.target_label(self_ty)));
                    let mut canonical = self.defs.get(scope).canonical.clone();
                    canonical.push(name.clone());
                    let span = self.ast.node(node).span;
                    let id = self.defs.alloc(
                        name,
                        DefKind::Namespace,
                        Visibility::Private,
                        Some(scope),
                        Some(self.file),
                        Some(span),
                        Some(node),
                        canonical,
                    );
                    self.ast.set_meta(node, DefMeta(id));
                    id
                }
                _ => host,
            };
            let span = self.ast.node(self_ty).span;
            let mut canonical = self.defs.get(owner).canonical.clone();
            canonical.push(Symbol::new("Self"));
            let id = self.defs.alloc(
                Symbol::new("Self"),
                DefKind::TypeAlias,
                Visibility::Private,
                Some(owner),
                Some(self.file),
                Some(span),
                Some(self_ty),
                canonical,
            );
            self.defs
                .get_mut(owner)
                .ns
                .members
                .insert(Symbol::new("Self"), id);
        }
        let outer = std::mem::replace(&mut self.in_impl, true);
        for item in items {
            self.collect_item(item, host);
        }
        self.in_impl = outer;
    }

    // ===< imports >===

    fn record_import(
        &mut self,
        pattern: NodeId,
        path: ImportPath,
        vis: Vis,
        scope: DefId,
        bind: NodeId,
    ) {
        let target = match path {
            ImportPath::Package(segs) => RawTarget::Package(segs),
            ImportPath::File(spec) => RawTarget::File(spec),
        };
        let span = self.ast.node(bind).span;
        // An `import` binding may carry a `#lang` tag, and the prelude is why
        // (§4.6): `core.prelude` is a *namespace*, so the only thing there is to
        // tag is the binding that names it. The tag cannot be registered here —
        // the target file may not be collected yet, and the namespace it names
        // is only known once the import is wired — so it travels with the
        // [`RawImport`] and [`super::imports::wire`] registers it.
        let lang = self.pending.iter().find_map(|d| match d.args.first() {
            Some(DirectiveArg::Str(s)) if d.is("lang") => Some(s.clone()),
            _ => None,
        });
        self.imports.push(RawImport {
            pattern,
            scope,
            reexport: !matches!(vis.level(), Visibility::Private),
            target,
            lang,
            span,
        });
    }

    // ===< helpers >===

    /// Allocate a def in `scope`, register it as a member, stamp [`DefMeta`] on
    /// its defining node, and record any `#lang` tag.
    fn define(
        &mut self,
        name: Symbol,
        kind: DefKind,
        vis: Visibility,
        scope: DefId,
        node: NodeId,
        lang: Option<Symbol>,
    ) -> DefId {
        let mut canonical = self.defs.get(scope).canonical.clone();
        canonical.push(name.clone());
        let span = self.ast.node(node).span;
        // Duplicate-member check (§4.3). Impl members are exempt: a type's
        // namespace is the host for *every* trait impl on it, so `impl Add for
        // Vec3` and `impl Mul for Vec3` both park an `Output` there and neither
        // is a redeclaration. Which one a use means is never asked of the
        // namespace — an associated type is projected through the impl that
        // bound it, and a method through the impl selection chose (§4.8) — so
        // the entry is a convenience, not the authority. Two impls that really
        // do overlap are caught as an ambiguity when one of them is selected.
        // Overloading is **explicit** (§4.3): two functions never share a name,
        // and `f :: func { a, b }` is how one name reaches several of them. So
        // a second declaration of a name is the conflict it always was.
        if !self.in_impl && self.defs.get(scope).ns.members.contains_key(&name) {
            self.report(
                node,
                format!("`{name}` is already defined in this namespace"),
            );
        }
        let id = self.defs.alloc(
            name.clone(),
            kind,
            vis,
            Some(scope),
            Some(self.file),
            Some(span),
            Some(node),
            canonical,
        );
        self.defs.get_mut(scope).ns.members.insert(name, id);
        if let Some(tag) = lang {
            self.defs.get_mut(id).lang = Some(tag.clone());
            if let Some(prev) = self.lang.set(tag.clone(), id, self.in_core) {
                let _ = prev;
                self.report(node, format!("duplicate `#lang(\"{tag}\")` item"));
            }
        }
        self.ast.set_meta(node, DefMeta(id));
        id
    }

    /// The bound name of a simple binding pattern (`name` / `mut name`).
    fn binding_name(&self, pattern: NodeId) -> Option<Symbol> {
        match &self.ast.node(pattern).kind {
            NodeKind::BindingPat { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    /// A short label for an `impl`'s target, used to name the anonymous
    /// namespace its members live in (`<impl []T>`).
    ///
    /// It is a *display* name — nothing resolves through it — so it only has to
    /// be readable in an IR dump and stable as the file around it changes. A
    /// shape it cannot render falls back to the running count, which is why the
    /// counter is bumped either way.
    fn target_label(&self, ty: NodeId) -> String {
        match self.ast.node(ty).kind.clone() {
            NodeKind::TypePath { path, .. } => match &self.ast.node(path).kind {
                NodeKind::Path { segments } => segments
                    .iter()
                    .map(Symbol::as_str)
                    .collect::<Vec<_>>()
                    .join("."),
                _ => self.anon_impls.to_string(),
            },
            NodeKind::PtrType { mutable, inner } => {
                let m = if mutable { "mut " } else { "" };
                format!("*{m}{}", self.target_label(inner))
            }
            NodeKind::SliceType { mutable, inner, .. } => {
                let m = if mutable { "mut " } else { "" };
                format!("[]{m}{}", self.target_label(inner))
            }
            NodeKind::ArrayType { mutable, inner, .. } => {
                let m = if mutable { "mut " } else { "" };
                format!("[N]{m}{}", self.target_label(inner))
            }
            NodeKind::TupleType { .. } => "(..)".to_string(),
            NodeKind::FuncType { .. } => "func".to_string(),
            NodeKind::DynType { inner } => format!("dyn {}", self.target_label(inner)),
            NodeKind::GenericApply { base, .. } => self.target_label(base),
            _ => self.anon_impls.to_string(),
        }
    }

    /// The head identifier of a type expression, for `impl` target matching:
    /// the first segment of a `TypePath`'s path.
    fn type_head_name(&self, ty: NodeId) -> Option<Symbol> {
        match &self.ast.node(ty).kind {
            NodeKind::TypePath { path, .. } => match &self.ast.node(*path).kind {
                NodeKind::Path { segments } => segments.first().cloned(),
                _ => None,
            },
            NodeKind::Path { segments } => segments.first().cloned(),
            _ => None,
        }
    }

    /// Whether one of `attrs` is `@name`.
    fn has_attr(&self, attrs: &[NodeId], want: &str) -> bool {
        attrs.iter().any(|&a| {
            matches!(&self.ast.node(a).kind, NodeKind::Attribute { name, .. } if name.as_str() == want)
        })
    }

    /// What an item's attributes say about who may name it, and who may name
    /// its members (§4.4).
    ///
    /// `@public` is the item alone. Its argument list carries two independent
    /// things: a **level** for the item — `@public(package)` — and a level for
    /// its members, written `fields: <level>`, of which `all` is the shorthand
    /// for `fields: public`. So `@public(package, fields: package)` is a type a
    /// package keeps to itself, fields and all.
    fn visibility(&mut self, attrs: &[NodeId]) -> Vis {
        let mut vis = Vis::PRIVATE;
        for &a in attrs {
            let NodeKind::Attribute { name, args } = self.ast.node(a).kind.clone() else {
                continue;
            };
            if name.as_str() != "public" {
                continue;
            }
            vis.level = Visibility::Public;
            for arg in args {
                match self.vis_arg(arg) {
                    Some(VisArg::Item(level)) => vis.level = level,
                    Some(VisArg::Fields(level)) => vis.fields = Some(level),
                    None => self.report(
                        arg,
                        "`@public` takes `package`, `all`, or `fields: <public|package|private>`",
                    ),
                }
            }
        }
        vis
    }

    /// One argument of `@public`.
    fn vis_arg(&self, arg: NodeId) -> Option<VisArg> {
        let NodeKind::Arg { name, value } = self.ast.node(arg).kind.clone() else {
            return None;
        };
        let level = self.level_named(value)?;
        match name.as_ref().map(|n| n.to_string()).as_deref() {
            // `all` is the old spelling of `fields: public`, and the common one.
            None if self.written_level(value)?.as_str() == "all" => {
                Some(VisArg::Fields(Visibility::Public))
            }
            None => Some(VisArg::Item(level)),
            Some("fields") => Some(VisArg::Fields(level)),
            Some(_) => None,
        }
    }

    /// The bare word an argument is, if it is one.
    fn written_level(&self, value: NodeId) -> Option<String> {
        match &self.ast.node(value).kind {
            NodeKind::Path { segments } if segments.len() == 1 => Some(segments[0].to_string()),
            _ => None,
        }
    }

    /// The visibility a bare word names.
    fn level_named(&self, value: NodeId) -> Option<Visibility> {
        match self.written_level(value)?.as_str() {
            "public" | "all" => Some(Visibility::Public),
            "package" => Some(Visibility::Package),
            "private" => Some(Visibility::Private),
            _ => None,
        }
    }

    /// The directives a `::`-RHS form carries on itself (`f :: #inline func …`).
    fn rhs_directives(&self, rhs_kind: &NodeKind) -> Vec<Directive> {
        match rhs_kind {
            NodeKind::FuncExpr { directives, .. }
            | NodeKind::StructType { directives, .. }
            | NodeKind::EnumType { directives, .. }
            | NodeKind::TraitType { directives, .. }
            | NodeKind::NamespaceExpr { directives, .. }
            | NodeKind::DistinctType { directives, .. } => self.directives(directives),
            _ => Vec::new(),
        }
    }

    /// `@link_name("x")` on this declaration, as the directive the mangler
    /// looks for.
    ///
    /// One string argument and nothing else: the whole of what the attribute
    /// says is the symbol a linker will look for, and an `@link_name` written
    /// with anything else has not said one. It is left off rather than
    /// reported here — `sema::resolve` is what checks an attribute's shape.
    fn link_name(&self, attrs: &[NodeId]) -> Option<Directive> {
        attrs.iter().find_map(|&a| {
            let NodeKind::Attribute { name, args } = &self.ast.node(a).kind else {
                return None;
            };
            if name.as_str() != "link_name" || args.len() != 1 {
                return None;
            }
            let arg = self.directive_arg(args[0]);
            matches!(arg, DirectiveArg::Str(_)).then(|| Directive {
                name: Symbol::new("link_name"),
                args: vec![arg],
            })
        })
    }

    /// `@no_mangle` on this declaration: emit the symbol under the name the
    /// program wrote, with no scheme applied.
    ///
    /// It is `@link_name` with the name left out — the declaration's own — and
    /// it is recorded as its own directive rather than rewritten into one so
    /// that the two can be told apart when they are both written, which is a
    /// contradiction and is reported here.
    ///
    /// **Why it exists separately from `@public`.** Visibility says whether a
    /// symbol leaves its unit; this says what it is *called*. A C caller — the
    /// runtime, a startup file, another language's linker — looks a name up, and
    /// a name this compiler chose the encoding of is not one anybody can write.
    fn no_mangle(&mut self, item: NodeId, attrs: &[NodeId]) -> Option<Directive> {
        let found = attrs.iter().find(|&&a| {
            matches!(&self.ast.node(a).kind, NodeKind::Attribute { name, .. }
                if name.as_str() == "no_mangle")
        })?;
        if let NodeKind::Attribute { args, .. } = &self.ast.node(*found).kind
            && !args.is_empty()
        {
            self.report(
                *found,
                "`@no_mangle` takes no arguments: it is `@link_name` with the declaration's own \
                 name, so there is nothing to write",
            );
        }
        if self.link_name(attrs).is_some() {
            self.report(
                item,
                "`@no_mangle` and `@link_name` both name the symbol, and they name different \
                 ones: keep the `@link_name`, or drop it and take the written name",
            );
            return None;
        }
        Some(Directive {
            name: Symbol::new("no_mangle"),
            args: Vec::new(),
        })
    }

    /// `@test` on this declaration: the function is a test.
    ///
    /// Recorded as a directive for the same reason `@link_name` is — what reads
    /// it is a later stage (monomorphization decides whether to keep it, and
    /// `crate::lir::entry` builds the table of them), and a later stage has the
    /// def and not the syntax tree.
    ///
    /// It takes no arguments: the whole of what it says is "this is a test", and
    /// what a test is named is what it is *called*.
    fn test(&mut self, attrs: &[NodeId]) -> Option<Directive> {
        let found = attrs.iter().find(|&&a| {
            matches!(&self.ast.node(a).kind, NodeKind::Attribute { name, .. }
                if name.as_str() == "test")
        })?;
        if let NodeKind::Attribute { args, .. } = &self.ast.node(*found).kind
            && !args.is_empty()
        {
            self.report(*found, "`@test` takes no arguments");
        }
        Some(Directive {
            name: Symbol::new("test"),
            args: Vec::new(),
        })
    }

    /// Read a run of `#name(args)` nodes into their [`Directive`] values.
    fn directives(&self, nodes: &[NodeId]) -> Vec<Directive> {
        nodes
            .iter()
            .filter_map(|&d| match &self.ast.node(d).kind {
                NodeKind::Directive { name, args } => Some(Directive {
                    name: name.clone(),
                    args: args.iter().map(|&a| self.directive_arg(a)).collect(),
                }),
                _ => None,
            })
            .collect()
    }

    /// One directive argument, out of the small literal vocabulary §9 uses.
    fn directive_arg(&self, arg: NodeId) -> DirectiveArg {
        let value = match &self.ast.node(arg).kind {
            NodeKind::Arg { value, .. } => *value,
            _ => arg,
        };
        match &self.ast.node(value).kind {
            NodeKind::Lit(crate::parser::ast::Lit::Str(s)) => DirectiveArg::Str(Symbol::new(s)),
            NodeKind::Lit(crate::parser::ast::Lit::Int(n)) => i128::try_from(n)
                .map(DirectiveArg::Int)
                .unwrap_or(DirectiveArg::Other),
            NodeKind::Path { segments } if segments.len() == 1 => {
                DirectiveArg::Name(segments[0].clone())
            }
            _ => DirectiveArg::Other,
        }
    }

    /// Extract a `#lang("tag")` from a form that carries directives.
    fn lang_tag(&self, rhs_kind: &NodeKind) -> Option<Symbol> {
        let directives = match rhs_kind {
            NodeKind::FuncExpr { directives, .. }
            | NodeKind::StructType { directives, .. }
            | NodeKind::EnumType { directives, .. }
            | NodeKind::TraitType { directives, .. }
            | NodeKind::NamespaceExpr { directives, .. }
            | NodeKind::DistinctType { directives, .. } => directives,
            _ => return None,
        };
        for &d in directives {
            if let NodeKind::Directive { name, args } = &self.ast.node(d).kind {
                if name.as_str() == "lang" {
                    if let Some(&arg) = args.first() {
                        if let NodeKind::Arg { value, .. } = &self.ast.node(arg).kind {
                            if let NodeKind::Lit(crate::parser::ast::Lit::Str(s)) =
                                &self.ast.node(*value).kind
                            {
                                return Some(Symbol::new(s));
                            }
                        }
                    }
                }
            }
        }
        None
    }

    fn report(&mut self, node: NodeId, message: impl Into<String>) {
        let span = self.ast.node(node).span;
        self.diags
            .push(Diagnostic::error(message).with_primary(FileSpan::new(self.file, span), ""));
    }
}

/// Map a `::`-RHS node to the [`DefKind`] the binding introduces.
fn def_kind_of(rhs: &NodeKind) -> DefKind {
    match rhs {
        NodeKind::FuncExpr { .. } => DefKind::Func,
        NodeKind::OverloadSet { .. } => DefKind::Overload,
        NodeKind::StructType { .. } => DefKind::Struct,
        NodeKind::EnumType { .. } => DefKind::Enum,
        NodeKind::TraitType { .. } => DefKind::Trait,
        NodeKind::NamespaceExpr { .. } => DefKind::Namespace,
        NodeKind::DistinctType { .. }
        | NodeKind::PtrType { .. }
        | NodeKind::SliceType { .. }
        | NodeKind::ArrayType { .. }
        | NodeKind::TupleType { .. }
        | NodeKind::FuncType { .. }
        | NodeKind::DynType { .. }
        | NodeKind::TypePath { .. } => DefKind::TypeAlias,
        _ => DefKind::Const,
    }
}
