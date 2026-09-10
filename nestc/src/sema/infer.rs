//! Type inference (§3.7) — the stage that gives every expression a [`Ty`].
//!
//! It runs **after** desugaring, so `for` / `.?` / `.!` are already gone and the
//! tree is the core control-flow forms. Inference is per **function body**: each
//! `func` with a body gets a fresh [`InferCtxt`], its parameters typed from the
//! signature, its locals typed as they are bound, and every expression node
//! annotated with a resolved [`Ty`] in the arena's metadata side table (read
//! back by [`super::lower`] and the pretty-printer).
//!
//! The engine is Hindley–Milner unification (see [`super::ty`]) plus a **trait
//! solver**: an operator, a method call, or a `.?` raises an [`Obligation`], and
//! selection picks the impl that discharges it — uniformly over user `impl`s and
//! the builtin rows of [`super::builtins`], so `i32 + i32` and `Vec3 + Vec3` go
//! through the same machinery and differ only in which candidate wins.
//!
//! Method resolution runs down a fixed ladder, and *which rung answered* is
//! recorded on the call as a [`MethodRes`], because it is what lowering needs
//! and what the receiver's type alone cannot say:
//!
//! 1. an inherent member of the receiver's own namespace;
//! 2. any `impl` whose target unifies with the receiver — inherent or trait,
//!    which is the only way to reach a method on a structural type like `[]T`;
//! 3. a bound on a generic parameter (`<T: Summing>`) — dispatch waits for
//!    monomorphization;
//! 4. a trait object (`*dyn Summing`) — dispatch waits for the vtable;
//! 5. one `@using` hop to an embedded sub-object (§3.10).
//!
//! What this stage deliberately leaves open: **generics are not
//! monomorphized** — a type parameter is a rigid opaque ([`Ty::Nominal`] over
//! the param's own def), and turbofish / `<Assoc = T>` arguments are recorded
//! but not propagated into a full substitution. Closures are typed by their
//! signature; captures are not threaded into the enclosing body's variables.

use std::collections::{HashMap, HashSet};

use crate::common::diagnostic::Diagnostic;
use crate::common::source::{FileId, FileSpan};
use crate::common::symbol::Symbol;
use crate::common::target::Target;
use crate::parser::ast::{
    Ast, BinOp, Lit, NodeId, NodeKind, SliceRest, UnOp, VariantArgs, VariantPatArgs, WideFloat,
};

use super::builtins::{self, Applies, BuiltinOp, BuiltinRow};
use super::def::{DefId, DefKind, DefTable, LangItems};
use super::impls::{ImplInfo, ImplTable};
use super::ty::{Const, FloatWidth, InferCtxt, Obligation, Ty, TyVarKind, primitive_ty};
use super::{DefMeta, Resolution};

/// Intrinsics that never return, so a call to one types as [`Ty::Never`] rather
/// than a value: it absorbs into whatever position it appears in instead of
/// leaving an unsolvable variable behind.
const DIVERGING_INTRINSICS: &[&str] = &["abort", "panic"];

/// One enclosing `loop` / `while` while its body is being inferred.
struct LoopFrame {
    /// The type its `break`s agree on.
    ty: Ty,
    /// Whether any `break` targeted it. A `loop` with none never finishes, so it
    /// types as [`Ty::Never`] rather than as an unsolved variable.
    broke: bool,
}

/// What an intrinsic's result type is made of.
///
/// Most `$`-intrinsics are generic in one type argument, but only some of them
/// *return* it: `$cast.<T>(x)` is a `T`, while `$new.<T>()` is a `*mut T` and
/// `$size_of.<T>()` is a `usize` regardless of `T` (§6.9, §12).
#[derive(Debug, Clone, Copy, PartialEq)]
enum IntrinsicResult {
    /// The first type argument, unchanged.
    Arg,
    /// `*mut` of the first type argument.
    PtrToArg,
    /// The first type argument, made mutable (`$make.<[]T>(n)` is `[]mut T`).
    MutableArg,
    /// Always `usize` — a size, an alignment, a count.
    Usize,
    /// Always `string`.
    Str,
    /// No value.
    Void,
}

/// The result shape of each known `$`-intrinsic. An intrinsic missing from this
/// table falls back to its first type argument, or to a context-inferred
/// variable when it has none.
const INTRINSIC_RESULTS: &[(&str, IntrinsicResult)] = &[
    ("cast", IntrinsicResult::Arg),
    ("transmute", IntrinsicResult::Arg),
    ("new", IntrinsicResult::PtrToArg),
    ("make", IntrinsicResult::MutableArg),
    ("len", IntrinsicResult::Usize),
    ("size_of", IntrinsicResult::Usize),
    ("align_of", IntrinsicResult::Usize),
    ("name", IntrinsicResult::Str),
    ("embed_file", IntrinsicResult::Str),
    ("assert", IntrinsicResult::Void),
    // The three the collector exposes (§6). Each is a statement, not a value:
    // what they do is change what the collector may do next, which is why none
    // of them hands anything back.
    ("gc_collect", IntrinsicResult::Void),
    ("gc_keep_alive", IntrinsicResult::Void),
    ("gc_pin", IntrinsicResult::Void),
];

/// How an operator (or other trait-dispatched) node resolved, stamped onto the
/// operator's AST node by the trait solver so [`super::lower`] can emit a
/// **uniform** [`crate::ir::Expr::Call`] whether the operand was a primitive or
/// a user type.
///
/// [`builtin`](OpResolution::builtin) is `Some` iff the resolved impl was a
/// builtin primitive op (see [`super::builtins`]); codegen keys on it to emit
/// the machine instruction in O(1) rather than a real call.
#[derive(Debug, Clone, Copy)]
pub struct OpResolution {
    /// The trait method the operator dispatches to (the `#lang` trait's method
    /// for a builtin, the impl's method for a user type).
    pub method: DefId,
    /// The `#lang` trait the operator went through. Kept because [`method`] on a
    /// user impl is the *impl's* member, whose parent is the host type — so it
    /// no longer says which trait was meant, and `Index` vs `IndexMut` is
    /// exactly that question.
    ///
    /// [`method`]: OpResolution::method
    pub trait_def: DefId,
    /// The builtin-op tag, or `None` for a user impl.
    pub builtin: Option<BuiltinOp>,
}

/// How a method call `recv.m(args)` resolved, stamped onto the call's **callee**
/// node (the `recv.m` field access) so [`super::lower`] can build the one shape
/// every call has in the IR: a [`crate::ir::Expr::Call`] whose `args[0]` is the
/// receiver.
///
/// The surface syntax hides three different things behind one dot — an inherent
/// method, a vtable slot, a bound awaiting monomorphization — and only this
/// stage knows which. Rather than leave lowering to re-derive it from the
/// receiver's type, the answer is recorded here in the form the IR wants.
#[derive(Debug, Clone)]
pub struct MethodRes {
    /// The function the call targets: an impl's member for a statically
    /// resolved call, the trait's own declaration for a virtual or generic one.
    pub method: DefId,
    /// How the callee is reached.
    pub dispatch: MethodDispatch,
    /// The adjustment the receiver needs to become the `self` argument.
    pub adjust: RecvAdjust,
    /// The receiver's type *after* the adjustment — i.e. the `self` parameter's
    /// type, which is also the first parameter of the callee's function type.
    pub self_ty: Ty,
}

/// The dispatch kind of a resolved [`MethodRes`], mirroring
/// [`crate::ir::Dispatch`] without the IR's already-substituted types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodDispatch {
    /// A direct call to a known function.
    Static,
    /// Through the vtable of a `*dyn Trait` receiver; the payload is the trait.
    Virtual(DefId),
    /// Through a type parameter's bound; the payload is the trait.
    Generic(DefId),
}

/// What lowering must do to the receiver expression to hand it to the `self`
/// parameter (§3.4). Nest has no implicit reference-taking in the type system —
/// the adjustment is decided here and *written out* in the IR, so `x.m()` on a
/// `*mut Self` method is an `&mut x` the later mutability check can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvAdjust {
    /// The receiver already has the `self` parameter's type.
    None,
    /// Take its address: the method wants `*Self` / `*mut Self` and the receiver
    /// is a value.
    Ref { mutable: bool },
    /// Read through it: the method wants `Self` and the receiver is a pointer.
    Deref,
}

/// Records that a node's value is converted on the way to the type the context
/// wanted — a `comptime_int` literal settling into a runtime integer, say.
///
/// The node keeps its **own** type (`comptime_int`); `to` is what the context
/// asked for. Lowering turns the pair into an explicit `$cast`, so no implicit
/// conversion survives into the IR.
#[derive(Debug, Clone)]
pub struct Coercion {
    /// The type the value is converted to.
    pub to: Ty,
}

/// Records that a `[N]T` was unsized to a `[]T` at this node (§3.2).
///
/// This is not a cast: taking a slice of a whole array is exactly `a[..]`, so
/// lowering emits the `$slice` that spells, with the `.full` range. Keeping it
/// distinct from [`Coercion`] is what stops a `(ptr, len)` view from looking
/// like a reinterpretation of the array's bits.
#[derive(Debug, Clone)]
pub struct SliceCoerce {
    /// The `[]T` produced.
    pub to: Ty,
    /// The `Range.<usize>` the `.full` bound has — the same value an explicit
    /// `a[..]` would build.
    pub range: Ty,
}

/// Records that a `*T` was unsized to a `*dyn Trait` at this node (§3.2): the
/// value becomes a fat pointer pairing the data pointer with `T`'s vtable for
/// the trait.
///
/// The concrete pointee is kept so a later stage can pick the right vtable —
/// that choice is exactly what the coercion erases from the type.
#[derive(Debug, Clone)]
pub struct DynCoerce {
    /// The trait the object is typed as.
    pub trait_def: DefId,
    /// The pointee type being erased.
    pub concrete: Ty,
}

/// Records that an expression reaches its expected type through an `@using`
/// field's implicit upcast (§3.10), attached to the coerced node so
/// [`super::lower`] can make the "take `e.field`" explicit.
///
/// A value upcast copies the sub-object; a pointer upcast takes its address, so
/// [`through_ptr`](Upcast::through_ptr) picks which of the two lowering emits.
#[derive(Debug, Clone)]
pub struct Upcast {
    /// The `@using` field the coercion goes through.
    pub field: DefId,
    /// The receiver is a pointer, so the result is `&base.field`, not `base.field`.
    pub through_ptr: bool,
    /// The type the coercion produces — the field's type, or a pointer to it.
    /// The node's own recorded type stays the *source* type.
    pub target: Ty,
}

/// Stamped on a method call's receiver when the method was found on the type a
/// `distinct` type is distinct *from*, rather than on the distinct type itself
/// (§2.4).
///
/// A `distinct T` has exactly `T`'s representation, so reaching `T`'s method is
/// a reinterpretation and nothing more — but the method's `self` is typed `T`,
/// not the distinct type, so the receiver has to be spelled as `T` before the
/// usual `&` / `.*` adjustment happens. `repr` is that type.
#[derive(Debug, Clone)]
pub struct DistinctRecv {
    pub repr: Ty,
}

/// The arguments of a call in **parameter** order, once any named argument has
/// been bound to the parameter it names (§5.3).
///
/// Only stamped on calls that need it — one that names an argument, or one that
/// leaves a defaulted parameter out. A purely positional call supplying every
/// parameter — the overwhelming majority — carries nothing and lowering takes
/// its arguments as written. Binding happens here, during inference, because
/// this is the one stage that has both the written arguments and the callee's
/// parameter *names*; every later stage then talks about arguments by position
/// alone.
#[derive(Debug, Clone)]
pub struct ArgOrder {
    /// One entry per parameter, in declaration order. A `None` is a parameter
    /// the call left out and whose **default** fills the slot; lowering supplies
    /// it, since only there does the default exist as an `Expr` to clone.
    pub args: Vec<Option<NodeId>>,
}

/// What binding a call's arguments to its parameters produced.
enum ArgBinding {
    /// A purely positional call: use the arguments exactly as written, and let
    /// the ordinary positional checks do the rest.
    AsWritten,
    /// Bound, in parameter order, with a `None` for each defaulted parameter
    /// the call left out.
    Bound(Vec<Option<NodeId>>),
    /// Did not bind, and a diagnostic said why. The arguments must **not** be
    /// checked against the signature afterwards — every such check would be a
    /// second complaint about the same mistake.
    Failed,
}

/// Infer types for every function body in `file`, annotating each expression
/// node with its resolved [`Ty`]. `asts` is the whole parsed program (read-only)
/// so a field access can reach a struct declared in another file.
#[allow(clippy::too_many_arguments)]
pub fn infer_file(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    diags: &mut Vec<Diagnostic>,
    lang: &LangItems,
    impls: &ImplTable,
    prelude_globs: &[DefId],
    file_ns: DefId,
    file: FileId,
    target: Target,
) {
    let ast = &asts[&file];
    // The set of trait defs a use site in this file may select impls of: only
    // in-scope traits are candidates (§ trait selection, Rust-style).
    let in_scope_traits = in_scope_traits(defs, prelude_globs, file_ns);
    // Every `func` with a body is its own inference problem. A bodyless
    // `extern("c") func` joins them: it has no body to check, but it is a real
    // symbol whose *signature* still has to be typed for calls to it — and for
    // the IR, which carries the declaration through (§11.3).
    //
    // A bodyless func that is not `extern` is a trait's requirement, not a
    // definition. Those are typed on demand by `func_def_ty` when a call selects
    // them, which is the only way an abstract `Self.Output` in the signature
    // ever gets a concrete answer.
    let fns: Vec<NodeId> = ast
        .ids()
        .filter(|&id| {
            matches!(
                &ast.node(id).kind,
                NodeKind::FuncExpr { body: Some(_), .. }
                    | NodeKind::FuncExpr {
                        body: None,
                        extern_abi: Some(_),
                        ..
                    }
            )
        })
        .collect();
    // Which `distinct` types stand over a numeric representation (§2.4).
    // Computed once for the whole program, because unification needs the answer
    // and has no def table of its own — see `InferCtxt::set_numeric_distincts`.
    let numeric_distincts = numeric_distincts(defs, asts);
    // What a string literal defaults to, for the same reason: unification
    // decides whether a `comptime_str` variable may become a given type, and
    // `str` is found by `#lang` tag, which unification cannot do.
    let str_ty = str_lang_ty(defs, lang);
    // Each pass below gets its own inference context: a `const` generic solved
    // for one function says nothing about the next.
    macro_rules! fresh {
        () => {
            Inferer {
                defs,
                asts,
                ast,
                diags,
                lang,
                impls,
                in_scope_traits: &in_scope_traits,
                file,
                target,
                cx: {
                    let mut cx = InferCtxt::new();
                    cx.set_numeric_distincts(numeric_distincts.clone());
                    cx.set_str_ty(str_ty.clone());
                    cx
                },
                env: HashMap::new(),
                types: HashMap::new(),
                ret: Ty::Void,
                breaks: Vec::new(),
                alias_stack: Vec::new(),
                const_stack: Vec::new(),
                int_values: HashMap::new(),
            }
        };
    }
    // Declaration-level check, once over the whole file: a `const` generic
    // parameter's *type* is checked where it is written, not where it is used, so
    // one that is declared and never used is still rejected. It cannot ride along
    // with the per-function passes below for the same reason — an `impl`'s
    // generics belong to no function.
    {
        let mut cx = fresh!();
        let const_params: Vec<NodeId> = ast
            .ids()
            .filter(|&id| matches!(&ast.node(id).kind, NodeKind::GenericConstParam { .. }))
            .collect();
        for g in const_params {
            cx.check_const_param(g);
        }
    }
    // Also declaration-level: resolve the declared type of every *member* — a
    // struct field, an enum variant's payload, a `distinct`'s representation —
    // and stamp it on its own node.
    //
    // Only inference can do this: a member's type node may name an alias, a
    // generic argument or `Self`, and resolving those is exactly what
    // `ty_from_node` is. Lowering then reads the answer back rather than
    // reimplementing it, which is how the IR gets type definitions to carry
    // (see [`crate::ir::TypeDef`]).
    {
        let mut cx = fresh!();
        cx.stamp_member_types();
    }
    for func in fns {
        let mut cx = fresh!();
        cx.infer_func(func);
        cx.finish();
    }
    // Every **value definition** this file declares: a namespace constant
    // `A :: 5`, a `#static` region (§2.6), an impl's associated constant, and a
    // trait's associated-constant default. Each is its own little inference
    // problem, and each needs `finish()` for the same reason a function body
    // does — a literal in it has to be defaulted and every node stamped before
    // lowering reads them back.
    //
    // They are checked whether or not anything *uses* them. `def_ty` types a
    // constant lazily, on demand at a use site, which is what gives `A :: 42`
    // its per-use comptime-ness; but a constant nothing mentions would then
    // never be looked at, and the mistake in it is in the declaration.
    let globals: Vec<DefId> = defs
        .iter()
        .filter(|d| d.file == Some(file) && d.kind == DefKind::Const)
        .filter(|d| d.node.is_some_and(|n| binds_a_value(defs, asts, ast, n)))
        .map(|d| d.id)
        .collect();
    for def in globals {
        let mut cx = fresh!();
        cx.infer_global(def);
        cx.finish();
    }
}

/// Whether the `::` binding at `node` defines a value (see
/// [`super::is_value_rhs`]).
fn binds_a_value(defs: &DefTable, asts: &HashMap<FileId, Ast>, ast: &Ast, node: NodeId) -> bool {
    match &ast.node(node).kind {
        NodeKind::ConstBind { rhs, .. } => super::is_value_rhs(defs, asts, ast, *rhs),
        _ => false,
    }
}

/// The `#lang("str")` type, or `None` when the program declares no such item.
///
/// `str` is not a compiler primitive — it is `distinct []u8` declared in core,
/// so that all the slice machinery (interior pointers, bounds, GC tracing) is
/// inherited rather than reimplemented. Found by tag, never by name or path,
/// like every other language item.
fn str_lang_ty(defs: &DefTable, lang: &LangItems) -> Option<Ty> {
    lang.get("str").map(|def| Ty::Nominal {
        def: defs.resolve_alias(def),
        args: Vec::new(),
    })
}

/// Every `distinct` type in the program whose representation is numeric, and
/// which family it belongs to.
///
/// Follows the chain, so a `distinct` over a `distinct` over an integer counts.
/// Resolution has already run, so each step is a def lookup rather than a
/// syntactic guess: the inner type node resolves either to a primitive — in
/// which case its name gives the family — or to another type def to follow.
fn numeric_distincts(defs: &DefTable, asts: &HashMap<FileId, Ast>) -> HashMap<DefId, TyVarKind> {
    /// The inner type node of `def`, if `def` is a `distinct` type.
    fn distinct_inner(
        defs: &DefTable,
        asts: &HashMap<FileId, Ast>,
        def: DefId,
    ) -> Option<(FileId, NodeId)> {
        let d = defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let ast = asts.get(&file)?;
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        match &ast.node(rhs).kind {
            NodeKind::DistinctType { inner, .. } => Some((file, *inner)),
            _ => None,
        }
    }

    let mut out = HashMap::new();
    for d in defs.iter() {
        let Some((mut file, mut inner)) = distinct_inner(defs, asts, d.id) else {
            continue;
        };
        // Walk the chain, with a bound: a `distinct` cycle is a separate error
        // and this pass must not hang on one.
        let mut kind = None;
        for _ in 0..16 {
            let Some(ast) = asts.get(&file) else { break };
            let Some(Resolution::Def(next)) = ast.meta::<Resolution>(inner) else {
                break;
            };
            let next = defs.resolve_alias(next);
            let nd = defs.get(next);
            if nd.kind == DefKind::Primitive {
                kind = match super::ty::primitive_ty(nd.name.as_str()) {
                    Some(Ty::Int { .. }) => Some(TyVarKind::Int),
                    Some(Ty::Float(_)) => Some(TyVarKind::Float),
                    _ => None,
                };
                break;
            }
            match distinct_inner(defs, asts, next) {
                Some((f, i)) => (file, inner) = (f, i),
                None => break,
            }
        }
        if let Some(k) = kind {
            out.insert(d.id, k);
        }
    }
    out
}

/// Gather every trait [`DefId`] nameable from `file_ns` — its own members and
/// imports, each enclosing namespace's, the globs pulled into any of them, and
/// the prelude (builtins + `core`). Mirrors the resolver's unqualified lookup,
/// restricted to traits: this is the candidate filter for impl selection.
pub(crate) fn in_scope_traits(
    defs: &DefTable,
    prelude_globs: &[DefId],
    file_ns: DefId,
) -> HashSet<DefId> {
    let mut set = HashSet::new();
    let add_public = |set: &mut HashSet<DefId>, ns: DefId| {
        for &m in defs.get(ns).ns.members.values() {
            let m = defs.resolve_alias(m);
            if defs.get(m).kind == DefKind::Trait && defs.get(m).vis.is_public() {
                set.insert(m);
            }
        }
    };
    for &g in prelude_globs {
        add_public(&mut set, g);
    }
    let mut cur = Some(file_ns);
    while let Some(n) = cur {
        for &m in defs
            .get(n)
            .ns
            .members
            .values()
            .chain(defs.get(n).ns.imported.values())
        {
            let m = defs.resolve_alias(m);
            if defs.get(m).kind == DefKind::Trait {
                set.insert(m);
            }
        }
        let globs = defs.get(n).ns.globs.clone();
        for g in globs {
            add_public(&mut set, g);
        }
        cur = defs.get(n).parent;
    }
    set
}

