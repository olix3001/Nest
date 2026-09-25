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
use crate::parser::ast::{
    Ast, BinOp, Lit, NodeId, NodeKind, SliceRest, StructKind, UnOp, VariantArgs, VariantPatArgs,
    WideFloat,
};

use super::builtins::{self, Applies, BuiltinOp, BuiltinRow};
use num_traits::ToPrimitive;

use crate::ir::ConstValue;

use super::decl::{ConstTy, DeclTable, Decls};
use super::def::{DefId, DefKind, DefTable, LangItems};
use super::impls::{ImplInfo, ImplTable, TypedImpl};
use super::ty::{Const, FloatWidth, InferCtxt, Obligation, Ty, TyVarKind, primitive_ty};
use super::{DefMeta, Resolution};

/// One enclosing `loop` / `while` while its body is being inferred.
struct LoopFrame {
    /// The type its `break`s agree on.
    ty: Ty,
    /// Whether any `break` targeted it. A `loop` with none never finishes, so it
    /// types as [`Ty::Never`] rather than as an unsolved variable.
    broke: bool,
}

/// How an operator (or other trait-dispatched) node resolved, stamped onto the
/// operator's AST node by the trait solver so [`super::lower`] can emit a
/// **uniform** [`crate::ir::Expr::Call`] whether the operand was a primitive or
/// a user type.
///
/// [`builtin`](OpResolution::builtin) is `Some` iff the resolved impl was a
/// builtin primitive op (see [`super::builtins`]); codegen keys on it to emit
/// the machine instruction in O(1) rather than a real call.
/// The discriminant one enum variant stores, stamped on the variant's own node
/// (§3.3).
///
/// It is the position for an enum nobody wrote a `= value` on, which is why the
/// two were one number until now; an explicit discriminant is what separates
/// them, and the tag is the one that reaches a value. Lowering copies it onto
/// the IR's [`crate::ir::Variant`], which is where every later pass reads it —
/// including for a *foreign* enum, whose tree this compilation does not have.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct VariantTag(pub i128);

/// A static trait call (`Trait.member(args)`) whose `Self` a type parameter's
/// bound answered: `FromIterator.from_iter(it)` inside `collect.<C>`, where
/// `Self` is `C`. No impl can be chosen until an instantiation says what `C` is,
/// so lowering makes it a [`crate::ir::Dispatch::Generic`] call and
/// monomorphization selects the impl, as it does for a method called through a
/// bound.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StaticTraitSelf {
    pub trait_def: DefId,
    pub self_ty: Ty,
    pub trait_args: Vec<Ty>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MethodDispatch {
    /// A direct call to a known function.
    Static,
    /// Through the vtable of a `*dyn Trait` receiver; the payload is the trait.
    Virtual(DefId),
    /// Through a type parameter's bound.
    Generic {
        trait_def: DefId,
        /// The trait's own generic arguments **as the bound wrote them**:
        /// `<T: Add.<f64>>` gives `[f64]`.
        ///
        /// They are carried because they are part of *which impl* the bound
        /// stands for, and monomorphization is the stage that has to pick one.
        /// `impl Add.<f64> for Vec3` and `impl Add.<i32> for Vec3` are two
        /// perfectly coherent impls (§4.9 — they do not overlap, because the
        /// trait's arguments differ), and with only the trait to go on there is
        /// nothing to tell them apart by.
        args: Vec<Ty>,
    },
}

/// What lowering must do to the receiver expression to hand it to the `self`
/// parameter (§3.4). Nest has no implicit reference-taking in the type system —
/// the adjustment is decided here and *written out* in the IR, so `x.m()` on a
/// `*mut Self` method is an `&mut x` the later mutability check can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
/// Marks a node whose value **inference already reported** as out of range for
/// the type it settled on, so the const evaluator's own range check does not say
/// it twice.
///
/// The two checks overlap on purpose and neither is redundant: inference catches
/// a literal at the site it is written, with the best span, and the evaluator
/// catches everything *computed* (`A: u8 :: 200 * 2`), which inference never
/// sees a value for. They only ever meet on a bare literal, and this is what
/// keeps that meeting from producing two diagnostics for one mistake.
///
/// Set on the AST node by inference and copied onto the lowered `$cast` by
/// [`super::lower`], because the evaluator reports against the cast.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct RangeReported;

/// A **compile-time value slot** this pass has already complained about: the
/// length in `[SIZE * 2]T`, or an explicit `const` generic argument.
///
/// It exists because a type node is resolved **once per use**, not once. An
/// alias `T :: [A - 10]i32` is expanded wherever `T` is written, so a length
/// that does not fit a `usize` would be reported once per mention of `T` — one
/// mistake, as many diagnostics as the program has uses. The value is read
/// every time (callers need the answer); only the sentence is said once.
///
/// Set on the node in the *type*, which is what makes it the right key: two
/// separate `[A - 10]i32`s written out are two nodes and two mistakes, and one
/// alias used twice is one node and one.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct ConstSlotReported;

/// A type-position path that named something that is not a type, already
/// reported.
///
/// Same shape and same reason as [`ConstSlotReported`]: a type node is resolved
/// **once per use**, so the sentence below would otherwise be said once for the
/// signature, once for the body's check and once more per alias expansion. One
/// written name is one mistake.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct TyPathReported;

/// An `a[i]` that is the **place of an assignment** rather than a value.
///
/// For a user container the two are different traits and the resolution says
/// which. For the built-in sequences they are the same impl (`core` gives them
/// `Index` only — see `core/slice.nest`), so the distinction has to be recorded:
/// an array's element pointer is `*mut T` when it is about to be written and
/// `*T` when it is only read, and only the statement knows which.
///
/// Set by [`Inferer::infer_index_place`], which is called for exactly the nodes
/// that are places.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct IndexWrite;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DynCoerce {
    /// The trait the object is typed as.
    pub trait_def: DefId,
    /// The whole object type, `dyn Trait.<args>` — the arguments and pinned
    /// associated types are part of what the fat pointer is typed as.
    pub object: Ty,
    /// The pointee type being erased.
    pub concrete: Ty,
}

/// Records that an expression reaches its expected type through an `@using`
/// field's implicit upcast (§3.10), attached to the coerced node so
/// [`super::lower`] can make the "take `e.field`" explicit.
///
/// A value upcast copies the sub-object; a pointer upcast takes its address, so
/// [`through_ptr`](Upcast::through_ptr) picks which of the two lowering emits.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
/// A closure's signature and what it is generic over, stamped on its node by
/// inference (§5.5).
///
/// The closure's *type* is `Ty::Nominal` over its own def, which names the
/// closure and says nothing about how it is called; this is the rest. `generics`
/// are the type parameters of the function it is written in, in that function's
/// order — the closure's type is instantiated with the same arguments.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClosureSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub generics: Vec<DefId>,
    /// The type of each local the closure shares, in [`super::Captures`] order:
    /// a binding's type lives nowhere lowering can read it but here.
    pub shared: Vec<Ty>,
}

/// What an `impl` return type is (§5.4), stamped on the return slot once the
/// body is typed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpaqueTy(pub Ty);

/// Marks the callee of a call on a value that is not a function pointer but
/// implements `Func` (§5.5) — a closure, or a generic parameter bounded by
/// `Func`. Lowering reads it to emit [`crate::ir::Dispatch::Func`], and the
/// callee's own type is what that dispatch is on.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct FuncCall;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArgOrder {
    /// One entry per parameter, in declaration order. A `None` is a parameter
    /// the call left out and whose **default** fills the slot; lowering supplies
    /// it, since only there does the default exist as an `Expr` to clone.
    pub args: Vec<Option<NodeId>>,
}

/// The generic parameters one declaration is generic over, in the **fixed
/// order** every instantiation of it is written in.
///
/// The order is: the parameters the declaration itself lists (`func <const N,
/// T>`), then any its signature mentions that it did not list — an enclosing
/// `impl`'s `<T>`, which a method is just as generic over without declaring —
/// in first-seen order. That is exactly the order
/// [`Inferer::instantiate_parts`] binds them in, and the two are built by the
/// same code so they cannot drift apart.
///
/// An empty list is the interesting case: it is what makes a function
/// *concrete*, and therefore what monomorphization keys on to decide that a
/// function needs no instantiation of its own. Note that a parameter used only
/// in the **body** (`func <T> () -> usize { return $size_of.<T>() }`) is listed
/// too — it is declared, so it is in the first half — which is why this is
/// recorded rather than recomputed from the signature later.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Generics {
    /// Every parameter, declared ones first.
    pub params: Vec<DefId>,
    /// How many of [`params`](Generics::params) the declaration listed itself.
    /// The rest are the enclosing `impl`'s.
    ///
    /// The split matters exactly once, and it matters a lot there: when
    /// monomorphization turns a call on a bound into a call on the impl that
    /// satisfied it, the two halves come from different places. The method's own
    /// arguments come from the **call site** — the trait's declaration and the
    /// impl's must list the same ones, so they line up by position — while the
    /// impl's come from *matching* the impl's target against the concrete self
    /// type. Without the split there is no way to tell which is which.
    pub own: usize,
}

/// One generic argument: what a call site bound one [`Generics`] entry to.
///
/// The two halves of §5's single positional list — `<T>` takes a type,
/// `<const N: u16>` takes a value — stay apart here because they are different
/// things to substitute into and because a mangled symbol encodes them
/// differently (`design/lir.md` §7).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum GenericArg {
    Ty(Ty),
    Const(Const),
}

/// The type a `Type.MEMBER` read has, kept on the read's own node.
///
/// A member reached through a type rather than a value is worked out by
/// inference (see [`Inferer::assoc_through_type`]), which stamps the member's
/// resolution so lowering emits a global for it. That stamp makes the node look
/// like an ordinary resolved name to any later visit, and an ordinary resolved
/// name is typed from its *declaration* — which for a constant in a generic
/// impl is `uint.<N>`, with nothing at the declaration to say what `N` is. So
/// the answer is recorded where it was computed.
#[derive(Debug, Clone)]
pub struct AssocTy(pub Ty);

/// What one call site instantiated its callee's [`Generics`] with, in the same
/// order.
///
/// Stamped on the call's **callee** node — the path for a free call, the
/// `recv.m` field access for a method call — which is the same node
/// [`MethodRes`] and [`ArgOrder`] hang off, so lowering reads all three from one
/// place.
///
/// This is recorded rather than recovered because only this stage knows it.
/// Monomorphization could in principle unify the callee's declared signature
/// against the type the call settled on and read the arguments back out of the
/// result, but that answer is wrong wherever the signature does not mention a
/// parameter (`$size_of.<T>()`), and it re-derives — with a second
/// implementation, free to disagree — something inference computed exactly once.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Instantiation(pub Vec<GenericArg>);

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

/// One `impl`'s target, resolved out of syntax into types.
///
/// [`ImplTable`] stores the impl's self type and trait arguments as **AST
/// nodes**, because it is built before there is anything to resolve them
/// against; turning a node into a [`Ty`] is what `ty_from_node` does, and that
/// belongs to inference. Every use during inference therefore resolves them
/// afresh, inside the inference context whose variables the candidate's
/// generics are freshened into.
///
/// Monomorphization needs the same answer and has none of that machinery: it
/// runs after the ASTs have done their work, holds a concrete self type, and
/// wants to know which impl matches it. So the resolution is done once, here,
/// and the result kept.
///
/// The impl's own generics stay **rigid** — a `<T>` is a
/// [`Ty::Nominal`] naming the type parameter, a `<const N>` a [`Const::Param`]
/// — rather than being freshened into variables. That is the difference between
/// this and what selection does during inference: there the impl is a candidate
/// being unified against, here it is a *pattern* being matched, and a pattern
/// wants its holes named rather than numbered.
///
#[derive(Debug, Clone)]
pub struct ImplTarget {
    /// The `for` target's type (`impl Add for Vec3` → `Vec3`).
    pub self_ty: Ty,
    /// The trait's own generic arguments (`impl Add.<f64> for Vec3` → `[f64]`).
    ///
    /// They are what separates two impls that a self type alone cannot: `impl
    /// Add.<f64> for Vec3` and `impl Add.<i32> for Vec3` do not overlap (§4.9)
    /// and both apply to `Vec3`, so a call reached through a `<T: Add.<f64>>`
    /// bound has to compare these to know which one it meant.
    pub trait_args: Vec<Ty>,
}

/// Resolve every impl's target and the types it binds, in
/// [`ImplTable::impls`] order, and record them on the impls themselves.
///
/// It runs **before** inference, and that is the point. Selection trials a
/// candidate impl by unifying its self type with the obligation, and it does
/// that for every candidate of every obligation — so resolving the impl's
/// syntax there meant resolving the same three type expressions thousands of
/// times over. Resolved once, a trial is a substitution.
///
/// Reading syntax is also the half an impl in another package cannot do: its
/// tree was left behind with its library. What travels is this
/// (see [`ImplInfo::typed`]).
pub fn resolve_impl_targets(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    decls: &DeclTable,
    diags: &mut Vec<Diagnostic>,
    lang: &LangItems,
    impls: &mut ImplTable,
) -> Vec<ImplTarget> {
    // No selection happens here — only `ty_from_node`, which reads syntax and
    // the def table. The two trait sets and the context's `#lang` wiring exist
    // for the solver, so an empty pair and a plain context are the honest
    // inputs rather than an approximation of a file's scope.
    let empty: HashSet<DefId> = HashSet::new();
    let frozen = ImplTable {
        impls: impls.impls.clone(),
    };
    let mut out = Vec::with_capacity(impls.impls.len());
    for i in 0..frozen.impls.len() {
        let imp = frozen.impls[i].clone();
        // An impl that arrived from a library is already resolved — that is
        // what its metadata carries — so there is nothing to work out here.
        if let Some(t) = &imp.typed {
            out.push(ImplTarget {
                self_ty: t.self_ty.clone(),
                trait_args: t.trait_args.clone(),
            });
            continue;
        }
        let Some(ast) = asts.get(&imp.file) else {
            out.push(ImplTarget {
                self_ty: Ty::Error,
                trait_args: Vec::new(),
            });
            continue;
        };
        let mut cx = Inferer {
            defs,
            asts,
            decls,
            ast,
            diags,
            lang,
            impls: &frozen,
            in_scope_traits: &empty,
            lang_traits: &empty,
            in_default: false,
            ctx: None,
            pkg_of: None,
            file: imp.file,
            cx: InferCtxt::new(),
            env: HashMap::new(),
            types: HashMap::new(),
            ret: Ty::Void,
            breaks: Vec::new(),
            func: None,
            alias_stack: Vec::new(),
            const_stack: Vec::new(),
            int_values: HashMap::new(),
            float_values: HashMap::new(),
        };
        let Some(sx) = imp.syntax.clone() else {
            out.push(ImplTarget {
                self_ty: Ty::Error,
                trait_args: Vec::new(),
            });
            continue;
        };
        let self_ty = cx.ty_from_node_in(imp.file, sx.self_node);
        let trait_args: Vec<Ty> = sx
            .trait_args
            .iter()
            .map(|&n| cx.ty_from_node_in(imp.file, n))
            .collect();
        let assoc: HashMap<Symbol, Ty> = sx
            .assoc
            .iter()
            .map(|(name, &n)| (name.clone(), cx.ty_from_node_in(imp.file, n)))
            .collect();
        // A type that still holds a variable is one this context invented and
        // the next would number differently, so it is not written down: the
        // syntax answers for that impl, as it did before.
        let settled = !self_ty.mentions_var()
            && !trait_args.iter().any(Ty::mentions_var)
            && !assoc.values().any(Ty::mentions_var);
        if settled {
            impls.impls[i].typed = Some(TypedImpl {
                self_ty: self_ty.clone(),
                trait_args: trait_args.clone(),
                assoc,
            });
        }
        out.push(ImplTarget {
            self_ty,
            trait_args,
        });
    }
    out
}

/// Resolve what every generic **type parameter** declared in `file` is bounded
/// by, for the declaration table to record
/// (`super::decl::record_param_decls`).
///
/// The trait's identity is already on the parameter's own def
/// ([`Def::param_bounds`]), written by name resolution. What needs inference is
/// everything a bound carries beyond it: the arguments it was written with
/// (`<T: Add.<f64>>`), and what an `<Assoc = T>` binding pinned. Both are
/// *types*, and a type is not known until now.
///
/// It is recorded because it does not otherwise survive: a parameter that
/// arrives with a library has no tree in the compilation reading it, so every
/// question asked of one used to be answered "nothing was written" — which for
/// a bound with arguments is not a conservative answer but a wrong one. A
/// method call through `<T: Add.<f64>>` in a library's generic function picked
/// its impl with an empty argument list.
///
/// **Nothing is reported.** A bound that does not resolve was reported where it
/// was written, by the pass that checked the declaration.
pub fn resolve_param_decls(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    decls: &DeclTable,
    lang: &LangItems,
    impls: &ImplTable,
    file: FileId,
) -> Vec<(DefId, super::decl::ParamDecl)> {
    let Some(ast) = asts.get(&file) else {
        return Vec::new();
    };
    let empty: HashSet<DefId> = HashSet::new();
    let mut discarded = Vec::new();
    let mut cx = Inferer {
        defs,
        asts,
        decls,
        ast,
        diags: &mut discarded,
        lang,
        impls,
        in_scope_traits: &empty,
        lang_traits: &empty,
        in_default: false,
        ctx: None,
        pkg_of: None,
        file,
        cx: InferCtxt::new(),
        env: HashMap::new(),
        types: HashMap::new(),
        ret: Ty::Void,
        breaks: Vec::new(),
        func: None,
        alias_stack: Vec::new(),
        const_stack: Vec::new(),
        int_values: HashMap::new(),
        float_values: HashMap::new(),
    };
    let mut out = Vec::new();
    for d in defs.iter() {
        if d.kind != DefKind::TypeParam || d.file != Some(file) {
            continue;
        }
        // A **synthesized** projection parameter — the `Item` of `T.Item` — has
        // no declaration of its own to read; what bounds it is recorded on the
        // associated type's def instead (`Def::assoc_bounds`), and what pins it
        // is the binding on the *base* parameter's bound, resolved below.
        let pinned = d
            .projection
            .as_ref()
            .and_then(|p| p.pinned)
            .map(|n| cx.ty_from_node_in(file, n))
            .filter(|t| !t.mentions_error());
        let bounds: Vec<(DefId, Vec<Ty>)> = match d.node {
            Some(node) => match ast.node(node).kind.clone() {
                NodeKind::GenericTypeParam {
                    constraint: Some(c),
                    ..
                } => cx
                    .bound_nodes(file, c)
                    .into_iter()
                    .filter_map(|b| {
                        let t = cx.type_head_def_in(file, b)?;
                        (defs.get(t).kind == DefKind::Trait)
                            .then(|| (t, cx.bound_trait_args_in(file, b)))
                    })
                    .collect(),
                _ => Vec::new(),
            },
            None => Vec::new(),
        };
        let revealed = d.node.and_then(|n| {
            let OpaqueTy(t) = ast.meta::<OpaqueTy>(n)?;
            let args = ast
                .meta::<super::OpaqueArgs>(n)
                .map(|a| a.0)
                .unwrap_or_default();
            Some((args, t))
        });
        if bounds.is_empty() && pinned.is_none() && revealed.is_none() {
            continue;
        }
        out.push((
            d.id,
            super::decl::ParamDecl {
                bounds,
                pinned,
                revealed,
            },
        ));
    }
    out
}

/// Fold the **value** of every constant declared in `file`, for the declaration
/// table to record (`super::decl::record_const_values`).
///
/// An array length and a `const` generic argument are the two places a
/// constant's value, rather than its type, is what a use site needs: `[SIZE]T`
/// is a `[4]T`, and a package compiled against this one has no right-hand side
/// to read the `4` from. It is the same fold a use in this package does
/// ([`Inferer::const_operand`]), run once at the declaration instead.
///
/// **Nothing is reported.** A constant whose value does not fold — one that
/// calls a function, say — is simply not recorded, and a use of it meets the
/// same limit and the same diagnostic it always did, where it is written.
pub fn fold_const_values(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    decls: &DeclTable,
    lang: &LangItems,
    impls: &ImplTable,
    file: FileId,
) -> Vec<(DefId, ConstValue)> {
    let Some(ast) = asts.get(&file) else {
        return Vec::new();
    };
    let empty: HashSet<DefId> = HashSet::new();
    // Thrown away: this pass answers "does it fold", and the diagnostics for
    // "it does not" belong to the use site that asked for a value.
    let mut discarded = Vec::new();
    let mut cx = Inferer {
        defs,
        asts,
        decls,
        ast,
        diags: &mut discarded,
        lang,
        impls,
        in_scope_traits: &empty,
        lang_traits: &empty,
        in_default: false,
        ctx: None,
        pkg_of: None,
        file,
        cx: InferCtxt::new(),
        env: HashMap::new(),
        types: HashMap::new(),
        ret: Ty::Void,
        breaks: Vec::new(),
        func: None,
        alias_stack: Vec::new(),
        const_stack: Vec::new(),
        int_values: HashMap::new(),
        float_values: HashMap::new(),
    };
    let mut out = Vec::new();
    for d in defs.iter() {
        if d.kind != DefKind::Const || d.file != Some(file) {
            continue;
        }
        let Some(node) = d.node else { continue };
        // Both spellings of a constant (§2.5): `SIZE :: 4` binds the value, and
        // `SIZE: u16 :: 4` writes the type before the binder and parses as an
        // `AssocConst` holding the two apart.
        let rhs = match ast.node(node).kind.clone() {
            NodeKind::ConstBind { rhs, .. } => rhs,
            NodeKind::AssocConst {
                default: Some(rhs), ..
            } => rhs,
            _ => continue,
        };
        if let Some(v) = cx.const_operand(file, rhs, "a constant", 0) {
            out.push((d.id, v));
        }
    }
    out
}

/// The signature of `def`, worked out from the tree the way a call site used to
/// ask for it — for the test that the recorded one is the same
/// (`super::decl::tests`).
#[cfg(test)]
pub(crate) fn signature_from_tree(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    lang: &LangItems,
    impls: &ImplTable,
    def: DefId,
) -> Ty {
    let empty: HashSet<DefId> = HashSet::new();
    let table = super::decl::DeclTable::new();
    let mut diags = Vec::new();
    let Some(file) = defs.get(def).file else {
        return Ty::Error;
    };
    let Some(ast) = asts.get(&file) else {
        return Ty::Error;
    };
    let mut cx = Inferer {
        defs,
        asts,
        decls: &table,
        ast,
        diags: &mut diags,
        lang,
        impls,
        in_scope_traits: &empty,
        lang_traits: &empty,
        in_default: false,
        ctx: None,
        pkg_of: None,
        file,
        cx: InferCtxt::new(),
        env: HashMap::new(),
        types: HashMap::new(),
        ret: Ty::Void,
        breaks: Vec::new(),
        func: None,
        alias_stack: Vec::new(),
        const_stack: Vec::new(),
        int_values: HashMap::new(),
        float_values: HashMap::new(),
    };
    let ty = cx.func_def_ty(def);
    cx.cx.resolve(&ty)
}

