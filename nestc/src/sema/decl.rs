//! The **declaration queries**: what a definition declares, read from the file
//! it was written in.
//!
//! Nearly every one of these answers a question about a def that a *later* pass
//! asks — the parameter names of a function it is calling, the fields of a
//! struct it is building, the generics of a type it is instantiating — and the
//! only record of the answer is the syntax tree the def came from. Reaching it
//! is always the same three steps: a [`Def`](super::def::Def) has a
//! [`FileId`] and a [`NodeId`], the node is usually the `name :: value` binding
//! rather than the value, and the value is then matched for the one shape the
//! question is about.
//!
//! Those three steps were written out at every call site, in [`super::infer`]
//! and [`super::lower`] alike, which is why they are here instead: **one place
//! reaches a declaration's tree**. That matters beyond tidiness. A definition in
//! another package has no tree here — its library carries what analysis
//! concluded, not the syntax it concluded it from — so every question asked of a
//! foreign def has to be answerable without one, and a question can only be
//! moved off the tree once there is a single place asking it.

use std::collections::HashMap;

use crate::common::source::FileId;
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, Lit, NodeId, NodeKind, StructKind};

use super::DefMeta;
use super::Resolution;
use super::def::{DefId, DefKind, DefTable};

/// What a trait member asks of an impl — see [`Decls::requirement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    Method,
    AssocType,
    AssocConst,
}

impl Requirement {
    /// How a diagnostic names it.
    pub fn label(self) -> &'static str {
        match self {
            Requirement::Method => "method",
            Requirement::AssocType => "associated type",
            Requirement::AssocConst => "associated constant",
        }
    }
}

/// The tables a declaration query reads: every definition, and every file's
/// tree.
///
/// A borrowed view rather than a stage of its own — the passes that ask these
/// questions hold both halves already, and each builds one of these for as long
/// as the question takes.
#[derive(Clone, Copy)]
pub struct Decls<'a> {
    pub defs: &'a DefTable,
    pub asts: &'a HashMap<FileId, Ast>,
}

impl<'a> Decls<'a> {
    pub fn new(defs: &'a DefTable, asts: &'a HashMap<FileId, Ast>) -> Self {
        Self { defs, asts }
    }

    /// Where `def` was written, and the node that *declares* it: the RHS of the
    /// `name :: value` binding, or the def's own node when it is not one.
    ///
    /// This is the entry point nearly every other query starts from. The
    /// distinction it settles is that a `Point :: struct { ... }` def points at
    /// the binding, and it is the `struct` that answers "what fields".
    pub fn declaration(&self, def: DefId) -> Option<(FileId, NodeId)> {
        let d = self.defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let ast = self.asts.get(&file)?;
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        Some((file, rhs))
    }

    /// The `FuncExpr` `def` is declared by, if it is a function.
    pub fn func(&self, def: DefId) -> Option<(FileId, NodeId)> {
        let (file, node) = self.declaration(def)?;
        matches!(self.asts[&file].node(node).kind, NodeKind::FuncExpr { .. })
            .then_some((file, node))
    }

    // ===< functions >===

    /// The **value** parameter nodes of a function def, in declaration order.
    ///
    /// A leading `self` is excluded: a method call's receiver is not one of its
    /// written arguments, so these line up with the call's arguments either way.
    pub fn param_nodes(&self, def: DefId) -> Option<Vec<NodeId>> {
        let (file, func) = self.func(def)?;
        let ast = &self.asts[&file];
        let NodeKind::FuncExpr { params, .. } = &ast.node(func).kind else {
            return None;
        };
        Some(
            params
                .iter()
                .copied()
                .filter(|&p| match &ast.node(p).kind {
                    NodeKind::Param { name, .. } => name.as_str() != "self",
                    _ => false,
                })
                .collect(),
        )
    }

