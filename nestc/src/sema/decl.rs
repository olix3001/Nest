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

use serde::{Deserialize, Serialize};

use crate::common::source::FileId;
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, Lit, NodeId, NodeKind, StructKind};

use super::DefMeta;
use super::Resolution;
use super::def::{DefId, DefKind, DefTable};

/// What a definition declares, as the pass that read its syntax concluded it.
///
/// One per definition that declares anything — a function, a type, a trait's
/// associated item. Everything else (a local, an import, a primitive) declares
/// nothing a use site has to look up, and has no entry.
///
/// This is what a **library** carries in place of the trees it was analyzed
/// from. A question asked of a definition in another package is answered from
/// here or not at all, which is why each variant holds the *answer* rather than
/// a position to go and look one up: the position would point into a file this
/// compilation does not have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Decl {
    /// A `func`, free or associated.
    Func(FuncDecl),
    /// A `struct`, an `enum` or a `trait`.
    Type(TypeDecl),
    /// A trait's associated type or constant.
    Assoc(AssocDecl),
}

/// What a call site needs to know about a function it is calling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuncDecl {
    /// Its **value** parameters, `self` excluded, in declaration order.
    ///
    /// The receiver is left out because it is not one of a call's written
    /// arguments, so these line up with the arguments either way.
    pub params: Vec<Param>,
    /// Its generic parameters — types **and** `const` values — in source order,
    /// which is the order `.<...>` arguments bind to.
    pub generics: Vec<GenericParam>,
    /// Whether it has a body. A trait method without one is a requirement an
    /// impl must satisfy; with one it is a default the impl may inherit.
    pub has_body: bool,
}

/// What a use of a `struct` / `enum` / `trait` needs to know about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeDecl {
    /// The generic parameters it declares, in source order. The number of type
    /// arguments a nominal use carries is this list's length.
    pub generics: Vec<GenericParam>,
    /// A record struct's field names, in declaration order — empty for a tuple
    /// struct, a unit struct, an `enum` or a `trait`.
    ///
    /// Order is the whole point: a literal that spreads a base fills the fields
    /// it did not write, and the IR reads a struct's members positionally, so
    /// two compilations have to agree on it.
    pub fields: Vec<Symbol>,
}

/// A trait's associated type or constant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssocDecl {
    /// Which of the two it is.
    pub kind: Requirement,
    /// Whether the trait already answered it: an associated constant with a
    /// `:=` default has an answer, and an associated type never does — a trait
    /// cannot guess it.
    pub answered: bool,
}

/// One generic parameter, as a declaration lists it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GenericParam {
    /// The def it introduced, when collection gave it one.
    pub def: Option<DefId>,
    /// A `<const N: T>` — a compile-time **value** rather than a type (§5).
    pub value: bool,
}

/// One **value** parameter of a function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Param {
    pub name: Symbol,
    /// Whether it carries a default.
    ///
    /// Only presence, not the expression: a call site never looks at the
    /// default itself. It was type-checked once at the declaration and is
    /// filled in by lowering, so all a caller needs to know is that the slot
    /// may legally be left empty.
    pub default: bool,
}

/// Every definition's [`Decl`], by [`DefId`] — the ones this compilation
/// recorded and the ones its libraries arrived with, in one table.
pub type DeclTable = HashMap<DefId, Decl>;

/// What a trait member asks of an impl — see [`Decls::requirement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// What [`record`] concluded, and what a library arrived with.
    ///
    /// Asked first, and the tree only when it has no answer. A definition this
    /// compilation analyzed has both; one from a library has this alone.
    pub table: &'a DeclTable,
}

impl<'a> Decls<'a> {
    pub fn new(defs: &'a DefTable, asts: &'a HashMap<FileId, Ast>, table: &'a DeclTable) -> Self {
        Self { defs, asts, table }
    }

