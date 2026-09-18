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

use crate::common::diagnostic::Diagnostic;
use crate::common::source::{FileId, FileSpan};
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, NodeId, NodeKind};

use super::decl::Decls;
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

/// Build the [`ImplTable`] from every collected, resolved file, checking each
/// impl's **coherence** (§4.8) as it goes.
pub fn build(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    pkg_of: &HashMap<FileId, String>,
    diags: &mut Vec<Diagnostic>,
    files: &[FileId],
) -> ImplTable {
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
                check_coherence(defs, ast, pkg_of, diags, id, &info);
                check_completeness(defs, asts, diags, id, &info);
                table.impls.push(info);
            }
        }
    }
    table
}

// ===< completeness >===

/// Every requirement a trait states must be met by its impl.
///
/// A trait is a promise about what a type can do; an impl that leaves a method
/// out breaks the promise silently. It matters more than it looks, because a
/// `dyn` coercion builds a **vtable** out of exactly this list — a missing
/// method is a hole in a table something will later jump through.
///
/// Three kinds of requirement, and the rule is the same for each: it must be
/// supplied unless the trait itself supplied a default.
///
/// - a **method**, unless the trait gave it a body;
/// - an **associated type**, always (a trait cannot guess it);
/// - an **associated constant**, unless the trait gave it a `:=` default.
///
/// All of them are reported in **one** diagnostic. An impl that forgot four
/// methods made one mistake — it was written against the wrong version of the
/// trait, or it is not finished — and four separate errors would say the same
/// thing four times.
fn check_completeness(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    diags: &mut Vec<Diagnostic>,
    node: NodeId,
    imp: &ImplInfo,
) {
    let Some(trait_def) = imp.trait_def else {
        return;
    };
    // A blanket impl (`impl <T> Trait for T`) is checked where it is written,
    // like any other. Nothing special is needed: its members are its own.
    let mut missing: Vec<String> = Vec::new();
    for (name, &member) in &defs.get(trait_def).ns.members {
        if imp.members.contains_key(name) || imp.assoc.contains_key(name) {
            continue;
        }
        let Some(kind) = Decls::new(defs, asts).requirement(member) else {
            continue;
        };
        let kind = kind.label();
        missing.push(format!("{kind} `{name}`"));
    }
    if missing.is_empty() {
        return;
    }
    // The def table's namespace is a map, so the order it yields is not stable
    // between runs. Sort, or the diagnostic reads differently on every build.
    missing.sort();
    let trait_name = defs.canonical_string(trait_def);
    let list = missing.join(", ");
    let ast = &asts[&imp.file];
    report(
        diags,
        imp.file,
        ast,
        node,
        format!("this `impl` of `{trait_name}` is missing {list}"),
    );
}

// ===< coherence >===

/// Which package a definition belongs to: `Some(name)` for a package's own
/// definitions, `None` for the program being compiled (files reached by path,
/// which are one unit and may implement each other's things freely).
///
/// A definition with no source file is a **synthesized primitive** — `i32`,
/// `bool`, the width-parameterized integers the resolver interns on demand.
/// Those are the language's, which for this purpose means `core`'s: the core
/// library is the one place that may hang methods off them.
fn owner<'a>(defs: &DefTable, pkg_of: &'a HashMap<FileId, String>, def: DefId) -> Option<&'a str> {
    match defs.get(def).file {
        Some(f) => pkg_of.get(&f).map(String::as_str),
        None => Some("core"),
    }
}

