//! The impl index — the whole-program table the trait solver selects over.
//!
//! Collection ([`super::collect`]) parks each `impl`'s members in a namespace
//! but does not record the impl *as a unit*: nothing knows that `impl Add for
//! Vec3` associates the trait `Add` with the self type `Vec3`, carries the
//! generics `<>`, and binds `Output :: Vec3`. Selecting an impl for an operator
//! or a trait-method obligation needs exactly that. This module walks every
//! collected, **resolved** `impl` block and records an [`ImplInfo`] per impl,
//! keyed for lookup by `(trait, self-head)`.
//!
//! It runs after name resolution (so the trait head, the self head, and each
//! member all carry a [`Resolution`]/[`DefMeta`]) and before inference (which
//! borrows the finished [`ImplTable`]). The table stores *syntax* — the self
//! type node, the trait-argument nodes, the associated-type binding nodes — and
//! leaves turning those into [`Ty`](super::ty::Ty)s to the solver, which owns
//! the [`InferCtxt`](super::ty::InferCtxt) the fresh variables live in.

use std::collections::HashMap;

use crate::common::source::FileId;
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, NodeId, NodeKind};

use super::def::{DefId, DefKind, DefTable};
use super::{DefMeta, Resolution};

/// One recorded `impl` block.
///
/// For an inherent impl (`impl Vec3 { … }`) [`trait_def`](ImplInfo::trait_def)
/// is `None`. For a trait impl (`impl [<g>] Trait[.<args>] for Self { … }`) it
/// is the trait's [`DefId`], [`self_node`](ImplInfo::self_node) is the `for`
/// target's type node, and [`trait_args`](ImplInfo::trait_args) are the trait's
/// own generic arguments.
#[derive(Debug, Clone)]
pub struct ImplInfo {
    /// The implemented trait, or `None` for an inherent impl.
    pub trait_def: Option<DefId>,
    /// The trait's generic-argument type nodes (`Add.<Rhs>` → the `Rhs` node),
    /// excluding `<Assoc = T>` bindings. Empty for an inherent impl or a bare
    /// `impl Trait for Self`.
    pub trait_args: Vec<NodeId>,
    /// The self type's type-expression node (the impl's target), used to build
    /// the concrete self [`Ty`](super::ty::Ty) with the impl's generics
    /// instantiated fresh.
    pub self_node: NodeId,
    /// The head [`DefId`] of the self type — a `struct`/`enum` (or, for a
    /// blanket impl, one of [`generics`](ImplInfo::generics)). Used to index
    /// candidates and to judge specificity (a generic self is less specific).
    pub self_head: Option<DefId>,
    /// The impl's own generic type parameters (`impl <T> …`), fresh-instantiated
    /// at each selection so one impl serves a whole family.
    pub generics: Vec<DefId>,
    /// Every named member the impl declares: methods and associated-type
    /// bindings, `name → def`.
    pub members: HashMap<Symbol, DefId>,
    /// Associated-type bindings, `assoc name → its RHS type-expression node`
    /// (e.g. `Output :: Vec3` → the `Vec3` node). Projection substitutes the
    /// impl's solved generics into this node.
    pub assoc: HashMap<Symbol, NodeId>,
    /// The file the impl (and its member/assoc nodes) lives in.
    pub file: FileId,
}

impl ImplInfo {
    /// Whether the self type is one of the impl's own generics (a blanket impl
    /// `impl <T> Trait for T`), which is strictly less specific than an impl for
    /// a named type.
    pub fn self_is_generic(&self) -> bool {
        self.self_head.is_some_and(|h| self.generics.contains(&h))
    }
}

/// The whole-program impl index.
#[derive(Debug, Default)]
pub struct ImplTable {
    /// Every recorded impl, in collection order.
    pub impls: Vec<ImplInfo>,
}

/// Build the [`ImplTable`] from every collected, resolved file.
pub fn build(defs: &DefTable, asts: &HashMap<FileId, Ast>, files: &[FileId]) -> ImplTable {
    let mut table = ImplTable::default();
    for &file in files {
        let ast = &asts[&file];
        for id in ast.ids() {
            let NodeKind::ImplBlock {
                generics,
                ty,
                for_ty,
                items,
            } = ast.node(id).kind.clone()
            else {
                continue;
            };
            if let Some(info) = record(defs, ast, file, &generics, ty, for_ty, &items) {
                table.impls.push(info);
            }
        }
    }
    table
}

