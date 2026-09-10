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
) -> Vec<RawImport> {
    let mut cx = Collector {
        defs,
        lang,
        diags,
        ast,
        file,
        imports: Vec::new(),
        in_impl: false,
        pending: Vec::new(),
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
    imports: Vec<RawImport>,
    /// Whether collection is inside an `impl` body, where a name may repeat
    /// across impls that share a host namespace.
    in_impl: bool,
    /// Directives written on the enclosing [`NodeKind::Decl`], waiting for the
    /// binding inside it to claim them.
    pending: Vec<Directive>,
    /// How many anonymous `impl` namespaces this file has needed so far; the
    /// count names them apart (`<impl 1>`, `<impl 2>`, …) so two impls on
    /// structural targets stay distinguishable in a def dump.
    anon_impls: usize,
}

/// Visibility gathered from an item's attributes.
#[derive(Clone, Copy)]
struct Vis {
    /// `@public` (or `@public(all)`) present.
    public: bool,
    /// `@public(all)` — also export aggregate fields/variants.
    all: bool,
}

impl Vis {
    const PRIVATE: Vis = Vis {
        public: false,
        all: false,
    };

    fn level(self) -> Visibility {
        if self.public {
            Visibility::Public
        } else {
            Visibility::Private
        }
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
                self.check_namespace_binding(item, &directives);
                // Directives written *before* the binding (`#inline\nf :: func …`)
                // and directives written on the RHS form (`f :: #inline func …`)
                // mean the same thing, so the binding collects both.
                let here = self.directives(&directives);
                let outer = std::mem::replace(&mut self.pending, here);
                self.collect_binding(item, vis, scope);
                self.pending = outer;
            }
            _ => {
                self.check_namespace_binding(node, &[]);
                self.collect_binding(node, Vis::PRIVATE, scope)
            }
        }
    }

    /// At namespace scope only `#static let ...` is a well-formed `let`/`const`
    /// (§13.1): a bare one there has no home to live in, and an immutable
    /// namespace binding is spelled `::`.
    fn check_namespace_binding(&mut self, item: NodeId, directives: &[NodeId]) {
        if !matches!(self.ast.node(item).kind, NodeKind::LocalDecl { .. }) {
            return;
        }
        if directives.iter().any(|&d| self.is_directive(d, "static")) {
            return;
        }
        self.report(
            item,
            "a `let` / `const` at namespace scope must be `#static`;              use `::` for an immutable binding",
        );
    }

    fn is_directive(&self, node: NodeId, want: &str) -> bool {
        matches!(&self.ast.node(node).kind, NodeKind::Directive { name, .. } if name.as_str() == want)
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
        let mut directives = std::mem::take(&mut self.pending);
        directives.extend(self.rhs_directives(&rhs_kind));
        self.defs.get_mut(def).directives = directives;

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
                if matches!(
                    self.ast.node(g).kind,
                    NodeKind::GenericConstParam { .. }
                ) {
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
                let member_vis = if vis.public {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
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
                let member_vis = if vis.all {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
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
                let hidden = attrs.iter().any(|&a| self.is_attr(a, "private"));
                let member_vis = if vis.all && !hidden {
                    Visibility::Public
                } else {
                    Visibility::Private
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
        if let NodeKind::ConstBind { pattern, rhs } = self.ast.node(member).kind.clone() {
            if let Some(name) = self.binding_name(pattern) {
                let kind = match self.ast.node(rhs).kind {
                    NodeKind::AssocType { .. } => DefKind::TypeAlias,
                    NodeKind::FuncExpr { .. } => DefKind::Func,
                    _ => DefKind::Const,
                };
                self.define(name, kind, Visibility::Public, trait_def, member, None);
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
        if target.is_none() {
            let span = self.ast.node(self_ty).span;
            let mut canonical = self.defs.get(host).canonical.clone();
            canonical.push(Symbol::new("Self"));
            let id = self.defs.alloc(
                Symbol::new("Self"),
                DefKind::TypeAlias,
                Visibility::Private,
                Some(host),
                Some(self.file),
                Some(span),
                Some(self_ty),
                canonical,
            );
            self.defs
                .get_mut(host)
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
        self.imports.push(RawImport {
            pattern,
            scope,
            reexport: vis.public,
            target,
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
        if !self.in_impl {
            if let Some(&prev) = self.defs.get(scope).ns.members.get(&name) {
                if !matches!(kind, DefKind::Func)
                    || !matches!(self.defs.get(prev).kind, DefKind::Func)
                {
                    self.report(
                        node,
                        format!("`{name}` is already defined in this namespace"),
                    );
                }
            }
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
            if let Some(prev) = self.lang.set(tag.clone(), id) {
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

    fn visibility(&self, attrs: &[NodeId]) -> Vis {
        let mut vis = Vis::PRIVATE;
        for &a in attrs {
            if let NodeKind::Attribute { name, args } = &self.ast.node(a).kind {
                if name.as_str() == "public" {
                    vis.public = true;
                    // `@public(all)` — a single positional `all` argument.
                    if args.iter().any(|&arg| self.is_all_arg(arg)) {
                        vis.all = true;
                    }
                }
            }
        }
        vis
    }

    fn is_all_arg(&self, arg: NodeId) -> bool {
        if let NodeKind::Arg { value, .. } = &self.ast.node(arg).kind {
            if let NodeKind::Path { segments } = &self.ast.node(*value).kind {
                return segments.len() == 1 && segments[0].as_str() == "all";
            }
        }
        false
    }

    /// The directives a `::`-RHS form carries on itself (`f :: #inline func …`).
    fn rhs_directives(&self, rhs_kind: &NodeKind) -> Vec<Directive> {
        match rhs_kind {
            NodeKind::FuncExpr { directives, .. }
            | NodeKind::StructType { directives, .. }
            | NodeKind::EnumType { directives, .. }
            | NodeKind::TraitType { directives, .. }
            | NodeKind::NamespaceExpr { directives, .. } => self.directives(directives),
            _ => Vec::new(),
        }
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
            NodeKind::Lit(crate::parser::ast::Lit::Int(n)) => {
                i128::try_from(n).map(DirectiveArg::Int).unwrap_or(DirectiveArg::Other)
            }
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
