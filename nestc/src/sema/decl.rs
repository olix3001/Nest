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
use crate::ir::IrId;
use crate::ir::const_eval::ConstValue;
use crate::parser::ast::{Ast, Lit, NodeId, NodeKind, StructKind};

use super::Resolution;
use super::def::{DefId, DefKind, DefTable};
use super::ty::{Ty, TyVarKind};
use super::{DefMeta, Expansion, Signature};

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
    /// A record field, or one position of a tuple struct: its declared type.
    Field(Ty),
    /// An `enum` variant: its payload, each entry named for a record variant
    /// and unnamed for a tuple one.
    Variant(Vec<(Option<Symbol>, Ty)>),
    /// A `distinct T` or a plain type alias.
    Alias(AliasDecl),
    /// A namespace-level `::` constant that names a **value**.
    Const(ConstDecl),
    /// A generic **type** parameter: what bounds it, and what a pinned
    /// projection fixes it to.
    Param(ParamDecl),
    /// An overload set (§4.3): the functions it names, resolved, in the order
    /// they were written.
    ///
    /// A member may itself be a set; it is flattened where the candidates are
    /// asked for rather than here, because the set it names may not have been
    /// recorded yet when this one is.
    Overload(Vec<DefId>),
}

/// One generic type parameter, as the declaration that listed it wrote it.
///
/// Its bounds are on [`Def::param_bounds`] as well, and deliberately: impl
/// selection asks which traits bound a parameter on every trial of every
/// obligation, and that is a question about the def, not about a declaration
/// anyone looked up. What is *here* is everything a bound carries beyond the
/// trait's identity — the arguments it was written with, and what an
/// `<Assoc = T>` binding pinned — because those are types, and a type is not
/// known until inference has run.
///
/// Both exist because a parameter that arrived with a **library** has no syntax
/// tree in this compilation: `<T: Add.<f64>>` is `f64` only if something wrote
/// `f64` down, and the tree that said so is not here (see
/// [`Inferer::bound_trait_args`](super::infer)).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamDecl {
    /// Each bound, as `(trait, the arguments it was written with)`. A bound
    /// with no arguments — the common case — carries an empty list, which is
    /// not the same answer as "not recorded".
    pub bounds: Vec<(DefId, Vec<Ty>)>,
    /// What a **pinned** associated-type parameter *is*: `<T: Holder.<Item =
    /// i32>>` says `T.Item` is `i32` inside the generic body, before any call
    /// site exists. `None` for every parameter nothing pinned.
    pub pinned: Option<Ty>,
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
    /// Whether its first parameter is a **receiver** — a `self`, making it a
    /// method rather than a free function.
    ///
    /// Not derivable from [`params`](FuncDecl::params), which leaves the
    /// receiver out; and the tree that would say so is not in a compilation
    /// that reads this function out of a library. The language server is what
    /// asks: a method is offered after `value.` and a free function is not.
    pub recv: bool,
    /// Its signature, generics left standing: the `Ty::Func` a call site
    /// instantiates and unifies its arguments against.
    ///
    /// `None` when inference could not settle one, which is a function already
    /// reported against — the tree answers for it, as it did before there was a
    /// table.
    pub sig: Option<Ty>,
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
    /// A tuple struct's positional types, in order — empty for everything else.
    ///
    /// A record struct's fields are defs of their own and carry their types
    /// there ([`Decl::Field`]); a tuple struct's positions are defs too, but
    /// the list as a whole is what a construction is checked against, so it is
    /// here as well.
    pub tuple: Vec<Ty>,
}

/// A `distinct T` or a plain type alias.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasDecl {
    /// What a `distinct` type is distinct **from** — the representation whose
    /// methods it inherits (§2.4). `None` for a plain alias, which is not a
    /// type of its own and expands instead.
    pub repr: Option<Ty>,
    /// What a plain alias **expands to**, which is what a use of it means.
    ///
    /// A `distinct` expands to itself and so has `None` here: that is the whole
    /// difference between the two.
    pub expands_to: Option<Ty>,
}