fn record(
    defs: &DefTable,
    ast: &Ast,
    file: FileId,
    generics: &[NodeId],
    ty: NodeId,
    for_ty: Option<NodeId>,
    items: &[NodeId],
) -> Option<ImplInfo> {
    // `impl Trait for Self` splits into (trait = ty, self = for_ty); an inherent
    // `impl Self` has no trait and self = ty.
    let (trait_node, self_node) = match for_ty {
        Some(target) => (Some(ty), target),
        None => (None, ty),
    };

    let generics: Vec<DefId> = generics.iter().filter_map(|&g| def_of(ast, g)).collect();

    let (trait_def, trait_args) = match trait_node {
        Some(t) => (resolved_def(defs, ast, t), trait_arg_nodes(ast, t)),
        None => (None, Vec::new()),
    };

    let self_head = resolved_def(defs, ast, head_of(ast, self_node));

    let mut members = HashMap::new();
    let mut assoc = HashMap::new();
    for &item in items {
        let bind = match ast.node(item).kind.clone() {
            NodeKind::Decl { item, .. } => item,
            _ => item,
        };
        if let NodeKind::ConstBind { pattern, rhs } = ast.node(bind).kind.clone() {
            let Some(def) = def_of(ast, bind) else {
                continue;
            };
            let name = defs.get(def).name.clone();
            let _ = pattern;
            members.insert(name.clone(), def);
            // An associated-type binding in an impl is a `Name :: <type>`: the
            // RHS is a type expression, not a `func` / value.
            if is_type_rhs(ast, rhs) {
                assoc.insert(name, rhs);
            }
        }
    }

    Some(ImplInfo {
        trait_def,
        trait_args,
        self_node,
        self_head,
        generics,
        members,
        assoc,
        file,
    })
}

/// The generic-argument type nodes of a trait head `Trait.<A, B>`, dropping any
/// `<Assoc = T>` binding.
fn trait_arg_nodes(ast: &Ast, trait_node: NodeId) -> Vec<NodeId> {
    match &ast.node(trait_node).kind {
        NodeKind::TypePath { generic_args, .. } => generic_args
            .iter()
            .copied()
            .filter(|&a| !matches!(ast.node(a).kind, NodeKind::AssocBinding { .. }))
            .collect(),
        _ => Vec::new(),
    }
}

/// The path node whose resolution is a type-expression's head def.
fn head_of(ast: &Ast, node: NodeId) -> NodeId {
    match ast.node(node).kind.clone() {
        NodeKind::TypePath { path, .. } => path,
        NodeKind::GenericApply { base, .. } => base,
        _ => node,
    }
}

/// Whether a `::`-binding RHS forms a type (so the binding is an associated-type
/// value, not a method or constant).
fn is_type_rhs(ast: &Ast, rhs: NodeId) -> bool {
    matches!(
        ast.node(rhs).kind,
        NodeKind::TypePath { .. }
            | NodeKind::Path { .. }
            | NodeKind::PtrType { .. }
            | NodeKind::SliceType { .. }
            | NodeKind::ArrayType { .. }
            | NodeKind::TupleType { .. }
            | NodeKind::FuncType { .. }
            | NodeKind::DynType { .. }
            | NodeKind::DistinctType { .. }
            | NodeKind::GenericApply { .. }
    )
}

fn def_of(ast: &Ast, node: NodeId) -> Option<DefId> {
    ast.meta::<DefMeta>(node).map(|m| m.0)
}

fn resolved_def(defs: &DefTable, ast: &Ast, node: NodeId) -> Option<DefId> {
    match ast.meta::<Resolution>(node)? {
        Resolution::Def(d) => {
            let d = defs.resolve_alias(d);
            // Only heads that name a type (or trait) are useful keys.
            matches!(
                defs.get(d).kind,
                DefKind::Struct
                    | DefKind::Enum
                    | DefKind::Trait
                    | DefKind::TypeParam
                    | DefKind::Primitive
                    | DefKind::TypeAlias
            )
            .then_some(d)
        }
        _ => None,
    }
}