/// Check the two rules that keep impls from colliding across libraries (§4.8).
///
/// 1. An **inherent** `impl T { … }` may only be written where `T` is defined.
///    Two packages both adding a `len` to `[]T` — or to someone else's
///    `Widget` — would be an unresolvable clash at every call site, and unlike a
///    trait impl there is no name to qualify it with. This is why `.len()` lives
///    in `core`: `[]T` is `core`'s type, so `core` is the only place it can.
/// 2. A **trait** impl must have something of its own in it: either the trait or
///    the self type belongs to the impl's package. `impl ForeignTrait for
///    ForeignType` is the case two libraries can write identically and neither
///    can be preferred, so it is refused here rather than discovered as an
///    ambiguity in whoever imports both.
///
/// The self type counts as the impl's own when a local type appears **anywhere**
/// in it, not just at its head: `impl FromResidual.<Io> for Result.<T, Cfg>` is
/// this package's business because `Cfg` is, even though `Result` is `core`'s.
fn check_coherence(
    defs: &DefTable,
    ast: &Ast,
    pkg_of: &HashMap<FileId, String>,
    diags: &mut Vec<Diagnostic>,
    node: NodeId,
    imp: &ImplInfo,
) {
    let here = pkg_of.get(&imp.file).map(String::as_str);
    let describe = |pkg: Option<&str>| match pkg {
        Some(p) => format!("package `{p}`"),
        None => "this program".to_string(),
    };
    match imp.trait_def {
        // --- inherent ---
        None => {
            // A structural target (`[]T`, `[N]T`, `*T`, a tuple) has no head def;
            // those types are the language's, so `core` owns them.
            let target = match imp.self_head {
                Some(h) if imp.generics.contains(&h) => {
                    report(
                        diags,
                        imp.file,
                        ast,
                        node,
                        "an inherent `impl` needs a concrete type; a type parameter is not \
                         one this package defines",
                    );
                    return;
                }
                Some(h) => owner(defs, pkg_of, h),
                None => Some("core"),
            };
            if target != here {
                let msg = format!(
                    "an inherent `impl` must live where its type is defined, and this \
                     type belongs to {} (not {})",
                    describe(target),
                    describe(here)
                );
                report(diags, imp.file, ast, node, msg);
            }
        }
        // --- trait ---
        Some(t) => {
            if owner(defs, pkg_of, t) == here {
                return;
            }
            if mentions_local(defs, ast, pkg_of, here, imp, imp.self_node) {
                return;
            }
            let msg = format!(
                "`{}` and this type both belong to other packages: a trait impl must \
                 live where the trait or the type is defined, or two libraries could \
                 write the same one",
                defs.canonical_string(t)
            );
            report(diags, imp.file, ast, node, msg);
        }
    }
}

/// Whether a type expression mentions a type defined in `here` — at its head or
/// among its generic arguments, recursively. The impl's own generic parameters
/// do not count: they stand for whatever the use site picks, including types
/// from anywhere.
fn mentions_local(
    defs: &DefTable,
    ast: &Ast,
    pkg_of: &HashMap<FileId, String>,
    here: Option<&str>,
    imp: &ImplInfo,
    node: NodeId,
) -> bool {
    if let Some(Resolution::Def(d)) = ast.meta::<Resolution>(node) {
        let d = defs.resolve_alias(d);
        if !imp.generics.contains(&d)
            && !matches!(defs.get(d).kind, DefKind::TypeParam | DefKind::ConstParam)
            && owner(defs, pkg_of, d) == here
        {
            return true;
        }
    }
    ast.node(node)
        .kind
        .children()
        .into_iter()
        .any(|c| mentions_local(defs, ast, pkg_of, here, imp, c))
}

fn report(
    diags: &mut Vec<Diagnostic>,
    file: FileId,
    ast: &Ast,
    node: NodeId,
    message: impl Into<String>,
) {
    let span = ast.node(node).span;
    diags.push(Diagnostic::error(message).with_primary(FileSpan::new(file, span), ""));
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

    // `int` / `uint` are family constructors, not types: an impl on `int.<N>` is
    // a **structural** target the same way `[]T` is, so it gets no head def. That
    // also puts it on the right side of the coherence rule below, which already
    // says the structural types belong to `core`.
    let self_head =
        resolved_def(defs, ast, head_of(ast, self_node)).filter(|&d| !defs.get(d).is_int_family());

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