struct Inferer<'a> {
    defs: &'a DefTable,
    asts: &'a HashMap<FileId, Ast>,
    ast: &'a Ast,
    diags: &'a mut Vec<Diagnostic>,
    /// The `#lang` registry, for mapping an operator to its trait.
    lang: &'a LangItems,
    /// The whole-program impl index the solver selects over.
    impls: &'a ImplTable,
    /// Traits selectable at this file's use sites (see [`in_scope_traits`]).
    in_scope_traits: &'a HashSet<DefId>,
    file: FileId,
    /// The machine being compiled for. Only `isize` / `usize` depend on it
    /// today, through [`IntWidth::bits`](super::ty::IntWidth::bits).
    target: Target,
    cx: InferCtxt,
    /// Type of each in-scope value def (params, locals) by [`DefId`].
    env: HashMap<super::def::DefId, Ty>,
    /// Per-node type, filled while inferring and finalized in [`Inferer::finish`].
    types: HashMap<NodeId, Ty>,
    /// Return type of the function currently being inferred.
    ret: Ty,
    /// One frame per enclosing `loop` / `while`, innermost last.
    breaks: Vec<LoopFrame>,
    /// Type-alias / associated-type defs currently being expanded, to break
    /// cycles in [`Inferer::expand_alias`].
    alias_stack: Vec<DefId>,
    /// Constants being typed, so a self-referential one cannot recurse forever.
    const_stack: Vec<DefId>,
    /// The exact `comptime_int` behind a node — a literal, or a use of a
    /// constant that is one — so its settled runtime type can be range-checked.
    int_values: HashMap<NodeId, num_bigint::BigInt>,
}

