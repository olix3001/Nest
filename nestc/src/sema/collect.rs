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
use super::def::{DefId, DefKind, DefTable, LangItems, Visibility};
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
            NodeKind::Decl { attrs, item, .. } => {
                let vis = self.visibility(&attrs);
                self.collect_binding(item, vis, scope);
            }
            _ => self.collect_binding(node, Vis::PRIVATE, scope),
        }
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
            // Tuple/unit structs have positional/no named members.
            StructKind::Tuple(_) | StructKind::Unit => return,
        };
        // At most one field per struct may be `@using` (§3.10); the first one
        // wins and any further one is an error.
        let mut upcast_seen = false;
        for f in fields {
            if let NodeKind::Field { attrs, name, .. } = self.ast.node(f).kind.clone() {
                let hidden = attrs.iter().any(|&a| self.is_attr(a, "private"));
                let member_vis = if vis.all && !hidden {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                let def = self.define(name, DefKind::Field, member_vis, ty, f, None);
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
        let host = target.unwrap_or_else(|| {
            self.define(
                Symbol::new("<impl>"),
                DefKind::Namespace,
                Visibility::Private,
                scope,
                node,
                None,
            )
        });
        for item in items {
            self.collect_item(item, host);
        }
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
        // Duplicate-member check (§4.3): impl namespaces are exempt but those go
        // through their own host, so a plain clash here is an error.
        if let Some(&prev) = self.defs.get(scope).ns.members.get(&name) {
            if !matches!(kind, DefKind::Func) || !matches!(self.defs.get(prev).kind, DefKind::Func)
            {
                self.report(
                    node,
                    format!("`{name}` is already defined in this namespace"),
                );
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

    /// Extract a `#lang("tag")` from a form that carries directives.
    fn lang_tag(&self, rhs_kind: &NodeKind) -> Option<Symbol> {
        let directives = match rhs_kind {
            NodeKind::FuncExpr { directives, .. }
            | NodeKind::StructType { directives, .. }
            | NodeKind::EnumType { directives, .. }
            | NodeKind::TraitType { directives, .. }
            | NodeKind::NamespaceExpr { directives, .. } => directives,
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