    /// The **value** parameter names of a function def, in declaration order.
    pub fn param_names(&self, def: DefId) -> Option<Vec<Symbol>> {
        let (file, _) = self.func(def)?;
        let ast = &self.asts[&file];
        Some(
            self.param_nodes(def)?
                .into_iter()
                .filter_map(|p| match &ast.node(p).kind {
                    NodeKind::Param { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .collect(),
        )
    }

    /// The default expression of each **value** parameter, in the same order and
    /// filtering as [`Self::param_names`], so the two zip.
    pub fn param_default_nodes(&self, def: DefId) -> Option<Vec<Option<NodeId>>> {
        let (file, _) = self.func(def)?;
        let ast = &self.asts[&file];
        Some(
            self.param_nodes(def)?
                .into_iter()
                .filter_map(|p| match &ast.node(p).kind {
                    NodeKind::Param { default, .. } => Some(*default),
                    _ => None,
                })
                .collect(),
        )
    }

    /// Which of `def`'s **value** parameters carry a default.
    ///
    /// Only presence, not the expression: a call site never looks at the default
    /// itself. It was type-checked once at the declaration and is filled in by
    /// lowering, so all inference needs to know is that the slot may legally be
    /// left empty.
    pub fn param_defaults(&self, def: DefId) -> Option<Vec<bool>> {
        Some(
            self.param_default_nodes(def)?
                .into_iter()
                .map(|d| d.is_some())
                .collect(),
        )
    }

    /// Whether `def` is a function with a body — the thing that separates a
    /// trait's **default** method from a bodyless requirement an impl must
    /// satisfy.
    pub fn has_body(&self, def: DefId) -> bool {
        let Some((file, func)) = self.func(def) else {
            return false;
        };
        matches!(
            self.asts[&file].node(func).kind,
            NodeKind::FuncExpr { body: Some(_), .. }
        )
    }

    /// What `def` **requires** of an impl, when it is a trait member that
    /// requires anything.
    ///
    /// A member the trait answered itself asks nothing: a method with a body is
    /// a default, and so is an associated constant with a `:=`. An associated
    /// type always requires an answer — a trait cannot guess it.
    pub fn requirement(&self, def: DefId) -> Option<Requirement> {
        let (file, node) = self.declaration(def)?;
        match &self.asts[&file].node(node).kind {
            NodeKind::FuncExpr { body: Some(_), .. } => None,
            NodeKind::FuncExpr { .. } => Some(Requirement::Method),
            NodeKind::AssocType { .. } => Some(Requirement::AssocType),
            NodeKind::AssocConst {
                default: Some(_), ..
            } => None,
            NodeKind::AssocConst { .. } => Some(Requirement::AssocConst),
            _ => None,
        }
    }

    /// The trait's own declaration of `name`, but only when it carries a
    /// **default body**: a bodyless signature is a requirement, not something
    /// callable through the impl.
    pub fn trait_default_method(&self, trait_def: DefId, name: &Symbol) -> Option<DefId> {
        let m = *self.defs.get(trait_def).ns.members.get(name)?;
        (self.defs.get(m).kind == DefKind::Func && self.has_body(m)).then_some(m)
    }

    // ===< generics >===

    /// Number of generic parameters a **type** def declares.
    ///
    /// Types only: a `struct` / `enum` / `trait` carries type arguments and
    /// nothing else, so this is the arity a nominal type has.
    pub fn generic_arity(&self, def: DefId) -> usize {
        self.type_generic_nodes(def).map_or(0, |(_, g)| g.len())
    }

    /// The generic **type**-parameter defs a type def declares, in order.
    pub fn type_param_defs(&self, def: DefId) -> Vec<DefId> {
        let Some((file, generics)) = self.type_generic_nodes(def) else {
            return Vec::new();
        };
        generics
            .iter()
            .filter_map(|&g| self.def_of(file, g))
            .collect()
    }

    /// A trait's declared generic **type** parameters, in source order.
    pub fn trait_generic_param_defs(&self, trait_def: DefId) -> Vec<DefId> {
        let Some((file, node)) = self.declaration(trait_def) else {
            return Vec::new();
        };
        let ast = &self.asts[&file];
        let NodeKind::TraitType { generics, .. } = ast.node(node).kind.clone() else {
            return Vec::new();
        };
        generics
            .iter()
            .filter(|&&g| matches!(ast.node(g).kind, NodeKind::GenericTypeParam { .. }))
            .filter_map(|&g| self.def_of(file, g))
            .collect()
    }

    /// The generic parameter nodes a `struct` / `enum` / `trait` declares.
    fn type_generic_nodes(&self, def: DefId) -> Option<(FileId, Vec<NodeId>)> {
        let (file, node) = self.declaration(def)?;
        let generics = match &self.asts[&file].node(node).kind {
            NodeKind::StructType { generics, .. }
            | NodeKind::EnumType { generics, .. }
            | NodeKind::TraitType { generics, .. } => generics.clone(),
            _ => return None,
        };
        Some((file, generics))
    }

    /// A function's declared generic parameters — types **and** `const` values —
    /// in source order, which is the order `.<...>` arguments bind to.
    pub fn func_generic_param_defs(&self, def: DefId) -> Vec<DefId> {
        let Some((file, func)) = self.func(def) else {
            return Vec::new();
        };
        let ast = &self.asts[&file];
        let NodeKind::FuncExpr { generics, .. } = &ast.node(func).kind else {
            return Vec::new();
        };
        generics
            .clone()
            .iter()
            .filter(|&&g| {
                matches!(
                    ast.node(g).kind,
                    NodeKind::GenericTypeParam { .. } | NodeKind::GenericConstParam { .. }
                )
            })
            .filter_map(|&g| self.def_of(file, g))
            .collect()
    }

    /// The individual trait nodes of a generic parameter's constraint, which is
    /// either a `+`-separated [`NodeKind::Bounds`] list or a single trait.
    pub fn bound_nodes(&self, file: FileId, constraint: NodeId) -> Vec<NodeId> {
        match self.asts[&file].node(constraint).kind.clone() {
            NodeKind::Bounds { bounds } => bounds,
            _ => vec![constraint],
        }
    }

    // ===< records >===

    /// The declared field names of a record struct, in declaration order.
    pub fn record_field_names(&self, def: DefId) -> Vec<Symbol> {
        let Some((file, node)) = self.declaration(def) else {
            return Vec::new();
        };
        let ast = &self.asts[&file];
        let NodeKind::StructType {
            kind: StructKind::Record(fields),
            ..
        } = ast.node(node).kind.clone()
        else {
            return Vec::new();
        };
        fields
            .iter()
            .filter_map(|&f| match &ast.node(f).kind {
                NodeKind::Field { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    // ===< constants >===

    /// The literal a chain of `::` constants ultimately binds, if it is one.
    ///
    /// Only `Int` and `Float` come back: they are the two whose value has to
    /// travel to the use site, because they are the two that settle on a
    /// *width* there. A string literal is open too, but every type it may become
    /// holds it, so there is nothing to check.
    pub fn const_lit_value(&self, file: FileId, node: NodeId) -> Option<Lit> {
        let mut def = self.resolved_def(file, node)?;
        for _ in 0..16 {
            if self.defs.get(def).kind != DefKind::Const {
                return None;
            }
            let (cfile, rhs) = self.const_binding(def)?;
            match self.asts[&cfile].node(rhs).kind.clone() {
                NodeKind::Lit(l @ (Lit::Int(_) | Lit::Float(_))) => return Some(l),
                NodeKind::Path { .. } => def = self.resolved_def(cfile, rhs)?,
                _ => return None,
            }
        }
        None
    }

    /// The right-hand side of `def`'s `name :: value` binding.
    ///
    /// Stricter than [`Self::declaration`] on purpose: a constant that is not a
    /// binding has no value to read, and answering with the def's own node
    /// would be reading something else.
    pub fn const_binding(&self, def: DefId) -> Option<(FileId, NodeId)> {
        let d = self.defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let NodeKind::ConstBind { rhs, .. } = self.asts.get(&file)?.node(node).kind else {
            return None;
        };
        Some((file, rhs))
    }

    // ===< the two side-table reads every query above rests on >===

    /// The def a declaring node introduced, as collection stamped it.
    pub fn def_of(&self, file: FileId, node: NodeId) -> Option<DefId> {
        self.asts.get(&file)?.meta::<DefMeta>(node).map(|m| m.0)
    }

    /// The def a *use* resolved to, aliases followed.
    pub fn resolved_def(&self, file: FileId, node: NodeId) -> Option<DefId> {
        match self.asts.get(&file)?.meta::<Resolution>(node)? {
            Resolution::Def(d) => Some(self.defs.resolve_alias(d)),
            _ => None,
        }
    }
}