impl Inferer<'_> {
    fn infer_func(&mut self, func: NodeId) {
        let NodeKind::FuncExpr {
            params, ret, body, ..
        } = self.ast.node(func).kind.clone()
        else {
            return;
        };
        // A defaulted parameter must trail the required ones (§5.2): a call
        // supplies its positional arguments left to right, so a hole in the
        // middle could never be filled without naming the ones after it — which
        // would make the default reachable only by a call that names arguments.
        let mut defaulted: Option<Symbol> = None;
        // Collected up front: a default may be written before the parameter it
        // illegally names (`func (a: i32 := b, b: i32)`), so the check cannot
        // rely on the loop having reached that parameter yet.
        let param_defs: HashSet<DefId> = params.iter().filter_map(|p| self.def_of(*p)).collect();
        for p in &params {
            if let NodeKind::Param { name, ty, default } = self.ast.node(*p).kind.clone() {
                let pty = match ty {
                    Some(t) => self.ty_from_node(t),
                    // A bare `self` (or an inferred closure param) gets a var.
                    None => self.cx.fresh(),
                };
                match (&default, &defaulted) {
                    (Some(_), _) => defaulted = Some(name.clone()),
                    (None, Some(prev)) => {
                        let msg = format!(
                            "parameter `{name}` has no default but follows `{prev}`, which does                              — every parameter after a defaulted one must be defaulted too"
                        );
                        self.report(*p, msg);
                    }
                    (None, None) => {}
                }
                // The default is checked here, at the declaration, **once** —
                // not at each call site, which is why a call can fill the hole
                // without re-inferring anything. It is expected against the
                // parameter's own type, so `y: i32 := 0` types the literal as
                // `i32` exactly as a written argument would.
                if let Some(d) = default {
                    self.reject_param_refs_in_default(d, &param_defs);
                    let dty = self.infer_expr(d);
                    self.expect(d, &dty, &pty);
                }
                if let Some(def) = self.def_of(*p) {
                    self.env.insert(def, pty.clone());
                }
                // Record the parameter's type on its node too, so lowering can
                // read it back (params are not value expressions).
                self.types.insert(*p, pty);
            }
        }
        self.ret = ret.map(|t| self.ty_from_node(t)).unwrap_or(Ty::Void);
        let ret = self.ret.clone();
        // Stash the function's return type on the `FuncExpr` node for lowering.
        self.types.insert(func, ret.clone());
        if let Some(b) = body {
            let bty = self.infer_expr(b);
            // The body's tail value is the function's result.
            self.expect_return(b, &bty, &ret);
        }
    }

    /// Discharge the queued trait/projection obligations to a fixpoint, then
    /// finalize every recorded node type (defaulting numeric literals, flagging
    /// genuine ambiguities) and stamp it onto the arena.
    fn finish(&mut self) {
        // Selection is a *search* interleaved with unification: solving one
        // obligation can concretize a variable that lets the next one commit, so
        // run to a fixpoint before finalizing.
        self.solve_to_fixpoint();
        // Any obligation still queued is stuck; report the genuinely
        // unsatisfiable ones (a concrete self with no matching impl).
        let leftover = self.cx.take_obligations();
        for ob in leftover {
            self.report_unsolved(&ob);
        }
        let entries: Vec<(NodeId, Ty)> = self.types.drain().collect();
        for (node, ty) in entries {
            // Inference is now complete enough to trust: a leftover **general**
            // variable is a real "type annotations needed" error (numeric ones
            // still default). One diagnostic per ambiguous node.
            let mut ambiguous = false;
            let resolved = self.cx.finalize(&ty, &mut || ambiguous = true);
            if ambiguous {
                self.report(node, "type annotations needed");
            }
            self.check_float_width(node, &resolved);
            self.check_int_range(node, &resolved);
            let resolved = self.record_comptime_coercion(node, resolved);
            self.ast.set_meta(node, resolved);
        }
        self.finalize_metas();
    }

    /// Resolve the types carried by the per-node facts the stage stamped along
    /// the way — an `@using` target, a method's `self`, an unsized pointee, a
    /// slice coercion's result.
    ///
    /// Each was captured *mid*-inference, when the variables it mentions may not
    /// have been solved yet; the node types above get finalized in the same
    /// sweep, and these have to travel with them or lowering would read a type
    /// that is still a `?3`.
    fn finalize_metas(&mut self) {
        for node in self.ast.ids() {
            if let Some(up) = self.ast.meta::<Upcast>(node) {
                let target = self.cx.finalize(&up.target, &mut || {});
                self.ast.set_meta(node, Upcast { target, ..up });
            }
            if let Some(m) = self.ast.meta::<MethodRes>(node) {
                let self_ty = self.cx.finalize(&m.self_ty, &mut || {});
                self.ast.set_meta(node, MethodRes { self_ty, ..m });
            }
            if let Some(d) = self.ast.meta::<DynCoerce>(node) {
                let concrete = self.cx.finalize(&d.concrete, &mut || {});
                self.ast.set_meta(node, DynCoerce { concrete, ..d });
            }
            if let Some(sc) = self.ast.meta::<SliceCoerce>(node) {
                let to = self.cx.finalize(&sc.to, &mut || {});
                let range = self.cx.finalize(&sc.range, &mut || {});
                self.ast.set_meta(node, SliceCoerce { to, range });
            }
        }
    }

    /// Reject a float literal whose text the parser flagged as outrunning `f64`
    /// ([`WideFloat`]) but whose settled type is `f64` or narrower.
    ///
    /// A float literal is a `comptime_float` — an `f128` — and collapses to
    /// `f64` when nothing pins its width. That collapse is silent and lossless
    /// for ordinary literals; for these it is neither, so the use site has to
    /// ask for an `f80` / `f128` explicitly.
    fn check_float_width(&mut self, node: NodeId, resolved: &Ty) {
        if self.ast.meta::<WideFloat>(node).is_none() {
            return;
        }
        let Ty::Float(w) = resolved else { return };
        if matches!(w, FloatWidth::F80 | FloatWidth::F128) {
            return;
        }
        let msg = format!(
            "float literal is too large or too precise for `{}`; annotate it as `f80` or `f128`",
            resolved.display(self.defs)
        );
        self.report(node, msg);
    }

    // ===< expressions >===

    /// Infer the type of `node`, record it, and return it.
    fn infer_expr(&mut self, node: NodeId) -> Ty {
        let ty = self.infer_expr_uncached(node);
        self.types.insert(node, ty.clone());
        ty
    }

    fn infer_expr_uncached(&mut self, node: NodeId) -> Ty {
        match self.ast.node(node).kind.clone() {
            NodeKind::Block { stmts, tail } => {
                for s in &stmts {
                    self.infer_stmt(*s);
                }
                match tail {
                    Some(t) => self.infer_expr(t),
                    // A tail-less block whose last statement diverges
                    // (`return`/`break`/`continue`) is itself divergent, so it
                    // does not force the enclosing context to `void`.
                    None if stmts.last().is_some_and(|&s| self.diverges(s)) => Ty::Never,
                    None => Ty::Void,
                }
            }
            NodeKind::Lit(lit) => {
                // Keep the literal's exact value so `finish` can check it fits
                // whatever runtime integer type it settles on.
                if let Lit::Int(n) = &lit {
                    self.int_values.insert(node, n.clone());
                }
                self.lit_ty(&lit)
            }
            NodeKind::InterpolatedStr { parts } => {
                for p in parts {
                    self.infer_expr(p);
                }
                self.str_ty()
            }
            NodeKind::Path { .. } => {
                let ty = self.path_ty(node);
                // A use of a `comptime_int` constant carries that constant's
                // value, so it is range-checked at *this* site.
                if let Some(v) = self.const_int_value(node) {
                    self.int_values.insert(node, v);
                }
                ty
            }
            NodeKind::Unary { op, operand } => self.infer_unary(node, op, operand),
            NodeKind::Binary { op, lhs, rhs } => self.infer_binary(node, op, lhs, rhs),
            NodeKind::Tuple { elems } => {
                if elems.is_empty() {
                    Ty::Void
                } else {
                    Ty::Tuple(elems.iter().map(|e| self.infer_expr(*e)).collect())
                }
            }
            NodeKind::FieldAccess { base, name } => {
                // A `namespace.member` access the resolver already linked to a def
                // (a function, type, or const) is typed from that def; a value
                // `place.field` is typed from the base's struct type.
                if let Some(def) = self.resolved_def(node) {
                    return self.def_ty(def);
                }
                let bty = self.infer_expr(base);
                let bty = self.pin_str(&bty);
                if let Some(ft) = self.field_ty(&bty, name.as_str()) {
                    return ft;
                }
                self.no_such_field(node, &bty, name.as_str())
            }
            NodeKind::TupleIndex { base, index } => {
                let bty = self.infer_expr(base);
                if let Ty::Tuple(elems) = self.autoderef(&bty) {
                    if let Some(t) = elems.get(index as usize) {
                        return t.clone();
                    }
                    // Say the arity rather than the type: the elements are often
                    // still unsolved variables at this point, and the count is
                    // the whole of what went wrong.
                    let msg = format!(
                        "index {index} is out of range for a tuple of {} element(s)",
                        elems.len()
                    );
                    self.report(node, msg);
                    return Ty::Error;
                }
                // A tuple struct's positional members are real fields named
                // `0`, `1`, … (see `collect_struct`), so `p.0` outside a tuple
                // is the very lookup a named field access does (§3.3).
                match self.field_ty(&bty, &index.to_string()) {
                    Some(ft) => ft,
                    None => self.no_such_field(node, &bty, &index.to_string()),
                }
            }
            NodeKind::Call { callee, args } => self.infer_call(callee, &args),
            NodeKind::GenericApply { base, args } => {
                // Record any `<Assoc = T>` bindings and infer holes; the base's
                // type carries through (turbofish is not yet a full substitution).
                for a in args {
                    self.record_generic_arg(a);
                }
                self.infer_expr(base)
            }
            NodeKind::Index { base, index } => {
                let bty = self.infer_expr(base);
                let bty = self.pin_str(&bty);
                let ity = self.infer_expr(index);
                match self.autoderef(&bty) {
                    // Indexing the built-in sequences is the language's own: the
                    // element type is right there in the type (§3.2).
                    Ty::Slice { inner, .. } | Ty::Array { inner, .. } => *inner,
                    // Anything else indexes through `Index` (§6.13): the result
                    // is the impl's `Output`, and `a[i]` means `index(&a, i).*`.
                    other => self.infer_index_op(node, other, ity),
                }
            }
            NodeKind::Slice { base, range } => {
                let bty = self.infer_expr(base);
                let rty = self.infer_expr(range);
                // Slice bounds are indices: pin the range's element type to
                // `usize` so an unbounded `a[..]` still has a solved type.
                if let Ty::Nominal { args, .. } = self.cx.shallow(&rty) {
                    if let Some(elem) = args.first() {
                        let elem = elem.clone();
                        self.expect(range, &elem, &Ty::usize());
                    }
                }
                // A sub-slice of anything sliceable is a read-only slice of its
                // element type.
                match self.autoderef(&bty) {
                    Ty::Slice { inner, .. } | Ty::Array { inner, .. } => Ty::Slice {
                        mutable: false,
                        inner,
                    },
                    _ => Ty::Error,
                }
            }
            NodeKind::Deref { base } => {
                let bty = self.infer_expr(base);
                let inner = self.cx.fresh();
                let ptr = Ty::Ptr {
                    mutable: false,
                    inner: Box::new(inner.clone()),
                };
                self.expect(base, &bty, &ptr);
                inner
            }
            NodeKind::MatchExpr { scrutinee, arms } => {
                let sty = self.infer_expr(scrutinee);
                let result = self.cx.fresh();
                for arm in arms {
                    if let NodeKind::MatchArm {
                        pattern,
                        guard,
                        body,
                    } = self.ast.node(arm).kind.clone()
                    {
                        self.bind_pattern(pattern, &sty);
                        if let Some(g) = guard {
                            let gty = self.infer_expr(g);
                            self.expect(g, &gty, &Ty::Bool);
                        }
                        let bty = self.infer_expr(body);
                        self.expect(body, &bty, &result);
                    }
                }
                result
            }
            NodeKind::If { cond, then, els } => {
                let cty = self.infer_expr(cond);
                self.expect(cond, &cty, &Ty::Bool);
                let then_ty = self.infer_expr(then);
                match els {
                    Some(e) => {
                        // Both branches contribute to one result, through a
                        // fresh variable rather than by making the `then`
                        // branch's type the answer. That is what lets `never` be
                        // the identity of the join: a diverging branch absorbs
                        // into the variable and leaves the other to decide it.
                        // Checking the `else` against the `then` instead gave
                        // `if c { panic() } else { 1 }` the type `never`, which
                        // is plainly wrong — it yields `1` half the time.
                        let result = self.cx.fresh();
                        self.expect(then, &then_ty, &result);
                        let else_ty = self.infer_expr(e);
                        self.expect(e, &else_ty, &result);
                        result
                    }
                    // An `if` without `else` yields `void`; the `then` block must too.
                    None => {
                        self.expect(then, &then_ty, &Ty::Void);
                        Ty::Void
                    }
                }
            }
            NodeKind::IfMatch {
                pattern,
                value,
                then,
                els,
            } => {
                let vty = self.infer_expr(value);
                self.bind_pattern(pattern, &vty);
                let then_ty = self.infer_expr(then);
                match els {
                    Some(e) => {
                        // One result variable per join; see `NodeKind::If`.
                        let result = self.cx.fresh();
                        self.expect(then, &then_ty, &result);
                        let else_ty = self.infer_expr(e);
                        self.expect(e, &else_ty, &result);
                        result
                    }
                    None => {
                        self.expect(then, &then_ty, &Ty::Void);
                        Ty::Void
                    }
                }
            }
            NodeKind::Loop { body } => {
                let ty = self.cx.fresh();
                self.breaks.push(LoopFrame { ty, broke: false });
                self.infer_expr(body);
                match self.breaks.pop() {
                    // A `loop` with no `break` never finishes. It has no value
                    // because control never leaves it, which is exactly `never`
                    // (§3.1) — and it is what lets `func () -> never { loop {} }`
                    // be written. Leaving the fresh variable unsolved instead
                    // reported "type annotations needed" for complete code.
                    Some(f) if !f.broke => Ty::Never,
                    Some(f) => f.ty,
                    None => Ty::Void,
                }
            }
            NodeKind::While { cond, body } => {
                let cty = self.infer_expr(cond);
                self.expect(cond, &cty, &Ty::Bool);
                // A `while` is a loop for `break` / `continue`, but it never
                // yields a value, so its breaks must be valueless.
                self.breaks.push(LoopFrame {
                    ty: Ty::Void,
                    broke: false,
                });
                self.infer_expr(body);
                self.breaks.pop();
                Ty::Void
            }
            NodeKind::IntrinsicCall {
                name,
                generic_args,
                args,
            } => {
                let arg_tys: Vec<Ty> = args.iter().map(|&a| self.infer_expr(a)).collect();
                // A diverging intrinsic never yields a value, so it types as
                // `never` and unifies with whatever position it appears in.
                if DIVERGING_INTRINSICS.contains(&name.as_str()) {
                    return Ty::Never;
                }
                // `$len(a)` is the primitive behind the `a.len` sugar, and is
                // callable directly (§3.2); it takes one array or slice.
                if name.as_str() == "len" {
                    self.check_len_intrinsic(node, &args, &arg_tys);
                    return Ty::usize();
                }
                // `$from_residual(r)` is the compiler-internal half of `.?`
                // (§8.3): rebuild the enclosing function's return type from a
                // propagated residual.
                if name.as_str() == "from_residual" {
                    return self.infer_from_residual(node, &args, &arg_tys);
                }
                let arg = generic_args
                    .first()
                    .filter(|&&g| !matches!(self.ast.node(g).kind, NodeKind::TypeHole))
                    .map(|&g| self.ty_from_node(g));
                let shape = INTRINSIC_RESULTS
                    .iter()
                    .find(|(n, _)| *n == name.as_str())
                    .map(|(_, r)| *r)
                    .unwrap_or(IntrinsicResult::Arg);
                match shape {
                    IntrinsicResult::Usize => Ty::usize(),
                    IntrinsicResult::Str => self.str_ty(),
                    IntrinsicResult::Void => Ty::Void,
                    IntrinsicResult::PtrToArg => Ty::Ptr {
                        mutable: true,
                        inner: Box::new(arg.unwrap_or_else(|| self.cx.fresh())),
                    },
                    // `$make.<[]T>(n)` is written with the slice already; it is
                    // the *mutability* the allocation adds.
                    IntrinsicResult::MutableArg => match arg {
                        Some(Ty::Slice { inner, .. }) => Ty::Slice {
                            mutable: true,
                            inner,
                        },
                        Some(t) => t,
                        None => self.cx.fresh(),
                    },
                    // Without a type argument the result is context-inferred.
                    IntrinsicResult::Arg => arg.unwrap_or_else(|| self.cx.fresh()),
                }
            }
            NodeKind::CompositeLit { ty, body } => {
                // Every element is typed on its own first, so the obligation
                // below only has to *unify* them with the target's members.
                self.infer_composite_elems(&body);
                match ty {
                    // `P { ... }` names its type: check the body right away.
                    Some(t) => {
                        let cty = self.ty_from_node(t);
                        self.check_composite_body(node, &cty);
                        cty
                    }
                    // `.{ ... }` gets its type from context — an annotation, a
                    // parameter, a return type — which unification has not seen
                    // yet. Defer the whole body until the variable is solved.
                    None => {
                        let recv = self.cx.fresh();
                        self.cx.register(Obligation::CompositeBody {
                            recv: recv.clone(),
                            origin: node,
                        });
                        recv
                    }
                }
            }
            NodeKind::VariantLit { name, args } => {
                // The enum is only known from context (the expected type), so the
                // result is a fresh variable and each payload argument is tied to
                // the variant's declared payload type by a deferred obligation,
                // discharged once that variable is solved to a `Nominal` enum.
                let arg_tys = self.variant_lit_arg_tys(&args);
                let recv = self.cx.fresh();
                // Registered even with no payload: a unit variant still has to
                // *exist* on whatever enum the context turns out to want.
                self.cx.register(Obligation::VariantPayload {
                    recv: recv.clone(),
                    variant: name,
                    args: arg_tys,
                    origin: node,
                });
                recv
            }
            NodeKind::Arg { value, .. } | NodeKind::FieldInit { value, .. } => {
                self.infer_expr(value)
            }
            NodeKind::Range { start, end, .. } => {
                // A range is a `Range.<T>` over its (unified) endpoint type — a
                // real nominal, so `for x in a..<b` resolves `IntoIterator` on it.
                let elem = self.cx.fresh();
                if let Some(s) = start {
                    let t = self.infer_expr(s);
                    self.expect(s, &t, &elem);
                }
                if let Some(e) = end {
                    let t = self.infer_expr(e);
                    self.expect(e, &t, &elem);
                }
                match self.lang.get("range") {
                    Some(def) => Ty::Nominal {
                        def: self.defs.resolve_alias(def),
                        args: vec![elem],
                    },
                    None => Ty::Error,
                }
            }
            // A closure / nested function used as a value: its type is its
            // signature; its body is inferred independently by the file walker.
            NodeKind::FuncExpr { .. } => self.func_sig_ty(node),
            // Type-forming and declaration nodes are not value expressions.
            _ => Ty::Error,
        }
    }

    /// Infer a composite literal's body, unifying each named field value with the
    /// struct's declared field type when the composite's type is a known nominal.
    /// Type every element of a composite literal's body, without yet relating
    /// them to the target type — that is [`check_composite_body`]'s job, which
    /// may have to wait for the target to be inferred.
    ///
    /// [`check_composite_body`]: Inferer::check_composite_body
    fn infer_composite_elems(&mut self, body: &crate::parser::ast::CompositeBody) {
        use crate::parser::ast::CompositeBody;
        match body {
            CompositeBody::Named(fields) => {
                for &f in fields {
                    if let NodeKind::FieldInit { value, .. } = self.ast.node(f).kind.clone() {
                        self.infer_expr(value);
                    }
                }
            }
            CompositeBody::Positional(elems) => {
                for &e in elems {
                    self.infer_expr(e);
                }
            }
            CompositeBody::Repeat { value, count } => {
                self.infer_expr(*value);
                self.infer_expr(*count);
            }
        }
    }

    // ===< statements >===

    fn infer_stmt(&mut self, node: NodeId) {
        match self.ast.node(node).kind.clone() {
            NodeKind::LocalDecl {
                pattern, ty, value, ..
            } => {
                let vty = self.infer_expr(value);
                let bound = match ty {
                    Some(t) => {
                        let ann = self.ty_from_node(t);
                        self.expect(value, &vty, &ann);
                        ann
                    }
                    None => vty,
                };
                self.bind_pattern(pattern, &bound);
            }
            // A `::` binding in statement position (a block-local const, or a
            // synthetic `__it` / `__try` the desugarer introduced): type its RHS
            // and bind the pattern, exactly like an un-annotated `let`.
            NodeKind::ConstBind { pattern, rhs } => {
                let vty = self.infer_expr(rhs);
                self.bind_pattern(pattern, &vty);
            }
            NodeKind::Assign { place, value, .. } => {
                // `a[i] = v` on a user type is the *write* side of indexing:
                // `IndexMut.index_mut`, not `Index.index` (§6.13). This is the
                // only place that knows the index expression is a place, so the
                // trait swap happens here rather than in `infer_expr`.
                let pty = self.infer_index_place(place);
                let vty = self.infer_expr(value);
                self.expect(value, &vty, &pty);
            }
            NodeKind::Return { value } => {
                let vty = match value {
                    Some(v) => self.infer_expr(v),
                    None => Ty::Void,
                };
                let ret = self.ret.clone();
                let anchor = value.unwrap_or(node);
                self.expect_return(anchor, &vty, &ret);
            }
            NodeKind::Break { value } => {
                let vty = match value {
                    Some(v) => self.infer_expr(v),
                    None => Ty::Void,
                };
                match self.breaks.last_mut() {
                    Some(frame) => {
                        // Record that this loop *can* be left, which is what
                        // decides whether it types as `never`.
                        frame.broke = true;
                        let expected = frame.ty.clone();
                        let anchor = value.unwrap_or(node);
                        self.expect(anchor, &vty, &expected);
                    }
                    None => self.report(node, "`break` outside of a loop"),
                }
            }
            NodeKind::Defer { body } => {
                self.infer_expr(body);
            }
            NodeKind::Continue => {
                if self.breaks.is_empty() {
                    self.report(node, "`continue` outside of a loop");
                }
            }
            // Any other statement position holds an expression.
            _ => {
                self.infer_expr(node);
            }
        }
    }

    // ===< operators >===

    fn infer_unary(&mut self, node: NodeId, op: UnOp, operand: NodeId) -> Ty {
        let oty = self.infer_expr(operand);
        match op {
            UnOp::Ref => Ty::Ptr {
                mutable: false,
                inner: Box::new(oty),
            },
            UnOp::RefMut => Ty::Ptr {
                mutable: true,
                inner: Box::new(oty),
            },
            UnOp::Neg => {
                // `-128` is one `comptime_int`, not a negation of `128`: the
                // range check has to see the sign, or the minimum of every
                // signed type would be rejected.
                if let Some(v) = self.int_values.remove(&operand) {
                    self.int_values.insert(operand, -v);
                }
                self.infer_prefix_op(node, "neg", "neg", oty)
            }
            UnOp::BitNot => self.infer_prefix_op(node, "bitnot", "bitnot", oty),
            // `!` is not a trait: boolean negation is the language's own, on
            // `bool` only (§6.13, "what is not a trait method").
            UnOp::Not => {
                self.expect(operand, &oty, &Ty::Bool);
                Ty::Bool
            }
        }
    }

    /// Type a prefix operator (`-a`, `~a`) through its `#lang` trait, exactly
    /// the way [`Inferer::infer_arith_op`] types a binary one: register the
    /// `Self.Output` projection and return the variable it solves.
    fn infer_prefix_op(&mut self, node: NodeId, lang: &str, method: &str, oty: Ty) -> Ty {
        let Some(trait_def) = self.lang.get(lang) else {
            return oty;
        };
        let trait_def = self.defs.resolve_alias(trait_def);
        let out = self.cx.fresh();
        // Same numeric-core threading as the binaries: a primitive (or still
        // unknown) operand keeps its own type through the operator, so a literal
        // flowing into `-x` is not cut off from what pins `x`.
        if !matches!(self.cx.shallow(&oty), Ty::Nominal { .. }) {
            let _ = self.cx.unify(&out, &oty);
        }
        self.cx.register(Obligation::Projection {
            self_ty: oty,
            trait_def,
            args: Vec::new(),
            assoc: Symbol::new("Output"),
            out: out.clone(),
            origin: node,
            method: Some(Symbol::new(method)),
        });
        out
    }

    /// Type an assignment's place, routing a user-type `a[i]` through
    /// `IndexMut` instead of `Index`. Every other place — including indexing an
    /// array or a slice, which needs no trait — types as an ordinary expression.
    fn infer_index_place(&mut self, place: NodeId) -> Ty {
        let NodeKind::Index { base, index } = self.ast.node(place).kind.clone() else {
            return self.infer_expr(place);
        };
        let bty = self.infer_expr(base);
        let ity = self.infer_expr(index);
        let head = self.autoderef(&bty);
        if matches!(head, Ty::Slice { .. } | Ty::Array { .. } | Ty::Error) {
            let ty = match head {
                Ty::Slice { inner, .. } | Ty::Array { inner, .. } => *inner,
                _ => Ty::Error,
            };
            self.types.insert(place, ty.clone());
            return ty;
        }
        let Some(trait_def) = self.lang.get("index_mut") else {
            return Ty::Error;
        };
        let trait_def = self.defs.resolve_alias(trait_def);
        let out = self.cx.fresh();
        self.cx.register(Obligation::Projection {
            self_ty: head,
            trait_def,
            args: vec![ity],
            assoc: Symbol::new("Output"),
            out: out.clone(),
            origin: place,
            method: Some(Symbol::new("index_mut")),
        });
        self.types.insert(place, out.clone());
        out
    }

    /// Type `a[i]` on a type that is not an array or a slice, through the
    /// `#lang("index")` trait: `Index.<Idx>`'s `Output` is the element type, and
    /// lowering emits `Index.index(&a, i).*` for it (§6.13).
    ///
    /// The write side (`a[i] = v` through `IndexMut`) is decided at the
    /// assignment, which is the only place that knows this node is a *place*;
    /// see [`Inferer::infer_stmt`]'s `Assign` arm.
    fn infer_index_op(&mut self, node: NodeId, base: Ty, index: Ty) -> Ty {
        if matches!(base, Ty::Error) {
            return Ty::Error;
        }
        let Some(trait_def) = self.lang.get("index") else {
            return Ty::Error;
        };
        let trait_def = self.defs.resolve_alias(trait_def);
        let out = self.cx.fresh();
        self.cx.register(Obligation::Projection {
            self_ty: base,
            trait_def,
            args: vec![index],
            assoc: Symbol::new("Output"),
            out: out.clone(),
            origin: node,
            method: Some(Symbol::new("index")),
        });
        out
    }

    fn infer_binary(&mut self, node: NodeId, op: BinOp, lhs: NodeId, rhs: NodeId) -> Ty {
        let lty = self.infer_expr(lhs);
        let rty = self.infer_expr(rhs);
        // An operator dispatches on its operands' types, so a string literal
        // has to have settled on one by now (see [`Inferer::pin_str`]).
        let lty = self.pin_str(&lty);
        let rty = self.pin_str(&rty);
        match op {
            BinOp::And | BinOp::Or => {
                self.expect(lhs, &lty, &Ty::Bool);
                self.expect(rhs, &rty, &Ty::Bool);
                Ty::Bool
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.expect(rhs, &rty, &lty);
                // Comparing a *user* type requires it to implement `Eq` (`==`
                // `!=`) or `Ord` (`<` `<=` `>` `>=`): register a trait bound the
                // solver must witness. Primitives compare directly (no builtin
                // `Eq`/`Ord` impl exists — they are the language's own).
                self.check_cmp_bound(node, op, &lty);
                Ty::Bool
            }
            // Arithmetic, bitwise, and shift binaries all dispatch through
            // their operator trait: the result is the projected `Output` of the
            // selected impl, chosen uniformly for primitives (a builtin row) and
            // user types (an `impl`) — §6.13.
            BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::Rem
            | BinOp::BitAnd
            | BinOp::BitOr
            | BinOp::BitXor
            | BinOp::Shl
            | BinOp::Shr => self.infer_arith_op(node, op, lty, rty),
        }
    }

    /// For a comparison whose operands are a concrete nominal type, require the
    /// corresponding equality/ordering trait via a [`Obligation::Trait`] bound,
    /// and stamp the method the selected impl supplies so lowering emits the
    /// call (`Eq.eq` for `==` / `!=`, `Ord.cmp` for the four relations — §6.13).
    ///
    /// A primitive or still-unknown operand is left alone: comparing the numeric
    /// core is the language's own, and lowering keeps it a primitive
    /// [`crate::ir::Expr::Binary`] rather than routing an `i32 < i32` through a
    /// three-way `cmp` the machine would only have to undo.
    fn check_cmp_bound(&mut self, node: NodeId, op: BinOp, lty: &Ty) {
        if !matches!(self.cx.shallow(lty), Ty::Nominal { .. }) {
            return;
        }
        let (lang, method) = match op {
            BinOp::Eq | BinOp::Ne => ("eq", "eq"),
            _ => ("ord", "cmp"),
        };
        let Some(trait_def) = self.lang.get(lang) else {
            return;
        };
        self.cx.register(Obligation::Trait {
            self_ty: lty.clone(),
            trait_def: self.defs.resolve_alias(trait_def),
            args: Vec::new(),
            origin: node,
            stamp: Some(Symbol::new(method)),
        });
    }

    /// Type an arithmetic operator via its `#lang` operator trait: register a
    /// projection obligation for `Self.Output` and return the (fresh) result
    /// variable, solved once the impl is selected. Operands are assumed
    /// homogeneous (the numeric core and bootstrap operator overloading both
    /// have `Rhs = Self`), matching the pre-trait numeric behavior.
    fn infer_arith_op(&mut self, node: NodeId, op: BinOp, lty: Ty, rty: Ty) -> Ty {
        let Some(trait_def) = self.lang.get(binop_lang(op)) else {
            // No operator trait registered: fall back to primitive typing.
            self.expect(node, &rty, &lty);
            return lty;
        };
        let trait_def = self.defs.resolve_alias(trait_def);
        let out = self.cx.fresh();
        // Numeric-core threading: for a primitive or still-unknown operand both
        // operands are the same type and the result *is* that type
        // (`Output = Self`), so link them eagerly. This keeps a literal's type
        // flowing through a chain of `+`s and back from the return, even while
        // the projection is still deferred.
        //
        // A concrete *nominal* operand is left entirely to the impl: its `Rhs`
        // need not be `Self` (`impl Shl.<i32> for BitSet` shifts by an `i32`)
        // and its `Output` need not be either. Forcing either here would reject
        // every heterogeneous operator before selection got a chance to look.
        // A `distinct` type over a numeric primitive is nominal but reaches a
        // *builtin* impl, which is homogeneous — so it belongs on the primitive
        // side of this test. Without it the literal in `port + 1` never learns
        // it should be a `HttpPort` and quietly defaults to `isize`.
        let l = self.cx.shallow(&lty);
        let homogeneous =
            !matches!(l, Ty::Nominal { .. }) || self.cx.numeric_distinct_kind(&l).is_some();
        if homogeneous {
            self.expect(node, &rty, &lty);
            let _ = self.cx.unify(&out, &lty);
        }
        self.cx.register(Obligation::Projection {
            self_ty: lty,
            trait_def,
            args: vec![rty],
            assoc: Symbol::new("Output"),
            out: out.clone(),
            origin: node,
            method: Some(Symbol::new(binop_method(op))),
        });
        out
    }

    // ===< trait solver: selection, projection, fulfillment >===

    /// Discharge queued obligations, retrying until a full sweep makes no
    /// progress. Solving one obligation can solve a variable that unblocks
    /// another, so a single pass is not enough; a pass that decides nothing new
    /// means the rest are stuck (reported by [`Inferer::report_unsolved`]).
    fn solve_to_fixpoint(&mut self) {
        while self.cx.has_obligations() {
            let obligations = self.cx.take_obligations();
            let mut progressed = false;
            let mut deferred = Vec::new();
            for ob in obligations {
                match self.try_solve(&ob) {
                    Outcome::Solved | Outcome::Failed => progressed = true,
                    Outcome::Deferred => deferred.push(ob),
                }
            }
            for ob in deferred {
                self.cx.register(ob);
            }
            if !progressed {
                break;
            }
        }
    }

    /// Attempt to discharge one obligation: select its impl, and for a
    /// projection also compute and unify the associated type. Returns whether it
    /// was solved, is still blocked on an unsolved variable, or failed (a
    /// diagnostic was reported).
    fn try_solve(&mut self, ob: &Obligation) -> Outcome {
        match ob {
            Obligation::Trait {
                self_ty,
                trait_def,
                args,
                origin,
                stamp,
            } => match self.select(self_ty, *trait_def, args) {
                Select::Ok(Choice::User(i)) => {
                    self.commit_impl(i, self_ty, args);
                    // Record which member the impl supplies, so lowering emits
                    // a call to it (a static trait call has no receiver for
                    // lowering to dispatch on).
                    if let Some(name) = stamp {
                        match self.impls.impls[i].members.get(name).copied() {
                            Some(method) => {
                                self.ast.set_meta(
                                    *origin,
                                    OpResolution {
                                        method,
                                        trait_def: *trait_def,
                                        builtin: None,
                                    },
                                );
                            }
                            None => {
                                let msg = format!("impl does not define `{name}`");
                                self.report(*origin, msg);
                            }
                        }
                    }
                    Outcome::Solved
                }
                Select::Ok(Choice::Builtin(_)) | Select::Error => Outcome::Solved,
                Select::Defer => Outcome::Deferred,
                Select::NoImpl => {
                    self.report_no_impl(*origin, self_ty, *trait_def, args);
                    Outcome::Failed
                }
                Select::Ambiguous => {
                    self.report_ambiguous(*origin, self_ty, *trait_def, args);
                    Outcome::Failed
                }
            },
            Obligation::Projection {
                self_ty,
                trait_def,
                args,
                assoc,
                out,
                origin,
                method,
            } => match self.select(self_ty, *trait_def, args) {
                Select::Ok(choice) => {
                    let assoc_ty = match choice {
                        Choice::Builtin(row) => self.builtin_output(row, self_ty),
                        Choice::User(i) => {
                            let map = self.commit_impl(i, self_ty, args);
                            // The impl is chosen; now hold the operands to the
                            // signature it actually declares. Nothing before
                            // this point could: which `Rhs` / `Idx` applies is a
                            // property of the winning impl, not of the operator.
                            if let Some(name) = method {
                                self.check_op_operands(i, name, self_ty, args, &map, *origin);
                            }
                            self.user_assoc(i, *origin, assoc, &map)
                        }
                    };
                    self.expect(*origin, &assoc_ty, out);
                    if let Some(name) = method {
                        self.stamp_op(*origin, choice, *trait_def, name);
                    }
                    Outcome::Solved
                }
                Select::Error => {
                    let _ = self.cx.unify(out, &Ty::Error);
                    Outcome::Solved
                }
                Select::Defer => Outcome::Deferred,
                Select::NoImpl => {
                    self.report_no_impl(*origin, self_ty, *trait_def, args);
                    let _ = self.cx.unify(out, &Ty::Error);
                    Outcome::Failed
                }
                Select::Ambiguous => {
                    self.report_ambiguous(*origin, self_ty, *trait_def, args);
                    let _ = self.cx.unify(out, &Ty::Error);
                    Outcome::Failed
                }
            },
            Obligation::VariantPayload {
                recv,
                variant,
                args,
                origin,
            } => {
                match self.cx.shallow(recv) {
                    // Enum still unknown: retry once it is solved.
                    Ty::Var(_) => Outcome::Deferred,
                    // Not an enum (or an error): nothing to constrain.
                    base if !matches!(base, Ty::Nominal { .. }) => Outcome::Solved,
                    base => {
                        let (variant, origin) = (variant.clone(), *origin);
                        let args = args.clone();
                        match self.variant_payload(&base, variant.as_str()) {
                            Some(payload) => {
                                // A tuple payload is positional, so its count is
                                // part of the variant's shape.
                                let positional = args.iter().all(|(n, _)| n.is_none());
                                if positional && payload.len() != args.len() {
                                    let msg = format!(
                                        "variant `.{variant}` takes {} value(s) but {} were supplied",
                                        payload.len(),
                                        args.len()
                                    );
                                    self.report(origin, msg);
                                }
                                for (i, (arg_name, arg_ty)) in args.iter().enumerate() {
                                    let target = match arg_name {
                                        Some(n) => payload
                                            .iter()
                                            .find(|(pn, _)| pn.as_ref() == Some(n))
                                            .map(|(_, t)| t.clone()),
                                        None => payload.get(i).map(|(_, t)| t.clone()),
                                    };
                                    match target {
                                        Some(t) => self.expect(origin, arg_ty, &t),
                                        None => {
                                            if let Some(n) = arg_name {
                                                let msg = format!(
                                                    "variant `.{variant}` has no field `{n}`"
                                                );
                                                self.report(origin, msg);
                                            }
                                        }
                                    }
                                }
                            }
                            None => {
                                let msg = format!(
                                    "`{}` has no variant `.{variant}`",
                                    self.cx.resolve(&base).display(self.defs)
                                );
                                self.report(origin, msg);
                            }
                        }
                        Outcome::Solved
                    }
                }
            }
            Obligation::CompositeBody { recv, origin } => {
                let target = self.cx.shallow(recv);
                if is_var(&target) {
                    return Outcome::Deferred;
                }
                let (origin, target) = (*origin, target);
                self.check_composite_body(origin, &target);
                Outcome::Solved
            }
        }
    }

    /// Match a composite literal's body against the type it turned out to have,
    /// unifying every element and reporting a body that does not fit.
    fn check_composite_body(&mut self, node: NodeId, target: &Ty) {
        use crate::parser::ast::CompositeBody;
        let NodeKind::CompositeLit { body, .. } = self.ast.node(node).kind.clone() else {
            return;
        };
        if matches!(target, Ty::Error) {
            return;
        }
        match body {
            CompositeBody::Named(fields) => self.check_record_body(node, target, &fields),
            CompositeBody::Positional(elems) => self.check_positional_body(node, target, &elems),
            CompositeBody::Repeat { value, count } => {
                match self.autoderef(target) {
                    Ty::Array { len, inner, .. } => {
                        let vty = self.node_ty(value);
                        self.expect(value, &vty, &inner);
                        // The count *is* the array's length, so it must be a
                        // compile-time value, and it must be the declared one.
                        let k = self.const_len_in(self.file, count);
                        if !matches!(k, Const::Error) && self.cx.unify_const(&len, &k).is_err() {
                            let msg = format!(
                                "this literal repeats {} time(s) but the array is `[{}]`",
                                k.display(self.defs),
                                self.cx.shallow_const(&len).display(self.defs)
                            );
                            self.report(node, msg);
                        }
                    }
                    Ty::Slice { inner, .. } => {
                        let vty = self.node_ty(value);
                        self.expect(value, &vty, &inner);
                    }
                    _ => self.report(node, "a `value ; count` literal builds an array"),
                }
                // The repeat count is a length, not an element.
                let cty = self.node_ty(count);
                self.expect(count, &cty, &Ty::usize());
            }
        }
    }

    /// `{ name: value, ... }` against a struct: every name must be one of the
    /// struct's fields, and every field must be given exactly once.
    fn check_record_body(&mut self, node: NodeId, target: &Ty, fields: &[NodeId]) {
        let Ty::Nominal { def, .. } = self.autoderef(target) else {
            let msg = format!(
                "`{}` is not a struct, so it cannot be built from named fields",
                self.cx.resolve(target).display(self.defs)
            );
            self.report(node, msg);
            return;
        };
        let mut seen: Vec<Symbol> = Vec::new();
        for &f in fields {
            let NodeKind::FieldInit { name, value } = self.ast.node(f).kind.clone() else {
                continue;
            };
            match self.field_ty(target, name.as_str()) {
                Some(ft) => {
                    let vty = self.node_ty(value);
                    self.expect(value, &vty, &ft);
                }
                None => {
                    let msg = format!(
                        "`{}` has no field `{name}`",
                        self.defs.canonical_string(def)
                    );
                    self.report(f, msg);
                    continue;
                }
            }
            if seen.contains(&name) {
                self.report(f, format!("field `{name}` is given more than once"));
            } else {
                seen.push(name);
            }
        }
        // Every declared field must be initialized.
        let missing: Vec<String> = self
            .record_field_names(def)
            .into_iter()
            .filter(|n| !seen.contains(n))
            .map(|n| format!("`{n}`"))
            .collect();
        if !missing.is_empty() {
            let msg = format!(
                "missing field{} {} in `{}`",
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", "),
                self.defs.canonical_string(def)
            );
            self.report(node, msg);
        }
    }

    /// `{ a, b, ... }` against an array/slice (all one element type), a tuple, or
    /// a tuple struct (positional member types).
    fn check_positional_body(&mut self, node: NodeId, target: &Ty, elems: &[NodeId]) {
        let members: Option<Vec<Ty>> = match self.autoderef(target) {
            // A sized array wants exactly its length; a slice takes any count.
            // A sized array wants exactly its length; an unsolved length (a
            // `[_]T`, or a variable flowing in) is *decided* by this literal.
            Ty::Array { len, inner, .. } => {
                let n = match self.cx.shallow_const(&len) {
                    Const::Value(l) => l as usize,
                    Const::Error => elems.len(),
                    other => {
                        let count = Const::Value(elems.len() as u64);
                        if self.cx.unify_const(&other, &count).is_err() {
                            let msg = format!(
                                "this literal has {} element(s) but the array is `[{}]`",
                                elems.len(),
                                other.display(self.defs)
                            );
                            self.report(node, msg);
                        }
                        elems.len()
                    }
                };
                Some(vec![(*inner).clone(); n])
            }
            Ty::Slice { inner, .. } => Some(vec![(*inner).clone(); elems.len()]),
            Ty::Tuple(ts) => Some(ts),
            Ty::Nominal { .. } => self.tuple_struct_tys(target),
            _ => None,
        };
        let Some(members) = members else {
            let msg = format!(
                "`{}` cannot be built from a positional literal",
                self.cx.resolve(target).display(self.defs)
            );
            self.report(node, msg);
            return;
        };
        if members.len() != elems.len() {
            let msg = format!(
                "this literal has {} element(s) but `{}` needs {}",
                elems.len(),
                self.cx.resolve(target).display(self.defs),
                members.len()
            );
            self.report(node, msg);
        }
        for (e, m) in elems.iter().zip(&members) {
            let ety = self.node_ty(*e);
            self.expect(*e, &ety, m);
        }
    }

    /// The declared field names of a record struct, in declaration order.
    fn record_field_names(&self, def: DefId) -> Vec<Symbol> {
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Vec::new();
        };
        let ast = &self.asts[&file];
        let rhs = match ast.node(node).kind.clone() {
            NodeKind::ConstBind { rhs, .. } => rhs,
            _ => node,
        };
        let NodeKind::StructType {
            kind: crate::parser::ast::StructKind::Record(fields),
            ..
        } = ast.node(rhs).kind.clone()
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

    /// The type already inferred for `node` (composite bodies are walked once,
    /// up front, so every element already has one).
    fn node_ty(&mut self, node: NodeId) -> Ty {
        self.types
            .get(&node)
            .cloned()
            .unwrap_or_else(|| self.infer_expr(node))
    }

    /// Pick the impl of `trait_def` that applies to `self_ty` (with trait
    /// arguments `args`). Builtins and user impls are considered uniformly. A
    /// concrete impl beats a generic (blanket) one; two equally specific matches
    /// are an ambiguity error. An unknown self type defers; a known one with no
    /// candidate is a "does not implement" error. Only [`in_scope`] traits are
    /// candidates.
    ///
    /// [`in_scope`]: Inferer::in_scope_traits
    fn select(&mut self, self_ty: &Ty, trait_def: DefId, args: &[Ty]) -> Select {
        let s = self.cx.shallow(self_ty);
        if matches!(s, Ty::Error) {
            return Select::Error;
        }
        if !self.in_scope_traits.contains(&trait_def) {
            return if is_var(&s) {
                Select::Defer
            } else {
                Select::NoImpl
            };
        }

        // Track the best (highest specificity) match, flagging a tie as
        // ambiguous. Concrete impls (and builtins) score 2; a blanket impl for
        // one of its own generics scores 1.
        let mut best: Option<(u8, Choice)> = None;
        let mut ambiguous = false;
        let consider =
            |score: u8, choice: Choice, best: &mut Option<(u8, Choice)>, ambiguous: &mut bool| {
                match best {
                    Some((bs, _)) if *bs > score => {}
                    Some((bs, _)) if *bs == score => *ambiguous = true,
                    _ => {
                        *best = Some((score, choice));
                        *ambiguous = false;
                    }
                }
            };

        if let Some(row) = self
            .builtin_row_for_trait(trait_def)
            .filter(|r| self.builtin_matches(r, &s))
        {
            consider(2, Choice::Builtin(row), &mut best, &mut ambiguous);
        }
        let candidates: Vec<usize> = (0..self.impls.impls.len())
            .filter(|&i| self.impls.impls[i].trait_def == Some(trait_def))
            .collect();
        for i in candidates {
            let generic = self.impls.impls[i].self_is_generic();
            if self.trial_impl(i, &s, args) {
                let score = if generic { 1 } else { 2 };
                consider(score, Choice::User(i), &mut best, &mut ambiguous);
            }
        }

        // Nothing fit directly. A `distinct` type then falls back to the type it
        // is distinct from (§2.4), inheriting its trait impls the way it
        // inherits its methods.
        //
        // Running this **only when the direct search found nothing** is what
        // gives the distinct type's own impl priority, with no scoring rule
        // needed. And note what is *not* substituted: `Self` stays bound to the
        // distinct type, so a builtin's `Output = Self` yields the distinct type
        // rather than the representation — `Meters + Meters` is `Meters`, which
        // is the whole reason `distinct` exists (§2.4). Only the *matching* is
        // done against the representation.
        if best.is_none() && !is_var(&s) {
            if let Some(repr) = self.distinct_repr(&s) {
                let repr = self.cx.shallow(&repr);
                if let Some(row) = self
                    .builtin_row_for_trait(trait_def)
                    .filter(|r| self.builtin_matches(r, &repr))
                {
                    // Trait arguments are deliberately *not* checked here, for
                    // the same reason the primitive path does not check them:
                    // `infer_arith_op` has already linked the operands eagerly
                    // for anything that reaches a builtin, so a mismatched `Rhs`
                    // has been reported once already. Re-checking it would make
                    // `Meters + f64` report twice where `i32 + f64` reports once.
                    consider(2, Choice::Builtin(row), &mut best, &mut ambiguous);
                }
                let inherited: Vec<usize> = (0..self.impls.impls.len())
                    .filter(|&i| self.impls.impls[i].trait_def == Some(trait_def))
                    .collect();
                for i in inherited {
                    let generic = self.impls.impls[i].self_is_generic();
                    if self.trial_impl(i, &repr, args) {
                        let score = if generic { 1 } else { 2 };
                        consider(score, Choice::User(i), &mut best, &mut ambiguous);
                    }
                }
            }
        }

        match best {
            // Several impls fit only because the self type is still unknown:
            // that is a question inference has not answered yet, not a genuine
            // ambiguity. Retry once something pins it down.
            Some(_) if ambiguous && is_var(&s) => Select::Defer,
            Some(_) if ambiguous => Select::Ambiguous,
            Some((_, choice)) => Select::Ok(choice),
            None if is_var(&s) => Select::Defer,
            None => Select::NoImpl,
        }
    }

    /// Speculatively unify a candidate impl's self type (and trait args) with
    /// the obligation, rolling back afterwards; returns whether it fit.
    fn trial_impl(&mut self, i: usize, s: &Ty, args: &[Ty]) -> bool {
        let imp = self.impls.impls[i].clone();
        let snap = self.cx.snapshot();
        let map = self.fresh_impl_map(&imp.generics);
        let impl_self = self.impl_self_ty(&imp, &map);
        let mut ok = !matches!(impl_self, Ty::Error) && self.cx.unify(s, &impl_self).is_ok();
        if ok && !imp.trait_args.is_empty() && imp.trait_args.len() == args.len() {
            for (&node, a) in imp.trait_args.iter().zip(args) {
                let t = self.ty_from_node_in(imp.file, node);
                let t = self.subst_type_params(&t, &map);
                if self.cx.unify(&t, a).is_err() {
                    ok = false;
                    break;
                }
            }
        }
        self.cx.rollback(snap);
        ok
    }

    /// Commit the chosen impl for real (no rollback), binding its generics; the
    /// returned map (impl generic → solved type) drives associated-type
    /// projection.
    fn commit_impl(&mut self, i: usize, self_ty: &Ty, args: &[Ty]) -> Subst {
        let imp = self.impls.impls[i].clone();
        let map = self.fresh_impl_map(&imp.generics);
        let impl_self = self.impl_self_ty(&imp, &map);
        let _ = self.cx.unify(self_ty, &impl_self);
        if !imp.trait_args.is_empty() && imp.trait_args.len() == args.len() {
            for (&node, a) in imp.trait_args.iter().zip(args) {
                let t = self.ty_from_node_in(imp.file, node);
                let t = self.subst_type_params(&t, &map);
                let _ = self.cx.unify(&t, a);
            }
        }
        map
    }

    /// Build the impl's self [`Ty`] with its generics substituted by `map`.
    fn impl_self_ty(&mut self, imp: &ImplInfo, map: &Subst) -> Ty {
        let raw = self.ty_from_node_in(imp.file, imp.self_node);
        self.subst_type_params(&raw, map)
    }

    /// A fresh inference variable per impl generic parameter — a type variable
    /// for a `<T>`, a const variable for a `<const N>`.
    fn fresh_impl_map(&mut self, generics: &[DefId]) -> Subst {
        let mut map = Subst::default();
        for &g in generics {
            if self.defs.get(g).kind == DefKind::ConstParam {
                let k = self.cx.fresh_const();
                map.consts.insert(g, k);
            } else {
                let t = self.cx.fresh();
                map.tys.insert(g, t);
            }
        }
        map
    }

    /// Unify an operator's operands with the parameters of the method the
    /// selected impl supplies.
    ///
    /// The receiver goes through [`Inferer::unify_self_param`] because an
    /// operator's `self` may be declared by pointer (`Index.index(self: *Self,
    /// …)`) while the obligation's self type is the value. The remaining
    /// parameters line up with the obligation's trait arguments — the `rhs` of a
    /// binary, the index of an `a[i]` — and are checked strictly, so
    /// `bits << "x"` is a type error against the impl's own `Rhs`.
    fn check_op_operands(
        &mut self,
        i: usize,
        name: &Symbol,
        self_ty: &Ty,
        args: &[Ty],
        map: &Subst,
        origin: NodeId,
    ) {
        let Some(&m) = self.impls.impls[i].members.get(name) else {
            return;
        };
        let sig = self.func_def_ty(m);
        let sig = self.subst_type_params(&sig, map);
        let Ty::Func { params, .. } = self.cx.shallow(&sig) else {
            return;
        };
        if let Some(p) = params.first() {
            let p = p.clone();
            self.unify_self_param(&p, self_ty);
        }
        for (p, a) in params.iter().skip(1).zip(args) {
            self.expect(origin, a, p);
        }
    }

    /// The associated type `assoc` a user impl binds, with the impl's generics
    /// substituted. Reports if the impl fails to bind it.
    fn user_assoc(&mut self, i: usize, origin: NodeId, assoc: &Symbol, map: &Subst) -> Ty {
        let imp = self.impls.impls[i].clone();
        match imp.assoc.get(assoc) {
            Some(&node) => {
                let t = self.ty_from_node_in(imp.file, node);
                self.subst_type_params(&t, map)
            }
            None => {
                self.report(
                    origin,
                    format!("impl does not define associated type `{assoc}`"),
                );
                Ty::Error
            }
        }
    }

    /// Stamp how an operator resolved onto its node, so lowering emits a uniform
    /// call (builtin-tagged for primitives).
    fn stamp_op(&mut self, origin: NodeId, choice: Choice, trait_def: DefId, name: &Symbol) {
        let (method, builtin) = match choice {
            // A builtin's callee is the `#lang` trait's own declaration: there
            // is no impl to point at, and the [`BuiltinOp`] tag is what codegen
            // reads anyway.
            Choice::Builtin(row) => (
                self.defs
                    .get(trait_def)
                    .ns
                    .members
                    .get(&Symbol::new(row.method))
                    .copied(),
                Some(row.op),
            ),
            Choice::User(i) => (self.impls.impls[i].members.get(name).copied(), None),
        };
        if let Some(method) = method {
            self.ast.set_meta(
                origin,
                OpResolution {
                    method,
                    trait_def,
                    builtin,
                },
            );
        }
    }

    /// The associated `Output` a builtin row produces for `self_ty`.
    fn builtin_output(&self, row: &BuiltinRow, self_ty: &Ty) -> Ty {
        match row.output {
            builtins::OutputRule::SameAsSelf => self.cx.shallow(self_ty),
        }
    }

    /// The builtin operator row a trait carries via its `#lang` tag, if any.
    fn builtin_row_for_trait(&self, trait_def: DefId) -> Option<&'static BuiltinRow> {
        let lang = self.defs.get(trait_def).lang.as_ref()?;
        builtins::row_for_lang(lang.as_str())
    }

    /// Whether a builtin row applies to a (shallow) self type — a concrete
    /// primitive of the right family, or a numeric literal variable of that
    /// family (still un-defaulted, but already known to become one).
    fn builtin_matches(&self, row: &BuiltinRow, self_shallow: &Ty) -> bool {
        if row.applies.matches(self_shallow) {
            return true;
        }
        match self.cx.var_kind(self_shallow) {
            Some(TyVarKind::Int) => matches!(row.applies, Applies::Int | Applies::Numeric),
            Some(TyVarKind::Float) => matches!(row.applies, Applies::Float | Applies::Numeric),
            _ => false,
        }
    }

    /// Report the obligations still stuck after the fixpoint. A concrete self
    /// with no impl is a real error; a self still unknown is suppressed here (its
    /// operand surfaces as "type annotations needed"), and the projection result
    /// is pinned to `Error` so it does not cascade.
    fn report_unsolved(&mut self, ob: &Obligation) {
        let (self_ty, trait_def, args, origin, out) = match ob {
            Obligation::Trait {
                self_ty,
                trait_def,
                args,
                origin,
                ..
            } => (self_ty.clone(), *trait_def, args.clone(), *origin, None),
            Obligation::Projection {
                self_ty,
                trait_def,
                args,
                origin,
                out,
                ..
            } => (
                self_ty.clone(),
                *trait_def,
                args.clone(),
                *origin,
                Some(out.clone()),
            ),
            // A variant or composite literal whose type was never determined:
            // the result variable itself surfaces as "type annotations needed"
            // in finalize, so there is nothing extra to say here.
            Obligation::VariantPayload { .. } | Obligation::CompositeBody { .. } => return,
        };
        let s = self.cx.shallow(&self_ty);
        if !is_var(&s) {
            self.report_no_impl(origin, &self_ty, trait_def, &args);
        }
        if let Some(o) = out {
            let _ = self.cx.unify(&o, &Ty::Error);
        }
    }

    fn report_no_impl(&mut self, node: NodeId, self_ty: &Ty, trait_def: DefId, args: &[Ty]) {
        let s = self.cx.resolve(self_ty);
        let msg = format!(
            "`{}` does not implement `{}`",
            s.display(self.defs),
            self.trait_string(trait_def, args)
        );
        self.report(node, msg);
    }

    fn report_ambiguous(&mut self, node: NodeId, self_ty: &Ty, trait_def: DefId, args: &[Ty]) {
        let s = self.cx.resolve(self_ty);
        let msg = format!(
            "multiple applicable impls of `{}` for `{}`",
            self.trait_string(trait_def, args),
            s.display(self.defs)
        );
        self.report(node, msg);
    }

    /// A trait with its arguments, as a use site writes it — the arguments are
    /// often the whole point of the diagnostic (`FromResidual.<IoError>` says
    /// *which* residual has no conversion).
    fn trait_string(&self, trait_def: DefId, args: &[Ty]) -> String {
        let name = self.defs.canonical_string(trait_def);
        if args.is_empty() {
            return name;
        }
        let inner = args
            .iter()
            .map(|a| self.cx.resolve(a).display(self.defs))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{name}.<{inner}>")
    }

    // ===< calls >===

    fn infer_call(&mut self, callee: NodeId, args: &[NodeId]) -> Ty {
        // `f.<T>(x)` / `recv.m.<T>()` — peel the turbofish. Resolution runs on
        // the callee it wraps; the explicit arguments only change how the
        // resolved signature is instantiated, so they ride along as `targs`.
        let (callee, targs) = match self.ast.node(callee).kind.clone() {
            NodeKind::GenericApply { base, args: targs } => (base, targs),
            _ => (callee, Vec::new()),
        };
        // A call whose callee names a type is a construction, not a function call.
        if let Some(def) = self.callee_type_def(callee) {
            let nominal = self.nominal_of(def);
            self.check_construction(callee, &nominal, args);
            self.types.insert(callee, nominal.clone());
            return nominal;
        }
        // A method call `recv.method(args)` on a value receiver (one the resolver
        // did not link to a namespace member): resolve `method` against the
        // receiver's nominal type and instantiate its generics.
        if let NodeKind::FieldAccess { base, name } = self.ast.node(callee).kind.clone() {
            if self.resolved_def(callee).is_none() {
                let recv = self.infer_expr(base);
                let recv = self.pin_str(&recv);
                // Inherent (or trait-impl) method already collected into the
                // receiver type's namespace: the fast path.
                if let Some(m) = self.method_def(&recv, name.as_str()) {
                    return self.infer_method_call(
                        callee,
                        &recv,
                        m,
                        MethodDispatch::Static,
                        args,
                        &targs,
                    );
                }
                // Otherwise search every impl whose self type unifies with the
                // receiver — the only way to reach a method on a structural
                // receiver (`[]T`, `[N]T`, a range), whose impl parks its
                // members outside any nominal namespace.
                if let Some(m) = self.impl_method_def(&recv, name.as_str()) {
                    // The impl was selected right here, so this is a direct
                    // call even though the method came from a trait.
                    return self.infer_method_call(
                        callee,
                        &recv,
                        m,
                        MethodDispatch::Static,
                        args,
                        &targs,
                    );
                }
                // A method on a bounded type parameter resolves in the bound:
                // `<I: Summing>` makes `it.total()` mean `Summing.total`, with
                // the concrete impl picked once `I` is instantiated.
                if let Some(m) = self.bound_method_def(&recv, name.as_str()) {
                    let d = self.method_dispatch(m, MethodDispatch::Generic);
                    return self.infer_method_call(callee, &recv, m, d, args, &targs);
                }
                // A method on a trait object resolves in the trait itself; which
                // impl runs is a vtable lookup a later stage performs.
                if let Some(m) = self.dyn_method_def(&recv, name.as_str()) {
                    let d = self.method_dispatch(m, MethodDispatch::Virtual);
                    return self.infer_method_call(callee, &recv, m, d, args, &targs);
                }
                // Last, the one ergonomic exception `@using` grants (§3.10): a
                // method the outer struct does not have resolves on the upcast
                // target, with the receiver bound to the embedded sub-object.
                if let Some((m, up)) = self.using_method_def(&recv, name.as_str()) {
                    self.ast.set_meta(base, up.clone());
                    return self.infer_method_call(
                        callee,
                        &up.target,
                        m,
                        MethodDispatch::Static,
                        args,
                        &targs,
                    );
                }
                // A `distinct` type inherits the methods of the type it is
                // distinct from (§2.4). Last in the chain, so anything the
                // distinct type declares itself takes priority.
                if let Some((m, repr)) = self.distinct_method_def(&recv, name.as_str()) {
                    self.ast.set_meta(base, DistinctRecv { repr: repr.clone() });
                    return self.infer_method_call(
                        callee,
                        &repr,
                        m,
                        MethodDispatch::Static,
                        args,
                        &targs,
                    );
                }
                // Nothing found. A field holding a function is still a valid
                // callee, so only complain when there is no such member at all.
                if self.field_ty(&recv, name.as_str()).is_none() {
                    let r = self.cx.resolve(&recv);
                    if !matches!(r, Ty::Error) && !is_var(&r) {
                        for a in args {
                            self.infer_expr(*a);
                        }
                        let msg = format!("no method `{name}` on `{}`", r.display(self.defs));
                        self.report(callee, msg);
                        return Ty::Error;
                    }
                }
            }
        }
        // A direct function call: build the signature and instantiate its generic
        // type parameters — with whatever the turbofish pinned, and a fresh
        // variable for every parameter it did not, so each call site infers its
        // own type arguments (Rust-style).
        if let Some(def) = self.resolved_def(callee) {
            if self.defs.get(def).kind == DefKind::Func {
                let sig = self.func_def_ty(def);
                let (inst, map) = self.instantiate_parts(&sig, def, &targs);
                // `Trait.member(args)` — a trait method named through the trait
                // rather than called on a value. Nothing here says what `Self`
                // is, so it becomes a variable the context solves.
                let inst = self.open_trait_self(callee, def, &inst, &map);
                self.types.insert(callee, inst.clone());
                // Named arguments are bound to their parameters here, so
                // `apply_call` — and every stage after it — sees one positional
                // list in declaration order.
                let args = match self.bind_args(callee, def, args) {
                    ArgBinding::AsWritten => args.iter().copied().map(Some).collect(),
                    ArgBinding::Bound(a) => a,
                    ArgBinding::Failed => {
                        self.infer_args_only(args);
                        return match self.cx.shallow(&inst) {
                            Ty::Func { ret, .. } => *ret,
                            _ => Ty::Error,
                        };
                    }
                };
                return self.apply_call(callee, &inst, &args);
            }
        }
        let cty = self.infer_expr(callee);
        self.reject_named_args(
            args,
            "this call goes through a function value, which has parameter types but no parameter names",
        );
        let slots: Vec<Option<NodeId>> = args.iter().copied().map(Some).collect();
        self.apply_call(callee, &cty, &slots)
    }

    /// `Pair(1, 2)` — a call whose callee names a type builds a value of it
    /// (§3.3). Only a **tuple** struct is built this way: a record struct is
    /// built with a composite literal, whose fields are checked where the
    /// literal is inferred, and a unit struct takes no arguments at all.
    ///
    /// The positional arguments are checked against the declared field types,
    /// so `Pair(1, 2)` on a `struct (i32, i32)` pins its literals to `i32`
    /// rather than leaving them at the default integer type.
    fn check_construction(&mut self, callee: NodeId, nominal: &Ty, args: &[NodeId]) {
        self.reject_named_args(
            args,
            "a tuple struct's fields are positions — use a composite literal to build one by field name",
        );
        let Some(fields) = self.tuple_struct_tys(nominal) else {
            // Not a tuple struct: still infer the arguments so their own errors
            // are reported, then say why the call shape is wrong.
            for a in args {
                self.infer_expr(*a);
            }
            if !args.is_empty() {
                let msg = format!(
                    "`{}` is not a tuple struct, so it cannot be constructed by a call — use a composite literal",
                    nominal.display(self.defs)
                );
                self.report(callee, msg);
            }
            return;
        };
        if fields.len() != args.len() {
            for a in args {
                self.infer_expr(*a);
            }
            let msg = format!(
                "`{}` has {} field(s) but {} were supplied",
                nominal.display(self.defs),
                fields.len(),
                args.len()
            );
            self.report(callee, msg);
            return;
        }
        for (&a, want) in args.iter().zip(&fields) {
            let got = self.infer_expr(a);
            self.expect(a, &got, want);
        }
    }

    /// The name a call argument was written with, if it was written by name.
    fn arg_name(&self, arg: NodeId) -> Option<Symbol> {
        match &self.ast.node(arg).kind {
            NodeKind::Arg { name, .. } => name.clone(),
            _ => None,
        }
    }

    /// The **value** parameter names of a function def, in declaration order.
    ///
    /// A leading `self` is excluded: a method call's receiver is not one of its
    /// written arguments, so the names line up with `args` either way.
    fn func_param_names(&self, def: DefId) -> Option<Vec<Symbol>> {
        let d = self.defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let ast = &self.asts[&file];
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let NodeKind::FuncExpr { params, .. } = &ast.node(rhs).kind else {
            return None;
        };
        Some(
            params
                .iter()
                .filter_map(|&p| match &ast.node(p).kind {
                    NodeKind::Param { name, .. } if name.as_str() != "self" => Some(name.clone()),
                    _ => None,
                })
                .collect(),
        )
    }

    /// Report any reference from a default argument to one of the function's own
    /// parameters (§5.2).
    ///
    /// A default must be constant, and a parameter is the opposite of that: it
    /// is a runtime value that does not exist yet when the default is evaluated.
    /// The hole is filled **at the call site**, before the callee's frame
    /// exists, so `func (a: i32, b: i32 := a)` does not mean "`a` as passed" —
    /// there is no `a` to read, and lowering would emit a load of whatever local
    /// happens to be named `a` in the *caller*. Rejecting it here, at the
    /// declaration, catches it once rather than at each call.
    fn reject_param_refs_in_default(&mut self, default: NodeId, params: &HashSet<DefId>) {
        let mut stack = vec![default];
        while let Some(n) = stack.pop() {
            if let Some(d) = self.resolved_def(n) {
                if params.contains(&d) {
                    let msg = format!(
                        "a default argument cannot name the parameter `{}`: it is evaluated at \
                         the call site, where no parameter of this function exists yet",
                        self.defs.get(d).name
                    );
                    self.report(n, msg);
                    // One diagnostic per default: naming a parameter twice is
                    // still the one mistake.
                    return;
                }
            }
            stack.extend(self.ast.children(n));
        }
    }

    /// Which of `def`'s **value** parameters carry a default, in declaration
    /// order — the same order and filtering as [`Self::func_param_names`], so
    /// the two zip.
    ///
    /// Only presence is reported, not the default expression: a call site never
    /// looks at the default itself. It was type-checked once at the declaration
    /// and is filled in by lowering, so all inference needs to know is that the
    /// slot may legally be left empty.
    fn func_param_defaults(&self, def: DefId) -> Option<Vec<bool>> {
        let d = self.defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let ast = &self.asts[&file];
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let NodeKind::FuncExpr { params, .. } = &ast.node(rhs).kind else {
            return None;
        };
        Some(
            params
                .iter()
                .filter_map(|&p| match &ast.node(p).kind {
                    NodeKind::Param { name, default, .. } if name.as_str() != "self" => {
                        Some(default.is_some())
                    }
                    _ => None,
                })
                .collect(),
        )
    }

    /// How many arguments a call to `def` must supply — its parameter count
    /// minus the defaulted tail (§5.2). Falls back to "all of them" for a callee
    /// with no reachable declaration, which is the pre-defaults behaviour.
    fn required_arity(&self, def: DefId, total: usize) -> usize {
        match self.func_param_defaults(def) {
            Some(d) => total - d.iter().rev().take_while(|has| **has).count(),
            None => total,
        }
    }

    /// Bind a call's arguments to `def`'s parameters and return them in
    /// **parameter** order, stamping the result on `callee` for lowering (§5.3).
    ///
    /// Positional arguments bind by position; a named one binds to the parameter
    /// it names. Once a named argument appears the rest of the call must also be
    /// named — a positional argument after one has no position left to mean,
    /// since the named arguments before it may have claimed any slot.
    ///
    /// Returns `None` for a purely positional call, which needs no reordering and
    /// nothing the positional path does not already check, and for a call that
    /// does not bind, having reported why.
    fn bind_args(&mut self, callee: NodeId, def: DefId, args: &[NodeId]) -> ArgBinding {
        let named = args.iter().any(|&a| self.arg_name(a).is_some());
        let Some(names) = self.func_param_names(def) else {
            return ArgBinding::AsWritten;
        };
        let has_default = self.func_param_defaults(def).unwrap_or_default();
        let required = self.required_arity(def, names.len());
        // Nothing to bind and nothing to fill: the call is already in parameter
        // order, so it takes the untouched path it took before either feature
        // existed and `apply_call` words any arity error.
        if !named && (args.len() == names.len() || required == names.len()) {
            return ArgBinding::AsWritten;
        }
        if !named {
            // Positional, but short or long against a signature that has
            // defaults — so the legal count is a *range* and the plain equality
            // message would name the wrong number.
            if args.len() > names.len() || args.len() < required {
                let msg = format!(
                    "this function takes {required} to {} argument(s) but {} were supplied",
                    names.len(),
                    args.len()
                );
                self.report(callee, msg);
                return ArgBinding::Failed;
            }
            let mut slots: Vec<Option<NodeId>> = args.iter().copied().map(Some).collect();
            slots.resize(names.len(), None);
            self.ast.set_meta(
                callee,
                ArgOrder {
                    args: slots.clone(),
                },
            );
            return ArgBinding::Bound(slots);
        }
        let mut slots: Vec<Option<NodeId>> = vec![None; names.len()];
        let mut seen_named = false;
        for (i, &a) in args.iter().enumerate() {
            match self.arg_name(a) {
                None => {
                    if seen_named {
                        self.report(
                            a,
                            "a positional argument cannot follow a named one — once a call names \
                             an argument, the rest must be named too",
                        );
                        return ArgBinding::Failed;
                    }
                    // A surplus positional argument is an arity error; let the
                    // arity check below word it.
                    match slots.get_mut(i) {
                        Some(slot) => *slot = Some(a),
                        None => return ArgBinding::Failed,
                    }
                }
                Some(n) => {
                    seen_named = true;
                    let Some(idx) = names.iter().position(|p| *p == n) else {
                        let msg = format!(
                            "`{}` has no parameter named `{n}`",
                            self.defs.canonical_string(def)
                        );
                        self.report(a, msg);
                        return ArgBinding::Failed;
                    };
                    if slots[idx].is_some() {
                        let msg = format!("argument for parameter `{n}` supplied twice");
                        self.report(a, msg);
                        return ArgBinding::Failed;
                    }
                    slots[idx] = Some(a);
                }
            }
        }
        // Naming arguments makes "3 of 4 supplied" unhelpful — say which. A slot
        // left empty for a **defaulted** parameter is not missing: that is the
        // whole point of the default, and it stays a `None` for lowering to fill.
        let missing: Vec<String> = slots
            .iter()
            .zip(&names)
            .enumerate()
            .filter(|(i, (s, _))| s.is_none() && !has_default.get(*i).copied().unwrap_or(false))
            .map(|(_, (_, n))| format!("`{n}`"))
            .collect();
        if !missing.is_empty() {
            let msg = format!("missing argument for parameter {}", missing.join(", "));
            self.report(callee, msg);
            return ArgBinding::Failed;
        }
        self.ast.set_meta(
            callee,
            ArgOrder {
                args: slots.clone(),
            },
        );
        ArgBinding::Bound(slots)
    }

    /// Infer every argument for its own sake, without checking any of them
    /// against a signature. Used after a binding failure: the arguments may well
    /// contain errors of their own worth reporting, but comparing them to
    /// parameters they were never matched to would not be.
    fn infer_args_only(&mut self, args: &[NodeId]) {
        for &a in args {
            self.infer_expr(a);
        }
    }

    /// Report any named argument on a call that cannot accept one — a call through
    /// a function-typed value or field, which carries types but no parameter
    /// names, and a tuple-struct construction, whose fields are positions.
    fn reject_named_args(&mut self, args: &[NodeId], what: &str) {
        for &a in args {
            if let Some(n) = self.arg_name(a) {
                let msg = format!("cannot pass argument `{n}` by name: {what}");
                self.report(a, msg);
            }
        }
    }

    /// Infer the arguments and unify them against a (already-instantiated) callee
    /// function type, returning its result type.
    /// A slot is `None` where the call left a **defaulted** parameter out. There
    /// is nothing to infer or check there: the default was type-checked against
    /// this very parameter once, at the declaration, and lowering fills it in.
    fn apply_call(&mut self, callee: NodeId, callee_ty: &Ty, args: &[Option<NodeId>]) -> Ty {
        let arg_tys: Vec<Option<Ty>> = args.iter().map(|a| a.map(|n| self.infer_expr(n))).collect();
        match self.cx.shallow(callee_ty) {
            Ty::Func { params, ret } => {
                if params.len() == arg_tys.len() {
                    for (a, (arg_node, aty)) in params.iter().zip(args.iter().zip(&arg_tys)) {
                        if let (Some(node), Some(aty)) = (arg_node, aty) {
                            self.expect(*node, aty, a);
                        }
                    }
                } else {
                    self.report(
                        callee,
                        format!(
                            "this function takes {} argument(s) but {} were supplied",
                            params.len(),
                            arg_tys.len()
                        ),
                    );
                }
                *ret
            }
            // Unknown callee type: don't cascade (and don't dangle a variable).
            _ => Ty::Error,
        }
    }

    /// Resolve a method `name` on a receiver's nominal type to its `Func` def.
    fn method_def(&self, recv: &Ty, name: &str) -> Option<super::def::DefId> {
        let Ty::Nominal { def, .. } = self.autoderef(recv) else {
            return None;
        };
        let m = *self
            .defs
            .get(def)
            .ns
            .members
            .get(&crate::common::symbol::Symbol::new(name))?;
        (self.defs.get(m).kind == DefKind::Func).then_some(m)
    }

    /// Resolve a method `name` by searching every `impl` whose self type unifies
    /// with the receiver — inherent impls (`impl <T> []T { len :: … }`) and
    /// in-scope trait impls alike.
    ///
    /// This is the only way to reach a method on a **structural** receiver
    /// (`[]T`, `[N]T`, a range): those types have no namespace to hang members
    /// off, so their impls park members anonymously and are found by unifying
    /// the target instead of by name. It is what makes `s.len()` on a slice work
    /// without the compiler knowing anything about `len`.
    ///
    /// Ranking, most specific first: an **inherent** impl beats a trait impl —
    /// a type's own method is not something a trait can take over — and a
    /// concrete target beats a blanket one. A tie is treated as unresolved: two
    /// equally specific candidates is a question the program has to answer.
    fn impl_method_def(&mut self, recv: &Ty, name: &str) -> Option<DefId> {
        let s = self.cx.shallow(recv);
        let s = self.autoderef(&s);
        if matches!(s, Ty::Error) || is_var(&s) {
            return None;
        }
        let sym = crate::common::symbol::Symbol::new(name);
        let mut best: Option<(u8, DefId)> = None;
        let mut ambiguous = false;
        for i in 0..self.impls.impls.len() {
            let imp = self.impls.impls[i].clone();
            let method = match imp.trait_def {
                // An impl that does not override the member still provides it
                // when the trait declared a default body (§ trait defaults).
                Some(td) => {
                    if !self.in_scope_traits.contains(&td) {
                        continue;
                    }
                    imp.members
                        .get(&sym)
                        .copied()
                        .or_else(|| self.trait_default_method(td, &sym))
                }
                None => imp.members.get(&sym).copied(),
            };
            let Some(method) = method else { continue };
            if self.defs.get(method).kind != DefKind::Func {
                continue;
            }
            if self.trial_impl(i, &s, &[]) {
                let inherent = imp.trait_def.is_none();
                let score = match (inherent, imp.self_is_generic()) {
                    (true, false) => 4,
                    (true, true) => 3,
                    (false, false) => 2,
                    (false, true) => 1,
                };
                match best {
                    Some((bs, _)) if bs > score => {}
                    Some((bs, _)) if bs == score => ambiguous = true,
                    _ => {
                        best = Some((score, method));
                        ambiguous = false;
                    }
                }
            }
        }
        if ambiguous {
            None
        } else {
            best.map(|(_, m)| m)
        }
    }

    /// Resolve `name` through the trait bounds of a generic type parameter
    /// receiver (`<I: Summing>` → `it.total()` is `Summing.total`).
    fn bound_method_def(&mut self, recv: &Ty, name: &str) -> Option<DefId> {
        let s = self.autoderef(&self.cx.shallow(recv));
        let Ty::Nominal { def, .. } = s else {
            return None;
        };
        let d = self.defs.get(def);
        if d.kind != DefKind::TypeParam {
            return None;
        }
        let (file, node) = (d.file?, d.node?);
        let NodeKind::GenericTypeParam { constraint, .. } =
            self.asts[&file].node(node).kind.clone()
        else {
            return None;
        };
        let sym = crate::common::symbol::Symbol::new(name);
        for bound in self.bound_nodes(file, constraint?) {
            let Some(t) = self.type_head_def_in(file, bound) else {
                continue;
            };
            if self.defs.get(t).kind != DefKind::Trait || !self.in_scope_traits.contains(&t) {
                continue;
            }
            if let Some(&m) = self.defs.get(t).ns.members.get(&sym) {
                if self.defs.get(m).kind == DefKind::Func {
                    return Some(m);
                }
            }
        }
        None
    }

    /// The individual trait nodes of a generic parameter's constraint, which is
    /// either a `+`-separated [`NodeKind::Bounds`] list or a single trait.
    fn bound_nodes(&self, file: FileId, constraint: NodeId) -> Vec<NodeId> {
        match self.asts[&file].node(constraint).kind.clone() {
            NodeKind::Bounds { bounds } => bounds,
            _ => vec![constraint],
        }
    }

    /// Unify a method's `self` parameter with the receiver, inserting the one
    /// reference adjustment the call site implies.
    ///
    /// The shapes line up directly more often than not — a `*Self` method called
    /// on a `*T` receiver — and going straight for the pointee (as if the
    /// receiver were always a value) silently fails there, leaving the impl's
    /// generics unsolved. So try the direct unification first, then `*Self`
    /// against a value receiver (the call takes its address), then a value
    /// `self` against a pointer receiver (the call derefs it).
    ///
    /// Each attempt unifies **receiver into parameter**, in that order: the
    /// receiver is the value and the parameter is the slot it flows into, which
    /// is what lets a `[]mut T` call a method declared on `[]T` — dropping a
    /// write permission is safe, and only this direction says so.
    fn unify_self_param(&mut self, param: &Ty, recv: &Ty) {
        let snap = self.cx.snapshot();
        if self.cx.unify(recv, param).is_ok() {
            return;
        }
        self.cx.rollback(snap);
        if let Ty::Ptr { inner, .. } = self.cx.shallow(param) {
            let snap = self.cx.snapshot();
            if self.cx.unify(recv, &inner).is_ok() {
                return;
            }
            self.cx.rollback(snap);
        }
        if let Ty::Ptr { inner, .. } = self.cx.shallow(recv) {
            let _ = self.cx.unify(&inner, param);
        }
    }

    /// Which adjustment [`Inferer::unify_self_param`] settled on, read back off
    /// the solved types: a pointer parameter with a value receiver took its
    /// address, a value parameter with a pointer receiver read through it.
    fn recv_adjust(&mut self, param: &Ty, recv: &Ty) -> RecvAdjust {
        let p = self.cx.shallow(param);
        let r = self.cx.shallow(recv);
        match (&p, &r) {
            (Ty::Ptr { mutable, .. }, other) if !matches!(other, Ty::Ptr { .. }) => {
                RecvAdjust::Ref { mutable: *mutable }
            }
            (other, Ty::Ptr { .. }) if !matches!(other, Ty::Ptr { .. }) => RecvAdjust::Deref,
            _ => RecvAdjust::None,
        }
    }

    /// The dispatch tag for a call that landed on a trait's own declaration:
    /// `wrap` applied to the owning trait, or plain [`MethodDispatch::Static`]
    /// if the method turns out not to be a trait member after all.
    fn method_dispatch(&self, method: DefId, wrap: fn(DefId) -> MethodDispatch) -> MethodDispatch {
        match self.defs.get(method).parent {
            Some(p) if self.defs.get(p).kind == DefKind::Trait => wrap(p),
            _ => MethodDispatch::Static,
        }
    }

    /// Open up a **static trait call** — `Trait.member(args)`, a trait method
    /// named through its trait instead of called on a value (`Make.make(3)`,
    /// and the `FromResidual.from_residual` that `.?` desugars to).
    ///
    /// There is no receiver to dispatch on, so `Self` cannot be read off an
    /// argument: it is decided by the *context* the call sits in. `Self` becomes
    /// a fresh variable, and a [`Obligation::Trait`] holds the choice of impl
    /// open until something — a `return`, an annotation, a later argument —
    /// solves it. Selecting the impl then stamps the member it resolved to, so
    /// lowering emits a call to the real implementation rather than to the
    /// trait's bodyless declaration.
    ///
    /// A call that already has a receiver never reaches here: those resolve in
    /// [`Inferer::infer_call`]'s method branch and keep their own `Self`.
    fn open_trait_self(&mut self, callee: NodeId, method: DefId, sig: &Ty, map: &Subst) -> Ty {
        let Some(trait_def) = self.defs.get(method).parent else {
            return sig.clone();
        };
        if self.defs.get(trait_def).kind != DefKind::Trait {
            return sig.clone();
        }
        // `Self` in the declaration is the trait's own nominal; swap it for a
        // variable so each call site gets its own.
        let self_ty = self.cx.fresh();
        let opened = self.subst_type_params(
            sig,
            &Subst::of_types(HashMap::from([(trait_def, self_ty.clone())])),
        );
        // The trait's generic arguments, as this instantiation freshened them:
        // `FromResidual.<R>`'s `R` is what the residual argument will solve.
        let args: Vec<Ty> = self
            .type_param_defs(trait_def)
            .into_iter()
            .map(|p| {
                map.tys.get(&p).cloned().unwrap_or_else(|| Ty::Nominal {
                    def: p,
                    args: Vec::new(),
                })
            })
            .collect();
        self.cx.register(Obligation::Trait {
            self_ty,
            trait_def,
            args,
            origin: callee,
            stamp: Some(self.defs.get(method).name.clone()),
        });
        opened
    }

    /// Rewrite a trait *declaration*'s `Self` to what the receiver actually is.
    ///
    /// Only calls that land on a trait's own declaration need this — dispatch
    /// through a trait object (`*dyn Summing`) or through a type parameter's
    /// bound (`<I: Summing>`). A call that selected a concrete impl already has
    /// the impl's signature and is left alone.
    fn subst_trait_self(&mut self, sig: &Ty, method: DefId, recv: &Ty) -> Ty {
        let Some(parent) = self.defs.get(method).parent else {
            return sig.clone();
        };
        if self.defs.get(parent).kind != DefKind::Trait {
            return sig.clone();
        }
        // Look through the receiver's pointer: `*dyn T` and `*I` both stand for
        // a `Self` of `dyn T` / `I`.
        let head = match self.cx.shallow(recv) {
            Ty::Ptr { inner, .. } => self.cx.shallow(&inner),
            other => other,
        };
        if matches!(head, Ty::Error) || is_var(&head) {
            return sig.clone();
        }
        let map = Subst::of_types(HashMap::from([(parent, head)]));
        self.subst_type_params(sig, &map)
    }

    /// The trait's own declaration of `name`, but only when it carries a
    /// **default body** — a bodyless signature is a requirement the impl must
    /// satisfy, not something callable through the impl.
    fn trait_default_method(&self, trait_def: DefId, name: &Symbol) -> Option<DefId> {
        let m = *self.defs.get(trait_def).ns.members.get(name)?;
        let d = self.defs.get(m);
        if d.kind != DefKind::Func {
            return None;
        }
        let (file, node) = (d.file?, d.node?);
        let ast = &self.asts[&file];
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        matches!(ast.node(rhs).kind, NodeKind::FuncExpr { body: Some(_), .. }).then_some(m)
    }

    /// Resolve `name` on a trait-object receiver (`dyn Trait` or `*dyn Trait`) to
    /// the trait's own method declaration.
    fn dyn_method_def(&mut self, recv: &Ty, name: &str) -> Option<DefId> {
        let s = self.cx.shallow(recv);
        let trait_def = match &s {
            Ty::Dyn(d) => *d,
            Ty::Ptr { inner, .. } => match self.cx.shallow(inner) {
                Ty::Dyn(d) => d,
                _ => return None,
            },
            _ => return None,
        };
        let m = *self
            .defs
            .get(trait_def)
            .ns
            .members
            .get(&crate::common::symbol::Symbol::new(name))?;
        (self.defs.get(m).kind == DefKind::Func).then_some(m)
    }

    /// Resolve `name` on the `@using` field's type when the receiver's own type
    /// does not have it, returning the method and the coercion that reaches it.
    ///
    /// Only one hop, and only when the outer struct has no such member itself —
    /// `@using` promotes nothing else onto the outer type (§3.10).
    /// The type a `distinct` type is distinct *from*, if `recv` is one.
    ///
    /// Looks through a pointer, so a `*str` receiver reaches `[]u8`'s methods the
    /// same way a `str` receiver does.
    fn distinct_repr(&mut self, recv: &Ty) -> Option<Ty> {
        let s = self.cx.shallow(recv);
        let (def, ptr) = match &s {
            Ty::Nominal { def, .. } => (*def, None),
            Ty::Ptr { inner, mutable } => match self.cx.shallow(inner) {
                Ty::Nominal { def, .. } => (def, Some(*mutable)),
                _ => return None,
            },
            _ => return None,
        };
        let d = self.defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let rhs = match &self.asts[&file].node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let NodeKind::DistinctType { inner, .. } = self.asts[&file].node(rhs).kind.clone() else {
            return None;
        };
        let repr = self.ty_from_node_in(file, inner);
        Some(match ptr {
            Some(mutable) => Ty::Ptr {
                mutable,
                inner: Box::new(repr),
            },
            None => repr,
        })
    }

    /// A method reached through a `distinct` type's **representation** (§2.4).
    ///
    /// `distinct T` inherits `T`'s methods; `T` does **not** gain the distinct
    /// type's. That asymmetry is the point: the distinct type is `T` plus an
    /// invariant and some extra operations, so everything `T` can do it can do,
    /// while the operations that assume the invariant stay off `T`.
    ///
    /// This runs **last** in the resolution chain, so a method the distinct type
    /// declares itself always wins over the inherited one of the same name.
    fn distinct_method_def(&mut self, recv: &Ty, name: &str) -> Option<(DefId, Ty)> {
        let repr = self.distinct_repr(recv)?;
        let m = self
            .method_def(&repr, name)
            .or_else(|| self.impl_method_def(&repr, name))?;
        Some((m, repr))
    }

    fn using_method_def(&mut self, recv: &Ty, name: &str) -> Option<(DefId, Upcast)> {
        let s = self.cx.shallow(recv);
        let head = match &s {
            Ty::Nominal { def, .. } => *def,
            Ty::Ptr { inner, .. } => match self.cx.shallow(inner) {
                Ty::Nominal { def, .. } => def,
                _ => return None,
            },
            _ => return None,
        };
        let field = self.defs.using_field(head)?;
        // The outer type keeps priority: `@using` only fills in what it lacks.
        if self.method_def(&s, name).is_some() {
            return None;
        }
        let target = self.field_ty(&s, self.defs.get(field).name.as_str())?;
        let method = self
            .method_def(&target, name)
            .or_else(|| self.impl_method_def(&target, name))?;
        // The receiver lowers to the sub-object itself (`e.t` / `p.*.t`); the
        // method's own `*Self` parameter re-addresses it as usual, so this is
        // never the pointer form of the coercion.
        Some((
            method,
            Upcast {
                field,
                through_ptr: false,
                target,
            },
        ))
    }

    /// Type a `recv.method(args)` call: instantiate the method signature, unify
    /// its `self` parameter with the receiver (linking the receiver's type
    /// arguments to the method's), then unify the rest against the arguments.
    fn infer_method_call(
        &mut self,
        callee: NodeId,
        recv: &Ty,
        method: super::def::DefId,
        dispatch: MethodDispatch,
        args: &[NodeId],
        targs: &[NodeId],
    ) -> Ty {
        let sig = self.func_def_ty(method);
        let inst = self.instantiate_with(&sig, method, targs);
        // Dispatching through a trait object or a bound reaches the trait's
        // *declaration*, whose `Self` is the trait's own nominal. For this call
        // `Self` is the receiver, so say so rather than leaving the signature
        // claiming a bare `Trait`.
        let inst = self.subst_trait_self(&inst, method, recv);
        self.types.insert(callee, inst.clone());
        let Ty::Func { params, ret } = self.cx.shallow(&inst) else {
            return Ty::Error;
        };
        // Bind the `self` parameter to the receiver, and record what the call
        // site has to do to the receiver expression to produce it.
        if let Some(self_param) = params.first() {
            let p = self_param.clone();
            self.unify_self_param(&p, recv);
            let adjust = self.recv_adjust(&p, recv);
            self.ast.set_meta(
                callee,
                MethodRes {
                    method,
                    dispatch,
                    adjust,
                    self_ty: self.cx.resolve(&p),
                },
            );
        }
        // Unify the remaining parameters with the call arguments.
        let value_params = &params[params.len().min(1)..];
        let args = match self.bind_args(callee, method, args) {
            ArgBinding::AsWritten => args.iter().copied().map(Some).collect::<Vec<_>>(),
            ArgBinding::Bound(a) => a,
            ArgBinding::Failed => {
                self.infer_args_only(args);
                return *ret;
            }
        };
        let args = &args[..];
        // A `None` slot is a defaulted parameter the call left out: checked once
        // at the declaration, filled in by lowering, nothing to do here.
        let arg_tys: Vec<Option<Ty>> = args.iter().map(|a| a.map(|n| self.infer_expr(n))).collect();
        if value_params.len() == arg_tys.len() {
            for (p, (arg_node, aty)) in value_params.iter().zip(args.iter().zip(&arg_tys)) {
                if let (Some(node), Some(aty)) = (arg_node, aty) {
                    self.expect(*node, aty, p);
                }
            }
        } else {
            let required = self.required_arity(method, value_params.len());
            let takes = if required == value_params.len() {
                value_params.len().to_string()
            } else {
                format!("{required} to {}", value_params.len())
            };
            let msg = format!(
                "`{}` takes {takes} argument(s) but {} were supplied",
                self.defs.get(method).name,
                arg_tys.len()
            );
            self.report(callee, msg);
        }
        *ret
    }

    // ===< generic instantiation >===

    /// Replace every generic type-parameter occurrence in `ty` with a fresh
    /// inference variable, consistently (the same parameter maps to the same
    /// variable). This is what makes a generic function/method infer fresh type
    /// arguments at each call site — e.g. `Vec.new()` yields `Vec.<?>` whose `?`
    /// is later solved by a `push`.
    /// Instantiate `sig` with the call site's **explicit** type arguments bound
    /// to `def`'s declared type parameters, in order.
    ///
    /// A missing argument, a `_` hole, or an `<Assoc = T>` binding leaves that
    /// parameter to inference, so `id.<i32>(x)` and `id(x)` differ only in how
    /// much was pinned up front. Too many arguments is an error.
    fn instantiate_with(&mut self, sig: &Ty, def: DefId, targs: &[NodeId]) -> Ty {
        self.instantiate_parts(sig, def, targs).0
    }

    /// [`Inferer::instantiate_with`], also returning the substitution it built,
    /// for callers that need to talk about a parameter it freshened (a static
    /// trait call needs the trait's own arguments — see
    /// [`Inferer::open_trait_self`]).
    fn instantiate_parts(&mut self, sig: &Ty, def: DefId, targs: &[NodeId]) -> (Ty, Subst) {
        // Type and const parameters share one positional list (§5): in
        // `func <const N: usize, T>`, `.<4, i32>` pins `N` then `T`.
        let params = self.func_generic_param_defs(def);
        let explicit: Vec<NodeId> = targs
            .iter()
            .copied()
            .filter(|&a| !matches!(self.ast.node(a).kind, NodeKind::AssocBinding { .. }))
            .collect();
        if explicit.len() > params.len() {
            let msg = format!(
                "`{}` takes {} generic argument(s) but {} were supplied",
                self.defs.canonical_string(def),
                params.len(),
                explicit.len()
            );
            let anchor = explicit[params.len()];
            self.report(anchor, msg);
        }
        // `<Assoc = T>` constraints still apply even when positional args do not.
        for &a in targs {
            self.record_generic_arg(a);
        }
        let mut map = Subst::default();
        for (i, &p) in params.iter().enumerate() {
            let arg = explicit
                .get(i)
                .copied()
                .filter(|&a| !matches!(self.ast.node(a).kind, NodeKind::TypeHole));
            if self.defs.get(p).kind == DefKind::ConstParam {
                let k = match arg {
                    Some(a) => self.const_arg(p, a),
                    None => self.cx.fresh_const(),
                };
                map.consts.insert(p, k);
            } else {
                let t = match arg {
                    Some(a) => self.ty_from_node(a),
                    None => self.cx.fresh(),
                };
                map.tys.insert(p, t);
            }
        }
        // A parameter the signature mentions but the declaration did not list
        // (defensive) still needs a variable.
        let (mut rest, mut rest_consts) = (Vec::new(), Vec::new());
        self.collect_generic_params(sig, &mut rest, &mut rest_consts);
        for d in rest {
            map.tys.entry(d).or_insert_with(|| self.cx.fresh());
        }
        for d in rest_consts {
            if let std::collections::hash_map::Entry::Vacant(e) = map.consts.entry(d) {
                e.insert(self.cx.fresh_const());
            }
        }
        let inst = self.subst_type_params(sig, &map);
        (inst, map)
    }

    /// Read one explicit `.<...>` argument in a `const` parameter's slot, and
    /// check it against the parameter's declared type.
    fn const_arg(&mut self, param: DefId, arg: NodeId) -> Const {
        let k = self.const_len_in(self.file, arg);
        // The argument is a value, so it must fit the parameter's type — the
        // only place a `const` parameter's `: usize` is enforced.
        let declared = self.const_param_ty(param);
        if !matches!(declared, Ty::Error) && !declared.is_int() {
            let msg = format!(
                "a `const` generic parameter must have an integer type, not `{}`",
                declared.display(self.defs)
            );
            self.report(arg, msg);
        }
        k
    }

    /// The declared type of a `<const N: T>` parameter — what `N` is worth as a
    /// value in the body, and what an explicit argument must satisfy.
    /// Check one `<const N: T>` declaration.
    ///
    /// A `const` generic is a compile-time *value* that takes part in type
    /// identity — `[3]i32` and `[4]i32` are different types (§3.2, §5) — and
    /// [`Const`] represents exactly one kind of value: an unsigned integer.
    /// Admitting a struct or an array here would mean teaching that little
    /// lattice structured values, structural equality, and a mangling, for no
    /// gain the language asks for. So anything but an integer is rejected at the
    /// declaration rather than silently degrading to a `Const::Error` at the
    /// first use.
    fn check_const_param(&mut self, node: NodeId) {
        let NodeKind::GenericConstParam { name, ty } = self.ast.node(node).kind.clone() else {
            return;
        };
        let t = self.ty_from_node(ty);
        if matches!(self.cx.shallow(&t), Ty::Int { .. } | Ty::Error) {
            return;
        }
        let msg = format!(
            "a `const` generic parameter must have an integer type, but `{name}` is `{}`",
            self.cx.resolve(&t).display(self.defs)
        );
        self.report(node, msg);
    }

    fn const_param_ty(&mut self, param: DefId) -> Ty {
        let d = self.defs.get(param);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Ty::Error;
        };
        let NodeKind::GenericConstParam { ty, .. } = self.asts[&file].node(node).kind.clone()
        else {
            return Ty::Error;
        };
        self.ty_from_node_in(file, ty)
    }

    /// A function's declared generic parameters — types **and** `const` values —
    /// in source order, which is the order `.<...>` arguments bind to.
    fn func_generic_param_defs(&self, def: DefId) -> Vec<DefId> {
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Vec::new();
        };
        let ast = &self.asts[&file];
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let NodeKind::FuncExpr { generics, .. } = &ast.node(rhs).kind else {
            return Vec::new();
        };
        generics
            .iter()
            .filter(|&&g| {
                matches!(
                    ast.node(g).kind,
                    NodeKind::GenericTypeParam { .. } | NodeKind::GenericConstParam { .. }
                )
            })
            .filter_map(|&g| self.def_meta_in(file, g))
            .collect()
    }

    fn collect_type_params(&self, ty: &Ty, out: &mut Vec<super::def::DefId>) {
        self.collect_generic_params(ty, out, &mut Vec::new());
    }

    /// Every generic parameter `ty` mentions, split by kind and in first-seen
    /// order.
    fn collect_generic_params(
        &self,
        ty: &Ty,
        out: &mut Vec<super::def::DefId>,
        consts: &mut Vec<super::def::DefId>,
    ) {
        match ty {
            Ty::Nominal { def, args } => {
                if args.is_empty() && self.defs.get(*def).kind == DefKind::TypeParam {
                    if !out.contains(def) {
                        out.push(*def);
                    }
                }
                for a in args {
                    self.collect_generic_params(a, out, consts);
                }
            }
            Ty::Array { len, inner, .. } => {
                if let Const::Param(d) = len {
                    if !consts.contains(d) {
                        consts.push(*d);
                    }
                }
                self.collect_generic_params(inner, out, consts)
            }
            Ty::Ptr { inner, .. } | Ty::Slice { inner, .. } => {
                self.collect_generic_params(inner, out, consts)
            }
            Ty::Tuple(elems) => {
                for e in elems {
                    self.collect_generic_params(e, out, consts);
                }
            }
            Ty::Func { params, ret } => {
                for p in params {
                    self.collect_generic_params(p, out, consts);
                }
                self.collect_generic_params(ret, out, consts);
            }
            _ => {}
        }
    }

    fn subst_type_params(&self, ty: &Ty, map: &Subst) -> Ty {
        match ty {
            // Keyed by def, not by shape: the map holds generic type parameters
            // (which never carry arguments) and, for a trait call, the trait
            // whose `Self` is being replaced — and that one *can* carry them
            // (`FromResidual.<R>`'s `Self` is still just `Self`).
            Ty::Nominal { def, .. } if map.tys.contains_key(def) => map.tys[def].clone(),
            Ty::Nominal { def, args } if args.is_empty() => {
                map.tys.get(def).cloned().unwrap_or_else(|| ty.clone())
            }
            Ty::Nominal { def, args } => Ty::Nominal {
                def: *def,
                args: args
                    .iter()
                    .map(|a| self.subst_type_params(a, map))
                    .collect(),
            },
            Ty::Ptr { mutable, inner } => Ty::Ptr {
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            Ty::Slice { mutable, inner } => Ty::Slice {
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            Ty::Array {
                len,
                mutable,
                inner,
            } => Ty::Array {
                // `[N]T` with `N` a `const` parameter: the instantiation says
                // what `N` is here.
                len: match len {
                    Const::Param(d) => map.consts.get(d).copied().unwrap_or(*len),
                    other => *other,
                },
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            Ty::Tuple(elems) => Ty::Tuple(
                elems
                    .iter()
                    .map(|e| self.subst_type_params(e, map))
                    .collect(),
            ),
            Ty::Func { params, ret } => Ty::Func {
                params: params
                    .iter()
                    .map(|p| self.subst_type_params(p, map))
                    .collect(),
                ret: Box::new(self.subst_type_params(ret, map)),
            },
            other => other.clone(),
        }
    }

    /// If `callee` is a bare path resolving to a `struct`/`enum` type, return its
    /// def (a tuple-struct / variant construction), else `None`.
    fn callee_type_def(&self, callee: NodeId) -> Option<super::def::DefId> {
        if !matches!(self.ast.node(callee).kind, NodeKind::Path { .. }) {
            return None;
        }
        let def = self.resolved_def(callee)?;
        matches!(self.defs.get(def).kind, DefKind::Struct | DefKind::Enum).then_some(def)
    }

    // ===< names and defs >===

    /// The type of a path expression from the def it resolved to.
    fn path_ty(&mut self, node: NodeId) -> Ty {
        match self.resolved_def(node) {
            Some(def) => self.def_ty(def),
            None => Ty::Error,
        }
    }

    /// The type a value-position reference to `def` has: a local/param from the
    /// environment, a function from its signature, a type used as a constructor
    /// value as its nominal type.
    fn def_ty(&mut self, def: super::def::DefId) -> Ty {
        if let Some(ty) = self.env.get(&def) {
            return ty.clone();
        }
        match self.defs.get(def).kind {
            DefKind::Func => self.func_def_ty(def),
            DefKind::Struct | DefKind::Enum => self.nominal_of(def),
            DefKind::Const => self.const_def_ty(def),
            // `<const N: usize>` names a value in the body, of the type it was
            // declared with (§5).
            DefKind::ConstParam => self.const_param_ty(def),
            _ => self.cx.fresh(),
        }
    }

    /// Pin an open `comptime_str` to `str` when something is about to ask a
    /// question of it that only a concrete type can answer — a method call, a
    /// field, an index, an operator.
    ///
    /// The openness of a string literal (§1.5) exists so that one may be *passed
    /// to* a `[]u8` or `[]char` slot; the type it *is* on its own is `str`, and
    /// every method, operator and impl a string literal reaches is `str`'s. A
    /// variable left open at one of these sites would find no impls at all and
    /// report "no method on `?3`", which names the compiler's bookkeeping
    /// instead of the program. Pinning here also keeps `==` on strings
    /// registering its `Eq` obligation, as it did when a literal was simply a
    /// `str`.
    ///
    /// Only an *unsolved* variable is pinned: once a use site has settled the
    /// literal on `[]u8`, indexing it is indexing a byte slice.
    fn pin_str(&mut self, ty: &Ty) -> Ty {
        self.cx.pin_str(ty)
    }

    /// The exact integer a path names, when it resolves to a constant whose
    /// value is an integer literal (following a chain of such constants).
    fn const_int_value(&self, node: NodeId) -> Option<num_bigint::BigInt> {
        let mut def = self.resolved_def(node)?;
        for _ in 0..16 {
            let d = self.defs.get(def);
            if d.kind != DefKind::Const {
                return None;
            }
            let (file, n) = (d.file?, d.node?);
            let NodeKind::ConstBind { rhs, .. } = self.asts[&file].node(n).kind.clone() else {
                return None;
            };
            match self.asts[&file].node(rhs).kind.clone() {
                NodeKind::Lit(Lit::Int(v)) => return Some(v),
                NodeKind::Path { .. } => def = self.resolved_def_in(file, rhs)?,
                _ => return None,
            }
        }
        None
    }

    /// Give a comptime literal back its `comptime_int` / `comptime_float` type
    /// and record the conversion the context asked for, so lowering can make it
    /// an explicit `$cast` instead of a silent change of type.
    ///
    /// Only literal-bearing nodes qualify: everything else already *had* the
    /// runtime type, rather than converting into it.
    fn record_comptime_coercion(&mut self, node: NodeId, resolved: Ty) -> Ty {
        let comptime = match &self.ast.node(node).kind {
            NodeKind::Lit(Lit::Int(_)) => Ty::ComptimeInt,
            NodeKind::Lit(Lit::Float(_)) => Ty::ComptimeFloat,
            NodeKind::Lit(Lit::Str(_)) => Ty::ComptimeStr,
            _ => return resolved,
        };
        // A literal that stayed untyped needs no conversion. For a string that
        // means: it settled on one of the three types §1.5 lets it become, and
        // the cast is what materializes the bytes as that type — the `[]char`
        // case really is a transcoding, and the const evaluator performs it.
        let converts = match comptime {
            Ty::ComptimeStr => self.cx.admits_str(&resolved),
            _ => matches!(resolved, Ty::Int { .. } | Ty::Float(_)),
        };
        if !converts {
            return resolved;
        }
        self.ast.set_meta(node, Coercion { to: resolved });
        comptime
    }

    /// Reject a `comptime_int` that does not fit the runtime integer type it
    /// settled on. The literal keeps its exact value until this point, so the
    /// check is exact however large the number was written.
    fn check_int_range(&mut self, node: NodeId, resolved: &Ty) {
        let Some(value) = self.int_values.get(&node).cloned() else {
            return;
        };
        let Ty::Int { signed, width } = resolved else {
            return;
        };
        if super::ty::int_fits(&value, *signed, *width, self.target) {
            return;
        }
        let msg = format!(
            "the literal `{value}` does not fit in `{}`",
            resolved.display(self.defs)
        );
        self.report(node, msg);
    }

    /// The type of a namespace-level `name :: value` constant, **at this use
    /// site**.
    ///
    /// A constant whose value is a numeric literal is a `comptime_int` /
    /// `comptime_float`: it has no single runtime type, so each use gets its own
    /// numeric variable and settles independently — `A :: 42` may be an `i8` in
    /// one place and an `i64` in another. Anything else has one concrete type,
    /// inferred from the right-hand side.
    /// Type one value definition at its **declaration**, and stamp the answer on
    /// the binding node so lowering can read it back.
    ///
    /// Two right-hand-side shapes reach here, and the difference is which side
    /// says what the type is:
    ///
    /// - `A :: 5` — the initializer decides, and a numeric literal stays
    ///   `comptime_int` so each use site can settle it for itself (§2.5).
    /// - `#static c :: u32 := 0`, `MAX :: i32 := 100` — the **type is declared**
    ///   and the initializer is checked against it. A static must be typed this
    ///   way: it is storage, and storage cannot be `comptime_int`. Its
    ///   initializer may also be absent, the region being zeroed (§2.6).
    fn infer_global(&mut self, def: DefId) {
        let Some(node) = self.defs.get(def).node else {
            return;
        };
        let NodeKind::ConstBind { rhs, .. } = self.ast.node(node).kind.clone() else {
            return;
        };
        let ty = match self.ast.node(rhs).kind.clone() {
            NodeKind::AssocConst { ty, default } => {
                let want = self.ty_from_node(ty);
                if let Some(d) = default {
                    let got = self.infer_expr(d);
                    self.expect(d, &got, &want);
                }
                want
            }
            _ => match self.comptime_rhs_ty(rhs) {
                Some(ty) => ty,
                None => self.infer_expr(rhs),
            },
        };
        self.types.insert(node, ty);
    }

    /// The `comptime_int` / `comptime_float` type of a constant whose RHS is a
    /// bare numeric literal, stamping every node of it on the way.
    ///
    /// §2.5: such a constant has **no single runtime type**. `A :: 42` is a
    /// `comptime_int`, and each use site settles it for itself — which is what
    /// lets one `A` be an `i8` here and an `i64` there. Running the literal
    /// through ordinary inference instead would leave an unconstrained numeric
    /// variable that `finish` defaults to `isize`, stamping a `$cast` onto the
    /// declaration and making the IR claim a width the source never chose. The
    /// const evaluator would then read that width back as if it meant
    /// something.
    ///
    /// A numeric variable cannot simply be *unified* with `comptime_int` to say
    /// this: [`TyVarKind::Int`] admits only runtime integers by design (see
    /// [`InferCtxt::bind_raw`]), because at every other site a literal really
    /// does have to become one. The declaration is the one place that is not
    /// true, so it takes the type directly rather than inferring it.
    ///
    /// Returns `None` for any other RHS — `A :: f()` has a concrete type and is
    /// inferred normally.
    fn comptime_rhs_ty(&mut self, rhs: NodeId) -> Option<Ty> {
        let ty = match self.ast.node(rhs).kind.clone() {
            NodeKind::Lit(Lit::Int(_)) => Ty::ComptimeInt,
            NodeKind::Lit(Lit::Float(_)) => Ty::ComptimeFloat,
            NodeKind::Lit(Lit::Str(_)) if self.lang.get("str").is_some() => Ty::ComptimeStr,
            // `-1` / `+1` are still literals for this purpose.
            NodeKind::Unary { operand, .. } => self.comptime_rhs_ty(operand)?,
            _ => return None,
        };
        self.types.insert(rhs, ty.clone());
        Some(ty)
    }

    fn const_def_ty(&mut self, def: DefId) -> Ty {
        // A constant defined in terms of itself has no type; break the cycle
        // rather than recursing forever.
        if self.const_stack.contains(&def) {
            return Ty::Error;
        }
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return self.cx.fresh();
        };
        let NodeKind::ConstBind { rhs, .. } = self.asts[&file].node(node).kind.clone() else {
            return self.cx.fresh();
        };
        self.const_stack.push(def);
        let ty = self.const_rhs_ty(file, rhs);
        self.const_stack.pop();
        ty
    }

    /// The type a constant's right-hand side gives it, read in the file the
    /// constant was declared in.
    fn const_rhs_ty(&mut self, file: FileId, rhs: NodeId) -> Ty {
        match self.asts[&file].node(rhs).kind.clone() {
            // Literals stay comptime: a fresh variable per use, so one use of
            // `A :: "hi"` may be a `str` and another a `[]u8`, exactly as one
            // use of `N :: 1` may be an `i8` and another an `i64`.
            NodeKind::Lit(Lit::Int(_)) => self.cx.fresh_of(TyVarKind::Int),
            NodeKind::Lit(Lit::Float(_)) => self.cx.fresh_of(TyVarKind::Float),
            NodeKind::Lit(l) => self.lit_ty(&l),
            // `-1` / `+1` are still literals for this purpose.
            NodeKind::Unary { operand, .. } => self.const_rhs_ty(file, operand),
            // A constant naming another constant inherits its comptime-ness.
            NodeKind::Path { .. } => match self.resolved_def_in(file, rhs) {
                Some(d) => self.def_ty(d),
                None => Ty::Error,
            },
            // `#static count :: u32 := 0` (§2.6) and an associated constant
            // share this RHS shape: a **declared type** with an optional `:=`
            // initializer. The type is written, so there is nothing to infer
            // from the initializer — and for a static there may be no
            // initializer at all, the region being zeroed. Reading the
            // annotation is also what keeps a static's type concrete: a global
            // is storage, and storage cannot be `comptime_int`.
            NodeKind::AssocConst { ty, .. } => self.ty_from_node_in(file, ty),
            // Anything else has a concrete type; infer it where it is written.
            _ if file == self.file => self.infer_expr(rhs),
            _ => self.cx.fresh(),
        }
    }

    /// Build the [`Ty::Func`] of a function def from its signature.
    fn func_def_ty(&mut self, def: super::def::DefId) -> Ty {
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return self.cx.fresh();
        };
        // The def's node is the `ConstBind`; its RHS is the `FuncExpr`.
        let ast = &self.asts[&file];
        let func = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            NodeKind::FuncExpr { .. } => node,
            _ => return self.cx.fresh(),
        };
        self.func_sig_ty_in(file, func)
    }

    /// The signature type of a `FuncExpr` in the current file.
    fn func_sig_ty(&mut self, func: NodeId) -> Ty {
        self.func_sig_ty_in(self.file, func)
    }

    /// The signature type of a `FuncExpr` living in `file`.
    fn func_sig_ty_in(&mut self, file: FileId, func: NodeId) -> Ty {
        let ast = &self.asts[&file];
        let NodeKind::FuncExpr { params, ret, .. } = ast.node(func).kind.clone() else {
            return self.cx.fresh();
        };
        let params = params
            .iter()
            .map(|&p| match &ast.node(p).kind {
                NodeKind::Param { ty: Some(t), .. } => self.ty_from_node_in(file, *t),
                _ => self.cx.fresh(),
            })
            .collect();
        let ret = ret
            .map(|t| self.ty_from_node_in(file, t))
            .unwrap_or(Ty::Void);
        Ty::Func {
            params,
            ret: Box::new(ret),
        }
    }

    /// A nominal type for `def`, its type arguments left as fresh variables to be
    /// solved by context (arity from the def's declared generics).
    fn nominal_of(&mut self, def: super::def::DefId) -> Ty {
        let arity = self.generic_arity(def);
        let args = (0..arity).map(|_| self.cx.fresh()).collect();
        Ty::Nominal { def, args }
    }

    /// Number of generic parameters a type def declares.
    fn generic_arity(&self, def: super::def::DefId) -> usize {
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return 0;
        };
        let ast = &self.asts[&file];
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        match &ast.node(rhs).kind {
            NodeKind::StructType { generics, .. }
            | NodeKind::EnumType { generics, .. }
            | NodeKind::TraitType { generics, .. } => generics.len(),
            _ => 0,
        }
    }

    /// Infer each variant-literal payload argument, tagging record entries with
    /// their field name (tuple entries get `None`).
    fn variant_lit_arg_tys(
        &mut self,
        args: &VariantArgs,
    ) -> Vec<(Option<crate::common::symbol::Symbol>, Ty)> {
        match args {
            VariantArgs::None => Vec::new(),
            VariantArgs::Tuple(ids) => ids.iter().map(|&a| (None, self.infer_expr(a))).collect(),
            VariantArgs::Record(ids) => ids
                .iter()
                .map(|&f| match self.ast.node(f).kind.clone() {
                    NodeKind::FieldInit { name, value } => (Some(name), self.infer_expr(value)),
                    _ => (None, self.infer_expr(f)),
                })
                .collect(),
        }
    }

    // ===< generic substitution over members >===

    /// The generic type-parameter [`DefId`]s a type def declares, in order.
    fn type_param_defs(&self, def: DefId) -> Vec<DefId> {
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Vec::new();
        };
        let ast = &self.asts[&file];
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let generics = match &ast.node(rhs).kind {
            NodeKind::StructType { generics, .. }
            | NodeKind::EnumType { generics, .. }
            | NodeKind::TraitType { generics, .. } => generics.clone(),
            _ => return Vec::new(),
        };
        generics
            .iter()
            .filter_map(|&g| self.def_meta_in(file, g))
            .collect()
    }

    /// The substitution `{ generic-param → type-arg }` for a nominal use
    /// `Type.<args>` — how a `T`-typed field / variant payload becomes concrete.
    ///
    /// Nominal types carry type arguments only: a `const` parameter on a
    /// `struct` / `enum` / `trait` is rejected at collection time (see
    /// [`super::collect`]), because [`Ty::Nominal`] has nowhere to put the
    /// value. `const` parameters live on functions and `impl` blocks, where the
    /// signature is structural and a [`Const`] has a place to sit.
    fn nominal_subst(&self, def: DefId, args: &[Ty]) -> Subst {
        Subst::of_types(
            self.type_param_defs(def)
                .into_iter()
                .zip(args.iter().cloned())
                .collect(),
        )
    }

    fn def_meta_in(&self, file: FileId, node: NodeId) -> Option<DefId> {
        self.asts[&file].meta::<DefMeta>(node).map(|m| m.0)
    }

    // ===< field access >===

    /// Type `$from_residual(r)`, the intrinsic `.?` desugars its failure arm to.
    ///
    /// The result is the **enclosing function's** return type — the one thing
    /// only the checker can supply, and the reason `.?` cannot desugar to an
    /// ordinary call. Requiring `Ret : FromResidual.<typeof r>` is what decides
    /// whether the propagation is legal: the identity impl each `Try` type
    /// provides for its own residual covers `Result.?` in a `Result` function
    /// and `Option.?` in an `Option` one, and a user impl is what lets a
    /// residual cross error types (§8.3). Selecting the impl also stamps the
    /// `from_residual` it resolved to, so lowering emits a real call.
    fn infer_from_residual(&mut self, node: NodeId, args: &[NodeId], arg_tys: &[Ty]) -> Ty {
        let [_] = args else {
            self.report(node, "`$from_residual` takes exactly one argument");
            return Ty::Error;
        };
        let Some(trait_def) = self.lang.get("from_residual") else {
            self.report(node, "`.?` requires the `#lang(\"from_residual\")` item");
            return Ty::Error;
        };
        let ret = self.ret.clone();
        self.cx.register(Obligation::Trait {
            self_ty: ret.clone(),
            trait_def: self.defs.resolve_alias(trait_def),
            args: vec![arg_tys[0].clone()],
            origin: node,
            stamp: Some(Symbol::new("from_residual")),
        });
        ret
    }

    /// `$len` takes exactly one array or slice; anything else has no length to
    /// report. A pointer to one counts — `core`'s `Len` impls take `self` by
    /// pointer and hand it straight to `$len`.
    fn check_len_intrinsic(&mut self, node: NodeId, args: &[NodeId], arg_tys: &[Ty]) {
        let [arg] = args else {
            self.report(node, "`$len` takes exactly one argument");
            return;
        };
        let ty = self.autoderef(&arg_tys[0]);
        // An unsolved receiver is not yet wrong; a `[N]T` / `[]T` is right.
        if has_len(&ty) || matches!(ty, Ty::Error) || is_var(&ty) {
            return;
        }
        let msg = format!(
            "`$len` needs an array or a slice, not `{}`",
            self.cx.resolve(&ty).display(self.defs)
        );
        self.report(*arg, msg);
    }

    /// The declared type of field `name` on a nominal struct type, if reachable,
    /// with the struct's generics substituted by the use-site's type arguments
    /// (so `Wrap.<i32>`'s `T` field reads back as `i32`). Auto-derefs through a
    /// pointer first (§3.2).
    /// The type of a field access that found no field. It is `Error`, not a
    /// fresh variable — a dangling variable would surface as a false "type
    /// annotations needed" (see `finish`). Says why, unless the base is itself
    /// already broken or still unsolved, where the real error is elsewhere.
    fn no_such_field(&mut self, node: NodeId, base: &Ty, name: &str) -> Ty {
        let base = self.cx.resolve(base);
        if !matches!(base, Ty::Error) && !is_var(&base) {
            let msg = format!("no field `{name}` on `{}`", base.display(self.defs));
            self.report(node, msg);
        }
        Ty::Error
    }

    fn field_ty(&mut self, base: &Ty, name: &str) -> Option<Ty> {
        let base = self.autoderef(base);
        let Ty::Nominal { def, args } = base else {
            return None;
        };
        let field = *self
            .defs
            .get(def)
            .ns
            .members
            .get(&crate::common::symbol::Symbol::new(name))?;
        if self.defs.get(field).kind != DefKind::Field {
            return None;
        }
        let d = self.defs.get(field);
        let (file, node) = (d.file?, d.node?);
        let t = match self.asts[&file].node(node).kind.clone() {
            NodeKind::Field { ty, .. } => self.ty_from_node_in(file, ty),
            // A tuple struct's field def points straight at the positional type
            // node: there is no `Field` node wrapping it (see `collect_struct`).
            _ => self.ty_from_node_in(file, node),
        };
        let map = self.nominal_subst(def, &args);
        Some(self.subst_type_params(&t, &map))
    }

    /// The declared payload types of enum variant `name` on `base`, in order,
    /// each paired with its field name (for record variants) and with the enum's
    /// generics substituted. `None` if `base` is not an enum with that variant.
    fn variant_payload(
        &mut self,
        base: &Ty,
        name: &str,
    ) -> Option<Vec<(Option<crate::common::symbol::Symbol>, Ty)>> {
        use crate::parser::ast::VariantPayload;
        let base = self.autoderef(base);
        let Ty::Nominal { def, args } = base else {
            return None;
        };
        let variant = *self
            .defs
            .get(def)
            .ns
            .members
            .get(&crate::common::symbol::Symbol::new(name))?;
        if self.defs.get(variant).kind != DefKind::Variant {
            return None;
        }
        let vd = self.defs.get(variant);
        let (file, node) = (vd.file?, vd.node?);
        let payload = match &self.asts[&file].node(node).kind {
            NodeKind::Variant { payload, .. } => payload.clone(),
            _ => return None,
        };
        let map = self.nominal_subst(def, &args);
        let mut out = Vec::new();
        match payload {
            VariantPayload::None => {}
            VariantPayload::Tuple(tys) => {
                for t in tys {
                    let ty = self.ty_from_node_in(file, t);
                    out.push((None, self.subst_type_params(&ty, &map)));
                }
            }
            VariantPayload::Record(fields) => {
                for f in fields {
                    if let NodeKind::Field { name, ty, .. } = self.asts[&file].node(f).kind.clone()
                    {
                        let ty = self.ty_from_node_in(file, ty);
                        out.push((Some(name), self.subst_type_params(&ty, &map)));
                    }
                }
            }
        }
        Some(out)
    }

    /// The positional field types of a tuple struct `Type(A, B, …)`, with
    /// generics substituted. `None` if `base` is not a tuple struct.
    fn tuple_struct_tys(&mut self, base: &Ty) -> Option<Vec<Ty>> {
        use crate::parser::ast::StructKind;
        let base = self.autoderef(base);
        let Ty::Nominal { def, args } = base else {
            return None;
        };
        let d = self.defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let rhs = match &self.asts[&file].node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let tys = match &self.asts[&file].node(rhs).kind {
            NodeKind::StructType {
                kind: StructKind::Tuple(tys),
                ..
            } => tys.clone(),
            _ => return None,
        };
        let map = self.nominal_subst(def, &args);
        Some(
            tys.iter()
                .map(|&t| {
                    let ty = self.ty_from_node_in(file, t);
                    self.subst_type_params(&ty, &map)
                })
                .collect(),
        )
    }

    /// Peel pointers off a (shallow-resolved) type for member/field access.
    fn autoderef(&self, ty: &Ty) -> Ty {
        let mut cur = self.cx.shallow(ty);
        while let Ty::Ptr { inner, .. } = cur {
            cur = self.cx.shallow(&inner);
        }
        cur
    }

    // ===< patterns >===

    /// Bind the locals a pattern introduces, unifying against the scrutinee type.
    fn bind_pattern(&mut self, pat: NodeId, ty: &Ty) {
        match self.ast.node(pat).kind.clone() {
            NodeKind::BindingPat { .. } => {
                if let Some(def) = self.def_of(pat) {
                    self.env.insert(def, ty.clone());
                }
            }
            NodeKind::AtPat { pattern, .. } => {
                if let Some(def) = self.def_of(pat) {
                    self.env.insert(def, ty.clone());
                }
                self.bind_pattern(pattern, ty);
            }
            NodeKind::TuplePat { elems } => {
                let parts: Vec<Ty> = (0..elems.len()).map(|_| self.cx.fresh()).collect();
                let tup = Ty::Tuple(parts.clone());
                let _ = self.cx.unify(ty, &tup);
                for (e, pty) in elems.iter().zip(parts) {
                    self.bind_pattern(*e, &pty);
                }
            }
            NodeKind::RefPat { pattern } => {
                let inner = self.cx.fresh();
                let ptr = Ty::Ptr {
                    mutable: false,
                    inner: Box::new(inner.clone()),
                };
                let _ = self.cx.unify(ty, &ptr);
                self.bind_pattern(pattern, &inner);
            }
            NodeKind::OrPat { alternatives } => {
                for a in alternatives {
                    self.bind_pattern(a, ty);
                }
            }
            // A variant pattern binds each payload sub-pattern to the variant's
            // declared payload type (generics substituted from the scrutinee).
            NodeKind::VariantPat { name, args } => match args {
                VariantPatArgs::None => {}
                VariantPatArgs::Tuple(elems) => {
                    let payload = self.variant_payload(ty, name.as_str());
                    for (i, e) in elems.iter().enumerate() {
                        let pty = payload
                            .as_ref()
                            .and_then(|p| p.get(i))
                            .map(|(_, t)| t.clone())
                            .unwrap_or_else(|| self.cx.fresh());
                        self.bind_pattern(*e, &pty);
                    }
                }
                VariantPatArgs::Record { fields, .. } => {
                    let payload = self.variant_payload(ty, name.as_str());
                    for f in fields {
                        self.bind_record_field(f, payload.as_deref());
                    }
                }
            },
            // A `FieldPat` reached on its own (defensive: normally handled by its
            // enclosing struct/variant record).
            NodeKind::FieldPat { .. } => self.bind_record_field(pat, None),
            NodeKind::StructPat { fields, .. } => {
                for f in fields {
                    let NodeKind::FieldPat { name, pattern, .. } = self.ast.node(f).kind.clone()
                    else {
                        continue;
                    };
                    let fty = self
                        .field_ty(ty, name.as_str())
                        .unwrap_or_else(|| self.cx.fresh());
                    match pattern {
                        Some(p) => self.bind_pattern(p, &fty),
                        None => {
                            if let Some(def) = self.def_of(f) {
                                self.env.insert(def, fty);
                            }
                        }
                    }
                }
            }
            NodeKind::TupleStructPat { elems, .. } => {
                let tys = self.tuple_struct_tys(ty);
                for (i, e) in elems.iter().enumerate() {
                    let pty = tys
                        .as_ref()
                        .and_then(|t| t.get(i))
                        .cloned()
                        .unwrap_or_else(|| self.cx.fresh());
                    self.bind_pattern(*e, &pty);
                }
            }
            NodeKind::SlicePat { elems, rest } => {
                // Every element pattern matches the scrutinee's element type.
                let elem = match self.autoderef(ty) {
                    Ty::Slice { inner, .. } | Ty::Array { inner, .. } => *inner,
                    _ => self.cx.fresh(),
                };
                for e in elems {
                    self.bind_pattern(e, &elem);
                }
                if let Some(SliceRest { name: Some(_), .. }) = rest {
                    if let Some(def) = self.def_of(pat) {
                        self.env.insert(
                            def,
                            Ty::Slice {
                                mutable: false,
                                inner: Box::new(elem),
                            },
                        );
                    }
                }
            }
            // Wildcards, literals, ranges bind nothing.
            _ => {}
        }
    }

    /// Bind one record `FieldPat` (`{ radius }` shorthand or `{ radius: p }`),
    /// typed from `payload` (the enclosing variant's field types) by name.
    fn bind_record_field(
        &mut self,
        f: NodeId,
        payload: Option<&[(Option<crate::common::symbol::Symbol>, Ty)]>,
    ) {
        let NodeKind::FieldPat { name, pattern, .. } = self.ast.node(f).kind.clone() else {
            return;
        };
        let fty = payload
            .and_then(|p| {
                p.iter()
                    .find(|(n, _)| n.as_ref() == Some(&name))
                    .map(|(_, t)| t.clone())
            })
            .unwrap_or_else(|| self.cx.fresh());
        match pattern {
            Some(p) => self.bind_pattern(p, &fty),
            None => {
                if let Some(def) = self.def_of(f) {
                    self.env.insert(def, fty);
                }
            }
        }
    }

    // ===< type expressions >===

    /// Convert a type-expression node into a [`Ty`], resolving named heads
    /// through the [`Resolution`] the name-resolver attached.
    fn ty_from_node(&mut self, node: NodeId) -> Ty {
        self.ty_from_node_in(self.file, node)
    }

    /// Stamp the declared type of every member in this file onto its own node,
    /// for lowering to read back when it builds the IR's type definitions.
    ///
    /// The types are **definition-relative**: a field of `Box.<T>` declared `T`
    /// is stamped as the type parameter, not as anything a use site
    /// substituted. Substituting is monomorphization's job, and a definition
    /// that had already been specialized would be no use to it.
    fn stamp_member_types(&mut self) {
        // Walk by node kind rather than by def, because only some members have
        // one: a struct's fields do, a variant's payload does not (it belongs to
        // the variant, not to the enum's namespace), and a `distinct`'s
        // representation is a bare type node.
        let nodes: Vec<NodeId> = self.ast.ids().collect();
        for n in nodes {
            match self.ast.node(n).kind.clone() {
                // A record member, of a struct or of a variant alike. The type
                // is stamped on the `Field` node itself, which is what the
                // member's def points at.
                NodeKind::Field { ty, .. } => {
                    let t = self.ty_from_node(ty);
                    self.ast.set_meta(n, t);
                }
                NodeKind::Variant {
                    payload: crate::parser::ast::VariantPayload::Tuple(types),
                    ..
                } => {
                    for t in types {
                        let ty = self.ty_from_node(t);
                        self.ast.set_meta(t, ty);
                    }
                }
                NodeKind::DistinctType { inner, .. } => {
                    let ty = self.ty_from_node(inner);
                    self.ast.set_meta(inner, ty);
                }
                // An associated constant declares a type every impl's value
                // must have. Stamped on the `AssocConst` node itself, which is
                // what the member's def points at.
                NodeKind::AssocConst { ty, .. } => {
                    let t = self.ty_from_node(ty);
                    self.ast.set_meta(n, t);
                }
                _ => {}
            }
        }

        // A **trait method** has no body, so the per-function passes never type
        // it: its signature is worked out on demand when a call selects it. A
        // vtable slot is not a call, though, and object safety is a question
        // about the signature alone — so stamp it here, once, on the method's
        // own `FuncExpr`.
        let trait_methods: Vec<DefId> = self
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Trait && d.file == Some(self.file))
            .flat_map(|d| d.ns.members.values().copied())
            .filter(|&m| self.defs.get(m).kind == DefKind::Func)
            .collect();
        for m in trait_methods {
            let ty = self.func_def_ty(m);
            let (Some(file), Some(node)) = (self.defs.get(m).file, self.defs.get(m).node) else {
                continue;
            };
            if file != self.file {
                continue;
            }
            // The def's node is the `ConstBind`; stamp the `FuncExpr` it binds,
            // which is the node lowering reads the signature off. It goes under
            // its own key rather than as the node's `Ty`, which `infer_func`
            // uses for the *return* type — a trait method with a default body
            // would otherwise have this overwritten by the per-function pass.
            let func = match &self.asts[&file].node(node).kind {
                NodeKind::ConstBind { rhs, .. } => *rhs,
                _ => node,
            };
            let ty = self.cx.resolve(&ty);
            self.asts[&file].set_meta(func, super::Signature(ty));
        }

        // A tuple struct's positions are `Field` defs whose node is the type
        // node itself — there is no `Field` node wrapping them (see
        // `collect_struct`), so the loop above did not reach them.
        let positions: Vec<NodeId> = self
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Field && d.file == Some(self.file))
            .filter_map(|d| d.node)
            .filter(|&n| !matches!(&self.ast.node(n).kind, NodeKind::Field { .. }))
            .collect();
        for at in positions {
            let t = self.ty_from_node(at);
            self.ast.set_meta(at, t);
        }
    }

    fn ty_from_node_in(&mut self, file: FileId, node: NodeId) -> Ty {
        let ast = &self.asts[&file];
        match ast.node(node).kind.clone() {
            NodeKind::TypeHole => self.cx.fresh(),
            NodeKind::PtrType { mutable, inner } => Ty::Ptr {
                mutable,
                inner: Box::new(self.ty_from_node_in(file, inner)),
            },
            NodeKind::SliceType { mutable, inner, .. } => Ty::Slice {
                mutable,
                inner: Box::new(self.ty_from_node_in(file, inner)),
            },
            NodeKind::ArrayType {
                len,
                mutable,
                inner,
                ..
            } => {
                let len = self.const_len_in(file, len);
                Ty::Array {
                    len,
                    mutable,
                    inner: Box::new(self.ty_from_node_in(file, inner)),
                }
            }
            NodeKind::TupleType { elems } => {
                if elems.is_empty() {
                    Ty::Void
                } else {
                    Ty::Tuple(
                        elems
                            .iter()
                            .map(|e| self.ty_from_node_in(file, *e))
                            .collect(),
                    )
                }
            }
            NodeKind::FuncType { params, ret, .. } => Ty::Func {
                params: params
                    .iter()
                    .map(|p| self.ty_from_node_in(file, *p))
                    .collect(),
                ret: Box::new(
                    ret.map(|t| self.ty_from_node_in(file, t))
                        .unwrap_or(Ty::Void),
                ),
            },
            NodeKind::DynType { inner } => match self.type_head_def_in(file, inner) {
                Some(def) => Ty::Dyn(def),
                None => Ty::Error,
            },
            NodeKind::DistinctType { inner, .. } => self.ty_from_node_in(file, inner),
            NodeKind::TypePath { generic_args, .. } => self.typepath_ty(file, node, &generic_args),
            // `Type.<args>` in expression position (e.g. a composite-literal head)
            // parses as a postfix generic application; resolve it like a typepath
            // whose head is the base.
            NodeKind::GenericApply { base, args } => self.typepath_ty(file, base, &args),
            NodeKind::Path { .. } => self.typepath_ty(file, node, &[]),
            _ => Ty::Error,
        }
    }

    /// Resolve a `TypePath` (or bare `Path` in type position) to a [`Ty`] from
    /// the def its head names.
    fn typepath_ty(&mut self, file: FileId, node: NodeId, generic_args: &[NodeId]) -> Ty {
        let Some(def) = self.resolved_def_in(file, node) else {
            return Ty::Error;
        };
        let kind = self.defs.get(def).kind;
        match kind {
            DefKind::Primitive => {
                primitive_ty(self.defs.get(def).name.as_str()).unwrap_or(Ty::Error)
            }
            DefKind::Struct | DefKind::Enum | DefKind::Trait => {
                let mut args: Vec<Ty> = generic_args
                    .iter()
                    .filter(|&&a| {
                        !matches!(self.asts[&file].node(a).kind, NodeKind::AssocBinding { .. })
                    })
                    .map(|&a| self.ty_from_node_in(file, a))
                    .collect();
                // Record `<Assoc = T>` constraints alongside.
                for &a in generic_args {
                    self.record_generic_arg_in(file, a);
                }
                // A generic type named without (all of) its arguments — `Box` for
                // `Box.<T>` — gets a fresh variable per missing parameter, so the
                // use site infers them. Naming none of them is the common case:
                // `Box { item: 4 }`.
                let declared = self.type_param_defs(def).len();
                while args.len() < declared {
                    args.push(self.cx.fresh());
                }
                Ty::Nominal { def, args }
            }
            // A type alias (a `distinct`/plain alias, or an impl's associated-type
            // binding `Output :: Vec3`) expands to its right-hand side. This is
            // what turns `Self.Output` on a concrete type into the impl's chosen
            // type (§ associated-type projection).
            DefKind::TypeAlias => self.expand_alias(def),
            // A generic type parameter is a rigid opaque type of its own def.
            DefKind::TypeParam => Ty::Nominal { def, args: vec![] },
            // `<const N: usize>` is a *value*; writing `N` where a type belongs
            // is the one confusion the two-kind generic list exists to prevent.
            DefKind::ConstParam => {
                let msg = format!(
                    "`{}` is a `const` generic parameter — a value, not a type",
                    self.defs.get(def).name
                );
                self.report_in(file, node, msg);
                Ty::Error
            }
            _ => Ty::Error,
        }
    }

    /// Expand a type-alias / associated-type binding to the type it names.
    ///
    /// For an impl's `Output :: Vec3` this is `Vec3`; for a plain alias it is the
    /// aliased type. A **`distinct`** alias is the exception: it is a fresh
    /// nominal type over its underlying one and deliberately does *not* expand,
    /// so `Meters` and `i32` never unify (§3.8) — converting between them takes
    /// an explicit `$cast`.
    ///
    /// An **abstract** associated type (a trait's `Output :: type`, reached when
    /// the self type is still generic) has no concrete value, so it becomes a
    /// fresh variable to be pinned by context (e.g. the enclosing return type).
    /// Cycles fall back to an opaque nominal.
    fn expand_alias(&mut self, def: DefId) -> Ty {
        if self.alias_stack.contains(&def) {
            return Ty::Nominal {
                def,
                args: Vec::new(),
            };
        }
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Ty::Nominal {
                def,
                args: Vec::new(),
            };
        };
        let rhs = match &self.asts[&file].node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        if matches!(
            self.asts[&file].node(rhs).kind,
            NodeKind::DistinctType { .. }
        ) {
            return Ty::Nominal {
                def,
                args: Vec::new(),
            };
        }
        if matches!(self.asts[&file].node(rhs).kind, NodeKind::AssocType { .. }) {
            return self.cx.fresh();
        }
        self.alias_stack.push(def);
        let ty = self.ty_from_node_in(file, rhs);
        self.alias_stack.pop();
        ty
    }

    /// Evaluate the length of a `[len]T` to the [`Const`] the type carries.
    ///
    /// Three things can stand there (§3.2, §5): a literal, a `const` generic
    /// parameter — which stays symbolic until monomorphization — and the hole
    /// `[_]T`, whose length a composite literal fills in. A named constant is
    /// followed one hop to its right-hand side, so `SIZE :: 4` makes `[SIZE]T`
    /// a `[4]T`. Anything else (an arithmetic expression, say) has no
    /// const-evaluator behind it yet and is a diagnostic.
    fn const_len_in(&mut self, file: FileId, node: NodeId) -> Const {
        self.const_len_depth(file, node, 0)
    }

    fn const_len_depth(&mut self, file: FileId, node: NodeId, depth: u32) -> Const {
        // A constant that names itself would otherwise loop forever.
        if depth > 8 {
            self.report_in(file, node, "array length refers to itself");
            return Const::Error;
        }
        match self.asts[&file].node(node).kind.clone() {
            NodeKind::Lit(Lit::Int(n)) => match u64::try_from(&n) {
                Ok(v) => Const::Value(v),
                Err(_) => {
                    self.report_in(file, node, "array length does not fit in a `usize`");
                    Const::Error
                }
            },
            // `[_]T` — the length is whatever the value supplies.
            NodeKind::TypeHole => self.cx.fresh_const(),
            NodeKind::Path { .. } | NodeKind::TypePath { .. } => {
                match self.resolved_def_in(file, node) {
                    Some(def) => self.const_len_of_def(file, node, def, depth),
                    // Unresolved: name resolution already complained.
                    None => Const::Error,
                }
            }
            _ => {
                self.report_in(
                    file,
                    node,
                    "an array length must be an integer literal, a constant, or a `const` generic parameter",
                );
                Const::Error
            }
        }
    }

    /// The length a definition standing in a `[len]T` denotes.
    fn const_len_of_def(&mut self, file: FileId, node: NodeId, def: DefId, depth: u32) -> Const {
        let def = self.defs.resolve_alias(def);
        let d = self.defs.get(def);
        match d.kind {
            DefKind::ConstParam => Const::Param(def),
            DefKind::Const => {
                let (Some(cfile), Some(cnode)) = (d.file, d.node) else {
                    return Const::Error;
                };
                let NodeKind::ConstBind { rhs, .. } = self.asts[&cfile].node(cnode).kind.clone()
                else {
                    return Const::Error;
                };
                self.const_len_depth(cfile, rhs, depth + 1)
            }
            _ => {
                let msg = format!(
                    "`{}` is a {} — an array length must be a constant value",
                    self.defs.canonical_string(def),
                    d.kind.label()
                );
                self.report_in(file, node, msg);
                Const::Error
            }
        }
    }

    /// Record a `<Assoc = T>` binding (ignoring plain type args / holes).
    fn record_generic_arg(&mut self, node: NodeId) {
        self.record_generic_arg_in(self.file, node);
    }

    fn record_generic_arg_in(&mut self, file: FileId, node: NodeId) {
        if let NodeKind::AssocBinding { name, ty } = self.asts[&file].node(node).kind.clone() {
            let t = self.ty_from_node_in(file, ty);
            self.cx.assoc.insert(name, t);
        }
    }

    // ===< literals / helpers >===

    /// The type of a string literal: the `#lang("str")` item in `core`.
    ///
    /// `str` is not a compiler primitive — it is `distinct []u8` declared in
    /// core, so that all the slice machinery (interior pointers, bounds, GC
    /// tracing) is inherited rather than reimplemented. Found by tag, never by
    /// name or path, like every other language item.
    fn str_ty(&self) -> Ty {
        str_lang_ty(self.defs, self.lang).unwrap_or(Ty::Error)
    }

    fn lit_ty(&mut self, lit: &Lit) -> Ty {
        match lit {
            Lit::Int(_) => self.cx.fresh_of(TyVarKind::Int),
            Lit::Float(_) => self.cx.fresh_of(TyVarKind::Float),
            // A string literal is a `comptime_str` (§1.5): a variable that the
            // use site settles on `str`, `[]u8` or `[]char`, exactly as a
            // numeric literal's settles on a width. It defaults to `str`.
            Lit::Str(_) => match self.lang.get("str") {
                // With no `#lang("str")` item there is nothing for the literal
                // to default to, and nothing it could unify with either. That is
                // a broken `core`, already reported as such; `Ty::Error` here
                // keeps it from cascading, which is what this case did before
                // the literal became open at all.
                None => Ty::Error,
                Some(_) => self.cx.fresh_of(TyVarKind::Str),
            },
            // A byte-string literal has exactly one type: it is bytes, and
            // nothing about it is open (§1.5). No UTF-8 promise attaches to it,
            // so it is *not* a `str`, and no variable is needed.
            Lit::Bytes(_) => Ty::Slice {
                mutable: false,
                inner: Box::new(Ty::Int {
                    signed: false,
                    width: crate::sema::ty::IntWidth::Fixed(8),
                }),
            },
            Lit::Char(_) => Ty::Char,
            Lit::Bool(_) => Ty::Bool,
        }
    }

    /// Unify `actual` with `expected`, reporting a mismatch anchored at `node`.
    ///
    /// A plain mismatch gets one more chance: a struct with an `@using` field
    /// implicitly upcasts to that field's type (§3.10), so try the coercion
    /// before reporting.
    /// Check a value against the function's declared **return** type.
    ///
    /// Identical to [`Inferer::expect`] except that a declared `-> never` is not
    /// treated as an impossible demand. It is a real and useful signature, and
    /// whether the body honours it — no reachable `return`, no reachable fall
    /// off the end — is a *reachability* question that the IR divergence pass
    /// answers (§3.1). Reporting it here as a type mismatch would give
    /// `func () -> never { loop {} }`, which is correct code, an error, and give
    /// `func () -> never { }` the wrong explanation.
    fn expect_return(&mut self, node: NodeId, actual: &Ty, expected: &Ty) {
        if matches!(self.cx.resolve(expected), Ty::Never) {
            let _ = self.cx.unify(actual, expected);
            return;
        }
        self.expect(node, actual, expected);
    }

    fn expect(&mut self, node: NodeId, actual: &Ty, expected: &Ty) {
        // `never` is one-way. It converts *to* every type, which is what lets a
        // diverging call sit in any expression position; nothing converts *to*
        // it, because a value of an uninhabited type cannot exist (§3.1).
        // `unify` cannot draw that line — it is symmetric, and its callers are
        // mostly joins where `never` really is the identity — so it is drawn
        // here, at the one place that knows which side is the value and which is
        // the demand.
        //
        // Without this, `let x: never := 5` type-checks and then converts `x`
        // into anything, which is the whole soundness argument for `never`
        // running backwards.
        let want = self.cx.resolve(expected);
        let got = self.cx.resolve(actual);
        if matches!(want, Ty::Never) && !matches!(got, Ty::Never | Ty::Error) {
            // Do not name a type that is still an inference variable: a literal
            // reaching here has not been defaulted yet, and `found `?0`` tells
            // the reader nothing they can act on.
            let found = match got {
                Ty::Var(_) => String::new(),
                other => format!(", found `{}`", other.display(self.defs)),
            };
            let msg = format!(
                "expected `never`{found}: `never` is uninhabited, so no value has that type"
            );
            self.report(node, msg);
            return;
        }
        let snapshot = self.cx.snapshot();
        if let Err((a, b)) = self.cx.unify(actual, expected) {
            self.cx.rollback(snapshot);
            if self.try_array_to_slice(node, actual, expected)
                || self.try_dyn_coerce(node, actual, expected)
                || self.try_upcast(node, actual, expected)
            {
                return;
            }
            // Render what is *known* about each side: the clash is reported
            // against the pre-attempt substitution (the rollback above), so a
            // nested variable an earlier step already solved prints as itself
            // rather than as `?3` / `?c0`.
            let msg = format!(
                "type mismatch: expected `{}`, found `{}`",
                self.cx.describe(&b, self.defs),
                self.cx.describe(&a, self.defs)
            );
            self.report(node, msg);
        }
    }

    /// Try to reach `expected` from `actual` by unsizing a fixed array to a
    /// read-only slice: `[N]T` coerces to `[]T` (§3.2), which is what lets one
    /// `func (s: []T)` serve every array length.
    ///
    /// Never to `[]mut T`: handing out a mutable view is a permission the
    /// coercion has no business granting silently. What the coercion *is* is
    /// taking the whole sub-slice — `a[..]` — so it is recorded as a
    /// [`SliceCoerce`] and lowers to that, not to a `$cast`.
    fn try_array_to_slice(&mut self, node: NodeId, actual: &Ty, expected: &Ty) -> bool {
        let (
            Ty::Array { inner, .. },
            Ty::Slice {
                mutable: false,
                inner: want,
            },
        ) = (self.cx.shallow(actual), self.cx.shallow(expected))
        else {
            return false;
        };
        // Without the range lang item there is no `a[..]` to lower to.
        let Some(range) = self.lang.get("range") else {
            return false;
        };
        let snapshot = self.cx.snapshot();
        if self.cx.unify(&inner, &want).is_err() {
            self.cx.rollback(snapshot);
            return false;
        }
        let to = self.cx.resolve(expected);
        let range = Ty::Nominal {
            def: self.defs.resolve_alias(range),
            args: vec![Ty::usize()],
        };
        self.ast.set_meta(node, SliceCoerce { to, range });
        true
    }

    /// Try to reach `expected` from `actual` by unsizing a concrete pointer to a
    /// trait object: `*T` coerces to `*dyn Trait` when `T: Trait` (§3.2),
    /// recording a [`DynCoerce`] on `node` so lowering builds the fat pointer.
    fn try_dyn_coerce(&mut self, node: NodeId, actual: &Ty, expected: &Ty) -> bool {
        // Only pointers unsize; the pointee must be a real type on the left and
        // the trait object on the right, with mutability the usual `*mut` → `*`.
        let (
            Ty::Ptr { mutable, inner },
            Ty::Ptr {
                mutable: em,
                inner: ei,
            },
        ) = (self.cx.shallow(actual), self.cx.shallow(expected))
        else {
            return false;
        };
        if em && !mutable {
            return false;
        }
        let Ty::Dyn(trait_def) = self.cx.shallow(&ei) else {
            return false;
        };
        let concrete = self.cx.shallow(&inner);
        if is_var(&concrete) || matches!(concrete, Ty::Error) {
            return false;
        }
        // The coercion is only sound when the concrete type really implements
        // the trait; an unsatisfied bound stays a plain type mismatch.
        if !matches!(self.select(&concrete, trait_def, &[]), Select::Ok(_)) {
            return false;
        }
        self.ast.set_meta(
            node,
            DynCoerce {
                trait_def,
                concrete,
            },
        );
        true
    }

    /// Try to reach `expected` from `actual` through an `@using` field's implicit
    /// upcast, recording an [`Upcast`] on `node` when it works.
    ///
    /// `Entity` coerces to `Transform` by copying the field; `*Entity` coerces to
    /// `*Transform` by taking the sub-object's address. Only one hop is tried: a
    /// chain of upcasts is deliberately not implicit.
    fn try_upcast(&mut self, node: NodeId, actual: &Ty, expected: &Ty) -> bool {
        let (head, through_ptr) = match self.cx.shallow(actual) {
            Ty::Nominal { def, .. } => (def, false),
            Ty::Ptr { inner, .. } => match self.cx.shallow(&inner) {
                Ty::Nominal { def, .. } => (def, true),
                _ => return false,
            },
            _ => return false,
        };
        let Some(field) = self.defs.using_field(head) else {
            return false;
        };
        let Some(fty) = self.field_ty(actual, self.defs.get(field).name.as_str()) else {
            return false;
        };
        // The upcast of a pointer yields a pointer to the sub-object, keeping the
        // receiver's mutability.
        let target = match (through_ptr, self.cx.shallow(actual)) {
            (true, Ty::Ptr { mutable, .. }) => Ty::Ptr {
                mutable,
                inner: Box::new(fty),
            },
            _ => fty,
        };
        let snapshot = self.cx.snapshot();
        if self.cx.unify(&target, expected).is_err() {
            self.cx.rollback(snapshot);
            return false;
        }
        self.ast.set_meta(
            node,
            Upcast {
                field,
                through_ptr,
                target,
            },
        );
        true
    }

    /// Whether a statement unconditionally transfers control out of its block.
    fn diverges(&self, node: NodeId) -> bool {
        match &self.ast.node(node).kind {
            NodeKind::Return { .. } | NodeKind::Break { .. } | NodeKind::Continue => true,
            // A diverging intrinsic in statement position ends the block just as
            // a `return` does — `.!` leans on this to type its abort arm.
            NodeKind::IntrinsicCall { name, .. } => DIVERGING_INTRINSICS.contains(&name.as_str()),
            _ => false,
        }
    }

    fn def_of(&self, node: NodeId) -> Option<super::def::DefId> {
        self.ast.meta::<DefMeta>(node).map(|m| m.0)
    }

    fn resolved_def(&self, node: NodeId) -> Option<super::def::DefId> {
        self.resolved_def_in(self.file, node)
    }

    fn resolved_def_in(&self, file: FileId, node: NodeId) -> Option<super::def::DefId> {
        match self.asts[&file].meta::<Resolution>(node)? {
            Resolution::Def(d) => Some(self.defs.resolve_alias(d)),
            _ => None,
        }
    }

    fn type_head_def_in(&self, file: FileId, node: NodeId) -> Option<super::def::DefId> {
        self.resolved_def_in(file, node)
    }

    fn report(&mut self, node: NodeId, message: impl Into<String>) {
        self.report_in(self.file, node, message);
    }

    /// Report against a node in another file's arena — a signature or a
    /// constant reached from the body currently being checked.
    fn report_in(&mut self, file: FileId, node: NodeId, message: impl Into<String>) {
        let span = self.asts[&file].node(node).span;
        self.diags
            .push(Diagnostic::error(message).with_primary(FileSpan::new(file, span), ""));
    }
}