/// A `::` constant that names a value — `A :: 42`, `NAME :: "nest"`, `K :: A`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstDecl {
    /// The type a **use** of it has.
    pub ty: ConstTy,
    /// Its **value**, when it has one that folds without running any code —
    /// see [`Decls::const_value`].
    pub value: Option<ConstValue>,
}

/// What type a use of a constant has, which is not always *a* type.
///
/// A constant bound to a bare literal has none: §2.5 gives it a fresh variable
/// at every use, so `A :: 1` is an `i8` where one caller wants one and an `i64`
/// where the next does. Recording a single [`Ty`] for it would pick one of those
/// and be wrong everywhere else, so what is recorded is the *shape* the use site
/// builds its own type from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConstTy {
    /// A comptime literal: a fresh variable of this kind, per use.
    Comptime(TyVarKind),
    /// Another constant's type, whatever that turns out to be — `B :: A`
    /// inherits `A`'s comptime-ness rather than settling it here.
    Same(DefId),
    /// One concrete type, the same at every use.
    Settled(Ty),
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
    /// An associated constant's declared type — the one every impl's value must
    /// have. `None` for an associated type, which declares no type of its own.
    pub ty: Option<Ty>,
    /// Its **value**, for the same reason and in the same cases as
    /// [`ConstDecl::value`]: `SIZE: u16 :: 4` declares its type and so is
    /// recorded here rather than as a [`ConstDecl`], and `[SIZE]T` in another
    /// package still has to know it is a `4`.
    pub value: Option<ConstValue>,
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
    /// Presence is all *inference* needs: it decides whether the slot may
    /// legally be left empty, and it is known from the syntax, before anything
    /// has been lowered.
    pub default: bool,
    /// **What** it defaults to, which is what *lowering* needs — see
    /// [`ParamDefault`]. `None` until the declaration has been lowered, which
    /// is the pass that works it out.
    pub lowered: Option<ParamDefault>,
}