/// Infer types for every function body in `file`, annotating each expression
/// node with its resolved [`Ty`]. `asts` is the whole parsed program (read-only)
/// so a field access can reach a struct declared in another file.
#[allow(clippy::too_many_arguments)]
pub fn infer_file(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    decls: &DeclTable,
    diags: &mut Vec<Diagnostic>,
    lang: &LangItems,
    impls: &ImplTable,
    prelude_globs: &[DefId],
    file_ns: DefId,
    file: FileId,
    pkg_of: &HashMap<FileId, String>,
) {
    let ast = &asts[&file];
    // The set of trait defs a use site in this file may select impls of: only
    // in-scope traits are candidates (§ trait selection, Rust-style).
    let mut in_scope_traits = in_scope_traits(defs, prelude_globs, file_ns);
    // A trait **named in a bound** is in scope for the parameter it bounds,
    // however it was written. Without this, `func <T: cmp.Eq>` type-checks the
    // bound and then cannot call `a.eq(b)`: the qualified path resolves to
    // `core.Eq` perfectly well, but the *file* only imported the `cmp`
    // namespace, so `Eq` was never a candidate for selection. Writing the bound
    // is as clear a statement that the trait is wanted here as importing its
    // name is.
    in_scope_traits.extend(bound_traits(defs, ast));
    // A `#lang`-tagged trait is always selectable, imported or not (§4.6). The
    // operator traits are the reason: `a + b` reaches `Add` **by its tag**, so
    // the compiler is the one that named it, and gating that on the program
    // having written `import <core/ops>` would make `+` require an import. The
    // name is still needed to *write* `impl Add.<Vec3> for Vec3`, and to call
    // `x.add(y)` by name — neither goes through impl selection by tag.
    let lang_traits: HashSet<DefId> = lang
        .iter()
        .map(|(_, d)| defs.resolve_alias(d))
        .filter(|&d| defs.get(d).kind == DefKind::Trait)
        .collect();
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
    let numeric_distincts = numeric_distincts(defs, asts, decls);
    // What a string literal defaults to, for the same reason: unification
    // decides whether a `comptime_str` variable may become a given type, and
    // `str` is found by `#lang` tag, which unification cannot do.
    let str_ty = str_lang_ty(defs, lang);
    // `usize` / `isize`, for the same reason and by the same route.
    let ptr_ints = ptr_int_lang_tys(defs, lang);
    // Each pass below gets its own inference context: a `const` generic solved
    // for one function says nothing about the next.
    macro_rules! fresh {
        () => {
            Inferer {
                defs,
                asts,
                decls,
                ast,
                diags,
                lang,
                impls,
                in_scope_traits: &in_scope_traits,
                lang_traits: &lang_traits,
                in_default: false,
                ctx: Some(file_ns),
                pkg_of: Some(pkg_of),
                file,
                cx: {
                    let mut cx = InferCtxt::new();
                    cx.set_numeric_distincts(numeric_distincts.clone());
                    cx.set_str_ty(str_ty.clone());
                    cx.set_ptr_int_tys(ptr_ints.clone());
                    cx
                },
                env: HashMap::new(),
                types: HashMap::new(),
                ret: Ty::Void,
                breaks: Vec::new(),
                func: None,
                alias_stack: Vec::new(),
                const_stack: Vec::new(),
                int_values: HashMap::new(),
                float_values: HashMap::new(),
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
    // Declaration-level, and the other half of what an `impl` promises: every
    // member it supplies must have the type the trait declared. Completeness —
    // *which* members are required — was checked in `impls::build`, which runs
    // before there are any types to compare.
    {
        let mut cx = fresh!();
        let mine: Vec<usize> = (0..impls.impls.len())
            .filter(|&i| impls.impls[i].file == file && impls.impls[i].trait_def.is_some())
            .collect();
        for i in mine {
            cx.check_impl_conformance(i);
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
    // Declaration-level too: what tag each enum variant's values store. It runs
    // after the member types because a discriminant is only legal on a variant
    // with **no** payload, and the payload is the thing that says so.
    {
        let mut cx = fresh!();
        cx.stamp_variant_tags();
    }
    // Declaration-level: what a **generic impl's associated constants** are
    // generic over. `impl <const N: u16> uint.<N> { MAX :: ... }` declares one
    // constant with a different value for every width, and nothing else records
    // that — [`Inferer::stamp_generics`] answers for functions alone.
    {
        let mut cx = fresh!();
        cx.stamp_assoc_const_generics();
    }
    // Declaration-level, and the last of them: expand every type alias the file
    // declares.
    //
    // An alias is expanded **on use** — that is what an alias is — so one that
    // nothing mentions is a right-hand side nothing ever looks at, and
    // `T :: [A - 10]i32` passes a build in silence. A declaration is a claim
    // about a type whether or not anything takes it up, so it is checked where
    // it is written. Uses stay quiet about what this already said, by the same
    // per-node rule everything in the `const_*` family follows (see
    // [`ConstSlotReported`]).
    {
        let mut cx = fresh!();
        cx.check_type_aliases();
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

/// The `#lang("usize")` and `#lang("isize")` types, or `None` when `core`
/// declares neither.
///
/// Both or nothing: they are declared together in `core/num.nest` and a `core`
/// with one and not the other is malformed in a way this pass has no useful
/// answer for.
fn ptr_int_lang_tys(defs: &DefTable, lang: &LangItems) -> Option<(Ty, Ty)> {
    let nominal = |tag: &str| {
        lang.get(tag).map(|def| Ty::Nominal {
            def: defs.resolve_alias(def),
            args: Vec::new(),
        })
    };
    Some((nominal("usize")?, nominal("isize")?))
}

/// The `#lang("location")` type, or `None` when the program declares no such
/// item.
///
/// `Location` is not a compiler primitive — it is an ordinary struct in core
/// (§5.2), found by tag like everything else the compiler wires syntax to, so a
/// `core` that names it differently still works.
fn location_lang_ty(defs: &DefTable, lang: &LangItems) -> Option<Ty> {
    lang.get("location").map(|def| Ty::Nominal {
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
/// The width argument of an `int.<N>` / `uint.<N>` written as a `distinct`'s
/// representation, when it is a compile-time number this early.
///
/// This pass runs **before** inference — it exists so unification can answer
/// "may this literal become that `distinct`?", which it is asked during
/// inference — so it cannot use the ordinary const machinery. What it can do is
/// the two cases that actually occur: a literal (`distinct uint.<8>`) and one
/// hop to a constant's literal (`distinct uint.<PTR_BITS>`, which is how `core`
/// defines `usize`). Anything else is left unknown, and the type simply is not
/// treated as a numeric `distinct`.
fn family_inner(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    ast: &Ast,
    node: NodeId,
) -> Option<u16> {
    let NodeKind::TypePath { generic_args, .. } = &ast.node(node).kind else {
        return None;
    };
    let &arg = generic_args.first()?;
    /// The literal `u16` a node denotes, following at most one constant.
    fn width_of(defs: &DefTable, asts: &HashMap<FileId, Ast>, ast: &Ast, n: NodeId) -> Option<u16> {
        if let NodeKind::Lit(Lit::Int(v)) = &ast.node(n).kind {
            return u16::try_from(v).ok();
        }
        let Some(Resolution::Def(d)) = ast.meta::<Resolution>(n) else {
            return None;
        };
        let d = defs.get(defs.resolve_alias(d));
        let (file, node) = (d.file?, d.node?);
        let cast = asts.get(&file)?;
        let NodeKind::ConstBind { rhs, .. } = &cast.node(node).kind else {
            return None;
        };
        // `PTR_BITS: u16 :: 64` writes the type before the binder, so the
        // right-hand side is an `AssocConst` wrapping the literal (§2.5).
        let rhs = match &cast.node(*rhs).kind {
            NodeKind::AssocConst {
                default: Some(v), ..
            } => *v,
            _ => *rhs,
        };
        match &cast.node(rhs).kind {
            NodeKind::Lit(Lit::Int(v)) => u16::try_from(v).ok(),
            _ => None,
        }
    }
    width_of(defs, asts, ast, arg)
}

/// which case its name gives the family — or to another type def to follow.
fn numeric_distincts(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    decls: &super::decl::DeclTable,
) -> HashMap<DefId, Ty> {
    /// The inner type node of `def`, if `def` is a `distinct` type this
    /// compilation has the tree of.
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

    /// The numeric primitive a **recorded** representation bottoms out at, the
    /// chain followed on types rather than on trees.
    ///
    /// A recorded representation is already a resolved [`Ty`], so a
    /// `distinct Metres :: Feet` over `distinct Feet :: f64` is two lookups and
    /// no syntax at all — which is the only form a definition in another
    /// package comes in.
    fn numeric_of(decls: &super::decl::Decls, mut ty: Ty) -> Option<Ty> {
        for _ in 0..16 {
            match ty {
                Ty::Int { .. } | Ty::Float(_) => return Some(ty),
                Ty::Nominal { def, .. } => ty = decls.distinct_repr(def)?,
                _ => return None,
            }
        }
        None
    }

    let q = super::decl::Decls::new(defs, asts, decls);
    let mut out = HashMap::new();
    for d in defs.iter() {
        let kind = match q.distinct_repr(d.id) {
            Some(t) => numeric_of(&q, t),
            None => {
                let Some((mut file, mut inner)) = distinct_inner(defs, asts, d.id) else {
                    continue;
                };
                // Walk the chain, with a bound: a `distinct` cycle is a separate
                // error and this pass must not hang on one.
                let mut kind = None;
                for _ in 0..16 {
                    let Some(ast) = asts.get(&file) else { break };
                    let Some(Resolution::Def(next)) = ast.meta::<Resolution>(inner) else {
                        break;
                    };
                    let next = defs.resolve_alias(next);
                    let nd = defs.get(next);
                    if nd.kind == DefKind::Primitive {
                        // A family constructor carries its width as an argument,
                        // so the name alone does not give the type —
                        // `uint.<PTR_BITS>` is what `usize` stands over, and
                        // reading it is what makes that declaration in `core`
                        // real rather than decorative.
                        kind = match nd.name.as_str() {
                            fam @ ("int" | "uint") => family_inner(defs, asts, ast, inner)
                                .map(|w| Ty::int(w, fam == "int")),
                            // The primitive itself, not just its family: a
                            // literal settling on this `distinct` type has to
                            // fit that width.
                            other => match super::ty::primitive_ty(other) {
                                Some(t @ (Ty::Int { .. } | Ty::Float(_))) => Some(t),
                                _ => None,
                            },
                        };
                        break;
                    }
                    // The chain may leave this compilation's trees behind: a
                    // local `distinct` over one a library declares.
                    if let Some(t) = q.distinct_repr(next) {
                        kind = numeric_of(&q, t);
                        break;
                    }
                    match distinct_inner(defs, asts, next) {
                        Some((f, i)) => (file, inner) = (f, i),
                        None => break,
                    }
                }
                kind
            }
        };
        if let Some(k) = kind {
            out.insert(d.id, k);
        }
    }
    out
}

/// [`Inferer::rigid_self`]'s walk.
fn rigid_self_in(ty: &Ty, trait_def: DefId, params: &[Ty]) -> Ty {
    let go = |t: &Ty| rigid_self_in(t, trait_def, params);
    match ty {
        Ty::Nominal { def, args }
            if *def == trait_def
                && args.len() == params.len()
                && args.iter().all(|a| matches!(a, Ty::Var(_))) =>
        {
            Ty::Nominal {
                def: *def,
                args: params.to_vec(),
            }
        }
        Ty::Nominal { def, args } => Ty::Nominal {
            def: *def,
            args: args.iter().map(go).collect(),
        },
        Ty::Ptr { mutable, inner } => Ty::Ptr {
            mutable: *mutable,
            inner: Box::new(go(inner)),
        },
        Ty::Slice { mutable, inner } => Ty::Slice {
            mutable: *mutable,
            inner: Box::new(go(inner)),
        },
        Ty::Array {
            len,
            mutable,
            inner,
        } => Ty::Array {
            len: len.clone(),
            mutable: *mutable,
            inner: Box::new(go(inner)),
        },
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(go).collect()),
        Ty::Dyn { def, assoc } => Ty::Dyn {
            def: *def,
            assoc: assoc.iter().map(|(n, t)| (n.clone(), (go)(t))).collect(),
        },
        Ty::Struct(fields) => Ty::Struct(fields.iter().map(|(n, t)| (n.clone(), go(t))).collect()),
        Ty::Func { params: ps, ret, c } => Ty::Func {
            c: *c,
            params: ps.iter().map(go).collect(),
            ret: Box::new(go(ret)),
        },
        other => other.clone(),
    }
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

/// Every trait named by a bound anywhere in `ast`.
///
/// A bound is the one place a trait is named without being *used* as a value or
/// a type, so an import of its name is easy to leave out — and the program that
/// left it out is exactly the one that meant the trait. The walk is over the
/// whole file rather than per generic list because selection is per file
/// ([`in_scope_traits`]), and a trait bounding one function is not a surprising
/// candidate inside another in the same file.
fn bound_traits(defs: &DefTable, ast: &Ast) -> HashSet<DefId> {
    let mut set = HashSet::new();
    for node in ast.ids() {
        // A `dyn` names its trait the same way a bound does: `*dyn reflect.Any`
        // is as clear a statement that `Any` is wanted as `T: reflect.Any`, and
        // the unsizing coercion selects its impl like any other use.
        let bounds = match ast.node(node).kind.clone() {
            NodeKind::Bounds { bounds } => bounds,
            NodeKind::DynType { inner } => vec![inner],
            _ => continue,
        };
        for b in bounds {
            // The head of `Trait.<Args>` is what names the trait; a bare path is
            // its own head.
            let head = match ast.node(b).kind {
                NodeKind::TypePath { path, .. } => path,
                NodeKind::GenericApply { base, .. } => base,
                _ => b,
            };
            let Some(Resolution::Def(d)) = ast.meta::<Resolution>(head) else {
                continue;
            };
            let d = defs.resolve_alias(d);
            if defs.get(d).kind == DefKind::Trait {
                set.insert(d);
            }
        }
    }
    set
}

struct Inferer<'a> {
    defs: &'a DefTable,
    asts: &'a HashMap<FileId, Ast>,
    /// What every definition declares — see [`super::decl`].
    decls: &'a DeclTable,
    ast: &'a Ast,
    diags: &'a mut Vec<Diagnostic>,
    /// The `#lang` registry, for mapping an operator to its trait.
    lang: &'a LangItems,
    /// The whole-program impl index the solver selects over.
    impls: &'a ImplTable,
    /// Traits selectable at this file's use sites (see [`in_scope_traits`]).
    in_scope_traits: &'a HashSet<DefId>,
    /// Traits carrying a `#lang` tag. Selectable everywhere, because the
    /// compiler — not the program — is what named them.
    lang_traits: &'a HashSet<DefId>,
    /// Whether the expression being inferred is a **default argument**. The one
    /// thing that cares is `#caller_location`, which is meaningless anywhere
    /// else: a default is filled in at the call site, and that is the whole of
    /// why it names the caller (§5.2).
    in_default: bool,
    file: FileId,
    /// Where the expressions being inferred are **written**, for privacy (§4.4):
    /// the function whose body this is, or the namespace of the file when it is
    /// not a body at all (a constant's initializer, a default argument).
    ///
    /// `None` for the pass that resolves impl targets, which types no
    /// expression and so asks nothing about a private member.
    ctx: Option<DefId>,
    /// Which package each file belongs to, for `@public(package)` (§4.4).
    ///
    /// `None` in the passes that check nothing about privacy, which are the
    /// same ones that carry no `ctx`.
    pkg_of: Option<&'a HashMap<FileId, String>>,
    cx: InferCtxt,
    /// Type of each in-scope value def (params, locals) by [`DefId`].
    env: HashMap<super::def::DefId, Ty>,
    /// Per-node type, filled while inferring and finalized in [`Inferer::finish`].
    types: HashMap<NodeId, Ty>,
    /// Return type of the function currently being inferred.
    ret: Ty,
    /// One frame per enclosing `loop` / `while`, innermost last.
    breaks: Vec<LoopFrame>,
    /// The `FuncExpr` whose body is being inferred — what a closure written in
    /// it is generic over (§5.5).
    func: Option<NodeId>,
    /// Type-alias / associated-type defs currently being expanded, to break
    /// cycles in [`Inferer::expand_alias`].
    alias_stack: Vec<DefId>,
    /// Constants being typed, so a self-referential one cannot recurse forever.
    const_stack: Vec<DefId>,
    /// The exact `comptime_int` behind a node — a literal, or a use of a
    /// constant that is one — so its settled runtime type can be range-checked.
    int_values: HashMap<NodeId, num_bigint::BigInt>,
    /// The same, for a `comptime_float`: the value behind a float literal or a
    /// use of a constant that is one, so the width it settles on can be checked.
    float_values: HashMap<NodeId, f64>,
}

impl Inferer<'_> {
    /// The declaration queries, over the tables this pass already holds.
    fn decls(&self) -> Decls<'_> {
        Decls::new(self.defs, self.asts, self.decls)
    }

    fn infer_func(&mut self, func: NodeId) {
        let NodeKind::FuncExpr {
            params, ret, body, ..
        } = self.ast.node(func).kind.clone()
        else {
            return;
        };
        // Whose body this is, for privacy (§4.4): a private field is readable
        // from the namespace that declares its struct and from anything nested
        // in it, and a method of that struct is exactly such a place.
        let outer = self.ctx;
        if let Some(def) = self.func_owner(func) {
            self.ctx = Some(def);
        }
        let outer_func = self.func.replace(func);
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
                            "parameter `{name}` has no default but follows `{prev}`, which does \
                             — every parameter after a defaulted one must be defaulted too"
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
                    let outer = std::mem::replace(&mut self.in_default, true);
                    let dty = self.infer_expr(d);
                    self.in_default = outer;
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
        let declared = ret.map(|t| self.ty_from_node(t)).unwrap_or(Ty::Void);
        // An `impl` return type is the body's to decide (§5.4): inside, it is
        // whatever the body returns, and only the signature says `impl`.
        let opaque =
            ret.filter(|&r| matches!(self.ast.node(r).kind, NodeKind::GenericTypeParam { .. }));
        self.ret = match opaque {
            Some(_) => self.cx.fresh(),
            None => declared.clone(),
        };
        let ret_ty = self.ret.clone();
        // Stash the function's return type on the `FuncExpr` node for lowering.
        self.types.insert(func, declared);
        if let Some(b) = body {
            let bty = self.infer_expr(b);
            // The body's tail value is the function's result.
            self.expect_return(b, &bty, &ret_ty);
        }
        if let Some(r) = opaque {
            self.reveal_opaque(r, &ret_ty);
        }
        self.stamp_generics(func);
        // A closure's body is inferred inside the body that wrote it, so the
        // place a name is written in is restored rather than dropped.
        self.ctx = outer;
        self.func = outer_func;
    }

    /// What an `impl` return type turned out to be (§5.4): hold it to the bounds
    /// the signature promised, and record it on the return slot for the
    /// declaration table to carry.
    fn reveal_opaque(&mut self, ret: NodeId, concrete: &Ty) {
        let Some(def) = self.def_of(ret) else { return };
        let mut map = Subst::default();
        map.tys.insert(def, concrete.clone());
        self.register_bounds(ret, &[def], &map);
        self.ast.set_meta(ret, OpaqueTy(concrete.clone()));
    }

    /// Record what this function is generic over, in the order every
    /// instantiation of it is written in (see [`Generics`]).
    ///
    /// The two halves are the declaration's own `<...>` list and whatever else
    /// its signature mentions — an enclosing `impl <T> Vec.<T>`'s `T`, which
    /// `push` is every bit as generic over without declaring it. They are
    /// collected here in exactly the order [`Inferer::instantiate_parts`] binds
    /// them, so a call site's [`Instantiation`] lines up with this list by
    /// position.
    fn stamp_generics(&mut self, func: NodeId) {
        if let Some(generics) = self.generics_of_func(func) {
            self.ast.set_meta(func, generics);
        }
    }

    /// What [`Inferer::stamp_generics`] records, computed without recording it
    /// — a closure asks mid-body, since it is generic over the same list.
    fn generics_of_func(&mut self, func: NodeId) -> Option<Generics> {
        let NodeKind::FuncExpr { generics, .. } = self.ast.node(func).kind.clone() else {
            return None;
        };
        let mut order: Vec<DefId> = generics
            .iter()
            .filter(|&&g| {
                matches!(
                    self.ast.node(g).kind,
                    NodeKind::GenericTypeParam { .. } | NodeKind::GenericConstParam { .. }
                )
            })
            .filter_map(|&g| self.def_of(g))
            .collect();
        let own = order.len();
        let sig = self.inferred_sig_ty(func);
        let (mut tys, mut consts) = (Vec::new(), Vec::new());
        self.collect_generic_params(&sig, &mut tys, &mut consts);
        for d in tys.into_iter().chain(consts) {
            if !order.contains(&d) {
                order.push(d);
            }
        }
        // A method is generic over *all* of its impl's parameters, not only
        // the ones its signature names: `Flatten`'s `next` never writes `U`,
        // which only `I: Iterator.<Item = U>` fixes, and its body is full of
        // it.
        if let Some(owner) = self.func_owner(func)
            && let Some(imp) = self
                .impls
                .impls
                .iter()
                .find(|i| i.members.values().any(|&m| m == owner))
        {
            for d in imp.generics.clone() {
                if !order.contains(&d) {
                    order.push(d);
                }
            }
        }
        self.close_over_projections(&mut order);
        Some(Generics { params: order, own })
    }

    /// Record what each of this file's **generic impls** makes its associated
    /// constants generic over.
    ///
    /// `impl <const N: u16> uint.<N> { MAX :: cast.<Self>((1 << N) - 1) }` is
    /// one declaration and 65535 values: a constant in a generic impl has no
    /// single one, and which it has is decided where it is read. So it is
    /// treated the way a generic function is — the parameters here, the
    /// arguments on the use ([`Instantiation`]), and the evaluator zipping the
    /// two — and this is the half nothing else would record, because
    /// [`Inferer::stamp_generics`] answers for `func` alone.
    ///
    /// `own` is the whole list: an impl's parameters are not split between the
    /// impl and anything else, there being no enclosing declaration to inherit
    /// from.
    fn stamp_assoc_const_generics(&mut self) {
        let mine: Vec<usize> = (0..self.impls.impls.len())
            .filter(|&i| self.impls.impls[i].file == self.file)
            .filter(|&i| !self.impls.impls[i].generics.is_empty())
            .collect();
        for i in mine {
            let imp = self.impls.impls[i].clone();
            for &m in imp.members.values() {
                let m = self.defs.resolve_alias(m);
                if self.defs.get(m).kind != DefKind::Const {
                    continue;
                }
                let Some(node) = self.defs.get(m).node else {
                    continue;
                };
                self.ast.set_meta(
                    node,
                    Generics {
                        params: imp.generics.clone(),
                        own: imp.generics.len(),
                    },
                );
            }
        }
    }

    /// Extend a generic-parameter list with every associated-type parameter
    /// reachable from it, transitively.
    ///
    /// `deep :: func <N: Nest> (n: *N) -> N.Inner.Item` is generic over three
    /// things, and its *signature* names only two: `N.Inner` appears nowhere
    /// but in the body, as the type of the value `n.peel()` returns. It still
    /// has to be in the list — monomorphization substitutes the body with
    /// exactly what this records, and a type it has no binding for is a call it
    /// cannot resolve.
    ///
    /// The order is by name at each step, because the source of these is a
    /// namespace (a hash map) rather than a written list, and an instantiation
    /// lines up with this list **by position**. An order that varied between
    /// two runs — or between the call site and the declaration — would pair
    /// arguments with the wrong parameters.
    fn close_over_projections(&self, order: &mut Vec<DefId>) {
        let mut i = 0;
        while i < order.len() {
            let mut kids: Vec<(Symbol, DefId)> = self
                .defs
                .get(order[i])
                .ns
                .members
                .iter()
                .filter(|&(_, &m)| self.defs.get(m).projection.is_some())
                .map(|(n, &m)| (n.clone(), m))
                .collect();
            kids.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, k) in kids {
                if !order.contains(&k) {
                    order.push(k);
                }
            }
            i += 1;
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
        // **In node order.** Finalizing one entry defaults the variables it
        // mentions, and a variable defaulted early is a variable a later entry
        // no longer gets to solve — so the order decides which node a leftover
        // is reported against, and in the worst case whether it is reported at
        // all. Draining a `HashMap` makes that order the hasher's, which
        // differs between runs of the same compiler on the same program: a
        // constant whose type came out settled on one run came out "type
        // annotations needed" on the next. Node order is arbitrary too, but it
        // is the *same* arbitrary order every time, and it is the order the
        // program was written in.
        let mut entries: Vec<(NodeId, Ty)> = self.types.drain().collect();
        entries.sort_unstable_by_key(|(node, _)| *node);
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
            self.check_float_range(node, &resolved);
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
                // A bound's trait arguments may name the enclosing function's
                // own generics, so they travel through the same sweep as
                // everything else captured mid-inference.
                let dispatch = match m.dispatch.clone() {
                    MethodDispatch::Generic { trait_def, args } => MethodDispatch::Generic {
                        trait_def,
                        args: args
                            .iter()
                            .map(|a| self.cx.finalize(a, &mut || {}))
                            .collect(),
                    },
                    other => other,
                };
                self.ast.set_meta(
                    node,
                    MethodRes {
                        self_ty,
                        dispatch,
                        ..m
                    },
                );
            }
            if let Some(st) = self.ast.meta::<StaticTraitSelf>(node) {
                let self_ty = self.cx.finalize(&st.self_ty, &mut || {});
                let trait_args = st
                    .trait_args
                    .iter()
                    .map(|a| self.cx.finalize(a, &mut || {}))
                    .collect();
                self.ast.set_meta(
                    node,
                    StaticTraitSelf {
                        self_ty,
                        trait_args,
                        ..st
                    },
                );
            }
            if let Some(d) = self.ast.meta::<DynCoerce>(node) {
                let concrete = self.cx.finalize(&d.concrete, &mut || {});
                let object = self.cx.finalize(&d.object, &mut || {});
                self.ast.set_meta(
                    node,
                    DynCoerce {
                        concrete,
                        object,
                        ..d
                    },
                );
            }
            if let Some(OpaqueTy(t)) = self.ast.meta::<OpaqueTy>(node) {
                let t = self.cx.finalize(&t, &mut || {});
                self.ast.set_meta(node, OpaqueTy(t));
            }
            if let Some(sig) = self.ast.meta::<ClosureSig>(node) {
                let params: Vec<Ty> = sig
                    .params
                    .iter()
                    .map(|t| self.cx.finalize(t, &mut || {}))
                    .collect();
                let ret = self.cx.finalize(&sig.ret, &mut || {});
                let shared: Vec<Ty> = sig
                    .shared
                    .iter()
                    .map(|t| self.cx.finalize(t, &mut || {}))
                    .collect();
                // A closure's type is generic over the function's **type**
                // parameters only — a nominal type's arguments are types — so a
                // `<const N>` can reach its body as a copied value (see
                // `Lowerer::lower_closure`) but not its signature.
                let sig_tys = || params.iter().chain(std::iter::once(&ret)).chain(&shared);
                // This sweep runs once per function finished in the file, so
                // the closure may have been judged already.
                let msg = "a closure's parameters, result and shared locals cannot name a \
                           `const` generic parameter yet; take the value as an argument, or \
                           write a function";
                let span = self.ast.node(node).span;
                let said = self.diags.iter().any(|d| {
                    d.message == msg
                        && d.labels
                            .iter()
                            .any(|l| l.span.file == self.file && l.span.span == span)
                });
                if !said && sig_tys().any(Ty::mentions_const_param) {
                    self.report(node, msg);
                }
                self.ast.set_meta(
                    node,
                    ClosureSig {
                        params,
                        ret,
                        shared,
                        ..sig
                    },
                );
            }
            if let Some(sc) = self.ast.meta::<SliceCoerce>(node) {
                let to = self.cx.finalize(&sc.to, &mut || {});
                let range = self.cx.finalize(&sc.range, &mut || {});
                self.ast.set_meta(node, SliceCoerce { to, range });
            }
            // A call site's generic arguments are variables when they are
            // recorded — `id(x)`'s `T` is solved by the argument, which is
            // checked *after* the instantiation — so they finalize here with
            // every other mid-inference fact. Monomorphization keys its
            // instances on these, and a `?3` in a key would make two instances
            // out of one.
            if let Some(Instantiation(args)) = self.ast.meta::<Instantiation>(node) {
                let args = args
                    .iter()
                    .map(|a| match a {
                        GenericArg::Ty(t) => GenericArg::Ty(self.cx.finalize(t, &mut || {})),
                        GenericArg::Const(k) => {
                            GenericArg::Const(self.cx.finalize_const(k, &mut || {}))
                        }
                    })
                    .collect();
                self.ast.set_meta(node, Instantiation(args));
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
            NodeKind::CallerLocation => self.infer_caller_location(node),
            NodeKind::Lit(lit) => {
                // Keep the literal's exact value so `finish` can check it fits
                // whatever runtime integer type it settles on.
                match &lit {
                    Lit::Int(n) => {
                        self.int_values.insert(node, n.clone());
                    }
                    Lit::Float(f) => {
                        self.float_values.insert(node, *f);
                    }
                    _ => {}
                }
                self.lit_ty(&lit)
            }
            NodeKind::Path { .. } => {
                let ty = self.path_ty(node);
                // A use of a `comptime_int` / `comptime_float` constant carries
                // that constant's value, so it is range-checked at *this* site.
                match self.const_lit_value(node) {
                    Some(Lit::Int(v)) => {
                        self.int_values.insert(node, v);
                    }
                    Some(Lit::Float(v)) => {
                        self.float_values.insert(node, v);
                    }
                    _ => {}
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
                // A member reached through a type, already worked out on an
                // earlier visit to this node. It has to come first: the answer
                // below stamped the member's resolution so lowering can emit a
                // global, and reading that back would re-derive the
                // *declaration's* type — `uint.<N>`, with nothing here to say
                // what `N` is — instead of this read's.
                if let Some(AssocTy(t)) = self.ast.meta::<AssocTy>(node) {
                    return t;
                }
                // A `namespace.member` access the resolver already linked to a def
                // (a function, type, or const) is typed from that def; a value
                // `place.field` is typed from the base's struct type.
                if let Some(def) = self.resolved_def(node) {
                    // A constant named through its namespace is range-checked
                    // at its use exactly as one named bare is (see `Path`).
                    match self.const_lit_value(node) {
                        Some(Lit::Int(v)) => {
                            self.int_values.insert(node, v);
                        }
                        Some(Lit::Float(v)) => {
                            self.float_values.insert(node, v);
                        }
                        _ => {}
                    }
                    return self.def_ty(node, def);
                }
                if self.failed_resolution(node) {
                    return Ty::Error;
                }
                // A member reached through a **type** rather than through a
                // value: `u8.MAX`, `Self.BITS` inside `impl <T: Float> ... for
                // T`. The resolver could not link these — a primitive and a
                // type parameter own no namespace, and a family impl's members
                // are parked anonymously because `uint` names no collected type
                // — so the type is worked out here, where types are known, and
                // the item found by the same impl search a method call uses.
                if let Some(t) = self.type_denoted_by(base) {
                    if let Some(ty) = self.assoc_through_type(node, &t, &name) {
                        return ty;
                    }
                    // A type names no value to take a field of, so there is
                    // nothing further to try: say what was asked for and which
                    // type was asked. Falling through would type the base as a
                    // value and report a missing *field*, which for `u8.NOPE`
                    // is a question about the compiler rather than the program.
                    let what = self.cx.resolve(&t).display(self.defs);
                    self.report(node, format!("`{what}` has no associated item `{name}`"));
                    self.ast.set_meta(node, Resolution::Error);
                    return Ty::Error;
                }
                let bty = self.infer_expr(base);
                let bty = self.pin_str(&bty);
                let bty = self.settle(&bty);
                if let Some(ft) = self.field_ty_at(Some(node), &bty, name.as_str()) {
                    return ft;
                }
                // Not *absent* — **not yet known**. A base that is still a
                // variable is a projection nothing has solved, and answering
                // now answers `Ty::Error`, which unifies with everything and
                // tells the rest of inference nothing at all.
                if is_var(&self.cx.shallow(&bty)) {
                    let out = self.cx.fresh();
                    self.cx.register(Obligation::Field {
                        recv: bty,
                        name,
                        out: out.clone(),
                        origin: node,
                    });
                    return out;
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
                if let Some(ft) = self.field_ty_at(Some(node), &bty, &index.to_string()) {
                    return ft;
                }
                // Not known yet — a closure's `{ p in p.0 }` before anything
                // says what `p` is — so wait for it, as a named field does.
                if is_var(&self.cx.shallow(&bty)) {
                    let out = self.cx.fresh();
                    self.cx.register(Obligation::Field {
                        recv: bty,
                        name: Symbol::new(&index.to_string()),
                        out: out.clone(),
                        origin: node,
                    });
                    return out;
                }
                self.no_such_field(node, &bty, &index.to_string())
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
                // **Every** `a[i]` goes through `Index` (§6.13): the result is
                // the impl's `Output`, and `a[i]` means `index(&a, i).*`. The
                // built-in sequences have impls in `core` like everything else
                // — theirs happen to be `#intrinsic` — so there is no case here
                // for them, and no way for the two routes to disagree about
                // what indexing means.
                let head = self.autoderef(&bty);
                self.infer_index_op(node, head, ity)
            }
            NodeKind::Slice { base, range } => {
                let bty = self.infer_expr(base);
                let rty = self.infer_expr(range);
                // Slice bounds are indices: pin the range's element type to
                // `usize` so an unbounded `a[..]` still has a solved type.
                if let Ty::Nominal { args, .. } = self.cx.shallow(&rty) {
                    if let Some(elem) = args.first() {
                        let elem = elem.clone();
                        let want = self.usize_ty();
                        self.expect(range, &elem, &want);
                    }
                }
                // A sub-slice of a slice is the **same elements**, so it
                // carries the same permission over them: `b[2..<6]` on a
                // `[]mut u8` is a `[]mut u8`. Answering `[]u8` here instead
                // made writing to part of a buffer unsayable — every reader
                // filling the tail of what it has already read, and every
                // container copying into the free half of its storage, needs
                // exactly this.
                //
                // An **array** is the one that does not inherit: `[N]T` has no
                // mutability in its type, because the permission over an
                // array's elements belongs to whatever holds the array
                // (`core/slice.nest`). A sub-slice of one is read-only, which
                // is the conservative half of that rule and the only one this
                // expression can decide on its own.
                match self.autoderef(&bty) {
                    Ty::Slice { inner, mutable } => Ty::Slice { mutable, inner },
                    Ty::Array { inner, .. } => Ty::Slice {
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
                // Reading through a `*opaque` is the one thing an opaque pointer
                // is for refusing: there is no value to produce, because the
                // type is the statement that we do not know what is there
                // (§3.1, §11). Reported here rather than left to layout — a
                // load of an unsized type otherwise surfaces as a backend
                // complaint about a type the program never wrote.
                if matches!(self.cx.shallow(&inner), Ty::Opaque) {
                    self.report(
                        node,
                        "cannot read through `*opaque`: an opaque type has no value to load",
                    );
                }
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
            NodeKind::Closure { .. } => self.infer_closure(node, None),
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
            CompositeBody::Named { fields, spread } => {
                for &f in fields {
                    if let NodeKind::FieldInit { value, .. } = self.ast.node(f).kind.clone() {
                        self.infer_expr(value);
                    }
                }
                // The spread is an element of the literal like any other: typed
                // up front here, held to the target type in `check_record_body`.
                if let Some(s) = spread {
                    self.infer_expr(*s);
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
                    // No annotation: there is no context left to type a
                    // `.{ ... }` on the right, so it settles here as the
                    // anonymous struct it is (§3.8). Doing it now rather than
                    // at the end of the body is what makes the spec's own
                    // example work — `const a := .{ x: 1, y: 2 }` followed by
                    // `const p: P := a` — because otherwise the *later* line
                    // would be the first thing to touch the variable and would
                    // solve `a` to `P` outright, leaving no anonymous value for
                    // the coercion to happen from.
                    None => {
                        if is_var(&self.cx.shallow(&vty)) {
                            self.solve_to_fixpoint();
                            self.default_anon_structs();
                        }
                        vty
                    }
                };
                self.bind_pattern(pattern, &bound);
            }
            // A `::` binding in statement position (a block-local const, or a
            // synthetic `__it` / `__try` the desugarer introduced): type its RHS
            // and bind the pattern, exactly like an un-annotated `let`.
            NodeKind::ConstBind { pattern, rhs } => {
                // A block-local binding may be **typed** — `#static n: u8 :: 0`
                // is the function-local form of §2.6, and a local `A: u8 :: 5`
                // pins a constant the same way a namespace one does. The type is
                // what the binding *is*, so it is read here rather than inferred
                // from the initializer.
                let vty = match self.ast.node(rhs).kind.clone() {
                    NodeKind::AssocConst { ty, default } => {
                        let want = self.ty_from_node(ty);
                        if let Some(d) = default {
                            let got = self.infer_expr(d);
                            self.expect(d, &got, &want);
                        }
                        want
                    }
                    _ => self.infer_expr(rhs),
                };
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
                if let Some(v) = self.float_values.remove(&operand) {
                    self.float_values.insert(operand, -v);
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

    /// Type an assignment's place, routing `a[i]` through `IndexMut` instead of
    /// `Index`. Every other place types as an ordinary expression.
    ///
    /// Which of the two traits a `a[i]` means is decided **here**, because this
    /// is the only place that knows the node is a place rather than a value.
    fn infer_index_place(&mut self, place: NodeId) -> Ty {
        let NodeKind::Index { base, index } = self.ast.node(place).kind.clone() else {
            return self.infer_expr(place);
        };
        let bty = self.infer_expr(base);
        let ity = self.infer_expr(index);
        // **Settle the base before asking what it is.** When the base is itself
        // an index — `g[1][2] = 6` — typing it registered an `Index.Output`
        // projection and handed back the variable it will solve to, so without
        // this the head is a variable, the array case below does not recognize
        // it, and the write is sent to `IndexMut` — which the built-in
        // sequences deliberately do not implement. The result was
        // "`[3]i32` does not implement `core.ops.IndexMut.<?7>`": the right
        // type, named by the wrong trait, once it was too late to matter.
        let bty = self.settle(&bty);
        let head = self.autoderef(&bty);
        // An errored base has already been reported; registering an obligation
        // about it would add a second diagnostic for one mistake.
        if matches!(head, Ty::Error) {
            self.types.insert(place, Ty::Error);
            return Ty::Error;
        }
        // The built-in sequences implement `Index` and **not** `IndexMut`, and
        // that is a fact about `core` rather than a case in the compiler: a
        // sequence's write permission is in its type, not in its receiver, so
        // `IndexMut`'s `*mut Self` asks the wrong question of it (§2.3, §3.2 —
        // and `core/slice.nest` says the same at length). A write therefore goes
        // through the same impl a read does, and what the element pointer
        // permits is decided when the call is lowered.
        if matches!(head, Ty::Slice { .. } | Ty::Array { .. }) {
            // The impl is the same one a read uses, so what tells lowering this
            // is a write is this mark and nothing else (see [`IndexWrite`]).
            self.ast.set_meta(place, IndexWrite);
            let out = self.infer_index_op(place, head, ity);
            self.types.insert(place, out.clone());
            return out;
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

    /// Type `a[i]` through the `#lang("index")` trait: `Index.<Idx>`'s `Output` is the element type, and
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
        let shallow = self.cx.shallow(lty);
        // Not *unknown* — **not yet known**. An operand that is still a
        // variable is a projection nothing has solved: `ms[i].name` is
        // `Index.Output` until the impl is selected, and answering now means
        // answering "not nominal", which compares a `str` as a machine word.
        if is_var(&shallow) {
            self.cx.register(Obligation::Comparison {
                self_ty: lty.clone(),
                op,
                origin: node,
            });
            return;
        }
        if !matches!(shallow, Ty::Nominal { .. }) {
            return;
        }
        // `usize` / `isize` are nominal — they are `distinct` declarations in
        // `core` (§3.1) — but they are the numeric core all the same, and
        // comparing two of them is the machine's own instruction. Requiring an
        // `Eq` impl for them would make `i < len` need one, which is exactly the
        // ceremony the primitive path exists to avoid.
        if self.is_ptr_sized(&shallow) {
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
    /// Resolve `ty`, running the solver first when it is still a variable.
    ///
    /// A receiver is the one place inference cannot afford to be lazy: what
    /// `.name` and `.len()` *mean* depends on what the receiver is, and the
    /// walk reaches them while a projection like `Index.Output` is still
    /// queued. The obligation was registered with everything needed to solve
    /// it, so asking the solver to run is not a guess — it is the same work,
    /// done when the answer is wanted rather than at the end of the body.
    fn settle(&mut self, ty: &Ty) -> Ty {
        let shallow = self.cx.shallow(ty);
        if !is_var(&shallow) {
            return shallow;
        }
        self.solve_to_fixpoint();
        self.cx.shallow(ty)
    }

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
            if !progressed && !self.default_anon_structs() {
                break;
            }
        }
    }

    /// Give a `.{ name: value, ... }` that nothing typed the **anonymous
    /// struct** type it has on its own (§3.8), and report whether that unstuck
    /// anything.
    ///
    /// `.{ ... }` normally takes its type from context, so its result starts as
    /// a variable waiting on an annotation, a parameter or a return type. When
    /// the sweep stalls there is no context coming, and the spec's answer is
    /// not "ambiguous": a record literal *is* a value of the anonymous struct
    /// whose fields are the ones written, and `let a := .{ x: 2 }` is the
    /// example §3.8 gives. The field types may still be unsolved literal
    /// variables at this point — that is fine, they default like any other.
    ///
    /// Only a **named** body defaults. A positional or repeat body builds an
    /// array or a tuple, and which one it is is exactly what the context was
    /// going to say, so there is nothing to fall back to and it stays the
    /// "type annotations needed" it already was. A body with a `..rest` spread
    /// does not default either: a spread fills in the fields the literal did
    /// not write, which is a question only a declaration can answer.
    fn default_anon_structs(&mut self) -> bool {
        use crate::parser::ast::CompositeBody;
        let pending = self.cx.take_obligations();
        let mut changed = false;
        for ob in &pending {
            let Obligation::CompositeBody { recv, origin } = ob else {
                continue;
            };
            if !is_var(&self.cx.shallow(recv)) {
                continue;
            }
            let NodeKind::CompositeLit { body, ty: None } = self.ast.node(*origin).kind.clone()
            else {
                continue;
            };
            let CompositeBody::Named {
                fields,
                spread: None,
            } = body
            else {
                continue;
            };
            let mut named: Vec<(Symbol, Ty)> = Vec::with_capacity(fields.len());
            for f in fields {
                let NodeKind::FieldInit { name, value } = self.ast.node(f).kind.clone() else {
                    continue;
                };
                let t = self.node_ty(value);
                named.push((name, t));
            }
            let anon = Ty::anon_struct(named);
            if self.cx.unify(recv, &anon).is_ok() {
                changed = true;
            }
        }
        for ob in pending {
            self.cx.register(ob);
        }
        changed
    }

    /// Attempt to discharge one obligation: select its impl, and for a
    /// projection also compute and unify the associated type. Returns whether it
    /// was solved, is still blocked on an unsolved variable, or failed (a
    /// diagnostic was reported).
    fn try_solve(&mut self, ob: &Obligation) -> Outcome {
        if let Some(outcome) = self.try_solve_func(ob) {
            return outcome;
        }
        match ob {
            Obligation::Trait {
                self_ty,
                trait_def,
                args,
                origin,
                stamp,
            } => match self.select(self_ty, *trait_def, args) {
                Select::Ok(Choice::User(i)) => {
                    self.commit_impl(i, self_ty, args, *origin);
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
                // The bound itself is the answer; monomorphization picks the
                // impl. There is no member to stamp, because there is no impl
                // yet to take one from — a call through a bound is dispatched
                // the way every other call through a bound is. A **static**
                // trait call says so, since nothing else about it does: its
                // callee names the trait's declaration and it has no receiver.
                Select::ByBound => {
                    if stamp.is_some() {
                        self.ast.set_meta(
                            *origin,
                            StaticTraitSelf {
                                trait_def: *trait_def,
                                self_ty: self_ty.clone(),
                                trait_args: args.clone(),
                            },
                        );
                    }
                    Outcome::Solved
                }
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
                            let map = self.commit_impl(i, self_ty, args, *origin);
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
                // A projection through a bound (`T.Item`) is answered from the
                // parameter's declaration long before it reaches selection, so
                // one arriving here is the ordinary missing impl it looks like.
                // A type parameter's bound is the proof, and what it projects is
                // the associated-type parameter the bound minted for it: `<I as
                // Iterator>.Item` for `<I: Iterator>` is `I.Item`.
                Select::ByBound => {
                    let head = self.cx.shallow(self_ty);
                    // A trait object says what its associated types are in
                    // its own type: `dyn Iterator.<Item = i32>`'s is `i32`.
                    if let Ty::Dyn { assoc: pins, .. } = &head
                        && let Some((_, t)) = pins.iter().find(|(n, _)| n == assoc)
                    {
                        let t = t.clone();
                        self.expect(*origin, &t, out);
                        return Outcome::Solved;
                    }
                    let minted = match &head {
                        Ty::Nominal { def, .. } => {
                            self.defs.get(*def).ns.members.get(assoc).copied()
                        }
                        _ => None,
                    };
                    match minted {
                        Some(synth) => {
                            let t = self.param_ty(synth);
                            self.expect(*origin, &t, out);
                            Outcome::Solved
                        }
                        None => {
                            self.report_no_impl(*origin, self_ty, *trait_def, args);
                            let _ = self.cx.unify(out, &Ty::Error);
                            Outcome::Failed
                        }
                    }
                }
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
                    // Already reported where it went wrong.
                    Ty::Error => Outcome::Solved,
                    // A variant literal names a variant of an **enum**, and the
                    // context it was written in wants something that is not one.
                    // `.left` in an `i32` slot has no enum to belong to, so the
                    // program is wrong here rather than at lowering, where the
                    // missing enum showed up as an `undef`.
                    base if !matches!(base, Ty::Nominal { .. }) => {
                        let msg = format!(
                            "a variant literal needs an enum type, but this position wants `{}`",
                            base.display(self.defs)
                        );
                        self.report(*origin, msg);
                        Outcome::Solved
                    }
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
            Obligation::Field {
                recv,
                name,
                out,
                origin,
            } => {
                let target = self.cx.shallow(recv);
                if is_var(&target) {
                    return Outcome::Deferred;
                }
                let (name, out, origin) = (name.clone(), out.clone(), *origin);
                // A deferred `p.0` whose base turned out to be a tuple.
                if let (Ty::Tuple(elems), Ok(i)) =
                    (self.autoderef(&target), name.as_str().parse::<usize>())
                    && let Some(t) = elems.get(i)
                {
                    let t = t.clone();
                    self.expect(origin, &t, &out);
                    return Outcome::Solved;
                }
                match self.field_ty_at(Some(origin), &target, name.as_str()) {
                    Some(t) => self.expect(origin, &t, &out),
                    None => {
                        self.no_such_field(origin, &target, name.as_str());
                    }
                }
                Outcome::Solved
            }
            Obligation::Comparison {
                self_ty,
                op,
                origin,
            } => {
                if is_var(&self.cx.shallow(self_ty)) {
                    return Outcome::Deferred;
                }
                let (self_ty, op, origin) = (self_ty.clone(), *op, *origin);
                self.check_cmp_bound(origin, op, &self_ty);
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
            CompositeBody::Named { fields, spread } => {
                self.check_record_body(node, target, &fields, spread)
            }
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
                let want = self.usize_ty();
                self.expect(count, &cty, &want);
            }
        }
    }

    /// `{ name: value, ... }` against a struct: every name must be one of the
    /// struct's fields, and every field must be given exactly once.
    fn check_record_body(
        &mut self,
        node: NodeId,
        target: &Ty,
        fields: &[NodeId],
        spread: Option<NodeId>,
    ) {
        // A record literal builds a **struct**. An enum is nominal too, and
        // reaches here with no fields to miss, so the kind is what has to be
        // checked rather than the shape — `E { ..e }` and `E { }` were both
        // silently accepted while this only looked for `Ty::Nominal`.
        // An **anonymous** struct is a struct too (§3.8), and it has no def:
        // its declared fields are in the type itself. `owner` is what a
        // diagnostic calls the thing being built, and `declared` is the field
        // list a missing-field check needs; the two shapes differ in nothing
        // else, so they are collected here and the body below is shared.
        let (def, owner, declared) = match self.autoderef(target) {
            Ty::Nominal { def, .. } if self.defs.get(def).kind == DefKind::Struct => (
                Some(def),
                self.defs.canonical_string(def),
                self.record_field_names(def),
            ),
            Ty::Struct(ref fields) => (
                None,
                self.cx.resolve(target).display(self.defs),
                fields.iter().map(|(n, _)| n.clone()).collect(),
            ),
            _ => {
                let msg = format!(
                    "`{}` is not a struct, so it cannot be built from named fields",
                    self.cx.resolve(target).display(self.defs)
                );
                self.report(node, msg);
                return;
            }
        };
        let _ = def;
        let mut seen: Vec<Symbol> = Vec::new();
        for &f in fields {
            let NodeKind::FieldInit { name, value } = self.ast.node(f).kind.clone() else {
                continue;
            };
            match self.field_ty_at(Some(f), target, name.as_str()) {
                Some(ft) => {
                    let vty = self.node_ty(value);
                    self.expect(value, &vty, &ft);
                }
                None => {
                    let msg = format!("`{owner}` has no field `{name}`");
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
        // `..rest` supplies every field the literal did not write, and it must
        // be a value of the type being built: a different struct that happened
        // to have the remaining field names would otherwise build this one.
        if let Some(s) = spread {
            let sty = self.node_ty(s);
            let want = self.autoderef(target);
            self.expect(s, &sty, &want);
            return;
        }
        // Every declared field must be initialized.
        let missing: Vec<String> = declared
            .into_iter()
            .filter(|n| !seen.contains(n))
            .map(|n| format!("`{n}`"))
            .collect();
        if !missing.is_empty() {
            let msg = format!(
                "missing field{} {} in `{owner}`",
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", "),
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
                let shallow = self.cx.shallow_const(&len);
                let n = match shallow.value() {
                    Some(l) => l as usize,
                    None if matches!(shallow, Const::Error) => elems.len(),
                    None => {
                        let other = shallow;
                        let count = Const::len(elems.len() as u64, self.usize_ty());
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
        self.decls().record_field_names(def)
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
    /// candidate is a "does not implement" error.
    ///
    /// **Every trait is a candidate here, in scope or not.** By the time a trait
    /// reaches selection it has already been named — by a `*dyn Trait` the value
    /// is coerced to, by the bound of the function being called, by the operator
    /// being lowered — and whether the file doing it imported the trait's name
    /// has nothing to say about whether a type implements it. Scope decides one
    /// question only: which trait a method *name* means, and that is asked where
    /// a name is looked up ([`Inferer::in_scope_traits`]).
    fn select(&mut self, self_ty: &Ty, trait_def: DefId, args: &[Ty]) -> Select {
        let s = self.cx.shallow(self_ty);
        if matches!(s, Ty::Error) {
            return Select::Error;
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
        // `Func(A) -> R` is only spelling for `Func.<Args = …, Output = …>`, but
        // no impl stands behind it: the compiler implements `Func` for every
        // closure and `*func` (§5.5), so the impl table has nothing to say. Being
        // callable is the proof, as a bound is for a type parameter — and the
        // signature is held to `Args`/`Output` by the projections, which the
        // `Func` solver answers from it.
        if self.is_func_trait(trait_def) && self.func_value_sig(&s).is_some() {
            return Select::ByBound;
        }
        // A trait object implements its own trait: that is what it is. The
        // vtable is the impl, chosen where the object was made.
        if let Ty::Dyn { def, .. } = &s
            && self.defs.resolve_alias(*def) == trait_def
        {
            return Select::ByBound;
        }
        // `Sized` is every type but a trait object (§3.4), and the compiler is
        // what knows which one it has. A type parameter is sized without saying
        // so: nothing holds a `dyn T` but through a pointer.
        if self.is_sized_trait(trait_def) {
            return match s {
                Ty::Var(_) => Select::Defer,
                Ty::Dyn { .. } => Select::NoImpl,
                _ => Select::ByBound,
            };
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

        // Still nothing, and the self type is a **type parameter**: its own
        // bound is the proof. There is no impl to find for `T` — it is not a
        // type yet — and `<T: Float>` is precisely the promise that whatever
        // instantiates it has one, which is what lets a generic function hand
        // its parameter to another generic function with the same bound.
        //
        // Monomorphization substitutes the real type here and selects the impl
        // then, so nothing is left unanswered: the question is postponed to the
        // one place that can answer it. The `*dyn` coercion proves a bound the
        // same way ([`Inferer::param_has_bound`]).
        if best.is_none() && self.param_has_bound(&s, trait_def) {
            return Select::ByBound;
        }
        // Inside a trait's own default body `Self` is the trait's nominal, and
        // it implements that trait by being it — `collect` handing `self` to
        // something that wants an `Iterator` is the body's whole promise.
        // Monomorphization substitutes the implementing type, as for a bound.
        if best.is_none() && matches!(&s, Ty::Nominal { def, .. } if *def == trait_def) {
            return Select::ByBound;
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
        let trait_args = self.impl_trait_args(&imp);
        if ok && !trait_args.is_empty() && trait_args.len() == args.len() {
            for (t, a) in trait_args.iter().zip(args) {
                let t = self.subst_type_params(t, &map);
                if self.cx.unify(&t, a).is_err() {
                    ok = false;
                    break;
                }
            }
        }
        // **The impl's own bounds are part of whether it applies.** `impl <T:
        // Float> Display for T` unifies its self type with anything, so without
        // this every type in the program would implement `Display` and the
        // mistake would surface as `internal: no impl of Float for Foo at
        // monomorphization` — a message about the compiler, from inside `core`,
        // for a program that simply never wrote an impl.
        //
        // Each parameter is checked against what the trial bound it to, and a
        // parameter still unsolved is *not* a failure: the self type it came
        // from is not known yet either, and `select` defers on that.
        if ok {
            for &p in &imp.generics {
                let Some(bound_on) = map.tys.get(&p).cloned() else {
                    continue;
                };
                if is_var(&self.cx.shallow(&bound_on)) {
                    continue;
                }
                for t in self.param_bound_traits(p) {
                    if !matches!(
                        self.select(&bound_on, t, &[]),
                        Select::Ok(_) | Select::ByBound | Select::Defer | Select::Error
                    ) {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
            }
        }
        self.cx.rollback(snap);
        ok
    }

    /// Whether `t` is the `#lang("sized")` trait.
    fn is_sized_trait(&self, t: DefId) -> bool {
        Some(t) == self.lang.get("sized").map(|d| self.defs.resolve_alias(d))
    }

    /// Whether `t` is the `#lang("func")` trait.
    fn is_func_trait(&self, t: DefId) -> bool {
        Some(t) == self.lang.get("func").map(|d| self.defs.resolve_alias(d))
    }

    /// Commit the chosen impl for real (no rollback), binding its generics; the
    /// returned map (impl generic → solved type) drives associated-type
    /// projection.
    ///
    /// The impl's own bounds are registered against what it was bound to. They
    /// decide more than whether it applies: a parameter the self type does not
    /// mention is solved **only** by them — `impl <I: Iterator, B, F: Func(I.Item)
    /// -> B> Iterator for Map.<I, F>` learns its `B` from the closure's `Output`,
    /// and its `I.Item` from `I`'s impl.
    fn commit_impl(&mut self, i: usize, self_ty: &Ty, args: &[Ty], origin: NodeId) -> Subst {
        let imp = self.impls.impls[i].clone();
        let mut map = self.fresh_impl_map(&imp.generics);
        let impl_self = self.impl_self_ty(&imp, &map);
        let _ = self.cx.unify(self_ty, &impl_self);
        let trait_args = self.impl_trait_args(&imp);
        if !trait_args.is_empty() && trait_args.len() == args.len() {
            for (t, a) in trait_args.iter().zip(args) {
                let t = self.subst_type_params(t, &map);
                let _ = self.cx.unify(&t, a);
            }
        }
        self.register_impl_bounds(origin, &imp.generics, &mut map);
        map
    }

    /// Register an impl's bounds against `map`, first giving each of its
    /// associated-type parameters (`I.Item`) a variable and the projection that
    /// solves it — the same two steps a generic call takes in
    /// [`Inferer::instantiate_parts`].
    fn register_impl_bounds(&mut self, origin: NodeId, generics: &[DefId], map: &mut Subst) {
        let mut order = generics.to_vec();
        self.close_over_projections(&mut order);
        for &d in &order[generics.len()..] {
            let out = self.cx.fresh();
            map.tys.insert(d, out.clone());
            let Some(p) = self.defs.get(d).projection.clone() else {
                continue;
            };
            let Some(base) = map.tys.get(&p.base).cloned() else {
                continue;
            };
            self.cx.register(Obligation::Projection {
                self_ty: base,
                trait_def: p.trait_def,
                args: Vec::new(),
                assoc: p.assoc,
                out,
                origin,
                method: None,
            });
        }
        self.register_bounds(origin, generics, map);
    }

    /// Build the impl's self [`Ty`] with its generics substituted by `map`.
    fn impl_self_ty(&mut self, imp: &ImplInfo, map: &Subst) -> Ty {
        let raw = match (&imp.typed, &imp.syntax) {
            (Some(t), _) => t.self_ty.clone(),
            (None, Some(sx)) => {
                let node = sx.self_node;
                self.ty_from_node_in(imp.file, node)
            }
            (None, None) => Ty::Error,
        };
        self.subst_type_params(&raw, map)
    }

    /// The impl's trait arguments, generics still rigid.
    fn impl_trait_args(&mut self, imp: &ImplInfo) -> Vec<Ty> {
        match (&imp.typed, &imp.syntax) {
            (Some(t), _) => t.trait_args.clone(),
            (None, Some(sx)) => sx
                .trait_args
                .clone()
                .iter()
                .map(|&n| self.ty_from_node_in(imp.file, n))
                .collect(),
            (None, None) => Vec::new(),
        }
    }

    /// What the impl binds the associated type `name` to, generics still rigid.
    fn impl_assoc(&mut self, imp: &ImplInfo, name: &Symbol) -> Option<Ty> {
        match (&imp.typed, &imp.syntax) {
            (Some(t), _) => t.assoc.get(name).cloned(),
            (None, Some(sx)) => {
                let node = *sx.assoc.get(name)?;
                Some(self.ty_from_node_in(imp.file, node))
            }
            (None, None) => None,
        }
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
        match self.impl_assoc(&imp, assoc) {
            Some(t) => self.subst_type_params(&t, map),
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

    /// What a call site owes its callee's bounds: for each generic parameter,
    /// that what it was instantiated with implements every trait bounding it —
    /// with the arguments the bound wrote, and the associated types it pinned.
    ///
    /// Without this a bound was only a promise the callee's body relied on: a
    /// call passing a type with no impl got through inference and was found at
    /// monomorphization, as a defect in the compiler rather than a mistake in
    /// the program. It is also what gives a closure argument its parameter
    /// types, which a `Func(i32)` bound on the parameter it fills states.
    fn register_bounds(&mut self, at: NodeId, params: &[DefId], map: &Subst) {
        for &p in params {
            if self.defs.get(p).kind != DefKind::TypeParam {
                continue;
            }
            let Some(self_ty) = map.tys.get(&p).cloned() else {
                continue;
            };
            for t in self.param_bound_traits(p) {
                let args: Vec<Ty> = self
                    .bound_args(p, t)
                    .iter()
                    .map(|a| self.subst_type_params(a, map))
                    .collect();
                self.cx.register(Obligation::Trait {
                    self_ty: self_ty.clone(),
                    trait_def: t,
                    args: args.clone(),
                    origin: at,
                    stamp: None,
                });
                let pinned: Vec<(Symbol, DefId)> = self
                    .defs
                    .get(p)
                    .ns
                    .members
                    .iter()
                    .filter(|(_, m)| {
                        let m = **m;
                        self.defs.get(m).projection.as_ref().is_some_and(|pr| {
                            pr.trait_def == t
                                && (pr.pinned.is_some() || self.decls().param_pinned(m).is_some())
                        })
                    })
                    .map(|(n, &m)| (n.clone(), m))
                    .collect();
                for (assoc, synth) in pinned {
                    let out = self.param_ty(synth);
                    let out = self.subst_type_params(&out, map);
                    self.cx.register(Obligation::Projection {
                        self_ty: self_ty.clone(),
                        trait_def: t,
                        args: args.clone(),
                        assoc,
                        out,
                        origin: at,
                        method: None,
                    });
                }
            }
        }
    }

    /// Type one call argument. A closure — written bare or as a named
    /// argument's value — is typed against the parameter it fills, which is
    /// where its own parameters' types come from (§5.5); anything else is an
    /// ordinary expression.
    fn infer_arg(&mut self, n: NodeId, wanted: Option<Ty>) -> Ty {
        let closure = match self.ast.node(n).kind {
            NodeKind::Closure { .. } => Some(n),
            NodeKind::Arg { value, .. }
                if matches!(self.ast.node(value).kind, NodeKind::Closure { .. }) =>
            {
                Some(value)
            }
            _ => None,
        };
        let Some(c) = closure else {
            return self.infer_expr(n);
        };
        let t = self.infer_closure(c, wanted);
        self.types.insert(c, t.clone());
        self.types.insert(n, t.clone());
        t
    }

    /// Type a closure where it is written (§5.5), and answer its type.
    ///
    /// It is typed **inside** the body that writes it — it shares that body's
    /// locals, and the variables inference has not solved yet — so this is an
    /// expression like any other rather than a function of its own. A parameter
    /// the closure did not give a type takes the one `expected` says, when it
    /// says: the parameter a closure is passed to is where its types come from.
    fn infer_closure(&mut self, node: NodeId, expected: Option<Ty>) -> Ty {
        let NodeKind::Closure {
            captures,
            params,
            ret,
            body,
        } = self.ast.node(node).kind.clone()
        else {
            return Ty::Error;
        };
        let Some(defs) = self.ast.meta::<super::ClosureDefs>(node) else {
            return Ty::Error;
        };
        // A copy is typed as what it copies.
        for c in &captures {
            let t = match self.resolved_def(*c) {
                Some(outer) => self.env.get(&outer).cloned().unwrap_or(Ty::Error),
                None => Ty::Error,
            };
            if let Some(inner) = self.def_of(*c) {
                self.env.insert(inner, t.clone());
            }
            self.types.insert(*c, t);
        }
        let hint = expected.and_then(|e| self.closure_hint(&e, params.len()));
        let mut ptys = Vec::with_capacity(params.len());
        for (i, p) in params.iter().enumerate() {
            let NodeKind::Param { ty, .. } = self.ast.node(*p).kind.clone() else {
                continue;
            };
            let hinted = hint.as_ref().and_then(|(ps, _)| ps.get(i).cloned());
            let pty = match (ty, hinted) {
                (Some(t), Some(h)) => {
                    let t = self.ty_from_node(t);
                    self.expect(*p, &h, &t);
                    t
                }
                (Some(t), None) => self.ty_from_node(t),
                (None, Some(h)) => h,
                (None, None) => self.cx.fresh(),
            };
            if let Some(def) = self.def_of(*p) {
                self.env.insert(def, pty.clone());
            }
            self.types.insert(*p, pty.clone());
            ptys.push(pty);
        }
        let rty = match (ret, hint.and_then(|(_, r)| r)) {
            (Some(t), _) => self.ty_from_node(t),
            (None, Some(r)) => r,
            (None, None) => self.cx.fresh(),
        };
        // Its own `return`s, and no `break` out of it into the loop around it.
        let outer_ret = std::mem::replace(&mut self.ret, rty.clone());
        let outer_breaks = std::mem::take(&mut self.breaks);
        let bty = self.infer_expr(body);
        self.expect_return(body, &bty, &rty);
        self.ret = outer_ret;
        self.breaks = outer_breaks;
        let mut generics: Vec<DefId> = match self.func.and_then(|f| self.generics_of_func(f)) {
            Some(g) => g
                .params
                .into_iter()
                .filter(|&p| self.defs.get(p).kind == DefKind::TypeParam)
                .collect(),
            None => Vec::new(),
        };
        // A default body is generic over `Self` without declaring it (mono
        // instantiates it once per implementing type), and so is every closure
        // written in it: its signature and captures may say `Self` or
        // `Self.Item`. The trait stands for `Self` and each associated type's
        // declaration for itself, so both are substituted per `Self` exactly
        // where the body's own types are.
        if let Some(trait_def) = self
            .func
            .and_then(|f| self.func_owner(f))
            .and_then(|f| self.defs.get(f).parent)
            .filter(|&p| self.defs.get(p).kind == DefKind::Trait)
        {
            generics.push(trait_def);
            let mut assoc: Vec<(Symbol, DefId)> = self
                .defs
                .get(trait_def)
                .ns
                .members
                .iter()
                .map(|(n, &d)| (n.clone(), self.defs.resolve_alias(d)))
                .filter(|&(_, d)| self.defs.get(d).kind == DefKind::TypeAlias)
                .collect();
            assoc.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            generics.extend(assoc.into_iter().map(|(_, d)| d));
        }
        let args = generics
            .iter()
            .map(|&p| Ty::Nominal {
                def: p,
                args: Vec::new(),
            })
            .collect();
        let shared = match self.ast.meta::<super::Captures>(node) {
            Some(super::Captures(defs)) => defs
                .iter()
                .map(|d| self.env.get(d).cloned().unwrap_or(Ty::Error))
                .collect(),
            None => Vec::new(),
        };
        self.ast.set_meta(
            node,
            ClosureSig {
                params: ptys,
                ret: rty,
                generics,
                shared,
            },
        );
        Ty::Nominal { def: defs.ty, args }
    }

    /// What `expected` says a closure of `arity` parameters takes and answers.
    ///
    /// A function pointer type says it outright. A variable says it through the
    /// `Func` bound its call site registered for it — `apply(f: impl Func(i32))`
    /// makes the argument's type a variable that must implement `Func.<(i32)>`,
    /// and the obligation is still queued when the argument is typed.
    fn closure_hint(&mut self, expected: &Ty, arity: usize) -> Option<(Vec<Ty>, Option<Ty>)> {
        let func = self.defs.resolve_alias(self.lang.get("func")?);
        match self.cx.shallow(expected) {
            Ty::Func { params, ret, .. } if params.len() == arity => Some((params, Some(*ret))),
            v @ Ty::Var(_) => {
                let mut params = None;
                let mut ret = None;
                for ob in self.cx.pending().to_vec() {
                    if let Obligation::Projection {
                        self_ty,
                        trait_def,
                        assoc,
                        out,
                        ..
                    } = ob
                        && trait_def == func
                        && self.cx.shallow(&self_ty) == v
                    {
                        match assoc.as_str() {
                            "Args" => params = Some(tuple_elems(&self.cx.shallow(&out))),
                            "Output" => ret = Some(out),
                            _ => {}
                        }
                    }
                }
                let params = params.filter(|p| p.len() == arity)?;
                Some((params, ret))
            }
            _ => None,
        }
    }

    /// Discharge an obligation on the `Func` trait (§5.5), which no impl is
    /// written for: a function pointer and a closure implement it by being what
    /// they are, and what their `Args` and `Output` are is their signature.
    ///
    /// `None` for an obligation on any other trait, and for a self type this
    /// does not answer for — a generic parameter, which its bound answers
    /// through ordinary selection, and anything that cannot be called, which
    /// selection then reports as the missing impl it is.
    fn try_solve_func(&mut self, ob: &Obligation) -> Option<Outcome> {
        let (self_ty, trait_def, args, origin, out) = match ob {
            Obligation::Trait {
                self_ty,
                trait_def,
                args,
                origin,
                ..
            } => (self_ty, *trait_def, args, *origin, None),
            Obligation::Projection {
                self_ty,
                trait_def,
                args,
                origin,
                out,
                ..
            } => (self_ty, *trait_def, args, *origin, Some(out)),
            _ => return None,
        };
        if Some(trait_def) != self.lang.get("func").map(|d| self.defs.resolve_alias(d)) {
            return None;
        }
        let s = self.cx.shallow(self_ty);
        if is_var(&s) {
            return Some(Outcome::Deferred);
        }
        if matches!(s, Ty::Error) {
            if let Some(out) = out {
                let _ = self.cx.unify(out, &Ty::Error);
            }
            return Some(Outcome::Solved);
        }
        let (params, ret) = self.func_value_sig(&s)?;
        // A projection answers `Args` or `Output` from the signature; a bare
        // `Func` obligation is met by having one.
        if let (Some(out), Obligation::Projection { assoc, .. }) = (out, ob) {
            let have = match assoc.as_str() {
                "Args" => args_tuple(&params),
                _ => ret,
            };
            self.expect(origin, &have, out);
        }
        let _ = args;
        Some(Outcome::Solved)
    }

    /// What a value of type `ty` takes and answers when it is called, if it can
    /// be: a function pointer's signature, and a generic parameter's `Func`
    /// bound — its arguments, and its `Output`.
    fn func_value_sig(&mut self, ty: &Ty) -> Option<(Vec<Ty>, Ty)> {
        match self.cx.shallow(ty) {
            Ty::Func { params, ret, .. } => Some((params, *ret)),
            // A trait object's type says the call it takes.
            Ty::Ptr { inner, .. } => match *inner {
                Ty::Dyn { def, assoc }
                    if Some(def) == self.lang.get("func").map(|d| self.defs.resolve_alias(d)) =>
                {
                    let pinned = |name: &str| {
                        assoc
                            .iter()
                            .find(|(n, _)| n.as_str() == name)
                            .map(|(_, t)| t.clone())
                    };
                    let params = pinned("Args").map(|a| tuple_elems(&a)).unwrap_or_default();
                    Some((params, pinned("Output").unwrap_or(Ty::Void)))
                }
                _ => None,
            },
            Ty::Nominal { def, args } if self.defs.get(def).kind == DefKind::Closure => {
                let d = self.defs.get(def);
                let (file, node) = (d.file?, d.node?);
                let sig = self.asts.get(&file)?.meta::<ClosureSig>(node)?;
                let mut map = Subst::default();
                for (p, a) in sig.generics.iter().zip(args) {
                    map.tys.insert(*p, a);
                }
                let params = sig
                    .params
                    .iter()
                    .map(|t| self.subst_type_params(t, &map))
                    .collect();
                let ret = self.subst_type_params(&sig.ret, &map);
                Some((params, ret))
            }
            Ty::Nominal { def, args } if self.defs.get(def).kind == DefKind::TypeParam => {
                let func = self.defs.resolve_alias(self.lang.get("func")?);
                if !self.param_bound_traits(def).contains(&func) {
                    return None;
                }
                let params = match self.defs.get(def).ns.members.get(&Symbol::new("Args")) {
                    Some(&synth) => tuple_elems(&self.param_ty(synth)),
                    None => Vec::new(),
                };
                let ret = match self.defs.get(def).ns.members.get(&Symbol::new("Output")) {
                    Some(&synth) => self.param_ty(synth),
                    None => Ty::Void,
                };
                // An `impl` return type's bound is written in its function's
                // parameters, and `args` are what this use instantiated them at.
                let mut map = Subst::default();
                for (p, a) in self.opaque_params(def).into_iter().zip(args) {
                    map.tys.insert(p, a);
                }
                let params = params
                    .iter()
                    .map(|t| self.subst_type_params(t, &map))
                    .collect();
                let ret = self.subst_type_params(&ret, &map);
                Some((params, ret))
            }
            _ => None,
        }
    }

    /// The type parameters an `impl` return type is generic over, in the
    /// order its type's arguments list them — recorded, or read off the tree.
    fn opaque_params(&self, def: DefId) -> Vec<DefId> {
        if let Some(params) = self.decls().opaque_params(def) {
            return params;
        }
        let d = self.defs.get(def);
        match (d.file, d.node) {
            (Some(file), Some(node)) => self
                .asts
                .get(&file)
                .and_then(|a| a.meta::<super::OpaqueArgs>(node))
                .map(|a| a.0)
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// The arguments `param`'s bound on `trait_def` was written with — the
    /// `f64` of `<T: Add.<f64>>`, the `(i32)` of `<F: Func(i32)>`.
    ///
    /// The recorded answer first, as everywhere a bound is read: a parameter
    /// that came out of a library has no tree here. For one declared in a file
    /// still being inferred there is no record yet, and the tree is read.
    fn bound_args(&mut self, param: DefId, trait_def: DefId) -> Vec<Ty> {
        if let Some(args) = self.decls().param_bound_args(param, trait_def) {
            return args;
        }
        let d = self.defs.get(param);
        if d.projection.is_some() {
            return Vec::new();
        }
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Vec::new();
        };
        let Some(ast) = self.asts.get(&file) else {
            return Vec::new();
        };
        let NodeKind::GenericTypeParam {
            constraint: Some(c),
            ..
        } = ast.node(node).kind.clone()
        else {
            return Vec::new();
        };
        let bounds = self.bound_nodes(file, c);
        match bounds
            .into_iter()
            .find(|&b| self.type_head_def_in(file, b) == Some(trait_def))
        {
            Some(b) => self.bound_trait_args_in(file, b),
            None => Vec::new(),
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
            Obligation::VariantPayload { .. }
            | Obligation::CompositeBody { .. }
            // A comparison still waiting on its operand's type is waiting on a
            // variable nothing solved, which is already reported as one.
            | Obligation::Comparison { .. }
            // A field access still waiting on its base is waiting on a
            // variable nothing solved, which is already reported as one.
            | Obligation::Field { .. } => return,
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
            if self.failed_resolution(callee) {
                self.infer_args_only(args);
                return Ty::Error;
            }
            if self.resolved_def(callee).is_none() {
                let recv = self.infer_expr(base);
                let recv = self.pin_str(&recv);
                // A receiver decides which method is called, so it is settled
                // here rather than at the end of the body — see `settle`.
                let recv = self.settle(&recv);
                // And a numeric literal settles here for the same reason: an
                // open `comptime_int` is a type no impl is written for, so the
                // whole chain below would find nothing and say nothing.
                let settled_lit = self.cx.var_kind(&recv);
                let recv = self.pin_numeric(&recv);
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
                // A method on a bounded type parameter resolves in the bound:
                // `<I: Summing>` makes `it.total()` mean `Summing.total`, with
                // the concrete impl picked once `I` is instantiated.
                if let Some((m, bound_args)) = self.bound_method_def(&recv, name.as_str()) {
                    // The dispatch is generic only when the method really is a
                    // trait's own declaration; `method_dispatch` is what decides
                    // that for the virtual case, and the reason is the same one.
                    let d = match self.defs.get(m).parent {
                        Some(p) if self.defs.get(p).kind == DefKind::Trait => {
                            MethodDispatch::Generic {
                                trait_def: p,
                                args: bound_args,
                            }
                        }
                        _ => MethodDispatch::Static,
                    };
                    return self.infer_method_call(callee, &recv, m, d, args, &targs);
                }
                // A method on a trait object resolves in the trait itself; which
                // impl runs is a vtable lookup a later stage performs.
                //
                // **Before the impl search**, and that order is the whole of
                // it: a blanket `impl <T> Trait for T` matches `T = dyn Trait`
                // as happily as it matches anything else, so searching impls
                // first turned every call on a trait object into a static call
                // to the blanket impl instantiated at the erased type — the
                // vtable built beside it went unused, and the answer was about
                // `dyn Trait` rather than about what was in it.
                if let Some(m) = self.dyn_method_def(&recv, name.as_str()) {
                    // A `<Self: Sized>` method has no slot (§3.4). It is still
                    // reachable when the *pointer* is itself an implementation
                    // — `impl <I: Iterator> Iterator for *mut I` — and then
                    // `Self` is the pointer, whose size is known.
                    if self.decls().sized_self(m) {
                        let through_ptr = matches!(self.cx.shallow(&recv), Ty::Ptr { .. })
                            && self.defs.get(m).parent.is_some_and(|t| {
                                matches!(self.select(&recv, t, &[]), Select::Ok(_))
                            });
                        if through_ptr {
                            return self.infer_method_call(
                                callee,
                                &recv,
                                m,
                                MethodDispatch::Static,
                                args,
                                &targs,
                            );
                        }
                        let msg = format!(
                            "`{name}` cannot be called on `{}`: it is bounded `Self: Sized`, and \
                             a trait object's size is not known",
                            self.cx.resolve(&recv).display(self.defs)
                        );
                        self.report(callee, msg);
                        self.infer_args_only(args);
                        return Ty::Error;
                    }
                    let d = self.method_dispatch(m, MethodDispatch::Virtual);
                    return self.infer_method_call(callee, &recv, m, d, args, &targs);
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
                    // The receiver is the **distinct** type, not the
                    // representation: the signature is rebound to match, so
                    // `self` is spelled as what the caller actually holds.
                    // `DistinctRecv` above is what tells lowering to reinterpret
                    // it, which is all the representation is still needed for.
                    let distinct = self.cx.shallow(&recv);
                    return self.infer_method_call_rebound(
                        callee,
                        &repr.clone(),
                        m,
                        MethodDispatch::Static,
                        args,
                        &targs,
                        Some((repr, distinct)),
                    );
                }
                // Nothing found. A field holding a function is still a valid
                // callee, so only complain when there is no such member at all.
                if self.field_ty(&recv, name.as_str()).is_none() {
                    let r = self.cx.resolve(&recv);
                    // A receiver that is already an error was reported where it
                    // went wrong; so is whatever a method on it returns.
                    if matches!(r, Ty::Error) {
                        self.infer_args_only(args);
                        return Ty::Error;
                    }
                    if !is_var(&r) {
                        for a in args {
                            self.infer_expr(*a);
                        }
                        // A call a format specifier wrote names a method the
                        // program never typed, so the message names the
                        // specifier instead (`parser::fmt::FormatCall`).
                        let msg = match self.ast.meta::<crate::parser::fmt::FormatCall>(callee) {
                            Some(f) => f.message(&r.display(self.defs)),
                            None => format!("no method `{name}` on `{}`", r.display(self.defs)),
                        };
                        // A literal that reached this point had nothing else to
                        // constrain it, so the type in the message is the
                        // default rather than anything the source wrote. Saying
                        // so is the difference between a puzzle and a fix.
                        match settled_lit {
                            Some(TyVarKind::Int | TyVarKind::Float) => self.report_with_note(
                                callee,
                                msg,
                                format!(
                                    "a literal with nothing to constrain it is `{}`; write the \
                                     type you meant, as in `cast.<i32>(..)`",
                                    r.display(self.defs)
                                ),
                            ),
                            _ => self.report(callee, msg),
                        }
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
            // An overload set is a name for several functions (§4.3); which one
            // this call means is decided here, before anything instantiates a
            // signature, and from here on it is an ordinary call to that one.
            let def = match self.defs.get(def).kind {
                DefKind::Overload => match self.select_overload(callee, def, args) {
                    Some(chosen) => chosen,
                    // Nothing in the set took this call, and that was reported:
                    // the arguments are still inferred, for their own sake.
                    None => {
                        self.infer_args_only(args);
                        return Ty::Error;
                    }
                },
                _ => def,
            };
            if self.defs.get(def).kind == DefKind::Func {
                let sig = self.func_def_ty(def);
                // `Trait.member(args)` — a trait method named through the trait
                // rather than called on a value. Nothing here says what `Self`
                // is, so it becomes a variable the context solves.
                let opened = self.open_trait_self(def);
                let seed = opened.as_ref().map(|o| o.subst.clone()).unwrap_or_default();
                let (inst, map) = self.instantiate_parts(callee, &sig, def, &targs, seed);
                if let Some(opened) = opened {
                    self.settle_trait_self(callee, def, opened, &map);
                }
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
                let variadic = self.defs.get(def).is_c_variadic();
                let result = self.apply_call_with(callee, &inst, &args, variadic);
                return self.intrinsic_result(def, result, &args);
            }
        }
        let cty = self.infer_expr(callee);
        // What a value is called as is decided by its type, so a callee still
        // waiting on an obligation — `fs[0]`, whose type is the `Index` impl's
        // `Output` — is settled here, the way a method's receiver is.
        let cty = self.settle(&cty);
        self.reject_named_args(
            args,
            "this call goes through a function value, which has parameter types but no parameter names",
        );
        let slots: Vec<Option<NodeId>> = args.iter().copied().map(Some).collect();
        self.apply_call(callee, &cty, &slots)
    }

    /// Which function of an overload set this call means (§4.3).
    ///
    /// Overloading is explicit here: the callee named a `func { a, b }`, and
    /// these are the functions it listed, with any set among them flattened in.
    /// A callee that named a function is that function and never reaches this.
    ///
    /// Two rounds, cheapest first. **What the call looks like** — how many
    /// arguments, and which parameters it names — settles most of it without
    /// typing anything. What is left is settled by **what the arguments are**:
    /// they are inferred once in a trial whose bindings are rolled back and
    /// whose diagnostics are discarded, and a candidate survives when every
    /// argument unifies with its parameter. A candidate with no generics beats
    /// one with them, so `f(i32)` wins over `f<T>(T)` for an `i32` — the
    /// generic one is what the concrete one exists to be overridden by.
    ///
    /// The choice is stamped on the callee, because everything after inference
    /// — lowering, monomorphization, the symbol — reads the resolution there.
    fn select_overload(&mut self, callee: NodeId, def: DefId, args: &[NodeId]) -> Option<DefId> {
        let name = self.defs.get(def).name.clone();
        let candidates = self.decls().overload_candidates(def);
        // An empty set is a set whose members did not resolve, which is already
        // reported where it was written.
        if candidates.is_empty() {
            return None;
        }

        let written: Vec<Symbol> = args.iter().filter_map(|&a| self.arg_name(a)).collect();
        let mut fit: Vec<DefId> = candidates
            .iter()
            .copied()
            .filter(|&c| self.call_shape_fits(c, args.len(), &written))
            .collect();
        if fit.len() > 1 {
            fit = self.overloads_matching(callee, &fit, args);
        }
        match fit.len() {
            0 => {
                let how = candidates
                    .iter()
                    .map(|&c| self.declaration_line(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.report(
                    callee,
                    format!("no overload of `{name}` takes these arguments; it has {how}"),
                );
                // And nothing further about this call: checking the arguments
                // against a member the call did not choose would report the
                // same mistake again, in the words of one candidate.
                None
            }
            1 => Some(self.stamp_overload(callee, fit[0])),
            _ => {
                // Two members that take the same parameters are a mistake in
                // the **set**, reported where it is written — every call
                // through it would otherwise repeat that one mistake.
                if !self.overloads_are_duplicates(&fit) {
                    let how = fit
                        .iter()
                        .map(|&c| self.declaration_line(c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.report(
                        callee,
                        format!(
                            "this call to `{name}` matches more than one of its overloads: {how}"
                        ),
                    );
                }
                Some(self.stamp_overload(callee, fit[0]))
            }
        }
    }

    /// Record which overload a call chose, so lowering calls that one.
    fn stamp_overload(&mut self, callee: NodeId, def: DefId) -> DefId {
        // A turbofish's base carries the resolution, not the `GenericApply` —
        // `infer_call` peeled it before it got here, so `callee` is already the
        // node the resolver wrote on.
        self.ast.set_meta(callee, Resolution::Def(def));
        def
    }

    /// Whether a candidate could take a call of `count` arguments naming
    /// `written` parameters — the question the argument *types* do not answer.
    fn call_shape_fits(&self, def: DefId, count: usize, written: &[Symbol]) -> bool {
        let Some(names) = self.decls().param_names(def) else {
            return true;
        };
        let defaults = self.decls().param_defaults(def).unwrap_or_default();
        let required = defaults.iter().filter(|&&d| !d).count();
        if self.defs.get(def).is_c_variadic() {
            return count >= names.len();
        }
        if count > names.len() || count < required {
            return false;
        }
        written.iter().all(|w| names.contains(w))
    }

    /// The candidates every argument's type fits, inferred once in a trial.
    fn overloads_matching(&mut self, callee: NodeId, fit: &[DefId], args: &[NodeId]) -> Vec<DefId> {
        let mark = self.diags.len();
        let outer = self.cx.snapshot();
        let arg_tys: Vec<Ty> = args.iter().map(|&a| self.infer_expr(a)).collect();
        let mut matched: Vec<(DefId, bool)> = Vec::new();
        for &c in fit {
            let snap = self.cx.snapshot();
            let sig = self.func_def_ty(c);
            let (inst, map) = self.instantiate_parts(callee, &sig, c, &[], Subst::default());
            if let Ty::Func { params, .. } = self.cx.shallow(&inst) {
                let fits = params.len() == arg_tys.len()
                    && params
                        .iter()
                        .zip(&arg_tys)
                        .all(|(p, a)| self.cx.unify(a, p).is_ok())
                    && self.bounds_hold(c, &map);
                if fits {
                    let generic = !self.func_generic_param_defs(c).is_empty();
                    matched.push((c, generic));
                }
            }
            self.cx.rollback(snap);
        }
        self.cx.rollback(outer);
        self.diags.truncate(mark);
        // A concrete signature beats a generic one that would also have taken
        // these arguments.
        if matched.iter().any(|&(_, generic)| !generic) {
            matched.retain(|&(_, generic)| !generic);
        }
        matched.into_iter().map(|(c, _)| c).collect()
    }

    /// Whether every candidate left takes the same parameters, which is a
    /// mistake in the declarations rather than in this call.
    fn overloads_are_duplicates(&mut self, fit: &[DefId]) -> bool {
        let params = |me: &mut Self, d: DefId| {
            let sig = me.func_def_ty(d);
            match me.cx.shallow(&sig) {
                Ty::Func { params, .. } => Some(params),
                _ => None,
            }
        };
        let Some(first) = params(self, fit[0]) else {
            return false;
        };
        fit[1..]
            .iter()
            .all(|&c| params(self, c).is_some_and(|p| super::same_params(self.defs, &first, &p)))
    }

    /// Whether what this trial bound a candidate's generic parameters to meets
    /// the bounds they were declared with.
    ///
    /// This is what makes two overloads that differ *only* in a bound —
    /// `<T: Eq>` and `<T: Display>` — a pair a call can tell apart: without it
    /// both signatures are `func(T)` and every call matches both. A parameter
    /// whose argument is not known yet is left alone: nothing has gone wrong,
    /// there is just nothing to check against.
    fn bounds_hold(&mut self, def: DefId, map: &Subst) -> bool {
        for g in self.func_generic_param_defs(def) {
            let Some(bounds) = self.defs.get(g).param_bounds.clone() else {
                continue;
            };
            let Some(arg) = map.tys.get(&g).cloned() else {
                continue;
            };
            let arg = self.cx.resolve(&arg);
            if is_var(&arg) || matches!(arg, Ty::Error) {
                continue;
            }
            for t in bounds {
                match self.select(&arg, t, &[]) {
                    Select::Ok(_) | Select::ByBound | Select::Defer | Select::Error => {}
                    Select::NoImpl | Select::Ambiguous => return false,
                }
            }
        }
        true
    }

    /// How a candidate reads in a diagnostic about which of them was meant.
    fn declaration_line(&mut self, def: DefId) -> String {
        let sig = self.func_def_ty(def);
        format!("`{}`", self.cx.resolve(&sig).display(self.defs))
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
    fn func_param_names(&self, def: DefId) -> Option<Vec<Symbol>> {
        self.decls().param_names(def)
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
    fn func_param_defaults(&self, def: DefId) -> Option<Vec<bool>> {
        self.decls().param_defaults(def)
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
        // A `#c_vararg` call has a tail no parameter names, so there is no
        // parameter order to bind into: the arguments are already in the only
        // order they have. Naming one is refused rather than ignored — the tail
        // would silently keep its position while the head moved.
        if self.defs.get(def).is_c_variadic() {
            if named {
                self.report(
                    callee,
                    "a `#c_vararg` call may not name an argument: the variadic tail has no \
                     parameter names to bind against",
                );
                return ArgBinding::Failed;
            }
            return ArgBinding::AsWritten;
        }
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
        self.apply_call_with(callee, callee_ty, args, false)
    }

    /// As [`apply_call`](Self::apply_call), but `variadic` says the callee is a
    /// `#c_vararg` declaration: the parameters are the **fixed** ones and
    /// everything past them is a C variadic tail, checked one argument at a time
    /// by [`check_vararg_arg`](Self::check_vararg_arg) rather than against a
    /// parameter type there is none of.
    fn apply_call_with(
        &mut self,
        callee: NodeId,
        callee_ty: &Ty,
        args: &[Option<NodeId>],
        variadic: bool,
    ) -> Ty {
        // A value that implements `Func` without being a function pointer is
        // called the way a function pointer with its signature would be, and
        // marked so lowering dispatches on its type instead (§5.5).
        let shallow = self.cx.shallow(callee_ty);
        if !matches!(shallow, Ty::Func { .. })
            && let Some((params, ret)) = self.func_value_sig(&shallow)
        {
            self.ast.set_meta(callee, FuncCall);
            let sig = Ty::Func {
                params,
                ret: Box::new(ret),
                c: false,
            };
            return self.apply_call_with(callee, &sig, args, variadic);
        }
        // A closure argument is typed against the parameter it fills, first:
        // that is where its own parameters' types come from (§5.5).
        let wanted: Vec<Ty> = match self.cx.shallow(callee_ty) {
            Ty::Func { params, .. } => params,
            _ => Vec::new(),
        };
        let arg_tys: Vec<Option<Ty>> = args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                a.map(|n| self.infer_arg(n, wanted.get(i).cloned()))
            })
            .collect();
        match self.cx.shallow(callee_ty) {
            Ty::Func { params, ret, .. } => {
                let fits = if variadic {
                    arg_tys.len() >= params.len()
                } else {
                    arg_tys.len() == params.len()
                };
                if fits {
                    for (a, (arg_node, aty)) in params.iter().zip(args.iter().zip(&arg_tys)) {
                        if let (Some(node), Some(aty)) = (arg_node, aty) {
                            self.expect(*node, aty, a);
                        }
                    }
                    // The tail. Each argument stands on its own — C's convention
                    // has no parameter to match it against — so what is checked
                    // is that it can cross at all, and what is recorded is the
                    // promotion C would have applied silently.
                    for (arg_node, aty) in args.iter().zip(&arg_tys).skip(params.len()) {
                        if let (Some(node), Some(aty)) = (arg_node, aty) {
                            self.check_vararg_arg(*node, aty);
                        }
                    }
                } else if variadic {
                    self.report(
                        callee,
                        format!(
                            "this function takes at least {} argument(s) but {} were supplied",
                            params.len(),
                            arg_tys.len()
                        ),
                    );
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
            Ty::Error => Ty::Error,
            other if is_var(&other) => Ty::Error,
            // A value that is not a function, which nothing else reports.
            other => {
                self.report(
                    callee,
                    format!(
                        "`{}` is not a function, so it cannot be called",
                        other.display(self.defs)
                    ),
                );
                Ty::Error
            }
        }
    }

    /// One argument in a `#c_vararg` tail: refuse what cannot cross, and record
    /// the **default argument promotion** C would have applied.
    ///
    /// The promotions are not a convenience. A variadic callee reads its tail
    /// with `va_arg`, which can only be asked for a promoted type, so a `u8`
    /// passed as a `u8` is read back as an `int` from a slot that was never
    /// filled. Recording a [`Coercion`] here is what makes `printf("%d", b)`
    /// pass the `int` the callee is about to read — and it is recorded rather
    /// than required of the program because C's rule is the *callee's*, not
    /// something the call site chose.
    ///
    /// What is refused is what this language owns the representation of: a `str`
    /// and a slice are two words with no C spelling, an array and a tuple have
    /// no argument convention here at all, and a `dyn` is a pair whose second
    /// half is a vtable. A **struct** is not refused — `c.ptr.<T>` is one, and an
    /// `extern("c")` signature is already a promise that its types are C's.
    fn check_vararg_arg(&mut self, node: NodeId, ty: &Ty) {
        // Nothing else will constrain this argument — there is no parameter for
        // it — so its literals settle here. The **default is C's**, not this
        // language's: a bare `1` in `printf(c"%d", 1)` is an `int` to everyone
        // who reads it and to the `va_arg` that will pick it up, where this
        // language's own default of `isize` would put eight bytes under a
        // conversion that reads four. A literal too large for an `int` keeps the
        // ordinary default, which is what C does with one too.
        let pinned = self.pin_str(ty);
        if is_var(&self.cx.shallow(&pinned)) && self.fits_c_int(node) {
            let _ = self.cx.unify(&pinned, &Ty::int(32, true));
        }
        let pinned = self.pin_numeric(&pinned);
        let resolved = self.settle(&pinned);
        if matches!(resolved, Ty::Error) || is_var(&resolved) {
            return;
        }
        // A string literal in a tail is the mistake worth its own sentence: the
        // C function is about to read a `char *`, and `c"..."` is the spelling
        // that produces one.
        if matches!(resolved, Ty::ComptimeStr) || self.cx.admits_str(&resolved) {
            self.report_with_note(
                node,
                "a `str` cannot be passed in a C variadic tail".to_string(),
                "a `str` is a pointer and a length, and C reads one argument — write `c\"...\"` \
                 for a literal, or `c.to_cstr(s)` for a value"
                    .to_string(),
            );
            return;
        }
        let bad = match &resolved {
            Ty::Slice { .. } => Some("a slice"),
            Ty::Array { .. } => Some("an array"),
            Ty::Tuple(elems) if !elems.is_empty() => Some("a tuple"),
            Ty::Void | Ty::Tuple(_) => Some("`void`"),
            Ty::Never => Some("`never`"),
            Ty::Dyn { .. } => Some("a trait object"),
            Ty::Func { .. } => Some("a function"),
            _ => None,
        };
        if let Some(what) = bad {
            let msg = format!(
                "{what} cannot be passed in a C variadic tail: `{}` has no C argument convention",
                resolved.display(self.defs)
            );
            self.report_with_note(
                node,
                msg,
                "the tail is read with `va_arg`, which names a C type — pass the parts this \
                 language owns the representation of separately"
                    .to_string(),
            );
            return;
        }
        // A literal that has not settled yet carries its own `Coercion`, and its
        // default is already a promoted type (`i32`, `f64`), so there is nothing
        // to add and overwriting would lose the settling.
        if self.ast.meta::<Coercion>(node).is_some() {
            return;
        }
        if let Some(to) = self.c_promotion(&resolved) {
            self.ast.set_meta(node, Coercion { to });
        }
    }

    /// Whether `node` is an integer literal whose value an `int` holds — the
    /// condition under which a C variadic tail settles one on `i32` rather than
    /// on this language's `isize`.
    fn fits_c_int(&self, node: NodeId) -> bool {
        // An argument may be wrapped in an `Arg` — that is where a name would
        // go — and the literal's value was recorded against the literal.
        let node = match &self.ast.node(node).kind {
            NodeKind::Arg { value, .. } => *value,
            _ => node,
        };
        let Some(value) = self.int_values.get(&node) else {
            return false;
        };
        num_bigint::BigInt::from(i32::MIN) <= *value && *value <= num_bigint::BigInt::from(i32::MAX)
    }

    /// C's default argument promotions: anything narrower than an `int` is read
    /// as an `int`, and a `float` is read as a `double` (C17 §6.5.2.2).
    ///
    /// `bool` promotes too — it is a one-bit value here and a full `int` in the
    /// register the callee reads. Everything `int`-wide or wider, every pointer
    /// and every struct is passed as it stands.
    fn c_promotion(&self, ty: &Ty) -> Option<Ty> {
        match ty {
            Ty::Bool => Some(Ty::int(32, true)),
            // A width still symbolic is inside a family impl, which a
            // `#c_vararg` call site cannot be: the declaration may not be
            // generic, and the argument is a concrete value by the time it is
            // checked. `None` here is "no promotion", which is right either way.
            Ty::Int { width, .. } if width.bits().is_some_and(|b| b < 32) => {
                Some(Ty::int(32, true))
            }
            Ty::Float(super::ty::FloatWidth::F32) => Some(Ty::Float(super::ty::FloatWidth::F64)),
            _ => None,
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
                    // A `#lang` trait's methods are reachable wherever the
                    // language's own syntax reaches them: `for` desugars to
                    // `.into_iter()` / `.next()` and `.?` to `.branch()`, and
                    // those calls are indistinguishable from written ones by the
                    // time they get here. Gating them on the program having
                    // imported `<core/iter>` would make `for` require an import.
                    if !self.in_scope_traits.contains(&td) && !self.lang_traits.contains(&td) {
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

    /// The type a **type-denoting** expression names, or `None` when the
    /// expression is a value.
    ///
    /// `u8`, `Self`, `T`, `Vec3`, `uint.<8>` in expression position all name a
    /// type; `x`, `f()`, `p.field` do not. The distinction is the def the
    /// resolver put on the head: only a type's def qualifies, and a `Ty` is
    /// then built by the ordinary type-expression path, so `Self` expands and a
    /// family application carries its width.
    ///
    /// A *namespace* is deliberately not one of them. `ns.MEMBER` is a name the
    /// resolver already linked, and reading it as a type here would answer a
    /// second time for something already answered.
    fn type_denoted_by(&mut self, node: NodeId) -> Option<Ty> {
        let head = match self.ast.node(node).kind.clone() {
            NodeKind::GenericApply { base, .. } => base,
            NodeKind::Path { .. } | NodeKind::FieldAccess { .. } => node,
            _ => return None,
        };
        let def = self.defs.resolve_alias(self.resolved_def(head)?);
        if !matches!(
            self.defs.get(def).kind,
            DefKind::Primitive
                | DefKind::Struct
                | DefKind::Enum
                | DefKind::TypeAlias
                | DefKind::TypeParam
                // A `::` binding whose right-hand side names a type is a type
                // (§2.4): `usize :: distinct uint.<PTR_BITS>` is a `Const` def
                // like every other `::`, and `usize.MAX` is as much a read
                // through a type name as `u64.MAX` is. One that binds a *value*
                // is filtered out below, by having no type to give.
                | DefKind::Const
        ) {
            return None;
        }
        let ty = self.ty_from_node(node);
        (!matches!(ty, Ty::Error) && !is_var(&self.cx.shallow(&ty))).then_some(ty)
    }

    /// Resolve `name` as an associated item of the **type** `ty`, in expression
    /// position, and give the type a use of it has.
    ///
    /// Two routes, and which one applies is decided by what `ty` is.
    ///
    /// A **type parameter** has no impls of its own — it is not a type yet — so
    /// its bounds are the proof, exactly as they are for a method call through
    /// one ([`Inferer::bound_method_def`]). The item found is the *trait's*
    /// declaration, which holds no value; monomorphization points it at
    /// whichever impl the instantiation selected. This is what makes `Self.BITS`
    /// work inside `impl <T: Float> Display for T`.
    ///
    /// Anything else goes through the impls, by unifying each impl's target
    /// against `ty` the way [`Inferer::impl_method_def`] does. That is the only
    /// route to a member of a **family** impl: `impl <const N: u16> uint.<N>`
    /// parks its members anonymously, so `u8.MAX` is found by matching `uint.<N>`
    /// against `u8` and never by a namespace hop. The match also says what the
    /// impl's generics are here, and the item's type is written in their terms
    /// (`MAX :: cast.<Self>(...)` is a `Self`), so it is substituted through.
    fn assoc_through_type(&mut self, node: NodeId, ty: &Ty, name: &Symbol) -> Option<Ty> {
        let s = self.cx.shallow(ty);
        if let Ty::Nominal { def, .. } = &s
            && self.defs.get(*def).kind == DefKind::TypeParam
        {
            let def = *def;
            for t in self.param_bound_traits(def) {
                let Some(&m) = self.defs.get(t).ns.members.get(name) else {
                    continue;
                };
                let m = self.defs.resolve_alias(m);
                if self.defs.get(m).kind != DefKind::Const {
                    continue;
                }
                self.ast.set_meta(node, Resolution::Def(m));
                let out = self.def_ty(node, m);
                self.ast.set_meta(node, AssocTy(out.clone()));
                return Some(out);
            }
            return None;
        }
        // A `distinct` type inherits its representation's associated items the
        // way it inherits its methods (§2.4, [`Inferer::distinct_method_def`]):
        // `usize` is `distinct uint.<PTR_BITS>`, and the width's ceiling is as
        // much a fact about it as `wrapping_add` is. Tried **last**, so an item
        // the distinct type declares itself always wins.
        //
        // What is matched is the representation; what `Self` *means* is not. An
        // item whose type is the one the impl was selected for comes back as the
        // distinct type, for the same reason `Meters + Meters` is `Meters` and
        // not an `f64`.
        let (matched, i, m) = match self.impl_assoc_def(&s, name) {
            Some((i, m)) => (s.clone(), i, m),
            None => {
                let repr = self.distinct_repr(&s)?;
                let repr = self.cx.shallow(&repr);
                let (i, m) = self.impl_assoc_def(&repr, name)?;
                (repr, i, m)
            }
        };
        let s = matched;
        let map = self.commit_impl(i, &s, &[], node);
        self.ast.set_meta(node, Resolution::Def(m));
        // What this read bound the impl's parameters to, in the order
        // `stamp_assoc_const_generics` recorded them. A constant in a generic
        // impl is a different value for every one of them, and this is the
        // pairing the evaluator zips against (see [`Instantiation`]).
        let generics = self.impls.impls[i].generics.clone();
        if !generics.is_empty() {
            let args = generics
                .iter()
                .map(|d| match map.consts.get(d) {
                    Some(k) => GenericArg::Const(k.clone()),
                    None => GenericArg::Ty(map.tys.get(d).cloned().unwrap_or(Ty::Error)),
                })
                .collect();
            self.ast.set_meta(node, Instantiation(args));
        }
        let raw = self.def_ty(node, m);
        // Resolved, not merely substituted: the match bound the impl's width to
        // a variable and then solved it against the type read through, and a
        // `uint.<?0>` handed back would be "type annotations needed" wherever
        // the context does not pin it down a second time.
        let out = self.subst_type_params(&raw, &map);
        let out = self.cx.resolve(&out);
        // `Self` back in the distinct type's terms, where that is what was read
        // through: `usize.MAX` is a `usize`.
        let out = if out == s { ty.clone() } else { out };
        self.ast.set_meta(node, AssocTy(out.clone()));
        Some(out)
    }

    /// The impl providing the associated **constant** `name` for `ty`, and the
    /// member itself.
    ///
    /// The same walk and the same ranking [`Inferer::impl_method_def`] does —
    /// an inherent impl beats a trait's, a concrete target beats a blanket one,
    /// and a tie is a question the program has to answer — asking for a
    /// constant instead of a function.
    fn impl_assoc_def(&mut self, ty: &Ty, name: &Symbol) -> Option<(usize, DefId)> {
        if matches!(ty, Ty::Error) || is_var(ty) {
            return None;
        }
        let mut best: Option<(u8, usize, DefId)> = None;
        let mut ambiguous = false;
        for i in 0..self.impls.impls.len() {
            let imp = self.impls.impls[i].clone();
            if let Some(td) = imp.trait_def
                && !self.in_scope_traits.contains(&td)
                && !self.lang_traits.contains(&td)
            {
                continue;
            }
            let Some(&member) = imp.members.get(name) else {
                continue;
            };
            let member = self.defs.resolve_alias(member);
            if self.defs.get(member).kind != DefKind::Const {
                continue;
            }
            if !self.trial_impl(i, ty, &[]) {
                continue;
            }
            let score = match (imp.trait_def.is_none(), imp.self_is_generic()) {
                (true, false) => 4,
                (true, true) => 3,
                (false, false) => 2,
                (false, true) => 1,
            };
            match best {
                Some((bs, _, _)) if bs > score => {}
                Some((bs, _, _)) if bs == score => ambiguous = true,
                _ => {
                    best = Some((score, i, member));
                    ambiguous = false;
                }
            }
        }
        if ambiguous {
            return None;
        }
        best.map(|(_, i, m)| (i, m))
    }

    /// Resolve `name` through the trait bounds of a generic type parameter
    /// receiver (`<I: Summing>` → `it.total()` is `Summing.total`).
    fn bound_method_def(&mut self, recv: &Ty, name: &str) -> Option<(DefId, Vec<Ty>)> {
        let s = self.autoderef(&self.cx.shallow(recv));
        let Ty::Nominal { def, .. } = s else {
            return None;
        };
        if self.defs.get(def).kind != DefKind::TypeParam {
            return None;
        }
        let sym = crate::common::symbol::Symbol::new(name);
        // The bound's own arguments — the `f64` of `T: Add.<f64>` — travel with
        // the method, because they are half of *which* impl this bound stands
        // for and the impl is picked long after this (see
        // [`MethodDispatch::Generic`]). They are read off the parameter's own
        // declaration, which a synthesized one does not have; a bounded
        // associated type is written `Item :: type: Holder`, with no place to
        // put arguments, so an empty list is the whole truth there.
        // The tree the bound was written in, when this compilation has it. A
        // **synthesized** parameter has no declaration of its own, and a
        // parameter that came out of a library names a file nothing here
        // parsed — both answer `None`, and the recorded arguments below are
        // what serves them.
        let written = match self.defs.get(def).projection {
            Some(_) => None,
            None => {
                let d = self.defs.get(def);
                match (d.file, d.node) {
                    (Some(file), Some(node)) => {
                        match self.asts.get(&file).map(|a| a.node(node).kind.clone()) {
                            Some(NodeKind::GenericTypeParam {
                                constraint: Some(c),
                                ..
                            }) => Some((file, self.bound_nodes(file, c))),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
        };
        for t in self.param_bound_traits(def) {
            if !self.in_scope_traits.contains(&t) {
                continue;
            }
            let Some(&m) = self.defs.get(t).ns.members.get(&sym) else {
                continue;
            };
            if self.defs.get(m).kind != DefKind::Func {
                continue;
            }
            // **The recorded answer first, always.** `<T: Add.<f64>>` is an
            // `f64` only because something wrote `f64` down, and for a
            // parameter out of a library the writing is not here — an empty
            // list there is not a cautious answer but a wrong one, and it
            // picked the wrong impl.
            if let Some(args) = self.decls().param_bound_args(def, t) {
                return Some((m, args));
            }
            let args = match &written {
                Some((file, bounds)) => {
                    let (file, bounds) = (*file, bounds.clone());
                    match bounds
                        .into_iter()
                        .find(|&b| self.type_head_def_in(file, b) == Some(t))
                    {
                        Some(b) => self.bound_trait_args_in(file, b),
                        None => Vec::new(),
                    }
                }
                None => Vec::new(),
            };
            return Some((m, args));
        }
        None
    }

    /// The type a type-parameter def stands for.
    ///
    /// Ordinarily the parameter itself. A **pinned** associated-type parameter
    /// is the exception: `<T: Holder.<Item = i32>>` says `T.Item` *is* `i32`,
    /// and that is true inside the generic body, before any call site exists —
    /// which is exactly where it has to be true, since that is where a function
    /// declared to return `i32` has to accept what `t.get()` gives back.
    fn param_ty(&mut self, def: DefId) -> Ty {
        // The recorded answer first: the node `pinned` names is in the file
        // that declared the parameter, and for one out of a library that file
        // was never parsed here — reading it would panic rather than answer.
        if let Some(t) = self.decls().param_pinned(def) {
            return t;
        }
        if let Some(p) = self.defs.get(def).projection.clone()
            && let (Some(file), Some(node)) = (self.defs.get(def).file, p.pinned)
            && self.asts.contains_key(&file)
        {
            return self.ty_from_node_in(file, node);
        }
        Ty::Nominal {
            def,
            args: Vec::new(),
        }
    }

    /// The traits that bound a type parameter.
    ///
    /// Two shapes answer this. An ordinary parameter carries its bounds in its
    /// own declaration, as written. A **synthesized** one — the `Item` of
    /// `T.Item` — has no declaration of its own: what bounds it is what the
    /// associated type's declaration said (`Item :: type: Holder`), which is
    /// recorded on that declaration's def because the file using the trait
    /// cannot read the file that wrote it.
    fn param_bound_traits(&mut self, def: DefId) -> Vec<DefId> {
        if let Some(p) = self.defs.get(def).projection.clone() {
            let Some(&adef) = self.defs.get(p.trait_def).ns.members.get(&p.assoc) else {
                return Vec::new();
            };
            let adef = self.defs.resolve_alias(adef);
            return self.defs.get(adef).assoc_bounds.clone().unwrap_or_default();
        }
        // The recorded answer first, always: a parameter that arrived with a
        // library has no tree here, and a tree-first branch would read as "no
        // bounds" for exactly the impls that most need them.
        if let Some(bounds) = self.defs.get(def).param_bounds.clone() {
            return bounds;
        }
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Vec::new();
        };
        // A parameter whose def came from a library names a file this
        // compilation never parsed. Nothing recorded its bounds — a library
        // written before they were recorded — so the honest answer is that
        // there are none to read, not a panic on a tree that is not here.
        let Some(ast) = self.asts.get(&file) else {
            return Vec::new();
        };
        let NodeKind::GenericTypeParam {
            constraint: Some(constraint),
            ..
        } = ast.node(node).kind.clone()
        else {
            return Vec::new();
        };
        self.bound_nodes(file, constraint)
            .into_iter()
            .filter_map(|b| self.type_head_def_in(file, b))
            .filter(|&t| self.defs.get(t).kind == DefKind::Trait)
            .collect()
    }

    /// Whether `ty` is a type parameter that `trait_def` bounds.
    ///
    /// The same walk [`Inferer::bound_method_def`] does, asking about the trait
    /// rather than about one of its methods. `in_scope_traits` is deliberately
    /// **not** consulted: that filter is about which names a method call may
    /// resolve through, and a bound the program wrote is a fact about the
    /// parameter whether or not the trait's name is in scope here.
    fn param_has_bound(&mut self, ty: &Ty, trait_def: DefId) -> bool {
        let s = self.cx.shallow(ty);
        let Ty::Nominal { def, .. } = s else {
            return false;
        };
        if self.defs.get(def).kind != DefKind::TypeParam {
            return false;
        }
        self.param_bound_traits(def).contains(&trait_def)
    }

    /// The trait arguments a bound was written with, as types.
    ///
    /// `<T: Summing>` gives an empty list, and so does a trait that takes no
    /// arguments; `<T: Add.<f64>>` gives `[f64]`. An `<Assoc = T>` binding is
    /// **not** one of them — it constrains a projection rather than filling a
    /// parameter — which is the same line [`ImplInfo::trait_args`] draws.
    fn bound_trait_args_in(&mut self, file: FileId, bound: NodeId) -> Vec<Ty> {
        // The tree, and only where there is one: a parameter that arrived with
        // a library names a file this compilation never parsed. The recorded
        // answer is what that case reads (`Decls::param_bound_args`), and this
        // is what recorded it.
        let Some(ast) = self.asts.get(&file) else {
            return Vec::new();
        };
        // Both spellings of an applied name: a bound is written in **type**
        // position, where `Scale.<i32>` is a `TypePath` carrying its arguments;
        // the postfix `GenericApply` is the expression-position form, and
        // reaches here through the paths that share this helper.
        let args = match ast.node(bound).kind.clone() {
            NodeKind::TypePath { generic_args, .. } => generic_args,
            NodeKind::GenericApply { args, .. } => args,
            _ => return Vec::new(),
        };
        let args: Vec<NodeId> = args
            .iter()
            .copied()
            .filter(|&a| {
                !self
                    .asts
                    .get(&file)
                    .is_some_and(|a2| matches!(a2.node(a).kind, NodeKind::AssocBinding { .. }))
            })
            .collect();
        args.iter()
            .map(|&a| self.ty_from_node_in(file, a))
            .collect()
    }

    /// The individual trait nodes of a generic parameter's constraint, which is
    /// either a `+`-separated [`NodeKind::Bounds`] list or a single trait.
    fn bound_nodes(&self, file: FileId, constraint: NodeId) -> Vec<NodeId> {
        self.decls().bound_nodes(file, constraint)
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
    ///
    /// `Self.Item` is as unknown as `Self`, so each associated type gets a
    /// variable too, solved by projecting through `Self` once it is known. The
    /// answer seeds the call's instantiation, so the member's own bounds —
    /// `from_iter :: func <I: Iterator.<Item = Self.Item>>` — see the variables
    /// rather than the trait's abstract declarations; the obligations that
    /// solve them are registered by [`Inferer::settle_trait_self`] once the
    /// instantiation has said what the trait's own arguments are.
    fn open_trait_self(&mut self, method: DefId) -> Option<OpenedSelf> {
        let trait_def = self.defs.get(method).parent?;
        if self.defs.get(trait_def).kind != DefKind::Trait {
            return None;
        }
        let self_ty = self.cx.fresh();
        let mut tys = HashMap::from([(trait_def, self_ty.clone())]);
        let mut assocs: Vec<(Symbol, DefId)> = self
            .defs
            .get(trait_def)
            .ns
            .members
            .iter()
            .filter(|&(_, &m)| self.defs.get(m).assoc_bounds.is_some())
            .map(|(n, &m)| (n.clone(), m))
            .collect();
        assocs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut outs = Vec::new();
        for (name, adef) in assocs {
            let out = self.cx.fresh();
            tys.insert(adef, out.clone());
            outs.push((name, out));
        }
        Some(OpenedSelf {
            trait_def,
            self_ty,
            assocs: outs,
            subst: Subst::of_types(tys),
        })
    }

    /// The obligations that decide an opened `Self` (see
    /// [`Inferer::open_trait_self`]): which impl it is, and what that impl's
    /// associated types are.
    fn settle_trait_self(
        &mut self,
        callee: NodeId,
        method: DefId,
        opened: OpenedSelf,
        map: &Subst,
    ) {
        let OpenedSelf {
            trait_def,
            self_ty,
            assocs,
            ..
        } = opened;
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
        for (assoc, out) in assocs {
            self.cx.register(Obligation::Projection {
                self_ty: self_ty.clone(),
                trait_def,
                args: args.clone(),
                assoc,
                out,
                origin: callee,
                method: None,
            });
        }
        self.cx.register(Obligation::Trait {
            self_ty,
            trait_def,
            args,
            origin: callee,
            stamp: Some(self.defs.get(method).name.clone()),
        });
    }

    /// What a trait *declaration*'s `Self` is at a call: the receiver, and
    /// each of the trait's associated types what the receiver binds it to.
    /// `None` when `method` is not a trait's own declaration or the receiver is
    /// not known yet.
    ///
    /// Only calls that land on a trait's own declaration need this — dispatch
    /// through a trait object (`*dyn Summing`), through a type parameter's bound
    /// (`<I: Summing>`), or to a default body. A call that selected a concrete
    /// impl's member already has the impl's signature and is left alone.
    ///
    /// It seeds the call's instantiation (see [`Inferer::instantiate_parts`]),
    /// so it reaches the method's **own generic bounds** as well as its
    /// signature — a default method `map :: func <F: Func(Self.Item) -> B> (…)`
    /// holds `F` to the receiver's `Item`, not to the trait's abstract one.
    fn trait_self_subst(&mut self, at: NodeId, method: DefId, recv: &Ty) -> Option<Subst> {
        let parent = self.defs.get(method).parent?;
        if self.defs.get(parent).kind != DefKind::Trait {
            return None;
        }
        // Look through the receiver's pointer: `*dyn T` and `*I` both stand for
        // a `Self` of `dyn T` / `I`. Except for a `<Self: Sized>` method on a
        // `*dyn T`, which is never `dyn T`'s: there the pointer is `Self`.
        let head = match self.cx.shallow(recv) {
            Ty::Ptr { inner, .. } => match self.cx.shallow(&inner) {
                Ty::Dyn { .. } if self.decls().sized_self(method) => self.cx.shallow(recv),
                inner => inner,
            },
            other => other,
        };
        if matches!(head, Ty::Error) || is_var(&head) {
            return None;
        }
        // `Self` is only half of it. A signature may also name `Self.Item`, and
        // what that *is* depends on the same receiver. For a type parameter it
        // is the associated-type parameter its bound minted (`N.Inner`), found
        // in the parameter's own namespace. For a concrete type it is whatever
        // the impl bound the name to — and which impl, and what its generics
        // are, is a selection: `Mapped.<I, F>`'s `Item :: B` is only a type once
        // the impl's `F: Func(I.Item) -> B` has solved `B`. So that is asked as
        // a projection, the way every other `<T as Trait>.Assoc` is.
        let mut tys = HashMap::from([(parent, head.clone())]);
        // A trait's own body calls through `Self`, which is the trait's nominal
        // and answers from its own namespace like a parameter does.
        let is_param = matches!(&head, Ty::Nominal { def, .. }
            if matches!(self.defs.get(*def).kind, DefKind::TypeParam | DefKind::Trait));
        if !is_param && matches!(head, Ty::Nominal { .. } | Ty::Ptr { .. }) {
            let assocs: Vec<(Symbol, DefId)> = self
                .defs
                .get(parent)
                .ns
                .members
                .iter()
                .filter(|&(_, &m)| self.defs.get(m).assoc_bounds.is_some())
                .map(|(n, &m)| (n.clone(), m))
                .collect();
            let args: Vec<Ty> = self
                .type_param_defs(parent)
                .into_iter()
                .map(|_| self.cx.fresh())
                .collect();
            for (assoc, adef) in assocs {
                let out = self.cx.fresh();
                self.cx.register(Obligation::Projection {
                    self_ty: head.clone(),
                    trait_def: parent,
                    args: args.clone(),
                    assoc,
                    out: out.clone(),
                    origin: at,
                    method: None,
                });
                tys.insert(adef, out);
            }
        } else if let Ty::Dyn { assoc: pins, .. } = &head {
            // A trait object carries its associated types in its type:
            // `dyn Get.<Out = i32>`'s `get` answers an `i32`. One the type
            // left unpinned is not known, and stays the trait's own.
            for (name, t) in pins {
                if let Some(&adef) = self.defs.get(parent).ns.members.get(name) {
                    tys.insert(self.defs.resolve_alias(adef), t.clone());
                }
            }
        } else if let Ty::Nominal { def: head_def, .. } = &head {
            let assocs: Vec<(Symbol, DefId)> = self
                .defs
                .get(parent)
                .ns
                .members
                .iter()
                .filter(|&(_, &m)| self.defs.get(m).assoc_bounds.is_some())
                .map(|(n, &m)| (n.clone(), m))
                .collect();
            for (name, adef) in assocs {
                if let Some(&bound) = self.defs.get(*head_def).ns.members.get(&name) {
                    let bound = self.defs.resolve_alias(bound);
                    let t = self.param_ty(bound);
                    tys.insert(adef, t);
                }
            }
        }
        Some(Subst::of_types(tys))
    }

    /// The trait's own declaration of `name`, but only when it carries a
    /// **default body** — a bodyless signature is a requirement the impl must
    /// satisfy, not something callable through the impl.
    fn trait_default_method(&self, trait_def: DefId, name: &Symbol) -> Option<DefId> {
        self.decls().trait_default_method(trait_def, name)
    }

    /// Resolve `name` on a trait-object receiver (`dyn Trait` or `*dyn Trait`) to
    /// the trait's own method declaration.
    fn dyn_method_def(&mut self, recv: &Ty, name: &str) -> Option<DefId> {
        let s = self.cx.shallow(recv);
        let trait_def = match &s {
            Ty::Dyn { def: d, .. } => *d,
            Ty::Ptr { inner, .. } => match self.cx.shallow(inner) {
                Ty::Dyn { def: d, .. } => d,
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
        let repr = match self.decls().distinct_repr(def) {
            Some(t) => t,
            None => {
                let d = self.defs.get(def);
                let (file, node) = (d.file?, d.node?);
                let rhs = match &self.asts[&file].node(node).kind {
                    NodeKind::ConstBind { rhs, .. } => *rhs,
                    _ => node,
                };
                let NodeKind::DistinctType { inner, .. } = self.asts[&file].node(rhs).kind.clone()
                else {
                    return None;
                };
                self.ty_from_node_in(file, inner)
            }
        };
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
        self.infer_method_call_rebound(callee, recv, method, dispatch, args, targs, None)
    }

    /// [`Inferer::infer_method_call`], with `Self` **rebound** for a method
    /// reached through a `distinct` type's representation (§2.4).
    ///
    /// A `distinct D :: T` inherits `T`'s methods, and those methods are written
    /// in terms of `T` — so a `func (self: Self) -> Self` on `T` instantiates as
    /// `func(T) -> T`. Handing that back unchanged makes `d.dup()` a `T`, and the
    /// distinction evaporates on the first call: exactly the failure `distinct`
    /// exists to prevent, and the same one the builtin operator rows avoid by
    /// computing `Output` from the self type as written.
    ///
    /// So every occurrence of the representation in the instantiated signature
    /// becomes the distinct type. That is a substitution on types rather than on
    /// a `Self` *name* because `Self` was already resolved to the representation
    /// before this point — which also means a method that names `T` explicitly is
    /// rebound too. That is the right reading: a `distinct` *is* its
    /// representation reinterpreted, so a `T` in its own method's signature is
    /// the receiver's type, not a coincidence.
    #[allow(clippy::too_many_arguments)]
    fn infer_method_call_rebound(
        &mut self,
        callee: NodeId,
        recv: &Ty,
        method: super::def::DefId,
        dispatch: MethodDispatch,
        args: &[NodeId],
        targs: &[NodeId],
        rebind: Option<(Ty, Ty)>,
    ) -> Ty {
        let sig = self.func_def_ty(method);
        // Dispatching through a trait object or a bound reaches the trait's
        // *declaration*, whose `Self` is the trait's own nominal. For this call
        // `Self` is the receiver, so the instantiation says so rather than
        // leaving the signature claiming a bare `Trait`.
        let self_subst = self
            .trait_self_subst(callee, method, recv)
            .unwrap_or_default();
        let inst = self
            .instantiate_parts(callee, &sig, method, targs, self_subst)
            .0;
        // Everything up to and including the `self` parameter below stays in the
        // **representation's** terms when rebinding. That is not a detail: the
        // representation may itself be generic — `str` inherits from
        // `impl <T> []T` — and unifying the `self` parameter against the
        // representation is what solves that `T`. Rebinding first would leave it
        // unsolved and the call would be "type annotations needed".
        self.types.insert(callee, inst.clone());
        let Ty::Func { params, ret, .. } = self.cx.shallow(&inst) else {
            return Ty::Error;
        };
        // Bind the `self` parameter to the receiver, and record what the call
        // site has to do to the receiver expression to produce it.
        if let Some(self_param) = params.first() {
            let p = self_param.clone();
            self.unify_self_param(&p, recv);
            let adjust = self.recv_adjust(&p, recv);
            // `self_ty` is what the receiver expression must *become*, and for an
            // inherited method that is the representation: lowering casts through
            // `DistinctRecv` and the callee really does take a `*Base`. The
            // rebound `*Wrapper` is the caller's view, which is already carried
            // by the expression's own type.
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
        // From here the signature is the **caller's** view: `Self` is the
        // `distinct` type, not the representation it was written over (§2.4).
        // The impl's own generics are solved by now, so the substitution has
        // concrete types to match against.
        let (params, ret) = match &rebind {
            Some((from, to)) => {
                let params = params
                    .iter()
                    .map(|p| rebind_ty(&self.cx.resolve(p), from, to))
                    .collect::<Vec<_>>();
                let ret = Box::new(rebind_ty(&self.cx.resolve(&ret), from, to));
                (params, ret)
            }
            None => (params, ret),
        };
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
        // at the declaration, filled in by lowering, nothing to do here. A
        // closure is typed against the parameter it fills, as in a plain call:
        // `max_by({ a, b in a.cmp(b) })` knows `a` from `max_by`'s bound.
        let arg_tys: Vec<Option<Ty>> = args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                a.map(|n| self.infer_arg(n, value_params.get(i).cloned()))
            })
            .collect();
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
    ///
    /// The substitution it built comes back too, for callers that need to talk
    /// about a parameter it freshened (a static trait call needs the trait's own
    /// arguments — see [`Inferer::open_trait_self`]).
    ///
    /// `at` is the node the instantiation is recorded on: the call's callee, so
    /// that monomorphization can read this call site's generic arguments back
    /// without re-deriving them (see [`Instantiation`]).
    fn instantiate_parts(
        &mut self,
        at: NodeId,
        sig: &Ty,
        def: DefId,
        targs: &[NodeId],
        // What a trait method's `Self` and `Self.Assoc` are at this call (see
        // [`Inferer::trait_self_subst`]); empty everywhere else.
        self_subst: Subst,
    ) -> (Ty, Subst) {
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
        // The declaration's own parameters come first, in declaration order;
        // everything the signature mentions that the declaration did not list
        // follows, in first-seen order. This is the one place that order is
        // decided, and [`Generics`] is built from the same two halves — a call
        // site's arguments and the declaration's parameters line up by position
        // because they are produced here, together.
        let mut order: Vec<DefId> = params.clone();
        let mut map = self_subst;
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
            if !order.contains(&d) {
                order.push(d);
            }
            map.tys.entry(d).or_insert_with(|| self.cx.fresh());
        }
        for d in rest_consts {
            if !order.contains(&d) {
                order.push(d);
            }
            if let std::collections::hash_map::Entry::Vacant(e) = map.consts.entry(d) {
                e.insert(self.cx.fresh_const());
            }
        }
        // The same closure [`Inferer::stamp_generics`] takes, so that a call
        // site's arguments and the declaration's parameters stay one list.
        self.close_over_projections(&mut order);
        for &d in &order {
            map.tys.entry(d).or_insert_with(|| self.cx.fresh());
        }
        // Record what this call site bound each parameter to. The arguments are
        // still variables here — `id(x)`'s `T` is solved by the argument below,
        // not above — so they travel through `finalize_metas` with every other
        // mid-inference fact before lowering reads them.
        if !order.is_empty() {
            let args = order
                .iter()
                .map(|d| match map.consts.get(d) {
                    Some(k) => GenericArg::Const(k.clone()),
                    None => GenericArg::Ty(map.tys.get(d).cloned().unwrap_or(Ty::Error)),
                })
                .collect();
            self.ast.set_meta(at, Instantiation(args));
        }
        // Every associated-type parameter the signature mentions — the `Item`
        // of `T.Item` — is solved by the equation its declaration recorded:
        // `<T as Holder>.Item`. `T` is a variable here, not yet a type, so this
        // is an obligation like any other and the solver discharges it once the
        // arguments below pin `T` down. Registering it *here* is what ties the
        // two: this is the one place where the call site's `T` and the call
        // site's `T.Item` are both in hand.
        for &d in &order {
            let Some(p) = self.defs.get(d).projection.clone() else {
                continue;
            };
            let (Some(base), Some(out)) = (map.tys.get(&p.base).cloned(), map.tys.get(&d).cloned())
            else {
                continue;
            };
            self.cx.register(Obligation::Projection {
                self_ty: base,
                trait_def: p.trait_def,
                args: Vec::new(),
                assoc: p.assoc,
                out,
                origin: at,
                method: None,
            });
        }
        self.register_bounds(at, &params, &map);
        let inst = self.subst_type_params(sig, &map);
        (inst, map)
    }

    /// Read one explicit `.<...>` argument in a `const` parameter's slot, and
    /// check it against the parameter's declared type.
    fn const_arg(&mut self, param: DefId, arg: NodeId) -> Const {
        // The declared type is what the argument is read *at*: a `3` in a
        // `<const N: u8>` slot is a `u8`, and that is half of the argument's
        // identity (§5, [`ConstArg`]).
        let declared = self.const_param_ty(param);
        let repr = self.numeric_repr(&declared);
        if !matches!(declared, Ty::Error) && !repr.is_primitive() {
            let msg = format!(
                "a `const` generic parameter must have a primitive type, not `{}`",
                declared.display(self.defs)
            );
            self.report(arg, msg);
            return Const::Error;
        }
        self.const_value_in(self.file, arg, &declared, "a `const` argument", 0)
    }

    /// Whether a `const` parameter declared at `declared` may fill a slot that
    /// wants `want` — the type-level face of implicit widening.
    ///
    /// A `const` parameter has no value here (that is the point of it), so this
    /// is decided entirely on the two types, exactly as it would be for a value
    /// of the declared type reaching the same slot.
    fn const_ty_widens(&mut self, declared: &Ty, want: &Ty) -> bool {
        if self.is_ptr_sized(declared) || self.is_ptr_sized(want) {
            return false;
        }
        match (declared.int_parts(), want.int_parts()) {
            (Some(from), Some(to)) => super::ty::int_widens(from, to),
            _ => false,
        }
    }

    /// The declared type of a `<const N: T>` parameter — what `N` is worth as a
    /// value in the body, and what an explicit argument must satisfy.
    /// Check one `<const N: T>` declaration.
    ///
    /// A `const` generic is a compile-time *value* that takes part in type
    /// identity — `[3]i32` and `[4]i32` are different types (§3.2, §5) — and the
    /// declared type may be **any primitive**: an integer of any width, `bool`,
    /// `char`, a float. Restricting it to `usize` would be an arbitrary line —
    /// `<const B: bool>` and `<const C: char>` are both ordinary things to want
    /// — though the integer families `int.<N>` / `uint.<N>` (§3.1) use only the
    /// `usize` case, their width.
    ///
    /// Aggregates are not admitted. A struct or an array as a generic argument
    /// would put structural equality of arbitrary values into type identity,
    /// which is a much larger promise than comparing two primitives — so it is
    /// rejected at the declaration rather than silently degrading to a
    /// `Const::Error` at the first use.
    fn check_const_param(&mut self, node: NodeId) {
        let NodeKind::GenericConstParam { name, ty } = self.ast.node(node).kind.clone() else {
            return;
        };
        let t = self.ty_from_node(ty);
        let shallow = self.cx.shallow(&t);
        // A `distinct` over a primitive is admissible, and `usize` is one (§3.1):
        // it is `distinct uint.<PTR_BITS>`, declared in `core` rather than built
        // in. Refusing it here would make `<const N: usize>` — the declaration
        // every array length is written with — illegal.
        let repr = self.numeric_repr(&shallow);
        if repr.is_primitive() || matches!(shallow, Ty::Error) {
            return;
        }
        let msg = format!(
            "a `const` generic parameter must have a primitive type, but `{name}` is `{}`",
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
        self.decls().func_generic_param_defs(def)
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
                if args.is_empty()
                    && self.defs.get(*def).kind == DefKind::TypeParam
                    && !self.defs.get(*def).opaque
                {
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
            // `int.<N>` — the width is a `const` parameter exactly as an array
            // length is, and this is what freshens the *impl's* `N` at a call on
            // `a.wrapping_add(b)`: the method declares no generics of its own, so
            // the only place `N` is ever found is here, in its signature.
            Ty::Int {
                width: Const::Param(d),
                ..
            } if !consts.contains(d) => consts.push(*d),
            Ty::Ptr { inner, .. } | Ty::Slice { inner, .. } => {
                self.collect_generic_params(inner, out, consts)
            }
            Ty::Tuple(elems) => {
                for e in elems {
                    self.collect_generic_params(e, out, consts);
                }
            }
            Ty::Func { params, ret, .. } => {
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
                len: subst_const(len, map),
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            // `int.<N>` with `N` a `const` parameter — the same substitution an
            // array length gets, and for the same reason: this is what turns the
            // family impl's symbolic `Self` into the width the call site has.
            Ty::Int { signed, width } => Ty::Int {
                signed: *signed,
                width: subst_const(width, map),
            },
            Ty::Tuple(elems) => Ty::Tuple(
                elems
                    .iter()
                    .map(|e| self.subst_type_params(e, map))
                    .collect(),
            ),
            Ty::Func { params, ret, c } => Ty::Func {
                c: *c,
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

    /// The def whose body `func` is, when a definition owns it — `None` for a
    /// closure, which is written inside whatever body already owns it.
    fn func_owner(&self, func: NodeId) -> Option<DefId> {
        self.defs
            .iter()
            .find(|d| {
                d.kind == DefKind::Func
                    && d.file == Some(self.file)
                    && d.node.is_some_and(|n| {
                        n == func
                            || matches!(self.ast.node(n).kind, NodeKind::ConstBind { rhs, .. } if rhs == func)
                    })
            })
            .map(|d| d.id)
    }

    /// Whether a **private** member of `owner` may be named where the current
    /// expression is written (§4.4).
    ///
    /// Privacy is lexical: a private item is visible to the namespace that
    /// declares it and to everything nested inside that namespace. For a field
    /// that means the namespace the *struct* was declared in — so a function
    /// beside the struct may build one, and a method of it may read one, while
    /// another namespace may do neither.
    fn visible_here(&self, owner: DefId) -> bool {
        let Some(ctx) = self.ctx else {
            // A pass that types no expression of a program's own (impl targets,
            // a folded constant) asks nothing about privacy.
            return true;
        };
        let home = self.defs.get(owner).parent.unwrap_or(owner);
        let mut at = Some(ctx);
        while let Some(d) = at {
            if d == home || d == owner {
                return true;
            }
            at = self.defs.get(d).parent;
        }
        false
    }

    /// Whether a field is exported far enough to be named in the file being
    /// inferred: `@public` always, `@public(package)` inside its own package.
    fn field_reaches_here(&self, field: DefId) -> bool {
        let Some(pkgs) = self.pkg_of else {
            return true;
        };
        let home = self
            .defs
            .get(field)
            .file
            .and_then(|f| pkgs.get(&f))
            .map(String::as_str);
        let at = pkgs.get(&self.file).map(String::as_str);
        self.defs.get(field).vis.reaches(home, at)
    }

    /// Report a field named from outside the namespace that declares its struct.
    ///
    /// One message for every way of naming one — a read, an assignment, a
    /// literal that initializes it, a pattern that binds it — because they are
    /// one rule, and the fix is the same: `@public(all)` on the struct, or
    /// `@public` on the field.
    fn check_field_visible(&mut self, at: NodeId, owner: DefId, field: DefId) {
        if self.field_reaches_here(field) || self.visible_here(owner) {
            return;
        }
        let (f, t) = (
            self.defs.get(field).name.clone(),
            self.defs.get(owner).name.clone(),
        );
        self.report_with_note(
            at,
            format!("the field `{f}` of `{t}` is private"),
            format!(
                "a field is private unless `{t}` says otherwise — `@public(all)`, \
                 `@public(fields: package)` — or the field itself is `@public` / \
                 `@public(package)`"
            ),
        );
    }

    /// The type of a path expression from the def it resolved to.
    fn path_ty(&mut self, node: NodeId) -> Ty {
        match self.resolved_def(node) {
            Some(def) => self.def_ty(node, def),
            None => Ty::Error,
        }
    }

    /// The type a value-position reference to `def` has: a local/param from the
    /// environment, a function from its signature, a type used as a constructor
    /// value as its nominal type.
    fn def_ty(&mut self, node: NodeId, def: super::def::DefId) -> Ty {
        if let Some(ty) = self.env.get(&def) {
            return ty.clone();
        }
        match self.defs.get(def).kind {
            DefKind::Func => {
                // A variadic signature has no function-pointer type: the tail
                // lives in the calling convention, and a `Ty::Func` says nothing
                // about it — so a pointer taken here would be indistinguishable
                // from one to the fixed-arity function of the same parameters,
                // and calling through it would use the wrong convention with
                // nothing to notice. The call form is the only form.
                if self.defs.get(def).is_c_variadic() {
                    self.report_with_note(
                        node,
                        format!(
                            "`{}` is `#c_vararg` and can only be called, not used as a value",
                            self.defs.get(def).name
                        ),
                        "a variadic tail is part of the calling convention, and a function \
                         pointer does not carry one"
                            .to_string(),
                    );
                    return Ty::Error;
                }
                self.func_def_ty(def)
            }
            // An overload set is a name for several functions and not a value of
            // its own: which function it stands for is what a call's arguments
            // decide, and a binding has no arguments to decide with (§4.3).
            DefKind::Overload => {
                let name = self.defs.get(def).name.clone();
                self.report_with_note(
                    node,
                    format!("`{name}` is an overload set, which can only be called"),
                    "which of its functions is meant is decided by the arguments of a call; \
                     name the one you want instead"
                        .to_string(),
                );
                Ty::Error
            }
            DefKind::Struct | DefKind::Enum => self.nominal_of(def),
            DefKind::Const => self.const_def_ty(node, def),
            // `<const N: usize>` names a value in the body, of the type it was
            // declared with (§5).
            DefKind::ConstParam => self.const_param_ty(def),
            // Something from an import that failed to load: already reported,
            // and whatever it is, nothing more can be known about it.
            DefKind::External => Ty::Error,
            // A namespace or a trait has no value to take. A fresh variable
            // here unifies with whatever the slot wants, so `let p: *mut T :=
            // mem` would type-check silently and reach code generation as
            // nothing at all.
            kind @ (DefKind::Namespace | DefKind::Trait) => {
                let name = self.written_path_in(self.file, node, def);
                self.report(node, format!("`{name}` is a {}, not a value", kind.label()));
                Ty::Error
            }
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

    /// Settle an open numeric literal on its default, so a method call on one
    /// looks the impls up against a real type. See [`Cx::pin_numeric`].
    fn pin_numeric(&mut self, ty: &Ty) -> Ty {
        self.cx.pin_numeric(ty)
    }

    /// The exact literal a path names, when it resolves to a constant bound to
    /// one (following a chain of such constants).
    ///
    /// Only `Int` and `Float` come back: they are the two whose value has to
    /// travel to the use site, because they are the two that settle on a
    /// *width* there. A string literal is open too, but every type it may
    /// become holds it, so there is nothing to check.
    fn const_lit_value(&self, node: NodeId) -> Option<Lit> {
        self.decls().const_lit_value(self.file, node)
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
        // `isize` / `usize` are `distinct` declarations (§3.1), so the test is
        // against what the type *stands over* — otherwise the commonest
        // conversion in the language, a bare integer literal settling on
        // `isize`, would stop being recorded and the `$cast` would vanish from
        // the IR.
        let converts = match comptime {
            Ty::ComptimeStr => self.cx.admits_str(&resolved),
            // `usize` / `isize` are `distinct` and so not `Ty::Int`, but they
            // are the numeric core: a bare integer literal settling on `isize`
            // is the commonest conversion in the language and the `$cast` has
            // to stay in the IR. A *user* `distinct` is deliberately not
            // included — a literal reaches one directly (§2.4), which is what
            // the existing lowering records.
            _ => matches!(resolved, Ty::Int { .. } | Ty::Float(_)) || self.is_ptr_sized(&resolved),
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
        // A `distinct` numeric is checked against what it stands over: §2.4
        // lets a literal reach one with no written cast, so this is the only
        // place `70000` meeting a `distinct u16` is caught.
        // A symbolic `int.<N, S>` has no range to check the literal against
        // until monomorphization picks its width, so there is nothing to say
        // here — and nothing to reach it today, since a literal can only settle
        // on a width the call site already fixed.
        let settled = self.numeric_repr(resolved);
        let Some((signed, bits)) = settled.int_parts() else {
            return;
        };
        if super::ty::int_fits(&value, signed, bits) {
            return;
        }
        let msg = format!(
            "the literal `{value}` does not fit in `{}`",
            resolved.display(self.defs)
        );
        self.report(node, msg);
        // The const evaluator checks the same conversion again, on the `$cast`
        // this node lowers to. Mark the node so it does not say it twice.
        self.ast.set_meta(node, RangeReported);
    }

    /// `usize` — the `#lang("usize")` declaration in `core` (§3.1).
    ///
    /// [`Ty::Error`] when there is none. That is not a silent failure: a program
    /// without a `core` has already been told so, and every use of the result
    /// here is a slot that accepts an error type without cascading.
    fn usize_ty(&self) -> Ty {
        self.cx.usize_ty().unwrap_or(Ty::Error)
    }

    /// Whether `ty` is one of the two pointer-sized `distinct`s.
    fn is_ptr_sized(&self, ty: &Ty) -> bool {
        is_ptr_sized(self.lang, self.defs, ty)
    }

    /// The primitive a settled type is checked against: itself, or — for a
    /// `distinct` numeric — the primitive it stands over (§2.4).
    fn numeric_repr(&self, ty: &Ty) -> Ty {
        self.cx
            .numeric_distinct_repr(ty)
            .unwrap_or_else(|| ty.clone())
    }

    /// The float counterpart: reject a `comptime_float` the type it settled on
    /// cannot hold at all — one that overflows to infinity, or a non-zero one
    /// that underflows to zero (§1.5, [`super::ty::float_fits`]).
    ///
    /// Ordinary rounding is *not* an error, and cannot be: `0.1` is not exactly
    /// an `f64` either. What is rejected is the conversion that loses the
    /// number entirely, because nothing in the source asked for `3.5e40` to
    /// become `inf`. A written `$cast.<f32>(x)` still may.
    fn check_float_range(&mut self, node: NodeId, resolved: &Ty) {
        let Some(value) = self.float_values.get(&node).copied() else {
            return;
        };
        let settled = self.numeric_repr(resolved);
        let Ty::Float(width) = &settled else {
            return;
        };
        if super::ty::float_fits(value, *width) {
            return;
        }
        let msg = format!(
            "the literal `{value:?}` does not fit in `{}`",
            resolved.display(self.defs)
        );
        self.report(node, msg);
        self.ast.set_meta(node, RangeReported);
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
    /// - `#static c: u32 :: 0`, `MAX: i32 :: 100` — the **type is declared**
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

    fn const_def_ty(&mut self, node: NodeId, def: DefId) -> Ty {
        // A constant defined in terms of itself has no type; break the cycle
        // rather than recursing forever.
        if self.const_stack.contains(&def) {
            return Ty::Error;
        }
        // The recorded shape first, always: a constant in another package has
        // one and no syntax, and while syntax still travels a tree-first read
        // would mean the recorded answer was never exercised here.
        if let Some(shape) = self.decls().const_ty(def) {
            self.const_stack.push(def);
            let ty = self.const_shape_ty(node, shape);
            self.const_stack.pop();
            return ty;
        }
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return self.cx.fresh();
        };
        let NodeKind::ConstBind { rhs, .. } = self.asts[&file].node(node).kind.clone() else {
            return self.cx.fresh();
        };
        self.const_stack.push(def);
        let ty = self.const_rhs_ty(file, node, rhs);
        self.const_stack.pop();
        ty
    }

    /// The type a use of a constant has, built from the shape recorded for it.
    ///
    /// The shape rather than a type because a comptime literal has no single
    /// one: each use gets its own variable and settles it, which is what this
    /// allocates. `node` is the use, and is where a diagnostic about the
    /// constant `Same` names would be anchored.
    fn const_shape_ty(&mut self, node: NodeId, shape: ConstTy) -> Ty {
        match shape {
            // No `#lang("str")` item leaves a string literal nothing to become
            // and nothing to unify with — a broken `core`, reported as such.
            ConstTy::Comptime(TyVarKind::Str) if self.lang.get("str").is_none() => Ty::Error,
            ConstTy::Comptime(kind) => self.cx.fresh_of(kind),
            ConstTy::Same(other) => self.def_ty(node, other),
            ConstTy::Settled(ty) => ty,
        }
    }

    /// The type a constant's right-hand side gives it, read in the file the
    /// constant was declared in.
    fn const_rhs_ty(&mut self, file: FileId, node: NodeId, rhs: NodeId) -> Ty {
        match self.asts[&file].node(rhs).kind.clone() {
            // Literals stay comptime: a fresh variable per use, so one use of
            // `A :: "hi"` may be a `str` and another a `[]u8`, exactly as one
            // use of `N :: 1` may be an `i8` and another an `i64`.
            NodeKind::Lit(Lit::Int(_)) => self.cx.fresh_of(TyVarKind::Int),
            NodeKind::Lit(Lit::Float(_)) => self.cx.fresh_of(TyVarKind::Float),
            NodeKind::Lit(l) => self.lit_ty(&l),
            // `-1` / `+1` are still literals for this purpose.
            NodeKind::Unary { operand, .. } => self.const_rhs_ty(file, node, operand),
            // A constant naming another constant inherits its comptime-ness.
            NodeKind::Path { .. } => match self.resolved_def_in(file, rhs) {
                Some(d) => self.def_ty(rhs, d),
                None => Ty::Error,
            },
            // `#static count: u32 :: 0` (§2.6) and an associated constant
            // share this RHS shape: a **declared type** with an optional `:=`
            // initializer. The type is written, so there is nothing to infer
            // from the initializer — and for a static there may be no
            // initializer at all, the region being zeroed. Reading the
            // annotation is also what keeps a static's type concrete: a global
            // is storage, and storage cannot be `comptime_int`.
            NodeKind::AssocConst { ty, .. } => self.ty_from_node_in(file, ty),
            // Anything else has a concrete type; infer it where it is written.
            _ if file == self.file => self.infer_expr(rhs),
            // Another file's, and the recorded shape could not answer: it is
            // recorded only once **every** file has been inferred
            // (`decl::record_types`), and a use site asks during. Inference of
            // that file has already run — files are inferred in dependency
            // order — so what it stamped on the declaration is the same answer,
            // one pass earlier. This is the route `u8.MAX` takes: `core`'s
            // `MAX` is a `cast`, not a literal, and a fresh variable here would
            // leave every read of it needing an annotation.
            //
            // Only what is settled. A type still carrying a variable is a
            // question that file did not answer either, and a generic impl's
            // own parameters are not variables — `uint.<N>` arrives rigid and
            // the caller substitutes it.
            _ => self
                .asts
                .get(&file)
                .and_then(|a| a.meta::<Ty>(node))
                .filter(|t| !t.mentions_var() && !t.mentions_error())
                .unwrap_or_else(|| self.cx.fresh()),
        }
    }

    /// Build the [`Ty::Func`] of a function def from its signature.
    fn func_def_ty(&mut self, def: super::def::DefId) -> Ty {
        if let Some(sig) = self.decls().signature(def) {
            return sig;
        }
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return self.cx.fresh();
        };
        // The def's node is the `ConstBind`; its RHS is the `FuncExpr`.
        let Some(ast) = self.asts.get(&file) else {
            return self.cx.fresh();
        };
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

    /// Whether the `FuncExpr` at `func`, in this file, has a C ABI — which makes
    /// its value a C function pointer rather than a Nest function value (§3.5).
    fn is_extern_func(&self, func: NodeId) -> bool {
        matches!(
            &self.ast.node(func).kind,
            NodeKind::FuncExpr {
                extern_abi: Some(_),
                ..
            }
        )
    }

    /// The signature of a function **this pass has just inferred**, read back
    /// from what it recorded rather than resolved a second time.
    ///
    /// [`Inferer::func_sig_ty`] exists for a signature nothing has looked at
    /// yet — a callee in another file — and resolving one is not a free
    /// operation: `ty_from_node` *reports*, so an array length that does not fit
    /// a `usize` produces a diagnostic every time its type node is walked. For
    /// the function being inferred, the answer is already in hand — each
    /// parameter's type was recorded on its own node and the return type on the
    /// `FuncExpr`'s — so asking again is both slower and, for exactly the two
    /// positions a signature has, a duplicate diagnostic.
    fn inferred_sig_ty(&mut self, func: NodeId) -> Ty {
        let NodeKind::FuncExpr { params, .. } = self.ast.node(func).kind.clone() else {
            return self.cx.fresh();
        };
        let params = params
            .iter()
            .map(|p| match self.types.get(p) {
                Some(t) => t.clone(),
                // A parameter whose node carries no type is one `infer_func`
                // never reached, which today means the tree was already in
                // error. A variable stands in; it mentions no generic, which is
                // the only question being asked here.
                None => self.cx.fresh(),
            })
            .collect();
        let ret = self.types.get(&func).cloned().unwrap_or(Ty::Void);
        Ty::Func {
            params,
            ret: Box::new(ret),
            c: self.is_extern_func(func),
        }
    }

    /// The signature type of a `FuncExpr` living in `file`.
    fn func_sig_ty_in(&mut self, file: FileId, func: NodeId) -> Ty {
        let ast = &self.asts[&file];
        let NodeKind::FuncExpr {
            params,
            ret,
            extern_abi,
            ..
        } = ast.node(func).kind.clone()
        else {
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
            c: extern_abi.is_some(),
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
        self.decls().generic_arity(def)
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
        self.decls().type_param_defs(def)
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

    // ===< field access >===

    /// Type `#caller_location` (§5.2).
    ///
    /// It is only ever a **default argument**. A default is filled in at the
    /// call site, which is exactly what makes this name the caller rather than
    /// the declaration; written anywhere else it could only mean "the position
    /// of this expression", which is a different thing and one nothing asked
    /// for. Refusing it there keeps the one meaning it has.
    fn infer_caller_location(&mut self, node: NodeId) -> Ty {
        if !self.in_default {
            self.report(
                node,
                "`#caller_location` is only a default argument — write it as \
                 `loc: Location := #caller_location` on the parameter that receives it",
            );
            return Ty::Error;
        }
        match location_lang_ty(self.defs, self.lang) {
            Some(t) => t,
            None => {
                self.report(
                    node,
                    "`#caller_location` requires the `#lang(\"location\")` item",
                );
                Ty::Error
            }
        }
    }

    /// Check that every member an impl supplies has the type its trait declared
    /// (§4.1: "the compiler checks every required method is present with a
    /// matching signature").
    ///
    /// Presence is `impls::build`'s job; this is the *matching* half, and it has
    /// to live here because it is a question about types. The trait's
    /// declarations are written in terms of `Self` and the trait's own generic
    /// parameters, so both sides are substituted into the impl's world first:
    /// `Self` becomes the impl's self type, `Rhs` becomes what the impl wrote
    /// for it, and the impl's own generics become fresh variables so
    /// `impl <T> Add for Wrap.<T>` compares as the family it is.
    fn check_impl_conformance(&mut self, i: usize) {
        let imp = self.impls.impls[i].clone();
        let Some(trait_def) = imp.trait_def else {
            return;
        };
        let trait_def = self.defs.resolve_alias(trait_def);
        let mut map = self.fresh_impl_map(&imp.generics);
        let self_ty = self.impl_self_ty(&imp, &map);
        // `Self` inside a trait resolves to the trait itself, so substituting the
        // trait's def is what replaces it.
        map.tys.insert(trait_def, self_ty.clone());
        // `impl Add.<i32> for V` — the trait's own parameters take what the impl
        // wrote for them.
        let trait_generics = self.trait_generic_param_defs(trait_def);
        for (idx, &g) in trait_generics.iter().enumerate() {
            let t = match self.impl_trait_args(&imp).get(idx) {
                Some(t) => {
                    let t = t.clone();
                    self.subst_type_params(&t, &map)
                }
                // A bare `impl Add for Vec3` names no arguments, which leaves
                // the trait's parameters for the impl's own members to decide:
                // `Rhs` is whatever `add` takes. A fresh variable is exactly
                // that — free, and pinned by the first member that mentions it.
                None => self.cx.fresh(),
            };
            map.tys.insert(g, t);
        }
        // And each abstract associated type takes what this impl bound it to.
        // The trait declares `get` as returning `Self.Item`, which is kept as
        // the associated type itself (see [`Inferer::self_assoc_ty`]) precisely
        // so that it can be substituted — and here is where the impl's own
        // `Item :: i32` becomes the answer. Without it the requirement and the
        // member disagree on every signature that mentions one.
        let assocs: Vec<(Symbol, DefId)> = self
            .defs
            .get(trait_def)
            .ns
            .members
            .iter()
            .filter(|&(_, &m)| self.defs.get(m).assoc_bounds.is_some())
            .map(|(n, &m)| (n.clone(), m))
            .collect();
        for (name, adef) in assocs {
            let t = match self.impl_assoc(&imp, &name) {
                Some(t) => self.subst_type_params(&t, &map),
                // An impl that does not bind it is incomplete, reported as such;
                // a variable keeps this check from adding a second complaint.
                None => self.cx.fresh(),
            };
            map.tys.insert(adef, t);
        }

        let members: Vec<(Symbol, DefId)> = self
            .defs
            .get(trait_def)
            .ns
            .members
            .iter()
            .map(|(n, &d)| (n.clone(), d))
            .collect();
        for (name, required) in members {
            let Some(&supplied) = imp.members.get(&name) else {
                // Absent: already reported as incomplete, or defaulted by the
                // trait, in which case there is nothing of the impl's to check.
                continue;
            };
            let want = match self.defs.get(required).kind {
                DefKind::Func => self.func_def_ty(required),
                // An associated **type** is what the impl is for — it supplies
                // the answer, so there is nothing to hold it to. An associated
                // constant declares a type, and that one is checkable.
                DefKind::Const => match self.declared_member_ty(required) {
                    Some(t) => t,
                    None => continue,
                },
                _ => continue,
            };
            let got = match self.defs.get(supplied).kind {
                DefKind::Func => self.func_def_ty(supplied),
                DefKind::Const => match self.declared_member_ty(supplied) {
                    // An impl that leaves the type out (`MAX :: 100`) has
                    // nothing to disagree with; its *value* is checked against
                    // the requirement wherever it is read.
                    Some(t) => t,
                    None => continue,
                },
                _ => continue,
            };
            // A method may have generics of its own, and the trait's `X` and the
            // impl's `X` are different defs however alike they read. Align them
            // positionally onto one fresh variable each, or two signatures that
            // are the same signature fail to unify.
            let mut map = map.clone();
            if self.defs.get(required).kind == DefKind::Func {
                let want_gs = self.func_generic_param_defs(required);
                let got_gs = self.func_generic_param_defs(supplied);
                for (idx, &wg) in want_gs.iter().enumerate() {
                    let is_const = self.defs.get(wg).kind == DefKind::ConstParam;
                    match (is_const, got_gs.get(idx)) {
                        // Onto the *trait's* parameter, not a fresh variable:
                        // both sides then agree, and a mismatch elsewhere in the
                        // signature reads `func(*S, X) -> bool` rather than
                        // `func(*S, ?7) -> bool`.
                        (false, Some(&gg)) => {
                            map.tys.insert(
                                gg,
                                Ty::Nominal {
                                    def: wg,
                                    args: Vec::new(),
                                },
                            );
                        }
                        (true, Some(&gg)) => {
                            map.consts.insert(gg, Const::Param(wg));
                        }
                        // Different arity: the signatures disagree, and saying
                        // so with the parameters left as written reads better
                        // than a message full of `?3`.
                        (_, None) => {}
                    }
                }
            }
            let want = self.subst_type_params(&want, &map);
            let got = self.subst_type_params(&got, &map);
            // Compare in a snapshot: this is a question, and answering it should
            // not bind anything the rest of the file then sees.
            let snapshot = self.cx.snapshot();
            let agree = self.cx.unify(&want, &got).is_ok();
            self.cx.rollback(snapshot);
            if agree {
                continue;
            }
            let msg = format!(
                "`{}` does not match the declaration in `{}`: expected `{}`, found `{}`",
                name,
                self.defs.canonical_string(trait_def),
                signature_text(&self.cx.resolve(&want).display(self.defs)),
                signature_text(&self.cx.resolve(&got).display(self.defs)),
            );
            // The member's own node, or the impl's target as a fallback.
            // Conformance is checked where the impl is *written*, so there is
            // always one of the two.
            let at = self
                .defs
                .get(supplied)
                .node
                .or_else(|| imp.syntax.as_ref().map(|sx| sx.self_node));
            if let Some(at) = at {
                self.report_in(imp.file, at, msg);
            }
        }
    }

    /// The type written on a member whose declaration carries one — an
    /// associated constant's `MAX: i32`. `None` when the member declares no
    /// type (an associated-type binding, or an impl constant that leaves it out).
    fn declared_member_ty(&mut self, def: DefId) -> Option<Ty> {
        if let Some(t) = self.decls().assoc_const_ty(def) {
            return Some(t);
        }
        let d = self.defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let NodeKind::ConstBind { rhs, .. } = self.asts[&file].node(node).kind.clone() else {
            return None;
        };
        match self.asts[&file].node(rhs).kind.clone() {
            NodeKind::AssocConst { ty, .. } => Some(self.ty_from_node_in(file, ty)),
            _ => None,
        }
    }

    /// A trait's declared generic parameters, in source order.
    fn trait_generic_param_defs(&self, trait_def: DefId) -> Vec<DefId> {
        self.decls().trait_generic_param_defs(trait_def)
    }

    /// Apply the one rule an intrinsic needs beyond its declared signature
    /// (§6.4), if `def` is one and it has such a rule.
    ///
    /// The list is short on purpose — see [`super::intrinsics`]. A call to an
    /// intrinsic is an ordinary call against an ordinary signature declared in
    /// `core`, and the whole value of declaring them there is lost if each one
    /// grows a special case here instead.
    fn intrinsic_result(&mut self, def: DefId, result: Ty, args: &[Option<NodeId>]) -> Ty {
        let Some(tag) = self.defs.get(def).intrinsic_tag() else {
            return result;
        };
        match super::intrinsics::lookup(tag.as_str()).and_then(|r| r.special) {
            // `make.<[]T>(n)` yields `[]mut T`: the type argument is the shape
            // and the mutability is what the allocation adds.
            Some(super::intrinsics::Special::MutableArg) => match self.cx.shallow(&result) {
                Ty::Slice { inner, .. } => Ty::Slice {
                    mutable: true,
                    inner,
                },
                Ty::Ptr { inner, .. } => Ty::Ptr {
                    mutable: true,
                    inner,
                },
                other => other,
            },
            // `len(x)` takes an array or a slice, which no bound in the language
            // says. The declared `T` is unbounded, so the check is here.
            Some(super::intrinsics::Special::SequenceArg) => {
                if let [Some(arg)] = args {
                    let arg_ty = self.node_ty(*arg);
                    let ty = self.autoderef(&arg_ty);
                    // An unsolved argument is not yet wrong; a `[N]T` / `[]T` is
                    // right.
                    if !has_len(&ty) && !matches!(ty, Ty::Error) && !is_var(&ty) {
                        let msg = format!(
                            "`len` needs an array or a slice, not `{}`",
                            self.cx.resolve(&ty).display(self.defs)
                        );
                        self.report(*arg, msg);
                    }
                }
                result
            }
            None => result,
        }
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
        self.field_ty_at(None, base, name)
    }

    /// The same, for a field the **program wrote** at `at`: its privacy is
    /// checked there (§4.4). `field_ty` is the form for the compiler's own
    /// lookups — an `@using` upcast, a structural conversion — which name no
    /// field on the program's behalf and so check nothing.
    fn field_ty_at(&mut self, at: Option<NodeId>, base: &Ty, name: &str) -> Option<Ty> {
        let base = self.autoderef(base);
        // An anonymous struct carries its fields in the type: there is no def
        // to look up and no generics to substitute.
        if let Ty::Struct(fields) = &base {
            return fields
                .iter()
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, t)| t.clone());
        }
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
        if let Some(at) = at {
            self.check_field_visible(at, def, field);
        }
        let t = match self.decls().field_ty(field) {
            Some(t) => t,
            None => {
                let d = self.defs.get(field);
                let (file, node) = (d.file?, d.node?);
                match self.asts[&file].node(node).kind.clone() {
                    NodeKind::Field { ty, .. } => self.ty_from_node_in(file, ty),
                    // A tuple struct's field def points straight at the
                    // positional type node: there is no `Field` node wrapping
                    // it (see `collect_struct`).
                    _ => self.ty_from_node_in(file, node),
                }
            }
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
        let map = self.nominal_subst(def, &args);
        if let Some(payload) = self.decls().variant_payload(variant) {
            return Some(
                payload
                    .into_iter()
                    .map(|(name, t)| (name, self.subst_type_params(&t, &map)))
                    .collect(),
            );
        }
        let vd = self.defs.get(variant);
        let (file, node) = (vd.file?, vd.node?);
        let payload = match &self.asts[&file].node(node).kind {
            NodeKind::Variant { payload, .. } => payload.clone(),
            _ => return None,
        };
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
        let map = self.nominal_subst(def, &args);
        if let Some(tys) = self.decls().tuple_tys(def) {
            return Some(
                tys.iter()
                    .map(|t| self.subst_type_params(t, &map))
                    .collect(),
            );
        }
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
        if matches!(self.cx.resolve(ty), Ty::Error) {
            self.bind_pattern_to_error(pat);
            return;
        }
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
    /// Bind every name in `pat` to [`Ty::Error`]: the value it destructures is
    /// already an error, so nothing is known about its parts, and leaving them
    /// as fresh variables would report each one as "type annotations needed".
    fn bind_pattern_to_error(&mut self, pat: NodeId) {
        let kind = self.ast.node(pat).kind.clone();
        let names = matches!(
            kind,
            NodeKind::BindingPat { .. } | NodeKind::AtPat { .. } | NodeKind::FieldPat { .. }
        );
        if names {
            if let Some(def) = self.def_of(pat) {
                self.env.insert(def, Ty::Error);
            }
        }
        for child in kind.children() {
            self.bind_pattern_to_error(child);
        }
    }

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

    /// Put the trait's **own** parameters back where `Self` left variables.
    ///
    /// `Self` inside a trait's declaration resolves to the trait, and naming a
    /// generic type with nothing applied to it yields a fresh variable per
    /// parameter — the right answer at a *use*, where context decides them.
    /// This is a declaration, and its parameters are decided: they are the
    /// trait's own. A signature recorded with variables in it would carry the
    /// numbering of the context that made them, which means nothing in the
    /// context that reads it back (see [`super::decl::record_types`]).
    ///
    /// Only arguments that *are* variables are replaced: a method that writes
    /// the trait out with arguments of its own said what it meant.
    fn rigid_self(&mut self, ty: Ty, trait_def: DefId) -> Ty {
        let params: Vec<Ty> = self
            .trait_generic_param_defs(trait_def)
            .into_iter()
            .map(|g| Ty::Nominal {
                def: g,
                args: Vec::new(),
            })
            .collect();
        let ty = self.cx.resolve(&ty);
        rigid_self_in(&ty, trait_def, &params)
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
        let trait_methods: Vec<(DefId, DefId)> = self
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Trait && d.file == Some(self.file))
            .flat_map(|d| d.ns.members.values().map(move |&m| (d.id, m)))
            .filter(|&(_, m)| self.defs.get(m).kind == DefKind::Func)
            .collect();
        for (trait_def, m) in trait_methods {
            let ty = self.func_def_ty(m);
            let ty = self.rigid_self(ty, trait_def);
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

        // A **bodyless** function is never typed by the per-function passes
        // either, and not every one of them is a trait method: an
        // `#intrinsic` and an `extern("c")` declaration are signatures with
        // nothing to infer. Their signature is still what a caller unifies
        // against, so it is worked out here, once, like a trait method's.
        let bodyless: Vec<DefId> = self
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Func && d.file == Some(self.file))
            .filter(|d| !self.decls().has_body(d.id))
            .map(|d| d.id)
            .collect();
        for m in bodyless {
            let Some((file, func)) = self.decls().func(m) else {
                continue;
            };
            if file != self.file || self.ast.meta::<super::Signature>(func).is_some() {
                continue;
            }
            let ty = self.func_def_ty(m);
            let ty = self.cx.resolve(&ty);
            self.ast.set_meta(func, super::Signature(ty));
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

    /// The tag every variant of every enum this file declares stores, stamped on
    /// the variant's own node as a [`VariantTag`] (§3.3).
    ///
    /// A variant with no `= value` takes one more than the variant before it,
    /// counting from zero — so an enum nobody wrote a discriminant on gets its
    /// positions, exactly as it did before there was a discriminant to write.
    ///
    /// Only a variant with **no payload** may have one. A discriminant exists so
    /// that an enum can be given C's numbering, and a C enumeration has no
    /// payload to number; a tagged union whose tag the program chose would be a
    /// different feature with different rules.
    fn stamp_variant_tags(&mut self) {
        use crate::parser::ast::VariantPayload;
        let enums: Vec<Vec<NodeId>> = self
            .ast
            .ids()
            .filter_map(|id| match &self.ast.node(id).kind {
                NodeKind::EnumType { variants, .. } => Some(variants.clone()),
                _ => None,
            })
            .collect();
        for variants in enums {
            // Where the implicit numbering has got to, and what has been used —
            // two variants sharing a tag are two variants a `match` cannot tell
            // apart, so the enum is refused rather than compiled into one that
            // loses values.
            let mut next: i128 = 0;
            let mut seen: Vec<(i128, Symbol)> = Vec::new();
            for v in variants {
                let NodeKind::Variant {
                    name,
                    payload,
                    value,
                    ..
                } = self.ast.node(v).kind.clone()
                else {
                    continue;
                };
                let tag = match value {
                    Some(expr) if !matches!(payload, VariantPayload::None) => {
                        let msg = format!(
                            "`{name}` carries a payload, so it may not be given an \
                             explicit discriminant"
                        );
                        self.report_in(self.file, expr, msg);
                        next
                    }
                    Some(expr) => self.variant_tag_value(expr).unwrap_or(next),
                    None => next,
                };
                if let Some((_, other)) = seen.iter().find(|(t, _)| *t == tag) {
                    let msg = format!("the discriminant {tag} is already `{other}`'s");
                    self.report_in(self.file, v, msg);
                }
                seen.push((tag, name));
                self.ast.set_meta(v, VariantTag(tag));
                // A tag at the very top of the range has no successor to give
                // the next variant. Saying so where the *next* variant is
                // written would be a diagnostic about the wrong line, so the
                // count saturates and the variant that has no room is the one
                // that reports.
                next = match tag.checked_add(1) {
                    Some(n) => n,
                    None => {
                        let msg = "the discriminant after this one would not fit in 128 bits";
                        self.report_in(self.file, v, msg.to_string());
                        tag
                    }
                };
            }
        }
    }

    /// One written discriminant, folded to the integer it is.
    ///
    /// The same fold every other compile-time value in a declaration goes
    /// through ([`Self::const_operand`]), so a named constant and `1 << 3` are
    /// each as good as a literal. `None` means a diagnostic was reported.
    fn variant_tag_value(&mut self, expr: NodeId) -> Option<i128> {
        let what = "an enum discriminant";
        let value = self.const_operand(self.file, expr, what, 0)?;
        match &value {
            ConstValue::Int(n) => match n.to_i128() {
                Some(n) => Some(n),
                None => {
                    let msg = format!("{what} must fit in 128 bits, and {n} does not");
                    self.report_const_in(self.file, expr, msg);
                    None
                }
            },
            other => {
                let msg = format!("{what} must be an integer, not `{}`", other.display());
                self.report_const_in(self.file, expr, msg);
                None
            }
        }
    }

    fn ty_from_node_in(&mut self, file: FileId, node: NodeId) -> Ty {
        let ast = &self.asts[&file];
        match ast.node(node).kind.clone() {
            NodeKind::TypeHole => self.cx.fresh(),
            NodeKind::ImplType { .. } => {
                self.report_in(
                    file,
                    node,
                    "`impl` is written as a parameter's type or as the return type (§5.4)",
                );
                Ty::Error
            }
            // An `impl` return type (§5.4): the type parameter resolution bound
            // in the return slot, over the function's own parameters.
            NodeKind::GenericTypeParam { .. } => {
                let ast = &self.asts[&file];
                match ast.meta::<DefMeta>(node) {
                    Some(DefMeta(def)) => Ty::Nominal {
                        def,
                        args: ast
                            .meta::<super::OpaqueArgs>(node)
                            .map(|a| a.0)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|p| Ty::Nominal {
                                def: p,
                                args: Vec::new(),
                            })
                            .collect(),
                    },
                    None => Ty::Error,
                }
            }
            // `*func(...)` is the function pointer, which is one type rather
            // than a pointer to something: there is no function value to point
            // at on its own (§3.5).
            NodeKind::PtrType { inner, .. }
                if matches!(ast.node(inner).kind, NodeKind::FuncType { .. }) =>
            {
                self.ty_from_node_in(file, inner)
            }
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
            NodeKind::FuncType {
                params,
                ret,
                extern_abi,
                ..
            } => Ty::Func {
                c: extern_abi.is_some(),
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
                Some(def) => {
                    let written = match self.asts[&file].node(inner).kind.clone() {
                        NodeKind::TypePath { generic_args, .. } => generic_args,
                        _ => Vec::new(),
                    };
                    let mut assoc = Vec::new();
                    for a in written {
                        match self.asts[&file].node(a).kind.clone() {
                            NodeKind::AssocBinding { name, ty } => {
                                assoc.push((name, self.ty_from_node_in(file, ty)));
                            }
                            _ => self.report_in(
                                file,
                                a,
                                "a trait object pins associated types (`dyn T.<Item = i32>`) and \
                                 takes no other arguments",
                            ),
                        }
                    }
                    assoc.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
                    Ty::Dyn { def, assoc }
                }
                None => Ty::Error,
            },
            // An anonymous `struct { ... }` written inline (§3.8). It is not a
            // declaration and gets no def: the fields *are* the type.
            NodeKind::StructType {
                generics, kind: sk, ..
            } => self.anon_struct_ty(file, node, &generics, &sk),
            NodeKind::DistinctType { inner, .. } => self.ty_from_node_in(file, inner),
            NodeKind::TypePath { generic_args, .. } => self.typepath_ty(file, node, &generic_args),
            // `Type.<args>` in expression position (e.g. a composite-literal head)
            // parses as a postfix generic application; resolve it like a typepath
            // whose head is the base.
            NodeKind::GenericApply { base, args } => self.typepath_ty(file, base, &args),
            NodeKind::Path { .. } => self.typepath_ty(file, node, &[]),
            // `ns.P { ... }` — a composite literal's head is parsed as an
            // **expression** (§ the tuple-struct form is a `Call`), so a
            // qualified type name reaches here as a member access rather than a
            // `Path`. Name resolution has already stamped the type's def on the
            // node, which is all `typepath_ty` reads, so the two spellings end
            // at the same type instead of one of them being a silent error.
            NodeKind::FieldAccess { .. } => self.typepath_ty(file, node, &[]),
            // `(A, B)` on the right of a `::`, which parses as a tuple value.
            NodeKind::Tuple { elems } if elems.is_empty() => Ty::Void,
            NodeKind::Tuple { elems } => Ty::Tuple(
                elems
                    .iter()
                    .map(|&e| self.ty_from_node_in(file, e))
                    .collect(),
            ),
            _ => Ty::Error,
        }
    }

    /// The type of an anonymous `struct { ... }` in a type position (§3.8).
    ///
    /// Everything a *declaration* may carry is refused here rather than
    /// ignored. An anonymous struct has no name to be generic over and no
    /// declaration order to promise, so generics and the tuple form have
    /// nowhere to go: `struct (A, B)` inline is the tuple `(A, B)` spelled
    /// wrong, and saying so is better than laying out something the program did
    /// not ask for.
    fn anon_struct_ty(
        &mut self,
        file: FileId,
        node: NodeId,
        generics: &[NodeId],
        kind: &StructKind,
    ) -> Ty {
        if let Some(&g) = generics.first() {
            self.report_in(
                file,
                g,
                "an anonymous `struct` cannot be generic: only a declared type takes parameters"
                    .to_string(),
            );
            return Ty::Error;
        }
        let members = match kind {
            StructKind::Record(ids) => ids.as_slice(),
            StructKind::Unit => &[],
            StructKind::Tuple(_) => {
                self.report_in(
                    file,
                    node,
                    "an anonymous `struct` has named fields: write the tuple type `(A, B)`                      for a positional one"
                        .to_string(),
                );
                return Ty::Error;
            }
        };
        let mut fields: Vec<(Symbol, Ty)> = Vec::with_capacity(members.len());
        for &m in members {
            // A record body may hold comptime items (`$assert`) beside its
            // fields; those are not members and carry no type.
            let NodeKind::Field { name, ty, .. } = self.asts[&file].node(m).kind.clone() else {
                continue;
            };
            if fields.iter().any(|(n, _)| *n == name) {
                self.report_in(file, m, format!("duplicate field `{name}`"));
                return Ty::Error;
            }
            let t = self.ty_from_node_in(file, ty);
            fields.push((name, t));
        }
        Ty::anon_struct(fields)
    }

    /// `int.<N>` / `uint.<N>` — one member of an integer family (§3.1).
    ///
    /// The width is an ordinary `const` argument, read by the same
    /// [`Inferer::const_value_in`] that reads an array length, so a literal
    /// (`int.<32>`), a named constant (`int.<WORD>`) and a `const` generic
    /// parameter (`int.<N>`, inside the family impl) all reach it by the one
    /// path. That is also what makes `int.<32>` and `i32` the *same* type
    /// rather than two that convert: both end as `Ty::Int` with the same width.
    ///
    /// The family name must carry its argument. `Box` may be written for
    /// `Box.<T>` and have its parameter inferred, but a bare `int` would be an
    /// integer of no particular width, and the only thing that could solve it
    /// is a use site — so it would turn every forgotten `.<32>` into an
    /// inference error somewhere else. Saying so here, where the name is, is
    /// the better diagnostic.
    fn int_family_ty(
        &mut self,
        file: FileId,
        node: NodeId,
        signed: bool,
        generic_args: &[NodeId],
    ) -> Ty {
        let family = if signed { "int" } else { "uint" };
        let [arg] = generic_args else {
            let msg = format!(
                "`{family}` is a family of integer types and needs its width: \
                 write `{family}.<N>`"
            );
            self.report_in(file, node, msg);
            return Ty::Error;
        };
        // A width is read at `u16` (§3.1 caps it at 65535), and it is *normalized*
        // to a bare `Const::Width` rather than kept as the value that was read.
        // That is what makes `int.<32>` and `i32` the same type down to the
        // representation: `i32` is built by `primitive_ty` with a bare width, and
        // a written `int.<32>` would otherwise carry a `u16`-typed `Const::Value`
        // that compares unequal to it.
        let read = self.const_value_in(file, *arg, &super::ty::width_ty(), "an integer width", 0);
        let width = match read.value() {
            Some(n) => match u16::try_from(n) {
                Ok(n) => Const::Width(n),
                // Out of a `u16` entirely; the range arm below names the bound.
                Err(_) => Const::Width(0),
            },
            // Still symbolic — a `const` parameter inside the family impl.
            None => read.clone(),
        };

        // A written-out width has to obey the same rules the `i<N>` / `u<N>`
        // spellings do, or the two ways of naming one type would disagree about
        // which types exist. In particular `uint.<1>` is `bool`, exactly as `u1`
        // is (§3.1) — if it were a distinct one-bit integer instead, `u1` and
        // `uint.<1>` would be different types and the sugar would be a lie.
        //
        // A width that is still symbolic cannot be checked here; the family impl
        // is generic over every legal `N`, and monomorphization is where a bad
        // one becomes visible.
        match width.value() {
            Some(1) if !signed => Ty::Bool,
            Some(1) => {
                self.report_in(
                    file,
                    *arg,
                    "a 1-bit signed integer is not a type".to_string(),
                );
                Ty::Error
            }
            Some(n) if n == 0 || n > 65535 => {
                // `read`, not `width`: the number the *program* wrote is what the
                // message has to name, and a value too large for a `u16` was
                // clamped above.
                let wrote = read.value().unwrap_or(n);
                let msg = format!("an integer width must be between 1 and 65535, not `{wrote}`");
                self.report_in(file, *arg, msg);
                Ty::Error
            }
            _ => Ty::Int { signed, width },
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
                let name = self.defs.get(def).name.clone();
                match name.as_str() {
                    "int" | "uint" => {
                        self.int_family_ty(file, node, name.as_str() == "int", generic_args)
                    }
                    other => primitive_ty(other).unwrap_or(Ty::Error),
                }
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
            DefKind::TypeAlias => match self.self_assoc_ty(file, node, def) {
                Some(t) => t,
                None => self.expand_alias(def),
            },
            // `K :: P.<u8>` is an alias too, and collection could not know it:
            // a `::`-RHS is parsed as an *expression*, so an instantiation
            // comes back as a `GenericApply` that is a type when its head
            // names one and a value (`f.<i32>`) when it does not — a
            // difference only resolution can see. Without this the binding is
            // a `Const`, a use of it in type position is a silent `Ty::Error`,
            // and the program type-checks against nothing at all.
            DefKind::Const => self.const_alias_ty(def).unwrap_or(Ty::Error),
            // A generic type parameter is a rigid opaque type of its own def —
            // except a **pinned** associated-type parameter, which is the type
            // its bound pinned it to: `T.Item` written in the signature of
            // `<T: Holder.<Item = i32>>` is `i32`, the same answer `t.get()`
            // gets through `param_ty`.
            DefKind::TypeParam => self.param_ty(def),
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
            // Anything else named where a type belongs: a namespace, a function,
            // a local, a field. **It has to be said**, and this is the only place
            // that can say it — a `Ty::Error` unifies with everything, so a
            // signature built from one type-checks against nothing and the
            // mistake surfaces as the backend refusing a `void` slot, in a
            // function that is not the one with the error in it.
            //
            // The common way to get here is a *shadowed* prelude type:
            // `str :: import <std/str>` binds the namespace over the type, and
            // then `-> str` names the import. That is what the note is for.
            //
            // [`DefKind::External`] is the exception and stays silent: it is a
            // member of a package that was deliberately not loaded, so "not a
            // type" is not something we know.
            DefKind::External => Ty::Error,
            kind => {
                // **Only about a file this compilation is looking at.** The
                // arm reports a mistake in a declaration, and a declaration
                // that arrived with a library was checked where the library was
                // compiled — there is no tree here to quote the name from, and
                // nothing to tell the reader to change. `Ty::Error` is still
                // the answer; the silence is the whole difference.
                let ours = self.asts.contains_key(&file);
                if ours && self.asts[&file].meta::<TyPathReported>(node).is_none() {
                    self.asts[&file].set_meta(node, TyPathReported);
                    let name = self.written_path_in(file, node, def);
                    let msg = format!("`{name}` is a {}, not a type", kind.label());
                    // A one-word namespace is nearly always an `import`
                    // binding, and an `import` binding shadows: `str :: import
                    // <std/str>` hides the `str` the prelude gives every file,
                    // so the signature under the caret looks right and names
                    // the wrong thing. Say so, because nothing else in the
                    // message hints that the name used to mean something.
                    match kind == DefKind::Namespace && !name.contains('.') {
                        true => self.report_with_note_in(
                            file,
                            node,
                            msg,
                            format!(
                                "a binding of `{name}` shadows any type of that \
                                 name \u{2014} bind the import under another \
                                 name to write both"
                            ),
                        ),
                        false => self.report_in(file, node, msg),
                    }
                }
                Ty::Error
            }
        }
    }

    /// The path as the program **wrote** it — `"str"`, `"c.int"` — for a
    /// message about a name in type position.
    ///
    /// Not the def's own name: `str :: import <std/str>` resolves to a namespace
    /// called `std`, and naming that in the diagnostic points at a package the
    /// program never wrote instead of the word under the caret.
    ///
    /// The *use* site's file is the one read, because the use site is what the
    /// caret is under. A file this compilation never parsed has no such text,
    /// and is answered with the def's own name — deliberately, rather than by
    /// recording the string: every name in type position in every library would
    /// have to carry one, to word a diagnostic that can only ever be reported
    /// about a program being compiled now.
    fn written_path_in(&self, file: FileId, node: NodeId, def: DefId) -> String {
        let Some(ast) = self.asts.get(&file) else {
            return self.defs.get(def).name.to_string();
        };
        let path = match ast.node(node).kind {
            NodeKind::TypePath { path, .. } => path,
            _ => node,
        };
        match &ast.node(path).kind {
            NodeKind::Path { segments } => segments
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("."),
            // A qualified name written in expression position — a composite
            // literal's head. Walk back down the chain so the message quotes
            // `ns.P` and not the last segment alone.
            NodeKind::FieldAccess { base, name } => {
                format!("{}.{name}", self.written_path_in(file, *base, def))
            }
            // Not a path at all — nothing was written to quote, so the def's
            // own name is the best there is.
            _ => self.defs.get(def).name.to_string(),
        }
    }

    /// Resolve the right-hand side of every type alias this file declares, for
    /// the side effect of checking it.
    ///
    /// The type itself is thrown away: an alias has no node of its own to stamp
    /// it on — every *use* of it carries the expansion — and computing it twice
    /// costs nothing, because the second time is what a use does anyway.
    fn check_type_aliases(&mut self) {
        let aliases: Vec<DefId> = self
            .defs
            .iter()
            .filter(|d| d.file == Some(self.file) && d.kind == DefKind::TypeAlias)
            .map(|d| d.id)
            .collect();
        // A `::` binding whose right-hand side names a type is an alias too,
        // and it is a `DefKind::Const` — `K :: P.<u8>` (§2.4). Its expansion is
        // worked out here for the same reason, and under the same key.
        let const_aliases: Vec<DefId> = self
            .defs
            .iter()
            .filter(|d| d.file == Some(self.file) && d.kind == DefKind::Const)
            .map(|d| d.id)
            .collect();
        for def in const_aliases {
            let Some(ty) = self.const_alias_ty(def) else {
                continue;
            };
            if let Some(node) = self.defs.get(def).node {
                let ty = self.cx.resolve(&ty);
                self.ast.set_meta(node, super::Expansion(ty));
            }
        }
        for def in aliases {
            let ty = self.expand_alias(def);
            // Stamped for [`super::decl::record_types`], which files it under
            // the alias itself: expanding one needs the tree it is written in,
            // and a use in another package does not have that.
            if let Some(node) = self.defs.get(def).node {
                let ty = self.cx.resolve(&ty);
                self.ast.set_meta(node, super::Expansion(ty));
            }
        }
    }

    /// Expand a `::` binding whose right-hand side *parses* as an expression
    /// but *names* a type — `K :: P.<u8>`, or a bare `K :: P`.
    ///
    /// `None` when the head does not name a type, which leaves a value used in
    /// type position exactly as it was: a `Ty::Error` reported elsewhere.
    fn const_alias_ty(&mut self, def: DefId) -> Option<Ty> {
        // Recorded where it was written: a binding in another package has no
        // tree here, and whether it named a type was settled there.
        if self.decls().get(def).is_some() {
            return self.decls().expansion(def);
        }
        if self.alias_stack.contains(&def) {
            return None;
        }
        let d = self.defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let ast = self.asts.get(&file)?;
        let NodeKind::ConstBind { rhs, .. } = ast.node(node).kind.clone() else {
            return None;
        };
        // A tuple of types is a type: `Item :: (usize, I.Item)`. Every element
        // has to name one, or the binding is a tuple *value*.
        if let NodeKind::Tuple { elems } = self.asts[&file].node(rhs).kind.clone() {
            if elems.is_empty() || !elems.iter().all(|&e| self.names_type_in(file, e)) {
                return None;
            }
            self.alias_stack.push(def);
            let ty = self.ty_from_node_in(file, rhs);
            self.alias_stack.pop();
            return Some(ty);
        }
        let (head, args) = match self.asts[&file].node(rhs).kind.clone() {
            NodeKind::GenericApply { base, args } => (base, args),
            // `Item :: I.Item` — a parameter's associated type, reached through
            // the parameter the way a namespace member is.
            NodeKind::Path { .. } | NodeKind::FieldAccess { .. } => (rhs, Vec::new()),
            _ => return None,
        };
        let head_def = self.resolved_def_in(file, head)?;
        // Whatever names a type: a primitive (`C :: u8`), a nominal, another
        // alias. A head that names a *value* is left alone — `N :: SIZE` is a
        // constant, and nothing here should turn it into one.
        if !matches!(
            self.defs.get(head_def).kind,
            DefKind::Primitive
                | DefKind::Struct
                | DefKind::Enum
                | DefKind::Trait
                | DefKind::TypeAlias
                | DefKind::TypeParam
                // A chain of these: `c.long :: C_LONG` over `C_LONG :: i64`.
                // Following it is safe because a head that really is a value
                // bottoms out at a literal, which is not a type either.
                | DefKind::Const
        ) {
            return None;
        }
        self.alias_stack.push(def);
        let ty = self.typepath_ty(file, head, &args);
        self.alias_stack.pop();
        Some(ty)
    }

    /// Whether `node`, read as an expression, names a type: a name or member
    /// access resolved to one, an instantiation of one, or a tuple of them.
    fn names_type_in(&self, file: FileId, node: NodeId) -> bool {
        let head = match self.asts[&file].node(node).kind.clone() {
            NodeKind::GenericApply { base, .. } => base,
            NodeKind::Path { .. } | NodeKind::FieldAccess { .. } => node,
            NodeKind::Tuple { elems } => {
                return !elems.is_empty() && elems.iter().all(|&e| self.names_type_in(file, e));
            }
            _ => return false,
        };
        self.resolved_def_in(file, head).is_some_and(|d| {
            matches!(
                self.defs.get(self.defs.resolve_alias(d)).kind,
                DefKind::Primitive
                    | DefKind::Struct
                    | DefKind::Enum
                    | DefKind::Trait
                    | DefKind::TypeAlias
                    | DefKind::TypeParam
            )
        })
    }

    /// `Self.Item` written **inside the trait that declares `Item`**, kept as
    /// the associated type itself rather than expanded to a fresh variable.
    ///
    /// [`Inferer::expand_alias`] hands an abstract associated type a fresh
    /// variable, on the grounds that context will pin it. That is true of the
    /// trait's own body, and false for the one caller that matters here: a call
    /// through a type parameter's bound (`n.peel()` where `<N: Nest>`), whose
    /// result is `N.Inner` — a type the signature can *name*, because §5.4 says
    /// it can. A variable cannot be substituted into, so the link between the
    /// declaration's `Self.Inner` and the caller's `N.Inner` would be lost
    /// before [`Inferer::trait_self_subst`] ever ran.
    ///
    /// Keeping the associated def is what gives that substitution something to
    /// rewrite. It is narrow on purpose — only `Self.Assoc` where `Self` is the
    /// declaring trait — so every other use of an abstract associated type
    /// keeps the variable it had.
    fn self_assoc_ty(&mut self, file: FileId, node: NodeId, def: DefId) -> Option<Ty> {
        let owner = self.defs.get(def).parent?;
        if self.defs.get(owner).kind != DefKind::Trait {
            return None;
        }
        if self.defs.get(def).assoc_bounds.is_none() {
            return None;
        }
        // The base has to be `Self` — the trait itself. `Holder.Item` written
        // as a path from outside is a different question.
        let segs = self.asts[&file].meta::<super::PathRes>(node)?;
        let base = match segs.0.get(segs.0.len().checked_sub(2)?)? {
            Resolution::Def(b) => self.defs.resolve_alias(*b),
            _ => return None,
        };
        (base == owner).then(|| Ty::Nominal {
            def,
            args: Vec::new(),
        })
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
        // A definition that arrived with a library answers from what its own
        // compilation concluded: there is no tree here to expand. An entry with
        // no expansion in it is an answer too — a `distinct`, or a binding that
        // names no type — and it stands for the alias itself, the same as a
        // cycle does below.
        if self.decls().get(def).is_some() {
            return self.decls().expansion(def).unwrap_or(Ty::Nominal {
                def,
                args: Vec::new(),
            });
        }
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
        let want = self.usize_ty();
        self.const_value_in(file, node, &want, "an array length", 0)
    }

    /// Read a compile-time value written in a type — an array length, or an
    /// explicit `const` generic argument — **at** the type `want`.
    ///
    /// `want` is not a check bolted on afterwards; it is half of what the value
    /// *is* (§5, [`ConstArg`]). An array length is always a `usize`, and a
    /// `<const B: bool>` argument is a `bool`, so the literal `3` and the
    /// literal `true` each arrive already knowing which.
    ///
    /// Four things can stand here (§3.2, §5): a literal, a `const` generic
    /// parameter — which stays symbolic until monomorphization — a named
    /// constant, followed one hop to its right-hand side so `SIZE :: 4` makes
    /// `[SIZE]T` a `[4]T`, and the hole `[_]T`, whose length a composite literal
    /// fills in. Anything else (an arithmetic expression, say) has no
    /// const-evaluator behind it yet and is a diagnostic.
    fn const_value_in(
        &mut self,
        file: FileId,
        node: NodeId,
        want: &Ty,
        what: &'static str,
        depth: u32,
    ) -> Const {
        // A constant that names itself would otherwise loop forever.
        if depth > 8 {
            let msg = format!("{what} may not refer to itself");
            self.report_const_in(file, node, msg);
            return Const::Error;
        }
        let kind = self.asts[&file].node(node).kind.clone();
        // A literal has to agree with the type the slot is declared at. Reading
        // `true` into a `<const N: usize>` is a mistake, and reporting it here —
        // where both the value and the declared type are in hand — is the only
        // place it reads as one.
        if let NodeKind::Lit(lit) = &kind {
            return self.const_from_lit(file, node, lit, want, what);
        }
        match kind {
            // `[_]T` — the length is whatever the value supplies.
            NodeKind::TypeHole => self.cx.fresh_const(),
            // A constant whose type was written before the binder (§2.5):
            // `SIZE: u16 :: 4` binds an `AssocConst` carrying the type and the
            // value, where `SIZE :: 4` binds the value directly. The value is
            // what a `const` slot wants either way — the declared type has
            // already been checked against `want` by whoever sent us here.
            NodeKind::AssocConst {
                default: Some(v), ..
            } => self.const_value_in(file, v, want, what, depth + 1),
            NodeKind::Path { .. } | NodeKind::TypePath { .. } => {
                match self.resolved_def_in(file, node) {
                    Some(def) => self.const_of_def(file, node, def, want, what, depth),
                    // Unresolved: name resolution already complained.
                    None => Const::Error,
                }
            }
            // `[m.SIZE]T` — a constant named through its namespace.
            NodeKind::FieldAccess { .. } if self.resolved_def_in(file, node).is_some() => {
                let def = self.resolved_def_in(file, node).expect("checked");
                self.const_of_def(file, node, def, want, what, depth)
            }
            // `[SIZE * 2]T`, `-3` — an expression built out of compile-time
            // values with operators. It is **folded** here, by the same
            // arithmetic the const evaluator runs on the IR (see
            // [`crate::ir::const_eval::binary_values`]); only the walk differs,
            // because there is no IR yet and a length is wanted before there is
            // one.
            //
            // Everything that is not one of the forms above comes through here,
            // so that the diagnostic naming what went wrong is written in one
            // place — the fold knows whether it met a call, a `const` parameter
            // under an operator, or something that is not a value at all, and a
            // second "must be a literal or a constant" here would only ever say
            // less.
            _ => match self.const_operand(file, node, what, depth) {
                Some(v) => self.const_from_value(file, node, v, want, what),
                None => Const::Error,
            },
        }
    }

    /// The value of one operand inside a folded `const` expression.
    ///
    /// Deliberately **not** read at the slot's type: in `[SIZE * 2]T` the `2` is
    /// a `comptime_int` and the arithmetic runs at arbitrary precision, exactly
    /// as it does in a `::` binding. Only the finished value is read at `want`,
    /// which is what makes `[SIZE * 2]T` and `LEN :: SIZE * 2` agree about what
    /// they computed and about whether it fits.
    ///
    /// `None` means a diagnostic was reported.
    fn const_operand(
        &mut self,
        file: FileId,
        node: NodeId,
        what: &'static str,
        depth: u32,
    ) -> Option<ConstValue> {
        if depth > 32 {
            self.report_const_in(file, node, format!("{what} may not refer to itself"));
            return None;
        }
        let ast_kind = self.asts[&file].node(node).kind.clone();
        match ast_kind {
            NodeKind::Lit(Lit::Int(n)) => Some(ConstValue::Int(n)),
            NodeKind::Lit(Lit::Float(f)) => Some(ConstValue::Float(f)),
            NodeKind::Lit(Lit::Bool(b)) => Some(ConstValue::Bool(b)),
            NodeKind::Lit(Lit::Char(c)) => Some(ConstValue::Char(c)),
            NodeKind::Lit(Lit::Str(t)) => Some(ConstValue::Str(t)),
            NodeKind::Lit(Lit::Bytes(b)) => Some(ConstValue::Bytes(b)),
            NodeKind::AssocConst {
                default: Some(v), ..
            } => self.const_operand(file, v, what, depth + 1),
            NodeKind::Unary { op, operand } => {
                let v = self.const_operand(file, operand, what, depth + 1)?;
                match crate::ir::const_eval::unary_op(op, &v) {
                    Ok(v) => Some(v),
                    Err(msg) => {
                        self.report_const_in(file, node, msg);
                        None
                    }
                }
            }
            NodeKind::Binary { op, lhs, rhs } => {
                // `&&` / `||` short-circuit, and that is not a detail even here:
                // the right operand of a guarded `&&` may be the one that would
                // fail to evaluate. It is handled by the walk rather than by the
                // shared arithmetic for exactly that reason.
                if matches!(op, BinOp::And | BinOp::Or) {
                    let a = self.const_truth(file, lhs, what, depth + 1)?;
                    if (op == BinOp::And) != a {
                        return Some(ConstValue::Bool(a));
                    }
                    let b = self.const_truth(file, rhs, what, depth + 1)?;
                    return Some(ConstValue::Bool(b));
                }
                let a = self.const_operand(file, lhs, what, depth + 1)?;
                let b = self.const_operand(file, rhs, what, depth + 1)?;
                match crate::ir::const_eval::binary_values(op, &a, &b) {
                    Ok(v) => Some(v),
                    Err(msg) => {
                        self.report_const_in(file, node, msg);
                        None
                    }
                }
            }
            NodeKind::Path { .. } | NodeKind::TypePath { .. } | NodeKind::FieldAccess { .. }
                if self.resolved_def_in(file, node).is_some() =>
            {
                let def = self.resolved_def_in(file, node)?;
                let def = self.defs.resolve_alias(def);
                let d = self.defs.get(def);
                match d.kind {
                    DefKind::Const => {
                        if let Some(v) = self.decls().const_value(def) {
                            return Some(v);
                        }
                        let (cfile, cnode) = (d.file?, d.node?);
                        let rhs = match self.asts[&cfile].node(cnode).kind.clone() {
                            NodeKind::ConstBind { rhs, .. } => rhs,
                            NodeKind::AssocConst {
                                default: Some(rhs), ..
                            } => rhs,
                            _ => return None,
                        };
                        self.const_operand(cfile, rhs, what, depth + 1)
                    }
                    // A `const` generic parameter has no value until an
                    // instantiation picks one, and a [`Const`] has no shape for
                    // an unevaluated expression — `[N * 2]T` would have to stay
                    // symbolic all the way to monomorphization. `[N]T` on its
                    // own is fine and goes through the other branch; this is
                    // only the combined form.
                    DefKind::ConstParam => {
                        let msg = format!(
                            "`{}` is a `const` generic parameter, so it cannot be combined with \
                             an operator in {what}: its value is not known until the function is \
                             instantiated",
                            d.name
                        );
                        self.report_const_in(file, node, msg);
                        None
                    }
                    _ => {
                        let msg = format!(
                            "`{}` is a {} — {what} must be built from constant values",
                            self.defs.canonical_string(def),
                            d.kind.label()
                        );
                        self.report_const_in(file, node, msg);
                        None
                    }
                }
            }
            // A call is the one form that needs a **body**, and a body is not
            // compiled until its types are known: the const evaluator runs on
            // the IR, the IR is built after inference, and this is inference
            // asking. Naming the call through a `::` constant does not help —
            // following the constant arrives right back here — so the limit is
            // stated rather than worked around.
            NodeKind::Call { .. } => {
                let msg = format!(
                    "a call cannot be evaluated inside {what}: a function's body is not compiled \
                     until after the types are known, and this is one of them"
                );
                self.report_const_in(file, node, msg);
                None
            }
            _ => {
                let msg = format!(
                    "{what} must be a literal, a constant, a `const` generic parameter, or an \
                     expression built from those"
                );
                self.report_const_in(file, node, msg);
                None
            }
        }
    }

    /// One operand of `&&` / `||`, which must be a `bool`.
    fn const_truth(
        &mut self,
        file: FileId,
        node: NodeId,
        what: &'static str,
        depth: u32,
    ) -> Option<bool> {
        match self.const_operand(file, node, what, depth)? {
            ConstValue::Bool(b) => Some(b),
            other => {
                let msg = format!("`{}` is not a `bool`", other.display());
                self.report_const_in(file, node, msg);
                None
            }
        }
    }

    /// Turn a literal into a [`Const`] of type `want`, or report why it cannot be
    /// one. Range is checked here for the same reason it is checked for a typed
    /// constant (§2.5): this is where the arbitrary-precision number is still in
    /// hand.
    fn const_from_lit(
        &mut self,
        file: FileId,
        node: NodeId,
        lit: &Lit,
        want: &Ty,
        what: &'static str,
    ) -> Const {
        let value = match lit {
            Lit::Int(n) => ConstValue::Int(n.clone()),
            Lit::Float(f) => ConstValue::Float(*f),
            Lit::Bool(b) => ConstValue::Bool(*b),
            Lit::Char(c) => ConstValue::Char(*c),
            Lit::Str(t) => ConstValue::Str(t.clone()),
            Lit::Bytes(b) => ConstValue::Bytes(b.clone()),
        };
        self.const_from_value(file, node, value, want, what)
    }

    /// Read a computed compile-time value at the type its slot is declared at,
    /// or report why it cannot be one.
    ///
    /// Both halves are identity (§5, [`ConstArg`]): the value and the type it is
    /// written at. This is also where the range is checked, for the same reason
    /// a typed constant's is (§2.5) — it is the last point at which the
    /// arbitrary-precision number is still in hand.
    fn const_from_value(
        &mut self,
        file: FileId,
        node: NodeId,
        value: ConstValue,
        want: &Ty,
        what: &'static str,
    ) -> Const {
        // The slot's type as a *primitive*: `usize` is `distinct uint.<PTR_BITS>`
        // (§3.1) and an array length is one, so a literal filling that slot has
        // to be read against what the `distinct` stands over. §2.4 is what makes
        // that right rather than a shortcut — a literal reaches a `distinct`
        // numeric with no written cast.
        let repr = self.numeric_repr(want);
        let value = match (&value, &repr) {
            (ConstValue::Int(n), Ty::Int { .. }) => {
                // A width still symbolic — a parameter declared at `int.<N>`
                // inside a generic that supplies `N` — has no range to check
                // against yet, so the value is taken as written and
                // monomorphization is left to reject it. Range-checking against
                // a guessed width would reject programs that are fine.
                let fits = repr
                    .int_parts()
                    .is_none_or(|(signed, bits)| super::ty::int_fits(n, signed, bits));
                if !fits {
                    let msg = format!("`{n}` does not fit in `{}`", want.display(self.defs));
                    self.report_const_in(file, node, msg);
                    return Const::Error;
                }
                value
            }
            // A float slot takes an integer too, the way a `f64` binding does.
            (ConstValue::Int(n), Ty::Float(_)) => match n.to_f64() {
                Some(f) => ConstValue::Float(f),
                None => {
                    self.report_const_in(
                        file,
                        node,
                        "this integer is not representable as a float",
                    );
                    return Const::Error;
                }
            },
            (ConstValue::Float(_), Ty::Float(_))
            | (ConstValue::Bool(_), Ty::Bool)
            | (ConstValue::Char(_), Ty::Char) => value,
            // An errored slot already reported; do not add to it.
            (_, Ty::Error) => return Const::Error,
            _ => {
                let msg = format!(
                    "{what} is a `{}`, and this is not one",
                    want.display(self.defs)
                );
                self.report_const_in(file, node, msg);
                return Const::Error;
            }
        };
        Const::known(want.clone(), value)
    }

    /// The value a definition standing in a `const` slot denotes.
    fn const_of_def(
        &mut self,
        file: FileId,
        node: NodeId,
        def: DefId,
        want: &Ty,
        what: &'static str,
        depth: u32,
    ) -> Const {
        let def = self.defs.resolve_alias(def);
        let d = self.defs.get(def);
        match d.kind {
            DefKind::ConstParam => {
                // A `const` parameter stays symbolic until monomorphization, so
                // the one thing checkable here is its *type*, and it has to be
                // one the slot accepts. An array length is a `usize` (§3.2), so
                // `func <const N: i32> () -> [N]i32` is a mistake — and one
                // worth catching at the declaration, because the alternative is
                // a `[4]i32` that does not equal `[4]i32` at the call site.
                //
                // "Accepts" is widening, not equality: a `<const N: u16>` is a
                // perfectly good `u32` argument for the same reason a `u16`
                // *value* is (§3.1). The narrowing direction stays an error.
                let declared = self.const_param_ty(def);
                if !matches!(declared, Ty::Error)
                    && !matches!(want, Ty::Error)
                    && declared != *want
                    && !self.const_ty_widens(&declared, want)
                {
                    let msg = format!(
                        "`{}` is a `const {}`, but {what} must be a `{}`",
                        self.defs.get(def).name,
                        declared.display(self.defs),
                        want.display(self.defs)
                    );
                    self.report_const_in(file, node, msg);
                    return Const::Error;
                }
                Const::Param(def)
            }
            DefKind::Const => {
                // The recorded value first, always: a constant in another
                // package has one and no right-hand side here, and while a
                // tree-first read would work for this package it would mean the
                // recorded answer was never exercised by anything.
                if let Some(v) = self.decls().const_value(def) {
                    return self.const_from_value(file, node, v, want, what);
                }
                let (Some(cfile), Some(cnode)) = (d.file, d.node) else {
                    return Const::Error;
                };
                // A constant is written either way (§2.5): `SIZE :: 4` binds a
                // value, and `SIZE: u16 :: 4` writes the type before the binder,
                // which parses as an `AssocConst` carrying the type and the
                // value separately. Both are a constant with a right-hand side,
                // and a width read from one — `uint.<PTR_BITS>` in `core` — has
                // no reason to care which spelling declared it.
                let rhs = match self.asts[&cfile].node(cnode).kind.clone() {
                    NodeKind::ConstBind { rhs, .. } => rhs,
                    NodeKind::AssocConst {
                        default: Some(rhs), ..
                    } => rhs,
                    _ => return Const::Error,
                };
                self.const_value_in(cfile, rhs, want, what, depth + 1)
            }
            _ => {
                let msg = format!(
                    "`{}` is a {} — {what} must be a constant value",
                    self.defs.canonical_string(def),
                    d.kind.label()
                );
                self.report_const_in(file, node, msg);
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
                inner: Box::new(Ty::u8()),
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
            if self.try_int_widen(node, actual, expected)
                || self.try_array_to_slice(node, actual, expected)
                || self.try_dyn_coerce(node, actual, expected)
                || self.try_anon_to_named(node, actual, expected)
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

    /// Try to reach `expected` from `actual` by **widening** an integer: a
    /// narrower type may stand where a wider one is wanted (§3.1).
    ///
    /// `f(x)` with `x: u16` and `f :: func (n: u32)` is the case this exists
    /// for, and a `<const N: u16>` read in a `u32` slot is the same thing one
    /// level up. The direction is the whole point — a `u32` reaching a `u16`
    /// stays an error, because that one loses values and the program should say
    /// which ones it meant to lose.
    ///
    /// The **pointer-sized** types take no part, in either direction. Their
    /// width is the target's, so `u64` into `usize` would be legal on a 64-bit
    /// machine and not on a 32-bit one — a coercion that silently appears and
    /// disappears with the target is worse than one that never happens, and
    /// `usize` is a distinct type precisely so that crossing into it is written
    /// down. `cast` is still one word away.
    ///
    /// A widening is exact, so it lowers to the same implicit `$cast` a
    /// `comptime_int` conversion does — [`Coercion`], not a new node kind.
    fn try_int_widen(&mut self, node: NodeId, actual: &Ty, expected: &Ty) -> bool {
        let got = self.cx.shallow(actual);
        let want = self.cx.shallow(expected);
        if self.is_ptr_sized(&got) || self.is_ptr_sized(&want) {
            return false;
        }
        let (Some(from), Some(to)) = (got.int_parts(), want.int_parts()) else {
            return false;
        };
        if !super::ty::int_widens(from, to) {
            return false;
        }
        if self.cx.unify(&want, expected).is_err() {
            return false;
        }
        self.ast.set_meta(node, Coercion { to: want });
        true
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
            args: vec![self.usize_ty()],
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
        let object = self.cx.shallow(&ei);
        let Ty::Dyn {
            def: trait_def,
            assoc: object_assoc,
        } = object.clone()
        else {
            return false;
        };
        let concrete = self.cx.shallow(&inner);
        if is_var(&concrete) || matches!(concrete, Ty::Error) {
            return false;
        }
        // `dyn Func(A) -> R` takes anything called with `A` that answers `R` —
        // which no impl says, and the value's own signature does (§5.5).
        if Some(trait_def) == self.lang.get("func").map(|d| self.defs.resolve_alias(d)) {
            // A closure's own call is what fills the one slot, and an `impl
            // Func` return type is the closure its body returned; a function
            // pointer has no data to point at.
            if !matches!(&concrete, Ty::Nominal { def, .. }
                if self.defs.get(*def).kind == DefKind::Closure || self.defs.get(*def).opaque)
            {
                return false;
            }
            let Some((params, ret)) = self.func_value_sig(&concrete) else {
                return false;
            };
            let snapshot = self.cx.snapshot();
            let fits = object_assoc.iter().all(|(n, t)| match n.as_str() {
                "Args" => self.cx.unify(&args_tuple(&params), t).is_ok(),
                "Output" => self.cx.unify(&ret, t).is_ok(),
                _ => false,
            });
            if !fits {
                self.cx.rollback(snapshot);
                return false;
            }
            let object = self.cx.resolve(&object);
            self.ast.set_meta(
                node,
                DynCoerce {
                    trait_def,
                    object,
                    concrete,
                },
            );
            return true;
        }
        // The coercion is only sound when the concrete type really implements
        // the trait; an unsatisfied bound stays a plain type mismatch.
        //
        // A **type parameter** proves it a different way. There is no impl to
        // find for `X` — it is not a type yet — but `<X: T>` is the promise that
        // whatever instantiates it has one, and that promise is exactly what the
        // coercion needs. Monomorphization substitutes the real type here and
        // builds the vtable from it, so by the time a vtable is wanted the
        // question has an ordinary answer.
        if !self.param_has_bound(&concrete, trait_def)
            && !matches!(self.select(&concrete, trait_def, &[]), Select::Ok(_))
        {
            return false;
        }
        self.ast.set_meta(
            node,
            DynCoerce {
                trait_def,
                object,
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

    /// Try to reach `expected` from `actual` by the one implicit struct→struct
    /// coercion §3.8 allows besides `@using`: an **anonymous** struct value
    /// becoming a named struct with the same field name→type set.
    ///
    /// It is one-way. A named struct never becomes anonymous on its own —
    /// that direction is an explicit `cast` — so there is no matching attempt
    /// the other way round.
    ///
    /// The conversion itself is nothing: both sides have the same fields at the
    /// same offsets, so it lowers to the implicit `$cast` every other exact
    /// coercion lowers to, which LIR turns into a read of the same address at
    /// the other type.
    fn try_anon_to_named(&mut self, node: NodeId, actual: &Ty, expected: &Ty) -> bool {
        let Ty::Struct(fields) = self.cx.shallow(actual) else {
            return false;
        };
        let want = self.cx.shallow(expected);
        let Ty::Nominal { def, .. } = &want else {
            return false;
        };
        if self.defs.get(*def).kind != DefKind::Struct {
            return false;
        }
        // The *set* has to match: a named struct with a field the value does
        // not carry has nothing to build that field from, and a value with a
        // field the struct does not declare has nowhere to put it.
        let declared = self.record_field_names(*def);
        if declared.len() != fields.len()
            || !declared.iter().all(|n| fields.iter().any(|(m, _)| m == n))
        {
            return false;
        }
        let snapshot = self.cx.snapshot();
        for (name, t) in &fields {
            let Some(ft) = self.field_ty(&want, name.as_str()) else {
                self.cx.rollback(snapshot);
                return false;
            };
            if self.cx.unify(t, &ft).is_err() {
                self.cx.rollback(snapshot);
                return false;
            }
        }
        let to = self.cx.resolve(&want);
        self.ast.set_meta(node, Coercion { to });
        true
    }

    /// Whether a statement unconditionally transfers control out of its block.
    fn diverges(&self, node: NodeId) -> bool {
        match &self.ast.node(node).kind {
            NodeKind::Return { .. } | NodeKind::Break { .. } | NodeKind::Continue => true,
            // A call that types as `never` ends the block just as a `return`
            // does — `.!` leans on this to type its `panic` arm. There is no
            // list of diverging intrinsics any more: `panic` is declared
            // `-> never` in `core`, so the signature says it and every other
            // diverging function gets the same treatment for free (§3.1).
            NodeKind::Call { .. } => matches!(self.types.get(&node), Some(Ty::Never)),
            _ => false,
        }
    }

    fn def_of(&self, node: NodeId) -> Option<super::def::DefId> {
        self.ast.meta::<DefMeta>(node).map(|m| m.0)
    }

    fn resolved_def(&self, node: NodeId) -> Option<super::def::DefId> {
        self.resolved_def_in(self.file, node)
    }

    /// Whether the resolver tried `node` and failed, which it has reported.
    fn failed_resolution(&self, node: NodeId) -> bool {
        matches!(self.ast.meta::<Resolution>(node), Some(Resolution::Error))
    }

    fn resolved_def_in(&self, file: FileId, node: NodeId) -> Option<super::def::DefId> {
        self.decls().resolved_def(file, node)
    }

    fn type_head_def_in(&self, file: FileId, node: NodeId) -> Option<super::def::DefId> {
        self.resolved_def_in(file, node)
    }

    fn report(&mut self, node: NodeId, message: impl Into<String>) {
        self.report_in(self.file, node, message);
    }

    /// Report against a node with a trailing note — used where the type in the
    /// message is one inference chose rather than one the source wrote.
    fn report_with_note(
        &mut self,
        node: NodeId,
        message: impl Into<String>,
        note: impl Into<String>,
    ) {
        let span = self.ast.node(node).span;
        self.diags.push(
            Diagnostic::error(message)
                .with_primary(FileSpan::new(self.file, span), "")
                .with_note(note),
        );
    }

    /// [`Inferer::report_with_note`] against another file's arena, for the same
    /// reason [`Inferer::report_in`] exists: a signature is checked from
    /// whatever body reached it, and the span belongs to the file that wrote it.
    fn report_with_note_in(
        &mut self,
        file: FileId,
        node: NodeId,
        message: impl Into<String>,
        note: impl Into<String>,
    ) {
        let span = self.asts[&file].node(node).span;
        self.diags.push(
            Diagnostic::error(message)
                .with_primary(FileSpan::new(file, span), "")
                .with_note(note),
        );
    }

    /// Report against a node in another file's arena — a signature or a
    /// constant reached from the body currently being checked.
    fn report_in(&mut self, file: FileId, node: NodeId, message: impl Into<String>) {
        let span = self.asts[&file].node(node).span;
        self.diags
            .push(Diagnostic::error(message).with_primary(FileSpan::new(file, span), ""));
    }

    /// Report about a compile-time value slot, **at most once per slot**.
    ///
    /// Every diagnostic in the `const_*` family below goes through this rather
    /// than through [`Inferer::report_in`]; see [`ConstSlotReported`] for why a
    /// type node can be visited many times and a mistake in it is still one
    /// mistake.
    fn report_const_in(&mut self, file: FileId, node: NodeId, message: impl Into<String>) {
        if self.asts[&file].meta::<ConstSlotReported>(node).is_some() {
            return;
        }
        self.asts[&file].set_meta(node, ConstSlotReported);
        self.report_in(file, node, message);
    }
}

/// A static trait call's `Self`, opened up before its instantiation (see
/// [`Inferer::open_trait_self`]).
struct OpenedSelf {
    trait_def: DefId,
    self_ty: Ty,
    /// A variable per associated type, by name.
    assocs: Vec<(Symbol, Ty)>,
    /// `Self` and each associated type to its variable.
    subst: Subst,
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
    /// The self type is a type parameter the trait bounds, so the obligation is
    /// discharged by that bound and the impl is chosen at monomorphization.
    ByBound,
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

/// Apply an instantiation to one `const` argument: a `const` generic parameter
/// becomes whatever the instantiation bound it to, and everything else — a known
/// value, a bare width, a variable — stands.
fn subst_const(c: &Const, map: &Subst) -> Const {
    match c {
        Const::Param(d) => map.consts.get(d).cloned().unwrap_or_else(|| c.clone()),
        other => other.clone(),
    }
}

/// Whether `ty` is `usize` / `isize` — a pointer-sized `distinct` whose width is
/// the target's rather than one the program wrote.
fn is_ptr_sized(lang: &LangItems, defs: &DefTable, ty: &Ty) -> bool {
    let Ty::Nominal { def, .. } = ty else {
        return false;
    };
    let def = defs.resolve_alias(*def);
    ["usize", "isize"]
        .iter()
        .filter_map(|t| lang.get(t))
        .any(|d| defs.resolve_alias(d) == def)
}

/// Replace every occurrence of `from` with `to` inside `ty`.
///
/// Used to rebind `Self` when a method is reached through a `distinct` type's
/// representation — see [`Inferer::infer_method_call_rebound`].
fn rebind_ty(ty: &Ty, from: &Ty, to: &Ty) -> Ty {
    if ty == from {
        return to.clone();
    }
    match ty {
        Ty::Ptr { mutable, inner } => Ty::Ptr {
            mutable: *mutable,
            inner: Box::new(rebind_ty(inner, from, to)),
        },
        Ty::Slice { mutable, inner } => Ty::Slice {
            mutable: *mutable,
            inner: Box::new(rebind_ty(inner, from, to)),
        },
        Ty::Array {
            len,
            mutable,
            inner,
        } => Ty::Array {
            len: len.clone(),
            mutable: *mutable,
            inner: Box::new(rebind_ty(inner, from, to)),
        },
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|e| rebind_ty(e, from, to)).collect()),
        Ty::Dyn { def, assoc } => Ty::Dyn {
            def: *def,
            assoc: assoc
                .iter()
                .map(|(n, t)| (n.clone(), (|e| rebind_ty(e, from, to))(t)))
                .collect(),
        },
        Ty::Func { params, ret, c } => Ty::Func {
            c: *c,
            params: params.iter().map(|p| rebind_ty(p, from, to)).collect(),
            ret: Box::new(rebind_ty(ret, from, to)),
        },
        Ty::Nominal { def, args } => Ty::Nominal {
            def: *def,
            args: args.iter().map(|a| rebind_ty(a, from, to)).collect(),
        },
        other => other.clone(),
    }
}

/// A member's type as a **signature** rather than a value: a method's type is
/// a function pointer's (`*func(S) -> i32`), and a message about what the impl
/// wrote reads better as what it wrote, `func(S) -> i32`.
fn signature_text(ty: &str) -> &str {
    ty.strip_prefix('*')
        .filter(|t| t.starts_with("func("))
        .unwrap_or(ty)
}

/// A call's arguments as the one type `Func` takes them as (§5.5): `()` for
/// none, and a tuple of them otherwise — a tuple of one included, which the
/// language has no spelling for and `Func(A)` is the only way to write.
fn args_tuple(params: &[Ty]) -> Ty {
    if params.is_empty() {
        Ty::Void
    } else {
        Ty::Tuple(params.to_vec())
    }
}

/// The inverse of [`args_tuple`]: the arguments a `Func` bound's tuple lists.
fn tuple_elems(args: &Ty) -> Vec<Ty> {
    match args {
        Ty::Void => Vec::new(),
        Ty::Tuple(elems) => elems.clone(),
        other => vec![other.clone()],
    }
}