/// One instantiation's answer for a set of generic parameters.
///
/// Generics come in two kinds (§5) — types (`<T>`) and compile-time values
/// (`<const N: usize>`) — and they substitute into different parts of a [`Ty`]:
/// a type parameter replaces a whole [`Ty::Nominal`] node, a const parameter
/// only the `len` of a [`Ty::Array`]. They travel together because one
/// signature can mention both (`func <const N: usize, T> (a: [N]T)`).
#[derive(Debug, Default, Clone)]
struct Subst {
    tys: HashMap<DefId, Ty>,
    consts: HashMap<DefId, Const>,
}

impl Subst {
    /// A substitution that only maps types — the common case.
    fn of_types(tys: HashMap<DefId, Ty>) -> Self {
        Subst {
            tys,
            consts: HashMap::new(),
        }
    }
}

/// Whether `$len` has an answer for this type: an array or a slice carries its
/// element count, nothing else does (§3.2, §6.4).
///
/// `.len()` itself is **not** compiler syntax — it is `core`'s `Len` trait,
/// implemented for `[N]T` and `[]T` with a body that is this intrinsic. That is
/// what makes `a.len()`, `s.len()`, and a future `Vector.len()` one spelling
/// with one meaning instead of a special case the checker has to know about.
pub fn has_len(base: &Ty) -> bool {
    matches!(base, Ty::Array { .. } | Ty::Slice { .. })
}