/// What an omitted argument is filled in with.
///
/// The default itself is IR, not a type, and it is the one thing in this table
/// that is: it was an expression where it was written, it was lowered and
/// checked once at its declaration (§5.2), and a call that omits the argument
/// gets that same expression cloned into it. So what travels is where the
/// lowered expression is — a key into the metadata a library carries — rather
/// than the expression again.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ParamDefault {
    /// `#caller_location`: the one default whose value is *which call site
    /// asked*, so it is built at each call and the declaration's own lowering
    /// of it is never the answer.
    CallerLocation,
    /// An ordinary default, kept on this parameter as
    /// [`ir::DefaultValue`](crate::ir::DefaultValue).
    Value(IrId),
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

    // ===< declared types >===
    //
    // Each is **definition-relative**: a field of `Box.<T>` declared `T` comes
    // back as the type parameter, not as anything a use site substituted. The
    // caller applies the substitution its own use implies, which is the only
    // place that knows it.

    /// The declared type of a record field or of one tuple-struct position.
    pub fn field_ty(&self, field: DefId) -> Option<Ty> {
        match self.table.get(&field)? {
            Decl::Field(t) => Some(t.clone()),
            _ => None,
        }
    }

    /// The declared payload of an `enum` variant, each entry named for a record
    /// variant and unnamed for a tuple one.
    pub fn variant_payload(&self, variant: DefId) -> Option<Vec<(Option<Symbol>, Ty)>> {
        match self.table.get(&variant)? {
            Decl::Variant(p) => Some(p.clone()),
            _ => None,
        }
    }

    /// The declared positional types of a tuple struct.
    pub fn tuple_tys(&self, def: DefId) -> Option<Vec<Ty>> {
        let t = self.type_decl(def)?;
        (!t.tuple.is_empty()).then(|| t.tuple.clone())
    }

    /// What a `distinct` type is distinct **from**.
    pub fn distinct_repr(&self, def: DefId) -> Option<Ty> {
        match self.table.get(&def)? {
            Decl::Alias(a) => a.repr.clone(),
            _ => None,
        }
    }

    /// The functions an overload set names, as its declaration wrote them:
    /// from the table when it was recorded, and otherwise off the tree.
    pub fn overload_members(&self, def: DefId) -> Vec<DefId> {
        if let Some(Decl::Overload(members)) = self.table.get(&def) {
            return members.clone();
        }
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Vec::new();
        };
        let Some(ast) = self.asts.get(&file) else {
            return Vec::new();
        };
        let set = match &ast.node(node).kind {
            crate::parser::ast::NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let crate::parser::ast::NodeKind::OverloadSet { members } = &ast.node(set).kind else {
            return Vec::new();
        };
        members
            .iter()
            .filter_map(|&m| match ast.meta::<crate::sema::Resolution>(m) {
                Some(crate::sema::Resolution::Def(d)) => Some(self.defs.resolve_alias(d)),
                _ => None,
            })
            .collect()
    }

    /// Every function a call through `def` may reach: the set's members with
    /// any set among them flattened into it, each listed once and in order.
    pub fn overload_candidates(&self, def: DefId) -> Vec<DefId> {
        let mut out: Vec<DefId> = Vec::new();
        let mut seen: std::collections::HashSet<DefId> = std::collections::HashSet::new();
        // A set that names itself, directly or through another, would otherwise
        // be walked forever; `seen` is what ends it, and the cycle is reported
        // where the set is checked. Depth first, so a nested set's members sit
        // where the set was written rather than after everything else — the
        // order is what a diagnostic lists them in.
        seen.insert(def);
        self.flatten_overload(def, &mut seen, &mut out);
        out
    }

    fn flatten_overload(
        &self,
        set: DefId,
        seen: &mut std::collections::HashSet<DefId>,
        out: &mut Vec<DefId>,
    ) {
        for m in self.overload_members(set) {
            if !seen.insert(m) {
                continue;
            }
            match self.defs.get(m).kind {
                DefKind::Overload => self.flatten_overload(m, seen, out),
                _ => out.push(m),
            }
        }
    }

    /// Whether an overload set reaches itself through the sets it names.
    pub fn overload_is_cyclic(&self, def: DefId) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut queue: Vec<DefId> = self.overload_members(def);
        while let Some(m) = queue.pop() {
            if m == def {
                return true;
            }
            if !seen.insert(m) || self.defs.get(m).kind != DefKind::Overload {
                continue;
            }
            queue.extend(self.overload_members(m));
        }
        false
    }

    /// The signature of a function: the `Ty::Func` a call instantiates.
    pub fn signature(&self, def: DefId) -> Option<Ty> {
        self.func_decl(def)?.sig.clone()
    }

    /// What a use of this alias means: its expansion for a plain alias, the
    /// type itself for a `distinct`, and nothing for an abstract associated
    /// type — which has no answer until a call site supplies one.
    pub fn expansion(&self, def: DefId) -> Option<Ty> {
        match self.table.get(&def)? {
            Decl::Alias(a) if a.repr.is_some() => Some(Ty::Nominal {
                def,
                args: Vec::new(),
            }),
            Decl::Alias(a) => a.expands_to.clone(),
            _ => None,
        }
    }

    /// The type a use of the constant `def` has — see [`ConstTy`].
    ///
    /// Both shapes of constant answer here. A `::` binding of a value carries
    /// its shape; `#static c: u32 :: 0` and an associated constant **declare** a
    /// type instead, and a declared type is the same at every use.
    pub fn const_ty(&self, def: DefId) -> Option<ConstTy> {
        match self.table.get(&def)? {
            Decl::Const(c) => Some(c.ty.clone()),
            Decl::Assoc(a) => a.ty.clone().map(ConstTy::Settled),
            _ => None,
        }
    }

    /// The **value** of the constant `def`, as the declaration folded it.
    ///
    /// What an array length or a `const` generic argument needs: `[SIZE]T` is a
    /// `[4]T`, and the `4` is in the declaring package. Only a value that folds
    /// out of literals and operators is here — a call cannot be evaluated
    /// before there is IR to evaluate — which is the same limit a use in this
    /// package meets.
    pub fn const_value(&self, def: DefId) -> Option<ConstValue> {
        match self.table.get(&def)? {
            Decl::Const(c) => c.value.clone(),
            Decl::Assoc(a) => a.value.clone(),
            _ => None,
        }
    }

    /// The arguments the bound of `def` naming `trait_def` was written with.
    ///
    /// `None` when nothing recorded this parameter — an older library, or a
    /// parameter this compilation has the tree for and never recorded — which
    /// is the caller's cue to read the tree. An empty `Vec` is a different
    /// answer: the bound was written, and it took no arguments.
    pub fn param_bound_args(&self, def: DefId, trait_def: DefId) -> Option<Vec<Ty>> {
        let Decl::Param(p) = self.table.get(&def)? else {
            return None;
        };
        p.bounds
            .iter()
            .find(|(t, _)| *t == trait_def)
            .map(|(_, args)| args.clone())
    }

    /// What a pinned associated-type parameter stands for — see
    /// [`ParamDecl::pinned`].
    pub fn param_pinned(&self, def: DefId) -> Option<Ty> {
        match self.table.get(&def)? {
            Decl::Param(p) => p.pinned.clone(),
            _ => None,
        }
    }

    /// Whether `def` was recorded as a generic parameter at all.
    pub fn has_param_decl(&self, def: DefId) -> bool {
        matches!(self.table.get(&def), Some(Decl::Param(_)))
    }

    /// The declared type of an associated constant.
    pub fn assoc_const_ty(&self, def: DefId) -> Option<Ty> {
        match self.table.get(&def)? {
            Decl::Assoc(a) => a.ty.clone(),
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

    /// Whether a function's first parameter is a `self` receiver.
    ///
    /// The recorded answer first, as everywhere: the tree this reads is not in
    /// a compilation that got the function out of a library.
    pub fn takes_receiver(&self, def: DefId) -> bool {
        if let Some(f) = self.func_decl(def) {
            return f.recv;
        }
        let Some((file, func)) = self.func(def) else {
            return false;
        };
        let Some(ast) = self.asts.get(&file) else {
            return false;
        };
        let NodeKind::FuncExpr { params, .. } = &ast.node(func).kind else {
            return false;
        };
        params.first().is_some_and(|&p| {
            matches!(&ast.node(p).kind, NodeKind::Param { name, .. } if name.as_str() == "self")
        })
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

    /// What `def`'s `i`-th **value** parameter defaults to, once lowering has
    /// recorded it — the answer a call that omits the argument fills in.
    pub fn param_default(&self, def: DefId, i: usize) -> Option<ParamDefault> {
        self.func_decl(def)?.params.get(i)?.lowered
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
            Some(_) => return None,
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

    /// The associated type or constant `def` declares, if it is one.
    ///
    /// The declared type is left out here and filled in by [`record_types`]:
    /// it is a type expression, and resolving one is inference's work rather
    /// than the syntax pass's.
    fn assoc(&self, def: DefId) -> Option<AssocDecl> {
        let (file, node) = self.declaration(def)?;
        let (kind, answered) = match &self.asts[&file].node(node).kind {
            NodeKind::AssocType { .. } => (Requirement::AssocType, false),
            NodeKind::AssocConst { default, .. } => (Requirement::AssocConst, default.is_some()),
            _ => return None,
        };
        Some(AssocDecl {
            kind,
            answered,
            ty: None,
            value: None,
        })
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
            // The recorded value first: a constant in another package has one
            // and has no right-hand side here to read instead. A chain of `::`
            // constants folded to its end when it was recorded, so there is
            // nothing left to follow.
            match self.const_value(def) {
                Some(ConstValue::Int(n)) => return Some(Lit::Int(n)),
                Some(ConstValue::Float(f)) => return Some(Lit::Float(f)),
                Some(_) => return None,
                None => {}
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
pub fn record(defs: &DefTable, asts: &HashMap<FileId, Ast>, table: &mut DeclTable, file: FileId) {
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
            DefKind::Overload => Decl::Overload(q.overload_members(d.id)),
            DefKind::Func => Decl::Func(FuncDecl {
                params: q
                    .param_names(d.id)
                    .unwrap_or_default()
                    .into_iter()
                    .zip(q.param_defaults(d.id).unwrap_or_default())
                    .map(|(name, default)| Param {
                        name,
                        default,
                        lowered: None,
                    })
                    .collect(),
                generics: q.generic_params(d.id),
                has_body: q.has_body(d.id),
                recv: q.takes_receiver(d.id),
                sig: None,
            }),
            DefKind::Struct | DefKind::Enum | DefKind::Trait => Decl::Type(TypeDecl {
                generics: q.generic_params(d.id),
                fields: q.record_field_names(d.id),
                tuple: Vec::new(),
            }),
            // An associated type or constant, of a trait or of an impl. Both
            // land in `DefKind::Const` or `DefKind::TypeAlias`, and which of
            // the two shapes a def has is a question about its declaration.
            DefKind::Const | DefKind::TypeAlias => match q.assoc(d.id) {
                Some(decl) => Decl::Assoc(decl),
                None => continue,
            },
            // A field's type, a variant's payload and a `distinct`'s
            // representation are type expressions, and resolving one is
            // inference's work — [`record_types`] adds them afterwards. Nothing
            // is written here in their place: an absent entry means "ask the
            // tree", and a placeholder would be an answer that is wrong.
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
            assert_eq!(
                table.generic_arity(d.id),
                tree.generic_arity(d.id),
                "{what}"
            );
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

    /// Every definition that declares a **type** has that type written down.
    ///
    /// Coverage is the property the tree can stop being kept for: a field whose
    /// type never reached the table is one a package compiled against this
    /// library could not ask about at all.
    fn every_declared_type_is_recorded(session: &Session) {
        let mut missing: Vec<String> = Vec::new();
        let mut counts = (0, 0, 0, 0);
        for d in session.defs.iter() {
            let Some(file) = d.file else { continue };
            let Some(ast) = session.asts.get(&file) else {
                continue;
            };
            let Some(node) = d.node else { continue };
            let what = session.defs.canonical_string(d.id);
            match (d.kind, session.decls.get(&d.id)) {
                (DefKind::Field, Some(Decl::Field(_))) => counts.0 += 1,
                (DefKind::Field, _) => missing.push(format!("field {what}")),
                (DefKind::Variant, Some(Decl::Variant(_))) => counts.1 += 1,
                (DefKind::Variant, _) => missing.push(format!("variant {what}")),
                // An associated constant declares a type every impl's value
                // must have; an associated *type* declares none.
                (DefKind::Const, Some(Decl::Assoc(a))) => {
                    if a.kind == Requirement::AssocConst && a.ty.is_none() {
                        missing.push(format!("associated constant {what}"));
                    } else {
                        counts.3 += 1;
                    }
                }
                (DefKind::TypeAlias, entry) => {
                    let rhs = match &ast.node(node).kind {
                        NodeKind::ConstBind { rhs, .. } => *rhs,
                        _ => node,
                    };
                    if !matches!(ast.node(rhs).kind, NodeKind::DistinctType { .. }) {
                        continue;
                    }
                    match entry {
                        Some(Decl::Alias(AliasDecl { repr: Some(_), .. })) => counts.2 += 1,
                        _ => missing.push(format!("distinct {what}")),
                    }
                }
                _ => {}
            }
        }
        assert!(missing.is_empty(), "not recorded: {missing:#?}");
        let (fields, variants, distincts, assocs) = counts;
        assert!(
            fields > 50 && variants > 20 && distincts > 0 && assocs > 0,
            "thin coverage: {fields} fields, {variants} variants, {distincts} distincts, \
             {assocs} associated constants"
        );
    }

    /// Every recorded signature is the one the tree gives.
    ///
    /// This is the query a **call** asks, so it is the one whose answer shows
    /// up in every inferred type in the program: the recorded signature is read
    /// back off what inference settled, and the tree's is resolved from the
    /// syntax a second time, and the two have to be the same type.
    fn signatures_agree_with_the_tree(session: &Session) {
        let mut checked = 0;
        for d in session.defs.iter() {
            let Some(Decl::Func(f)) = session.decls.get(&d.id) else {
                continue;
            };
            let Some(recorded) = f.sig.clone() else {
                continue;
            };
            let tree = crate::sema::infer::signature_from_tree(
                &session.defs,
                &session.asts,
                &session.lang_items,
                &session.impls,
                d.id,
            );
            // A signature the tree cannot resolve on its own is one that needed
            // the context a call site has; there is nothing to compare it with.
            if tree.mentions_error() || tree.mentions_var() {
                continue;
            }
            assert_eq!(
                recorded,
                tree,
                "{}: recorded {}, the tree says {}",
                session.defs.canonical_string(d.id),
                recorded.display(&session.defs),
                tree.display(&session.defs),
            );
            checked += 1;
        }
        assert!(checked > 200, "only {checked} signatures were compared");
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
        every_declared_type_is_recorded(&session);
        signatures_agree_with_the_tree(&session);
    }
}

/// Record the **types** every declaration in `file` declares.
///
/// The second half of [`record`], and separate from it for one reason: a
/// field's type, a variant's payload, a `distinct`'s representation and an
/// associated constant's type are all type *expressions*, and working out what
/// one denotes — an alias, a generic argument, `Self` — is inference's job.
/// Inference has already done it by the time this runs: each of these is
/// stamped on its own node (`Inferer::stamp_member_types`), and this reads the
/// answers back and files them under the definition they belong to.
///
/// A type that still mentions an inference variable is **not** recorded. A
/// variable is a hole in one context and means nothing in another, so an
/// unsettled type is left for the tree to answer as it did before.
pub fn record_types(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    table: &mut DeclTable,
    file: FileId,
) {
    let Some(ast) = asts.get(&file) else { return };
    let settled = |t: Option<Ty>| t.filter(|t| !t.mentions_var());
    // Reads resolutions only — a constant naming another constant needs the def
    // its path resolved to — so the table it is given does not matter.
    let empty = DeclTable::new();
    let q = Decls::new(defs, asts, &empty);
    for d in defs.iter() {
        if d.file != Some(file) {
            continue;
        }
        let Some(node) = d.node else { continue };
        match d.kind {
            // A record field carries its type on the `Field` node; a tuple
            // struct's position has no `Field` node around it, and the def
            // points at the type node itself (see `collect_struct`). Either
            // way it is the node the def points at that was stamped.
            DefKind::Field => {
                if let Some(t) = settled(ast.meta::<Ty>(node)) {
                    table.insert(d.id, Decl::Field(t));
                }
            }
            DefKind::Variant => {
                if let Some(payload) = variant_payload(ast, node, &settled) {
                    table.insert(d.id, Decl::Variant(payload));
                }
            }
            DefKind::Func => {
                let (Some(Decl::Func(f)), Some(sig)) =
                    (table.get(&d.id), settled(signature(ast, node)))
                else {
                    continue;
                };
                // An errored signature is one already reported against, and
                // recording it would hand every later caller the error instead
                // of the diagnostic that explains it.
                if sig.mentions_error() {
                    continue;
                }
                let f = FuncDecl {
                    sig: Some(sig),
                    ..f.clone()
                };
                table.insert(d.id, Decl::Func(f));
            }
            DefKind::TypeAlias | DefKind::Const => {
                let rhs = match &ast.node(node).kind {
                    NodeKind::ConstBind { rhs, .. } => *rhs,
                    _ => node,
                };
                match &ast.node(rhs).kind {
                    NodeKind::DistinctType { inner, .. } => {
                        if let Some(t) = settled(ast.meta::<Ty>(*inner)) {
                            table.insert(
                                d.id,
                                Decl::Alias(AliasDecl {
                                    repr: Some(t),
                                    expands_to: None,
                                }),
                            );
                        }
                    }
                    // Stamped on the `AssocConst` node itself, which is the one
                    // the member's def points at.
                    NodeKind::AssocConst { .. } => {
                        if let (Some(Decl::Assoc(a)), Some(t)) =
                            (table.get(&d.id), settled(ast.meta::<Ty>(rhs)))
                        {
                            let a = AssocDecl {
                                ty: Some(t),
                                ..a.clone()
                            };
                            table.insert(d.id, Decl::Assoc(a));
                        }
                    }
                    // A plain alias, or a `::` binding that names a type:
                    // what a use of it means, worked out once by the pass that
                    // checks it. An entry with nothing in it is an answer too —
                    // it says this binding is not a type.
                    // An associated item already has its entry, and it is not
                    // an alias: leave it alone.
                    _ if table.contains_key(&d.id) => {}
                    _ => {
                        if let Some(Expansion(t)) = ast.meta::<Expansion>(node)
                            && !t.mentions_var()
                            && !t.mentions_error()
                        {
                            table.insert(
                                d.id,
                                Decl::Alias(AliasDecl {
                                    repr: None,
                                    expands_to: Some(t),
                                }),
                            );
                        } else if let Some(ty) = const_ty(&q, ast, file, node, rhs, &settled) {
                            // It names a value, not a type. A use of it asks
                            // what type it has, and that is the one question
                            // about a constant a caller cannot answer from its
                            // own file.
                            table.insert(d.id, Decl::Const(ConstDecl { ty, value: None }));
                        } else {
                            table.insert(
                                d.id,
                                Decl::Alias(AliasDecl {
                                    repr: None,
                                    expands_to: None,
                                }),
                            );
                        }
                    }
                }
            }
            DefKind::Struct => {
                let rhs = match &ast.node(node).kind {
                    NodeKind::ConstBind { rhs, .. } => *rhs,
                    _ => node,
                };
                let NodeKind::StructType {
                    kind: StructKind::Tuple(tys),
                    ..
                } = ast.node(rhs).kind.clone()
                else {
                    continue;
                };
                let recorded: Vec<Ty> = tys
                    .iter()
                    .filter_map(|&t| settled(ast.meta::<Ty>(t)))
                    .collect();
                if recorded.len() != tys.len() {
                    continue;
                }
                if let Some(Decl::Type(t)) = table.get(&d.id) {
                    let t = TypeDecl {
                        tuple: recorded,
                        ..t.clone()
                    };
                    table.insert(d.id, Decl::Type(t));
                }
            }
            _ => {}
        }
    }
}

/// Record each constant's **value**, as the declaration folded it.
///
/// Called with what [`super::infer::fold_const_values`] worked out, for the same
/// reason [`record_types`] is separate from [`record`]: a constant's value is an
/// expression, and folding one is inference's work.
///
/// A constant with no entry is one recorded as something other than a constant —
/// a binding that names a type — and a value it folded to is not about it.
pub fn record_const_values(table: &mut DeclTable, values: Vec<(DefId, ConstValue)>) {
    for (def, value) in values {
        match table.get_mut(&def) {
            Some(Decl::Const(c)) => c.value = Some(value),
            Some(Decl::Assoc(a)) => a.value = Some(value),
            _ => {}
        }
    }
}

/// Record what each generic type parameter's bounds carry, as inference
/// resolved them.
///
/// The bounds' *identity* is written by name resolution, on the parameter's own
/// def; what this adds is the types in them — see [`ParamDecl`].
pub fn record_param_decls(table: &mut DeclTable, params: Vec<(DefId, ParamDecl)>) {
    for (def, decl) in params {
        // `record` wrote nothing for a type parameter, so there is nothing here
        // to overwrite and no guard needed — but if that ever changes, the rule
        // is the same one `record_types` follows: what an earlier pass put
        // there stands.
        table.entry(def).or_insert(Decl::Param(decl));
    }
}

/// Record **what** each parameter defaults to, as lowering worked it out.
///
/// The third and last thing a declaration has written down about it, and the
/// only one that is IR rather than a type: a default is an expression, lowered
/// once against its own declaration (§5.2, [`ir::DefaultValue`]), and a call
/// that omits the argument is given that expression. Before this, a caller
/// lowered it again out of the declaring file's tree — which a caller in
/// another package does not have.
///
/// Called with what [`super::lower::lower_file`] collected while it lowered
/// them, since the [`IrId`]s it hands out are the only record of where each
/// finished expression went.
pub fn record_defaults(table: &mut DeclTable, defaults: Vec<(DefId, Vec<Option<ParamDefault>>)>) {
    for (def, lowered) in defaults {
        let Some(Decl::Func(f)) = table.get(&def) else {
            continue;
        };
        let mut f = f.clone();
        for (p, l) in f.params.iter_mut().zip(lowered) {
            p.lowered = l;
        }
        table.insert(def, Decl::Func(f));
    }
}

/// The signature of the function whose def points at `node`, read back off
/// what inference stamped.
///
/// A function with a body was typed by the per-function pass, which left each
/// parameter's type on its own node and the **return** type on the `FuncExpr`.
/// A trait method has no body and so was never typed that way; its signature
/// was worked out once for the vtable and stamped whole, as a [`Signature`].
fn signature(ast: &Ast, node: NodeId) -> Option<Ty> {
    let func = match &ast.node(node).kind {
        NodeKind::ConstBind { rhs, .. } => *rhs,
        _ => node,
    };
    let NodeKind::FuncExpr { params, .. } = ast.node(func).kind.clone() else {
        return None;
    };
    if let Some(Signature(t)) = ast.meta::<Signature>(func) {
        return Some(t);
    }
    Some(Ty::Func {
        params: params
            .iter()
            .map(|&p| ast.meta::<Ty>(p))
            .collect::<Option<Vec<Ty>>>()?,
        ret: Box::new(ast.meta::<Ty>(func)?),
    })
}

/// The shape of the constant bound by `node`, whose right-hand side is `rhs`.
///
/// Mirrors what `Inferer::const_rhs_ty` reads off the same syntax at a use site,
/// which is the answer this has to reproduce for a caller that has no syntax.
/// `None` for a binding that declares no value type — an `AssocConst` shape
/// declares one and is recorded as an associated constant instead, and an
/// expression whose type never settled is left for the tree to answer.
fn const_ty(
    q: &Decls,
    ast: &Ast,
    file: FileId,
    node: NodeId,
    rhs: NodeId,
    settled: &impl Fn(Option<Ty>) -> Option<Ty>,
) -> Option<ConstTy> {
    match &ast.node(rhs).kind {
        NodeKind::Lit(Lit::Int(_)) => Some(ConstTy::Comptime(TyVarKind::Int)),
        NodeKind::Lit(Lit::Float(_)) => Some(ConstTy::Comptime(TyVarKind::Float)),
        NodeKind::Lit(Lit::Str(_)) => Some(ConstTy::Comptime(TyVarKind::Str)),
        // `-1` / `+1` are still literals for this purpose.
        NodeKind::Unary { operand, .. } => const_ty(q, ast, file, node, *operand, settled),
        // A constant naming another inherits its comptime-ness, so the *other*
        // constant's shape is the answer — worked out at the use site, where the
        // variable it may need belongs.
        NodeKind::Path { .. } => q.resolved_def(file, rhs).map(ConstTy::Same),
        // `#static count: u32 :: 0` (§2.6) declares its type, and the pass above
        // recorded it as an associated constant.
        NodeKind::AssocConst { .. } => None,
        // Anything else has one concrete type, which inference stamped on the
        // binding when it typed the declaration.
        _ => settled(ast.meta::<Ty>(node))
            .filter(|t| !t.mentions_error())
            .map(ConstTy::Settled),
    }
}

/// The stamped payload types of the variant at `node`, or `None` if any of them
/// is missing or unsettled — a half-recorded payload would be worse than none.
fn variant_payload(
    ast: &Ast,
    node: NodeId,
    settled: &impl Fn(Option<Ty>) -> Option<Ty>,
) -> Option<Vec<(Option<Symbol>, Ty)>> {
    use crate::parser::ast::VariantPayload;
    let NodeKind::Variant { payload, .. } = ast.node(node).kind.clone() else {
        return None;
    };
    match payload {
        VariantPayload::None => Some(Vec::new()),
        VariantPayload::Tuple(tys) => tys
            .iter()
            .map(|&t| settled(ast.meta::<Ty>(t)).map(|t| (None, t)))
            .collect(),
        VariantPayload::Record(fields) => fields
            .iter()
            .filter_map(|&f| match ast.node(f).kind.clone() {
                NodeKind::Field { name, .. } => Some((f, name)),
                _ => None,
            })
            .map(|(f, name)| settled(ast.meta::<Ty>(f)).map(|t| (Some(name), t)))
            .collect(),
    }
}