    /// What `def` declares, if anything recorded it.
    pub fn get(&self, def: DefId) -> Option<&'a Decl> {
        self.table.get(&def)
    }

    fn func_decl(&self, def: DefId) -> Option<&'a FuncDecl> {
        match self.table.get(&def)? {
            Decl::Func(f) => Some(f),
            _ => None,
        }
    }

    fn type_decl(&self, def: DefId) -> Option<&'a TypeDecl> {
        match self.table.get(&def)? {
            Decl::Type(t) => Some(t),
            _ => None,
        }
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
        if let Some(f) = self.func_decl(def) {
            return Some(f.params.iter().map(|p| p.name.clone()).collect());
        }
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
        if let Some(f) = self.func_decl(def) {
            return Some(f.params.iter().map(|p| p.default).collect());
        }
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
        if let Some(f) = self.func_decl(def) {
            return f.has_body;
        }
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
        match self.table.get(&def) {
            Some(Decl::Func(f)) => return (!f.has_body).then_some(Requirement::Method),
            Some(Decl::Assoc(a)) => return (!a.answered).then_some(a.kind),
            Some(Decl::Type(_)) => return None,
            None => {}
        }
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
        if let Some(t) = self.type_decl(def) {
            return t.generics.len();
        }
        self.type_generic_nodes(def).map_or(0, |(_, g)| g.len())
    }

    /// The generic **type**-parameter defs a type def declares, in order.
    pub fn type_param_defs(&self, def: DefId) -> Vec<DefId> {
        if let Some(t) = self.type_decl(def) {
            return t.generics.iter().filter_map(|g| g.def).collect();
        }
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
        if self.defs.get(trait_def).kind != DefKind::Trait {
            return Vec::new();
        }
        if let Some(t) = self.type_decl(trait_def) {
            return t
                .generics
                .iter()
                .filter(|g| !g.value)
                .filter_map(|g| g.def)
                .collect();
        }
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
        if let Some(f) = self.func_decl(def) {
            return f.generics.iter().filter_map(|g| g.def).collect();
        }
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

    /// The generic parameters a def declares, in source order.
    fn generic_params(&self, def: DefId) -> Vec<GenericParam> {
        let Some((file, node)) = self.declaration(def) else {
            return Vec::new();
        };
        let ast = &self.asts[&file];
        let generics = match &ast.node(node).kind {
            NodeKind::StructType { generics, .. }
            | NodeKind::EnumType { generics, .. }
            | NodeKind::TraitType { generics, .. }
            | NodeKind::FuncExpr { generics, .. } => generics.clone(),
            _ => return Vec::new(),
        };
        generics
            .iter()
            .map(|&g| GenericParam {
                def: self.def_of(file, g),
                value: matches!(ast.node(g).kind, NodeKind::GenericConstParam { .. }),
            })
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
        if let Some(t) = self.type_decl(def) {
            return t.fields.clone();
        }
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

/// Record what every definition in `file` declares.
///
/// Runs once per file of the package being compiled, after resolution — which
/// is what a generic parameter's def and a bound's trait are read from — and
/// before anything asks a question of a definition it did not write.
///
/// The answers are the same ones [`Decls`] would have read off the tree, worked
/// out once instead of at each asking. That they are *written down* is the
/// point: a package compiled against this one has the table and not the tree.
pub fn record(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    table: &mut DeclTable,
    file: FileId,
) {
    // Read against an empty table: this is where the entries come from, and a
    // query that consulted a half-filled one would answer differently depending
    // on the order the defs happen to be in.
    let empty = DeclTable::new();
    let q = Decls::new(defs, asts, &empty);
    for d in defs.iter() {
        if d.file != Some(file) {
            continue;
        }
        let decl = match d.kind {
            DefKind::Func => Decl::Func(FuncDecl {
                params: q
                    .param_names(d.id)
                    .unwrap_or_default()
                    .into_iter()
                    .zip(q.param_defaults(d.id).unwrap_or_default())
                    .map(|(name, default)| Param { name, default })
                    .collect(),
                generics: q.generic_params(d.id),
                has_body: q.has_body(d.id),
            }),
            DefKind::Struct | DefKind::Enum | DefKind::Trait => Decl::Type(TypeDecl {
                generics: q.generic_params(d.id),
                fields: q.record_field_names(d.id),
            }),
            // A trait's associated items. `DefKind::Const` covers both, and a
            // `::` constant that is not one has no requirement to record.
            DefKind::Const | DefKind::TypeAlias => match q.requirement(d.id) {
                Some(kind) => Decl::Assoc(AssocDecl {
                    kind,
                    answered: false,
                }),
                None => continue,
            },
            _ => continue,
        };
        table.insert(d.id, decl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sema::session::Session;

    /// Every recorded answer is the one the tree gives.
    ///
    /// This is the property the whole table rests on, and the one that has to
    /// keep holding while the queries move off the tree one at a time: as long
    /// as both can answer, they must agree, or a package compiled against a
    /// library would be compiled against something other than its source.
    fn agrees_with_the_tree(session: &Session) {
        let empty = DeclTable::new();
        let tree = Decls::new(&session.defs, &session.asts, &empty);
        let table = Decls::new(&session.defs, &session.asts, &session.decls);
        let mut checked = 0;
        for d in session.defs.iter() {
            if !session.decls.contains_key(&d.id) {
                continue;
            }
            checked += 1;
            let what = &d.name;
            assert_eq!(table.param_names(d.id), tree.param_names(d.id), "{what}");
            assert_eq!(
                table.param_defaults(d.id),
                tree.param_defaults(d.id),
                "{what}"
            );
            assert_eq!(table.has_body(d.id), tree.has_body(d.id), "{what}");
            assert_eq!(table.requirement(d.id), tree.requirement(d.id), "{what}");
            assert_eq!(table.generic_arity(d.id), tree.generic_arity(d.id), "{what}");
            assert_eq!(
                table.type_param_defs(d.id),
                tree.type_param_defs(d.id),
                "{what}"
            );
            assert_eq!(
                table.trait_generic_param_defs(d.id),
                tree.trait_generic_param_defs(d.id),
                "{what}"
            );
            assert_eq!(
                table.func_generic_param_defs(d.id),
                tree.func_generic_param_defs(d.id),
                "{what}"
            );
            assert_eq!(
                table.record_field_names(d.id),
                tree.record_field_names(d.id),
                "{what}"
            );
        }
        assert!(checked > 100, "only {checked} declarations were recorded");
    }

    #[test]
    fn the_table_answers_what_the_tree_would() {
        // twig over `std` over `core`: the largest program there is, and the one
        // that exercises the most shapes a declaration can have.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../twig/src/main.nest");
        let mut session = Session::new();
        let file = session.load_entry(path).expect("twig's entry loads");
        crate::sema::analyze(&mut session, file);
        assert!(!session.has_errors(), "{:#?}", session.diagnostics);
        agrees_with_the_tree(&session);
    }
}