/// The outcome of attempting one [`Obligation`].
enum Outcome {
    /// Discharged.
    Solved,
    /// Blocked on an unsolved variable; retry after more inference.
    Deferred,
    /// Unsatisfiable; a diagnostic was reported.
    Failed,
}

/// The result of impl selection for an obligation.
enum Select {
    /// A unique best impl was found.
    Ok(Choice),
    /// The self type is not yet known; try again later.
    Defer,
    /// No candidate impl applies to a known self type.
    NoImpl,
    /// Two or more equally specific impls apply.
    Ambiguous,
    /// The self type is already `Error`; absorb without further diagnostics.
    Error,
}

/// The selected impl: a builtin primitive op, or a user impl (by table index).
#[derive(Clone, Copy)]
enum Choice {
    Builtin(&'static BuiltinRow),
    User(usize),
}

fn is_var(ty: &Ty) -> bool {
    matches!(ty, Ty::Var(_))
}

/// The `#lang` tag of the operator trait a [`BinOp`] dispatches to (§6.13).
///
/// Only the operators that *are* trait calls appear: `&&` / `||` and the
/// comparisons are not in this table — the first two are control flow, and the
/// comparisons reach `Eq` / `Ord` through [`Inferer::check_cmp_bound`], whose
/// method (`eq` / `cmp`) is not named after the operator.
fn binop_lang(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Rem => "rem",
        BinOp::BitAnd => "bitand",
        BinOp::BitOr => "bitor",
        BinOp::BitXor => "bitxor",
        BinOp::Shl => "shl",
        BinOp::Shr => "shr",
        _ => "",
    }
}

/// The trait method name a [`BinOp`] calls.
fn binop_method(op: BinOp) -> &'static str {
    // For every operator trait in the table the method name equals the tag.
    binop_lang(op)
}
