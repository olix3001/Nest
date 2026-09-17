//! The type representation and the inference context shared by the type checker
//! ([`super::infer`]) and the IR lowering ([`super::lower`]).
//!
//! A [`Ty`] is the checker's view of a type. Identity follows §3.8: **nominal**
//! types ([`Ty::Nominal`], a named `struct`/`enum`/`trait`/`distinct`, plus its
//! type arguments) are equal only when their [`DefId`]s match, so two structs
//! with identical fields are different types; **structural** types (pointers,
//! slices, arrays, tuples, function types, …) are equal when their components
//! match.
//!
//! Inference is Hindley–Milner-style unification over [`Ty::Var`]s held in an
//! [`InferCtxt`] union-find. Untyped numeric literals get a *numeric* variable
//! (kind [`TyVarKind::Int`] / [`TyVarKind::Float`]) that unifies only with a
//! compatible concrete type; one left unconstrained at the end **defaults** —
//! `isize` for integers, `f64` for floats (the compile-time `comptime_int` /
//! `comptime_float` collapse of §1: a float literal *is* a `comptime_float`,
//! i.e. `f128`, but collapses to `f64` when nothing pins its width — a literal
//! too big or too precise to survive that collapse is an error unless its use
//! really is an `f80` / `f128`). A string literal gets the same treatment with
//! kind [`TyVarKind::Str`]: it is a `comptime_str` that becomes `str`, `[]u8`
//! or `[]char` depending on its use site, and defaults to `str`. A general
//! variable that is never solved is a "type annotations needed" error.

use num_bigint::BigInt;
use std::collections::HashMap;

use crate::common::symbol::Symbol;
use crate::ir::const_eval::ConstValue;
use crate::parser::ast::NodeId;

use super::def::DefId;

/// The five legal float widths (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FloatWidth {
    F16,
    F32,
    F64,
    F80,
    F128,
}

/// An inference variable: an index into [`InferCtxt::subst`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct TyVar(pub u32);

/// What a fresh [`TyVar`] may unify with, and how it defaults if left unsolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TyVarKind {
    /// Any type; an unsolved one is a "type annotations needed" error.
    General,
    /// An integer literal: unifies only with an integer type; defaults `isize`.
    Int,
    /// A float literal (`comptime_float`, conceptually `f128`): unifies only
    /// with a float type, and collapses to `f64` when left unconstrained.
    Float,
    /// A string literal (`comptime_str`): unifies with `str`, `[]u8` or
    /// `[]char`, and defaults to `str` (§1.5).
    ///
    /// The set is exactly the three types whose contents the compiler can
    /// produce from the literal's bytes on its own — `str` and `[]u8` *are*
    /// those bytes, and `[]char` is them transcoded, which the const evaluator
    /// does at compile time. Any other string-like type is a library type, and
    /// how a literal reaches one is a conversion question shared with
    /// `[]T` → `Vec.<T>`; it is deliberately not answered here.
    Str,
}

/// A **const-generic** inference variable: an index into
/// [`InferCtxt::const_subst`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ConstVar(pub u32);

/// A compile-time *value* appearing inside a type — an array length (§3.2
/// `[N]T`) or a `const` generic argument (§5 `<const N: Ty>`).
///
/// These take part in type identity: `[3]i32` and `[4]i32` are different types,
/// so they need their own tiny unification lattice alongside [`Ty`]'s. A
/// [`Const::Param`] is the symbolic stand-in for a `const` generic parameter
/// that survives all the way to monomorphization; a [`Const::Var`] is the
/// inference variable a call site instantiates it to, or the hole `[_]T` leaves
/// for a literal to fill.
///
/// Since phase 5 a [`Const`] is also an *integer type's* argument: `i32` is
/// `int.<32>` and `u8` is `uint.<8>`, and that width lives here (§3.1). The
/// signedness does not — it chooses which of the two families the type belongs
/// to, not what it is applied to. That is also why [`Const::Width`] is a case of
/// its own rather than a value at a type; see its own note.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Const {
    /// A known value, at the type it was written at (see [`ConstArg`]).
    Value(Box<ConstArg>),
    /// A bare integer **width** in bits — the `N` of `int.<N>` / `uint.<N>`.
    ///
    /// A width is a `u16` (§3.1 caps a width at 65535), and it is a case of its
    /// own rather than a [`Const::Value`] at type `u16` for a structural reason:
    /// a `Value` carries the [`Ty`] it was written at, and that `Ty` for a width
    /// would be `u16` — which is itself `uint.<16>`, whose width would carry a
    /// `u16`, without end. Holding the number bare is what makes the
    /// representation finite.
    ///
    /// It is also what a width *is*. Nothing stores one at run time, so any
    /// compile-time integer that fits converts into one implicitly, and one that
    /// does not is an error where it is written.
    Width(u16),

    /// An as-yet-uninstantiated `const` generic parameter, by its [`DefId`].
    Param(DefId),
    /// An unsolved inference variable.
    Var(ConstVar),
    /// A value that could not be determined; a diagnostic was already reported.
    /// Unifies with anything so one error does not cascade.
    Error,
}

/// A known `const` argument: a value **and the type it was written at**.
///
/// Both halves are identity (§5). `Foo.<3u8>` and `Foo.<3usize>` are different
/// instantiations, and comparing only the values would collapse them into one —
/// a mistake nothing would catch until monomorphization tried to emit a single
/// body for two different argument types.
///
/// The value is [`ConstValue`], the const evaluator's own representation, rather
/// than a second one invented here: the evaluator already produces exactly these
/// and a `const` argument is the same kind of thing a `::` binding holds.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConstArg {
    pub ty: Ty,
    pub value: ConstValue,
}

impl Const {
    /// A known value of type `ty`.
    pub fn known(ty: Ty, value: ConstValue) -> Const {
        Const::Value(Box::new(ConstArg { ty, value }))
    }

    /// A length — what an array length always is (§3.2), at the `usize` the
    /// caller found.
    ///
    /// The type is handed in because `usize` is a `core` declaration and this
    /// module has no def table to look one up in.
    pub fn len(n: u64, usize_ty: Ty) -> Const {
        Const::known(usize_ty, ConstValue::Int(n.into()))
    }

    /// The value as a length, when it is already a known non-negative integer.
    pub fn value(&self) -> Option<u64> {
        match self {
            Const::Width(n) => Some(*n as u64),
            Const::Value(a) => match &a.value {
                ConstValue::Int(n) => u64::try_from(n).ok(),
                _ => None,
            },
            _ => None,
        }
    }

    /// A bit width — the `N` of `int.<N>` / `uint.<N>`.
    ///
    /// The number is held **bare**, which is what keeps the representation from
    /// recursing: a width written at a type would be written at `u16`, which is
    /// `uint.<16>`, whose width would be written at `u16`, without end. See
    /// [`Const::Width`].
    pub fn bits_of(n: u16) -> Const {
        Const::Width(n)
    }

    /// The width in bits this const denotes.
    ///
    /// The target is **not** consulted, and no site that asks this question may
    /// consult one: a pointer-sized type is `distinct uint.<PTR_BITS>` over a
    /// constant `core` supplies (§3.1), so its width arrives here as an ordinary
    /// number like any other.
    ///
    /// `None` when the argument is still symbolic — inside a family impl
    /// (`impl <const N: usize> int.<N>`, and its `uint.<N>` twin) no width is
    /// known until monomorphization, and every caller has to say what it does
    /// then rather than invent a number.
    pub fn bits(&self) -> Option<u32> {
        match self {
            Const::Width(n) => Some(*n as u32),
            _ => None,
        }
    }

    /// A short, human-readable rendering (`3`, `true`, `N`, `?c1`, `_`).
    pub fn display(&self, defs: &super::def::DefTable) -> String {
        match self {
            Const::Value(a) => match &a.value {
                ConstValue::Int(n) => n.to_string(),
                ConstValue::Bool(b) => b.to_string(),
                ConstValue::Char(c) => format!("{c:?}"),
                ConstValue::Float(f) => f.to_string(),
                other => format!("{other:?}"),
            },
            Const::Width(n) => n.to_string(),
            Const::Param(d) => defs.get(*d).name.to_string(),
            Const::Var(v) => format!("?c{}", v.0),
            Const::Error => "_".into(),
        }
    }
}

/// A type. Cheap to clone; nested types are boxed.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Ty {
    /// An unsolved inference variable.
    Var(TyVar),
    /// An integer — a member of one of the two families `int.<N>` (signed) and
    /// `uint.<N>` (unsigned), where `N` is the width in bits (§3.1).
    ///
    /// The **width** is a [`Const`] rather than a `u16` because the families are
    /// real generic types: `impl <const N: usize> int.<N>` is how `wrapping_add`
    /// and its neighbours are written once per signedness in `core` instead of
    /// once per width — `u4096` is a legal type, so there is no finite list to
    /// enumerate — and inside that impl the width is not a number yet.
    ///
    /// The **signedness** stays a plain `bool` because it is not an argument at
    /// all: it names which of the two families the type belongs to. Nothing is
    /// ever generic over it, so making it a `Const` would buy an inference
    /// variable that no program could ever solve, and would let `int.<N>` and
    /// `uint.<N>` unify through it.
    ///
    /// `i32` and `u8` are sugar for particular widths, not separate cases — see
    /// [`Ty::int`]. `usize` and `isize` are not sugar at all: they are
    /// `distinct` declarations in `core` over `uint.<PTR_BITS>` / `int.<PTR_BITS>`
    /// (§3.1), so they are [`Ty::Nominal`]s that happen to stand over one of
    /// these.
    Int {
        signed: bool,
        width: Const,
    },
    /// A floating-point number.
    Float(FloatWidth),
    /// An untyped integer literal — arbitrary precision, no runtime
    /// representation. It survives into the IR only for a constant; anything a
    /// program can hold at runtime converts out of it first, which lowering
    /// makes explicit as a `$cast`.
    ComptimeInt,
    /// An untyped float literal, the `comptime_float` counterpart.
    ComptimeFloat,
    /// An untyped string literal: a sequence of bytes with no chosen runtime
    /// representation. Like [`Ty::ComptimeInt`] it survives into the IR only as
    /// the type of a constant and of the literal under a `$cast`; §1.5.
    ComptimeStr,
    Bool,
    Char,
    /// The unit type `void` (the empty tuple).
    Void,
    /// The uninhabited type of a diverging expression (`return`, `break`, an
    /// infinite `loop` with no `break`). Coerces to any type on unification.
    Never,
    /// A named `struct` / `enum` / `trait` / `distinct` type plus its type
    /// arguments (§3.8 nominal identity: equal iff `def` and `args` match).
    Nominal {
        def: DefId,
        args: Vec<Ty>,
    },
    /// `*T` / `*mut T`.
    Ptr {
        mutable: bool,
        inner: Box<Ty>,
    },
    /// `[]T` / `[]mut T`.
    Slice {
        mutable: bool,
        inner: Box<Ty>,
    },
    /// `[N]T` — `len` is the element count, which may still be a `const`
    /// generic parameter or an unsolved inference variable (see [`Const`]).
    Array {
        len: Const,
        mutable: bool,
        inner: Box<Ty>,
    },
    /// `(A, B, ...)` — a tuple. The empty tuple is spelled [`Ty::Void`].
    Tuple(Vec<Ty>),
    /// An anonymous `struct { a: A, b: B }` — a **structural** type (§3.8): two
    /// of them are the same type when their field name→type sets match, and a
    /// named struct with the same fields is a different type.
    ///
    /// The fields are kept **sorted by name**, by every constructor of this
    /// variant ([`Ty::anon_struct`]). §3.8 makes the identity the field *set*,
    /// so `struct { a: i32, b: i32 }` and `struct { b: i32, a: i32 }` have to
    /// be one type — sorting is what makes the derived `PartialEq` say so, and
    /// it fixes a layout order for the two spellings at the same time.
    Struct(Vec<(Symbol, Ty)>),
    /// `func(params) -> ret`.
    Func {
        params: Vec<Ty>,
        ret: Box<Ty>,
    },
    /// `dyn Trait` — a trait object (the trait's [`DefId`]).
    Dyn(DefId),
    /// A type that could not be determined; a diagnostic was already reported.
    /// Unifies with anything so one error does not cascade.
    Error,
}

impl Ty {
    /// `int.<bits>` or `uint.<bits>` — what `i32`, `u8` and `u4096` each spell.
    pub fn int(bits: u16, signed: bool) -> Ty {
        Ty::Int {
            signed,
            width: Const::bits_of(bits),
        }
    }

    /// An anonymous `struct { ... }` from its fields, in whatever order they
    /// were written. The order is **not** part of the type (§3.8 makes the
    /// identity the field set), so the fields are sorted by name here and
    /// nowhere else has to think about it again.
    pub fn anon_struct(mut fields: Vec<(Symbol, Ty)>) -> Ty {
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Ty::Struct(fields)
    }

    /// `u8` — common enough (a byte string's element, a `str`'s
    /// representation) to be worth its own name.
    pub fn u8() -> Ty {
        Ty::int(8, false)
    }

    /// Whether this (already-resolved) type is an integer.
    pub fn is_int(&self) -> bool {
        matches!(self, Ty::Int { .. })
    }

    /// Whether [`Ty::Error`] appears anywhere inside this type.
    ///
    /// What it is for is **diagnostic suppression**, and only that. A `Ty::Error`
    /// is the placeholder inference leaves where it already reported something,
    /// so a later pass that finds one is looking at the consequence of a mistake
    /// rather than at a mistake: `size_of.<u65536>()` is "cannot resolve name
    /// `u65536`" once, not that plus "`<error>` has no known layout". A pass
    /// asks this before speaking, never before working.
    pub fn mentions_error(&self) -> bool {
        match self {
            Ty::Error => true,
            Ty::Ptr { inner, .. } | Ty::Slice { inner, .. } | Ty::Array { inner, .. } => {
                inner.mentions_error()
            }
            Ty::Tuple(elems) => elems.iter().any(Ty::mentions_error),
            Ty::Struct(fields) => fields.iter().any(|(_, t)| t.mentions_error()),
            Ty::Nominal { args, .. } => args.iter().any(Ty::mentions_error),
            Ty::Func { params, ret } => {
                params.iter().any(Ty::mentions_error) || ret.mentions_error()
            }
            _ => false,
        }
    }

    /// The signedness and width in bits of a **concrete** integer type — the
    /// pair every site that has to compute with an integer's range needs.
    ///
    /// `None` for anything that is not an integer, and for an integer whose
    /// width is still symbolic (`int.<N>` inside a family impl). The signedness
    /// is always known — it is which family this is — so the width is the only
    /// half that can be missing. A caller getting `None` has to decide what
    /// "not known until monomorphization" means for it; there is no number it
    /// could be given that would not be a guess.
    pub fn int_parts(&self) -> Option<(bool, u32)> {
        match self {
            Ty::Int { signed, width } => Some((*signed, width.bits()?)),
            _ => None,
        }
    }

    /// Whether this (already-resolved) type is a float.
    pub fn is_float(&self) -> bool {
        matches!(self, Ty::Float(_))
    }

    /// Whether this (already-resolved) type is a **primitive**: a scalar the
    /// compiler knows the values of without consulting a declaration.
    ///
    /// This is the admissible set for a `const` generic parameter (§5). The line
    /// is not arbitrary: a primitive has compile-time values the type system
    /// already compares and substitutes, while an aggregate as a generic
    /// argument would put structural equality of arbitrary values into type
    /// identity — a much larger promise.
    pub fn is_primitive(&self) -> bool {
        matches!(
            self,
            Ty::Int { .. } | Ty::Float(_) | Ty::Bool | Ty::Char
        )
    }

    /// A short, human-readable rendering (`i32`, `*mut Foo`, `(A, B)`, `?3`).
    pub fn display(&self, defs: &super::def::DefTable) -> String {
        match self {
            Ty::Var(v) => format!("?{}", v.0),
            // Print the sugar when the width is known, the family form when it
            // is not. A diagnostic about `i32` must never say `int.<32>` — that
            // is the spelling the *compiler* chose, not the one the program did
            // — while inside a family impl there is no sugar to print, because
            // `N` really is the argument there.
            Ty::Int { signed, width } => match width.bits() {
                Some(n) => format!("{}{n}", if *signed { 'i' } else { 'u' }),
                None => format!(
                    "{}.<{}>",
                    if *signed { "int" } else { "uint" },
                    width.display(defs)
                ),
            },
            Ty::Float(w) => match w {
                FloatWidth::F16 => "f16",
                FloatWidth::F32 => "f32",
                FloatWidth::F64 => "f64",
                FloatWidth::F80 => "f80",
                FloatWidth::F128 => "f128",
            }
            .to_string(),
            Ty::ComptimeInt => "comptime_int".into(),
            Ty::ComptimeFloat => "comptime_float".into(),
            Ty::ComptimeStr => "comptime_str".into(),
            Ty::Bool => "bool".into(),
            Ty::Char => "char".into(),
            Ty::Void => "void".into(),
            Ty::Never => "never".into(),
            // The pointer-sized integers print by their bare name. They are
            // `distinct` declarations in `core` like any other nominal (§3.1),
            // but they stand exactly where a primitive used to: every program
            // writes `usize`, and a diagnostic that says `core.usize` names a
            // path no source ever wrote. Keyed on the `#lang` tag, not on the
            // name, so `core` may still spell them however it likes.
            Ty::Nominal { def, args }
                if args.is_empty()
                    && defs
                        .get(*def)
                        .lang
                        .as_ref()
                        .is_some_and(|l| matches!(l.as_str(), "usize" | "isize")) =>
            {
                defs.get(*def).name.to_string()
            }
            Ty::Nominal { def, args } => {
                let name = defs.canonical_string(*def);
                if args.is_empty() {
                    name
                } else {
                    let inner = args
                        .iter()
                        .map(|a| a.display(defs))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{name}.<{inner}>")
                }
            }
            Ty::Ptr { mutable, inner } => {
                format!(
                    "*{}{}",
                    if *mutable { "mut " } else { "" },
                    inner.display(defs)
                )
            }
            Ty::Slice { mutable, inner } => {
                format!(
                    "[]{}{}",
                    if *mutable { "mut " } else { "" },
                    inner.display(defs)
                )
            }
            Ty::Array {
                len,
                mutable,
                inner,
            } => {
                let l = len.display(defs);
                format!(
                    "[{l}]{}{}",
                    if *mutable { "mut " } else { "" },
                    inner.display(defs)
                )
            }
            Ty::Tuple(elems) => {
                let inner = elems
                    .iter()
                    .map(|e| e.display(defs))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inner})")
            }
            Ty::Struct(fields) => {
                let inner = fields
                    .iter()
                    .map(|(n, t)| format!("{n}: {}", t.display(defs)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("struct {{ {inner} }}")
            }
            Ty::Func { params, ret } => {
                let ps = params
                    .iter()
                    .map(|p| p.display(defs))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("func({ps}) -> {}", ret.display(defs))
            }
            Ty::Dyn(def) => format!("dyn {}", defs.canonical_string(*def)),
            Ty::Error => "<error>".into(),
        }
    }
}

/// The result of a unification attempt.
pub type UnifyResult = Result<(), (Ty, Ty)>;

/// A pending type-class fact the plain union-find cannot decide on its own — it
/// needs a **search** (which `impl` satisfies this?), possibly deferred until a
/// variable is solved. The solver ([`super::infer`]) drains these to a fixpoint
/// after each function body (§ trait selection).
///
/// This is the piece pure unification is missing: re-running [`InferCtxt::unify`]
/// finds no new equalities, but selecting an impl and projecting its associated
/// types can both solve variables *and* unblock further obligations.
#[derive(Debug, Clone)]
pub enum Obligation {
    /// `self_ty : Trait.<args>` must hold — some in-scope `impl` (or builtin)
    /// selects. Used for operator/method traits without a projected result
    /// (e.g. `Eq`, `Ord`).
    Trait {
        self_ty: Ty,
        trait_def: DefId,
        args: Vec<Ty>,
        /// The AST node that raised the obligation (for diagnostics).
        origin: NodeId,
        /// When set, the selected impl's member of this name is stamped onto
        /// `origin` so lowering can emit the call it resolved to. This is how a
        /// *static* trait call — one with no receiver to dispatch on, such as
        /// `$from_residual`'s `FromResidual.from_residual` — reaches the IR.
        stamp: Option<Symbol>,
    },
    /// `<self_ty as Trait.<args>>.assoc == out` — the projected associated type.
    /// Solving it selects the impl (proving the `Trait` bound too), substitutes
    /// the impl's solved generics into its `assoc` binding, and unifies with
    /// `out`.
    Projection {
        self_ty: Ty,
        trait_def: DefId,
        args: Vec<Ty>,
        assoc: Symbol,
        out: Ty,
        origin: NodeId,
        /// The trait method this projection's operator calls (`"add"`, `"neg"`,
        /// `"index"`, …), when it has one. Fulfillment stamps the member the
        /// selected impl supplies onto `origin`, so lowering emits a uniform
        /// [`crate::ir::Expr::Call`] whether a builtin or a user impl won.
        method: Option<Symbol>,
    },
    /// A variant literal (`.some(x)`) whose enum is only known from context:
    /// once `recv` resolves to a `Nominal` enum, unify each payload argument
    /// with the variant's (generic-substituted) declared payload type. `args`
    /// entries carry a field name for record variants, `None` for tuple ones.
    VariantPayload {
        recv: Ty,
        variant: Symbol,
        args: Vec<(Option<Symbol>, Ty)>,
        origin: NodeId,
    },
    /// A composite literal whose target type is only known from context — the
    /// inferred `.{ ... }` form. Once `recv` resolves, the body is matched
    /// against it: named fields against a struct's declared field types,
    /// positional elements against an array/slice element or a tuple's members.
    ///
    /// The literal cannot be checked eagerly because its type flows *in* (from
    /// an annotation, a parameter, or a return type), so this defers the whole
    /// body until that type is available.
    CompositeBody {
        recv: Ty,
        /// The literal's body node, re-read when the obligation is discharged.
        origin: NodeId,
    },
    /// A comparison whose operand type was still a variable when the operator
    /// was typed — `ms[i].name == s`, where the left side is `Index.Output`
    /// until the impl is selected.
    ///
    /// Whether `==` is a machine instruction or a call to `Eq.eq` is decided by
    /// what the operand *is*, so asking before it is known gets the wrong
    /// answer silently: a `str` compared as a machine word rather than by its
    /// bytes. This defers the question to when the type has one.
    Comparison {
        self_ty: Ty,
        op: crate::parser::ast::BinOp,
        origin: NodeId,
    },
    /// `recv.name` where `recv`'s type is not known **yet** — `ms[i].name`,
    /// whose base is `Index.Output` until the impl is selected.
    ///
    /// Asking eagerly answers `Ty::Error`, which unifies with everything and so
    /// says nothing; the field's type then reaches the rest of inference as an
    /// error that was never reported. This defers the lookup to when the base
    /// has a type, and `out` is what the access was given in the meantime.
    Field {
        recv: Ty,
        name: Symbol,
        out: Ty,
        origin: NodeId,
    },
}

/// A restorable checkpoint of the union-find, so the solver can trial-unify a
/// candidate impl and roll back if it does not fit.
#[derive(Debug)]
pub struct Snapshot {
    subst: Vec<Option<Ty>>,
    kinds_len: usize,
    const_subst: Vec<Option<Const>>,
}

/// The union-find substitution and variable bookkeeping for one inference run
/// (one function body, in practice — see [`super::infer`]).
#[derive(Debug, Default)]
pub struct InferCtxt {
    /// `subst[v]` is `Some(ty)` once variable `v` is solved, else `None`.
    subst: Vec<Option<Ty>>,
    /// The kind of each variable, parallel to `subst`.
    kinds: Vec<TyVarKind>,
    /// Variables that have been joined with [`Ty::Never`] and nothing else yet.
    ///
    /// `never` absorbs rather than binds (see `unify`), so a variable it meets
    /// stays free for the *other* contributors to a join to solve. When there
    /// are none — every arm of a `match` diverges, both branches of an `if` do —
    /// the variable would otherwise finalize as "type annotations needed" for
    /// code that is complete and correct. It finalizes as `never` instead, which
    /// is the right answer: a join of nothing but diverging paths diverges.
    saw_never: std::collections::HashSet<TyVar>,
    /// The same union-find, one level down, for the compile-time *values* that
    /// appear in types: `const_subst[c]` is `Some(k)` once const variable `c` is
    /// solved. Kept separate from `subst` because a [`Const`] and a [`Ty`] never
    /// unify with each other.
    const_subst: Vec<Option<Const>>,
    /// Associated-type bindings (`Trait.Assoc` name → chosen type) gathered from
    /// `<Assoc = T>` turbofish arguments. Keyed by the associated item name; a
    /// pragmatic flat map sufficient for the bootstrap.
    pub assoc: HashMap<Symbol, Ty>,
    /// Pending trait/projection [`Obligation`]s, drained to a fixpoint by the
    /// solver after each function body.
    obligations: Vec<Obligation>,
    /// Every `distinct` type whose representation is numeric, and which family
    /// it belongs to (§2.4).
    ///
    /// A numeric-literal variable is allowed to become one of these, so
    /// `const p: HttpPort := 80` works for `HttpPort :: distinct u16` exactly as
    /// it does for `u16` itself. Unification cannot work that out on its own —
    /// it has no [`DefTable`](super::def::DefTable) and a `distinct` type is an
    /// ordinary [`Ty::Nominal`] here — so the answer is computed once, up front,
    /// and handed in.
    numeric_distincts: HashMap<DefId, Ty>,
    /// The `#lang("str")` type, when the program has one.
    ///
    /// Handed in for the same reason [`InferCtxt::numeric_distincts`] is:
    /// unification decides what a string-literal variable may become, and `str`
    /// is an ordinary [`Ty::Nominal`] here — found by tag, never by name, so
    /// only the caller can say which one it is (§1.5).
    str_ty: Option<Ty>,
    /// The `#lang("usize")` / `#lang("isize")` types. See
    /// [`InferCtxt::set_ptr_int_tys`].
    ptr_ints: Option<(Ty, Ty)>,
}

impl InferCtxt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record which `distinct` types stand over a numeric representation, so a
    /// numeric-literal variable may become one. Computed once per program by
    /// [`super::infer::infer_file`] and installed into every context it builds.
    pub fn set_numeric_distincts(&mut self, m: HashMap<DefId, Ty>) {
        self.numeric_distincts = m;
    }

    /// Record the `#lang("str")` type a string literal defaults to. Computed
    /// once per program by [`super::infer::infer_file`], like
    /// [`InferCtxt::set_numeric_distincts`].
    pub fn set_str_ty(&mut self, ty: Option<Ty>) {
        self.str_ty = ty;
    }

    /// Record the `#lang("usize")` / `#lang("isize")` types, in that order.
    ///
    /// They are ordinary `distinct` declarations in `core` (§3.1) rather than
    /// primitives, so — exactly as for `str` — unification cannot find them: it
    /// has no def table and cannot do a `#lang` lookup. The one place it needs
    /// them is defaulting an unconstrained integer literal to `isize`.
    pub fn set_ptr_int_tys(&mut self, tys: Option<(Ty, Ty)>) {
        self.ptr_ints = tys;
    }

    /// `usize`, when `core` declared one.
    pub fn usize_ty(&self) -> Option<Ty> {
        self.ptr_ints.as_ref().map(|(u, _)| u.clone())
    }

    /// `isize`, when `core` declared one.
    pub fn isize_ty(&self) -> Option<Ty> {
        self.ptr_ints.as_ref().map(|(_, i)| i.clone())
    }

    /// Whether `ty` is one of the three types a string literal may become
    /// (§1.5): `str` itself, `[]u8`, or `[]char`.
    ///
    /// The slices are the immutable ones on purpose. A literal lives in
    /// read-only data, so handing one out as `[]mut u8` would offer a write to
    /// memory that cannot take it.
    pub fn admits_str(&self, ty: &Ty) -> bool {
        if self.str_ty.as_ref() == Some(ty) {
            return true;
        }
        match ty {
            Ty::Slice {
                mutable: false,
                inner,
            } => **inner == Ty::Char || **inner == Ty::u8(),
            _ => false,
        }
    }

    /// Render `ty` for a **diagnostic**, naming an unsolved literal variable as
    /// the comptime type it is rather than as `?0`.
    ///
    /// `f("hi")` against a `[]mut u8` parameter is a mismatch whose right-hand
    /// side is still a variable, because a literal only settles when something
    /// accepts it — and "found `?0`" names the compiler's bookkeeping instead of
    /// the program. "found `comptime_str`" is the same fact in the language's
    /// own words.
    pub fn describe(&self, ty: &Ty, defs: &super::def::DefTable) -> String {
        self.name_literals(ty).display(defs)
    }

    /// [`InferCtxt::resolve`], with every still-unsolved literal variable
    /// replaced by its comptime type.
    fn name_literals(&self, ty: &Ty) -> Ty {
        match self.resolve(ty) {
            Ty::Var(v) => match self.kind(v) {
                TyVarKind::Int => Ty::ComptimeInt,
                TyVarKind::Float => Ty::ComptimeFloat,
                TyVarKind::Str => Ty::ComptimeStr,
                TyVarKind::General => Ty::Var(v),
            },
            Ty::Ptr { mutable, inner } => Ty::Ptr {
                mutable,
                inner: Box::new(self.name_literals(&inner)),
            },
            Ty::Slice { mutable, inner } => Ty::Slice {
                mutable,
                inner: Box::new(self.name_literals(&inner)),
            },
            Ty::Array {
                len,
                mutable,
                inner,
            } => Ty::Array {
                len,
                mutable,
                inner: Box::new(self.name_literals(&inner)),
            },
            Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|e| self.name_literals(e)).collect()),
            Ty::Struct(fields) => Ty::Struct(
                fields
                    .iter()
                    .map(|(n, t)| (n.clone(), self.name_literals(t)))
                    .collect(),
            ),
            Ty::Func { params, ret } => Ty::Func {
                params: params.iter().map(|p| self.name_literals(p)).collect(),
                ret: Box::new(self.name_literals(&ret)),
            },
            Ty::Nominal { def, args } => Ty::Nominal {
                def,
                args: args.iter().map(|a| self.name_literals(a)).collect(),
            },
            other => other,
        }
    }

    /// Settle an open `comptime_str` variable on `str`, returning the type
    /// either way. Anything else is returned resolved and untouched.
    ///
    /// Called where a use is about to dispatch on the type — see
    /// [`super::infer::Inferer::pin_str`], which is the caller that explains
    /// why.
    pub fn pin_str(&mut self, ty: &Ty) -> Ty {
        let resolved = self.shallow(ty);
        let Ty::Var(v) = resolved else {
            return resolved;
        };
        if self.kind(v) != TyVarKind::Str {
            return Ty::Var(v);
        }
        match self.str_ty.clone() {
            Some(str_ty) => {
                let _ = self.unify(&Ty::Var(v), &str_ty);
                str_ty
            }
            None => Ty::Var(v),
        }
    }

    /// Pin an unsolved numeric literal to its default (`isize` / `f64`), for
    /// the reason [`Cx::pin_str`] pins a string literal: a method call has to
    /// ask a question only a concrete type can answer.
    ///
    /// A `comptime_int` is not a type any impl is written for, so a lookup
    /// against the open variable finds nothing at all. Settling it here is the
    /// same collapse [`Cx::finalize`] performs at the end of the body, only
    /// early enough for the call to see it — which means a method call on a
    /// literal reaches exactly the impls a `let x: isize` would.
    pub fn pin_numeric(&mut self, ty: &Ty) -> Ty {
        let resolved = self.shallow(ty);
        let Ty::Var(v) = resolved else {
            return resolved;
        };
        let default = match self.kind(v) {
            TyVarKind::Int => self.isize_ty(),
            TyVarKind::Float => Some(Ty::Float(FloatWidth::F64)),
            _ => return Ty::Var(v),
        };
        // No `#lang` `isize` means a program without `core`; leaving the
        // variable open keeps that a single diagnostic, reported elsewhere.
        match default {
            Some(d) => {
                let _ = self.unify(&Ty::Var(v), &d);
                d
            }
            None => Ty::Var(v),
        }
    }

    /// Whether `ty` is a `distinct` type standing over a numeric primitive, and
    /// which family. `None` for everything else.
    pub fn numeric_distinct_kind(&self, ty: &Ty) -> Option<TyVarKind> {
        Some(match self.numeric_distinct_repr(ty)? {
            Ty::Int { .. } => TyVarKind::Int,
            Ty::Float(_) => TyVarKind::Float,
            _ => return None,
        })
    }

    /// The **primitive** a `distinct` numeric type ultimately stands over,
    /// following a chain of them. `None` for everything else.
    ///
    /// A literal that settles on a `distinct` numeric has to be range-checked
    /// against that primitive: `HttpPort :: distinct u16` is a `u16` in every
    /// way that concerns whether `70000` fits, and §2.4's whole point is that
    /// the literal reaches it *without* a written cast — so nothing else would
    /// check it.
    pub fn numeric_distinct_repr(&self, ty: &Ty) -> Option<Ty> {
        match ty {
            Ty::Nominal { def, .. } => self.numeric_distincts.get(def).cloned(),
            _ => None,
        }
    }

    /// Allocate a fresh general-purpose variable.
    pub fn fresh(&mut self) -> Ty {
        self.fresh_of(TyVarKind::General)
    }

    /// Allocate a fresh variable of a given kind (numeric literals use
    /// [`TyVarKind::Int`] / [`TyVarKind::Float`]).
    pub fn fresh_of(&mut self, kind: TyVarKind) -> Ty {
        let v = TyVar(self.subst.len() as u32);
        self.subst.push(None);
        self.kinds.push(kind);
        Ty::Var(v)
    }

    /// Allocate a fresh const-generic variable — an array length still to be
    /// decided (a `[_]T` hole, or a `<const N>` parameter at a call site).
    pub fn fresh_const(&mut self) -> Const {
        let v = ConstVar(self.const_subst.len() as u32);
        self.const_subst.push(None);
        Const::Var(v)
    }

    /// Follow bound const variables to the current representative.
    pub fn shallow_const(&self, k: &Const) -> Const {
        let mut cur = k.clone();
        while let Const::Var(v) = cur {
            match &self.const_subst[v.0 as usize] {
                Some(bound) => cur = bound.clone(),
                None => return Const::Var(v),
            }
        }
        cur
    }

    /// Unify two compile-time values. Only equal values (or a variable and
    /// anything) agree — there is no const-expression arithmetic, so a `[N]T`
    /// and a `[3]T` with `N` still symbolic are simply not the same type.
    pub fn unify_const(&mut self, a: &Const, b: &Const) -> Result<(), (Const, Const)> {
        let a = self.shallow_const(a);
        let b = self.shallow_const(b);
        match (&a, &b) {
            // An errored length absorbs, so one bad `[expr]T` does not cascade.
            (Const::Error, _) | (_, Const::Error) => Ok(()),
            (Const::Var(x), Const::Var(y)) if x == y => Ok(()),
            (Const::Var(v), other) | (other, Const::Var(v)) => {
                self.const_subst[v.0 as usize] = Some((*other).clone());
                Ok(())
            }
            // Both halves of a known argument are identity (§5): `3u8` and
            // `3usize` are different arguments, so comparing the values alone
            // would make `Foo.<3u8>` and `Foo.<3usize>` one type.
            (Const::Value(x), Const::Value(y)) if x == y => Ok(()),
            (Const::Param(x), Const::Param(y)) if x == y => Ok(()),
            // Two widths agree when they are the same number. A width carries no
            // type to compare, because a width has only one type (§3.1).
            (Const::Width(x), Const::Width(y)) if x == y => Ok(()),

            _ => Err((a, b)),
        }
    }

    fn kind(&self, v: TyVar) -> TyVarKind {
        self.kinds[v.0 as usize]
    }

    /// The kind of a type that shallow-resolves to a variable, else `None`.
    pub fn var_kind(&self, ty: &Ty) -> Option<TyVarKind> {
        match self.shallow(ty) {
            Ty::Var(v) => Some(self.kind(v)),
            _ => None,
        }
    }

    // ===< obligations >===

    /// Queue an [`Obligation`] for the solver to discharge later. Unification is
    /// left untouched — this is the separate search layer (§ trait selection).
    pub fn register(&mut self, obligation: Obligation) {
        self.obligations.push(obligation);
    }

    /// Take the pending obligations, leaving the queue empty. The solver
    /// re-registers any it could not yet decide.
    pub fn take_obligations(&mut self) -> Vec<Obligation> {
        std::mem::take(&mut self.obligations)
    }

    /// Whether any obligation is still queued.
    pub fn has_obligations(&self) -> bool {
        !self.obligations.is_empty()
    }

    // ===< trial checkpoints >===

    /// Capture the union-find state so a speculative unification can be undone.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            subst: self.subst.clone(),
            kinds_len: self.kinds.len(),
            const_subst: self.const_subst.clone(),
        }
    }

    /// Restore a [`Snapshot`], discarding every binding and fresh variable made
    /// since it was taken.
    pub fn rollback(&mut self, snap: Snapshot) {
        self.subst = snap.subst;
        self.kinds.truncate(snap.kinds_len);
        self.const_subst = snap.const_subst;
    }

    /// Follow bound variables to the current representative (one level of the
    /// structure; nested types are resolved lazily by [`InferCtxt::resolve`]).
    pub fn shallow(&self, ty: &Ty) -> Ty {
        let mut cur = ty.clone();
        while let Ty::Var(v) = cur {
            match &self.subst[v.0 as usize] {
                Some(bound) => cur = bound.clone(),
                None => return Ty::Var(v),
            }
        }
        cur
    }

    /// Fully apply the substitution, replacing every solved variable throughout
    /// the structure ("zonking"). Unsolved variables remain as [`Ty::Var`].
    pub fn resolve(&self, ty: &Ty) -> Ty {
        let ty = self.shallow(ty);
        match ty {
            Ty::Ptr { mutable, inner } => Ty::Ptr {
                mutable,
                inner: Box::new(self.resolve(&inner)),
            },
            Ty::Slice { mutable, inner } => Ty::Slice {
                mutable,
                inner: Box::new(self.resolve(&inner)),
            },
            Ty::Array {
                len,
                mutable,
                inner,
            } => Ty::Array {
                len: self.shallow_const(&len),
                mutable,
                inner: Box::new(self.resolve(&inner)),
            },
            // An integer's width is a variable like any other while a family
            // impl's call sites are being solved, so zonking has to reach it.
            Ty::Int { signed, width } => Ty::Int {
                signed,
                width: self.shallow_const(&width),
            },
            Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|e| self.resolve(e)).collect()),
            // Already sorted — resolving a field's type cannot change its name,
            // so this rebuilds the variant directly rather than through
            // `anon_struct` and its sort.
            Ty::Struct(fields) => Ty::Struct(
                fields
                    .iter()
                    .map(|(n, t)| (n.clone(), self.resolve(t)))
                    .collect(),
            ),
            Ty::Func { params, ret } => Ty::Func {
                params: params.iter().map(|p| self.resolve(p)).collect(),
                ret: Box::new(self.resolve(&ret)),
            },
            Ty::Nominal { def, args } => Ty::Nominal {
                def,
                args: args.iter().map(|a| self.resolve(a)).collect(),
            },
            other => other,
        }
    }

    /// Unify two types, solving variables as needed. On mismatch returns the two
    /// (resolved) types that clashed so the caller can render a diagnostic.
    pub fn unify(&mut self, a: &Ty, b: &Ty) -> UnifyResult {
        let a = self.shallow(a);
        let b = self.shallow(b);
        match (&a, &b) {
            // Errors and `never` absorb: they unify with anything without
            // producing further diagnostics.
            //
            // `never` absorbs **both ways here on purpose**, even though the
            // language rule is one-way (`never` converts to everything, nothing
            // converts to `never` — §3.1). `unify` is the symmetric operation:
            // most of its callers are *joins* — the two arms of an `if`, the
            // arms of a `match`, an operator's output against its expected type
            // — where one side being `never` should simply yield the other, and
            // a direction would be meaningless. The one-way rule belongs where a
            // direction exists, which is `Inferer::expect`; see the guard there.
            (Ty::Error, _) | (_, Ty::Error) => Ok(()),

            // A join against a still-free variable: absorb, but *remember*. The
            // variable must stay free so a later contributor can solve it —
            // binding it to `never` here would make `if c { diverge() } else { 1 }`
            // a `never` — while a variable that meets nothing else has no answer
            // but `never`. See `saw_never`.
            (Ty::Never, Ty::Var(v)) | (Ty::Var(v), Ty::Never) => {
                self.saw_never.insert(*v);
                Ok(())
            }
            (Ty::Never, _) | (_, Ty::Never) => Ok(()),

            (Ty::Var(x), Ty::Var(y)) if x == y => Ok(()),
            (Ty::Var(v), _) => self.bind(*v, &b),
            (_, Ty::Var(v)) => self.bind(*v, &a),

            // Two integers agree when they are the same family and their widths
            // do. The width goes through `unify_const` rather than `==` so that
            // one which is still a variable gets *solved* — `uint.<N>` meeting
            // `u8` is how a call on the family impl learns `N = 8`. The
            // signedness is compared, never solved: nothing is generic over it,
            // so `int.<N>` and `uint.<N>` must simply not unify.
            //
            // The error is reported at the `Ty` level even so: "expected `i32`,
            // found `u8`" is what the program can act on, where "expected `32`,
            // found `8`" names an argument the program never wrote.
            (
                Ty::Int {
                    signed: s1,
                    width: w1,
                },
                Ty::Int {
                    signed: s2,
                    width: w2,
                },
            ) => {
                if s1 != s2 || self.unify_const(w1, w2).is_err() {
                    Err((a, b))
                } else {
                    Ok(())
                }
            }
            (Ty::Float(x), Ty::Float(y)) if x == y => Ok(()),
            (Ty::Bool, Ty::Bool) | (Ty::Char, Ty::Char) | (Ty::Void, Ty::Void) => Ok(()),

            (
                Ty::Ptr {
                    mutable: m1,
                    inner: i1,
                },
                Ty::Ptr {
                    mutable: m2,
                    inner: i2,
                },
            ) => {
                // `*mut T` coerces to `*T` (§3.8): allow when the target is not
                // asking for more permission than the source has.
                if *m1 == *m2 || (*m1 && !*m2) {
                    self.unify(i1, i2)
                } else {
                    Err((a.clone(), b.clone()))
                }
            }
            (
                Ty::Slice {
                    mutable: m1,
                    inner: i1,
                },
                Ty::Slice {
                    mutable: m2,
                    inner: i2,
                },
            ) => {
                if *m1 == *m2 || (*m1 && !*m2) {
                    self.unify(i1, i2)
                } else {
                    Err((a.clone(), b.clone()))
                }
            }
            (
                Ty::Array {
                    len: l1,
                    mutable: m1,
                    inner: i1,
                },
                Ty::Array {
                    len: l2,
                    mutable: m2,
                    inner: i2,
                },
            ) => {
                // The length is part of the type (§3.8): `[3]i32` and `[4]i32`
                // are different types, and a `[_]T` hole is solved here.
                if m1 != m2 || self.unify_const(l1, l2).is_err() {
                    return Err((a.clone(), b.clone()));
                }
                self.unify(i1, i2)
            }
            (Ty::Tuple(xs), Ty::Tuple(ys)) if xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys) {
                    self.unify(x, y)?;
                }
                Ok(())
            }
            // Both sides are sorted by name, so equal field *sets* line up
            // position by position and a single zip decides it.
            (Ty::Struct(xs), Ty::Struct(ys))
                if xs.len() == ys.len()
                    && xs.iter().zip(ys).all(|((n, _), (m, _))| n == m) =>
            {
                for ((_, x), (_, y)) in xs.iter().zip(ys) {
                    self.unify(x, y)?;
                }
                Ok(())
            }
            (
                Ty::Func {
                    params: p1,
                    ret: r1,
                },
                Ty::Func {
                    params: p2,
                    ret: r2,
                },
            ) if p1.len() == p2.len() => {
                for (x, y) in p1.iter().zip(p2) {
                    self.unify(x, y)?;
                }
                self.unify(r1, r2)
            }
            (Ty::Nominal { def: d1, args: a1 }, Ty::Nominal { def: d2, args: a2 })
                if d1 == d2 && a1.len() == a2.len() =>
            {
                for (x, y) in a1.iter().zip(a2) {
                    self.unify(x, y)?;
                }
                Ok(())
            }
            (Ty::Dyn(d1), Ty::Dyn(d2)) if d1 == d2 => Ok(()),

            _ => Err((a, b)),
        }
    }

    /// Bind variable `v` to `ty`, honoring the variable's kind (a numeric
    /// variable rejects an incompatible concrete type) and the occurs check.
    fn bind(&mut self, v: TyVar, ty: &Ty) -> UnifyResult {
        // `v := v` is a no-op.
        if let Ty::Var(w) = ty {
            if *w == v {
                return Ok(());
            }
        }
        // Numeric-literal variables constrain what they may become.
        match self.kind(v) {
            TyVarKind::Int => match ty {
                Ty::Int { .. } => {}
                // A `distinct` type over an integer is one, for the purpose of
                // what a literal may become (§2.4). Without this a `distinct`
                // numeric would be unusable: every literal assigned to one would
                // need a `$cast`, which is precisely the ceremony the type is
                // meant to buy back.
                Ty::Nominal { def, .. }
                    if matches!(self.numeric_distincts.get(def), Some(Ty::Int { .. })) => {}
                // Two int literals meeting: keep the other variable numeric too.
                Ty::Var(w) if self.kind(*w) == TyVarKind::Int => {}
                Ty::Var(w) if self.kind(*w) == TyVarKind::General => {
                    // Push the integer requirement onto the general var by
                    // binding it back to this numeric var instead.
                    return self.bind_raw(*w, &Ty::Var(v));
                }
                _ => return Err((Ty::Var(v), ty.clone())),
            },
            TyVarKind::Float => match ty {
                Ty::Float(_) => {}
                Ty::Nominal { def, .. }
                    if matches!(self.numeric_distincts.get(def), Some(Ty::Float(_))) => {}
                Ty::Var(w) if self.kind(*w) == TyVarKind::Float => {}
                Ty::Var(w) if self.kind(*w) == TyVarKind::General => {
                    return self.bind_raw(*w, &Ty::Var(v));
                }
                _ => return Err((Ty::Var(v), ty.clone())),
            },
            TyVarKind::Str => match ty {
                _ if self.admits_str(ty) => {}
                Ty::Var(w) if self.kind(*w) == TyVarKind::Str => {}
                Ty::Var(w) if self.kind(*w) == TyVarKind::General => {
                    return self.bind_raw(*w, &Ty::Var(v));
                }
                _ => return Err((Ty::Var(v), ty.clone())),
            },
            TyVarKind::General => {}
        }
        if self.occurs(v, ty) {
            return Err((Ty::Var(v), ty.clone()));
        }
        self.bind_raw(v, ty)
    }

    fn bind_raw(&mut self, v: TyVar, ty: &Ty) -> UnifyResult {
        self.subst[v.0 as usize] = Some(ty.clone());
        Ok(())
    }

    /// Occurs check: does `v` appear anywhere inside `ty` (after resolution)?
    fn occurs(&self, v: TyVar, ty: &Ty) -> bool {
        match self.shallow(ty) {
            Ty::Var(w) => w == v,
            Ty::Ptr { inner, .. } | Ty::Slice { inner, .. } | Ty::Array { inner, .. } => {
                self.occurs(v, &inner)
            }
            Ty::Tuple(elems) => elems.iter().any(|e| self.occurs(v, e)),
            Ty::Struct(fields) => fields.iter().any(|(_, t)| self.occurs(v, t)),
            Ty::Func { params, ret } => {
                params.iter().any(|p| self.occurs(v, p)) || self.occurs(v, &ret)
            }
            Ty::Nominal { args, .. } => args.iter().any(|a| self.occurs(v, a)),
            _ => false,
        }
    }

    /// The length counterpart of [`InferCtxt::finalize`]. A const variable left
    /// unsolved is an ambiguity too — nothing said how long the array is — and
    /// becomes [`Const::Error`] so it does not cascade.
    pub fn finalize_const(&mut self, k: &Const, on_ambiguous: &mut dyn FnMut()) -> Const {
        match self.shallow_const(k) {
            Const::Var(_) => {
                on_ambiguous();
                Const::Error
            }
            other => other,
        }
    }

    /// Resolve `ty`, then default any still-unsolved numeric variable to its
    /// fallback (`isize` / `f64`). A remaining **general** variable is reported
    /// through `on_ambiguous` and rendered as [`Ty::Error`]. Used at the end of a
    /// function body to finalize every node's type.
    pub fn finalize(&mut self, ty: &Ty, on_ambiguous: &mut dyn FnMut()) -> Ty {
        let ty = self.shallow(ty);
        match ty {
            Ty::Var(v) => {
                let default = match self.kind(v) {
                    // `isize` is a `core` declaration now, so a program with no
                    // `core` has nothing to default to; `Ty::Error` there is
                    // already the shape for "reported elsewhere".
                    TyVarKind::Int => Some(self.isize_ty().unwrap_or(Ty::Error)),
                    TyVarKind::Float => Some(Ty::Float(FloatWidth::F64)),
                    // A string literal nothing pinned is a `str`. When the
                    // program has no `#lang("str")` item there is nothing to
                    // default *to*; that is already an error reported where the
                    // literal was typed, so leave it ambiguous rather than
                    // inventing a second complaint.
                    TyVarKind::Str => self.str_ty.clone(),
                    // A general variable whose only constraint was `never`.
                    TyVarKind::General if self.saw_never.contains(&v) => Some(Ty::Never),
                    TyVarKind::General => None,
                };
                match default {
                    Some(d) => {
                        self.subst[v.0 as usize] = Some(d.clone());
                        d
                    }
                    None => {
                        on_ambiguous();
                        Ty::Error
                    }
                }
            }
            Ty::Ptr { mutable, inner } => Ty::Ptr {
                mutable,
                inner: Box::new(self.finalize(&inner, on_ambiguous)),
            },
            Ty::Slice { mutable, inner } => Ty::Slice {
                mutable,
                inner: Box::new(self.finalize(&inner, on_ambiguous)),
            },
            Ty::Array {
                len,
                mutable,
                inner,
            } => Ty::Array {
                len: self.finalize_const(&len, on_ambiguous),
                mutable,
                inner: Box::new(self.finalize(&inner, on_ambiguous)),
            },
            Ty::Int { signed, width } => Ty::Int {
                signed,
                width: self.finalize_const(&width, on_ambiguous),
            },
            Ty::Tuple(elems) => Ty::Tuple(
                elems
                    .iter()
                    .map(|e| self.finalize(e, on_ambiguous))
                    .collect(),
            ),
            Ty::Struct(fields) => Ty::Struct(
                fields
                    .iter()
                    .map(|(n, t)| (n.clone(), self.finalize(t, on_ambiguous)))
                    .collect(),
            ),
            Ty::Func { params, ret } => Ty::Func {
                params: params
                    .iter()
                    .map(|p| self.finalize(p, on_ambiguous))
                    .collect(),
                ret: Box::new(self.finalize(&ret, on_ambiguous)),
            },
            Ty::Nominal { def, args } => Ty::Nominal {
                def,
                args: args
                    .iter()
                    .map(|a| self.finalize(a, on_ambiguous))
                    .collect(),
            },
            other => other,
        }
    }
}

/// The type an integer **width** is read at (§3.1): a `u16`, because a width is
/// capped at 65535 and nothing wider could ever be a legal one.
pub fn width_ty() -> Ty {
    Ty::int(16, false)
}

/// Whether every value of one integer type is a value of another — the rule for
/// **implicit widening** (§3.1).
///
/// A narrower integer may stand where a wider one is wanted, because no value is
/// lost doing it; the reverse may not, because most values would be. That is the
/// whole rule, and it is stated as a range containment rather than as "more
/// bits" so that the signed/unsigned cases fall out of it instead of being
/// special-cased:
///
///   - **Same signedness**, more bits: always. `u8` into `u32`, `i8` into `i64`.
///   - **Unsigned into signed** needs one extra bit for the sign, so it must be
///     *strictly* wider: `u8` into `i16` yes, `u8` into `i8` no — `255` has
///     nowhere to go in an `i8`.
///   - **Signed into unsigned**: never, at any width. A negative value has no
///     representation at all, and widening must not be the thing that decides
///     what `-1 as u32` means.
///
/// Note this is about *types*, not values: a `comptime_int` literal reaching a
/// runtime type is a different rule (§1.5) and is checked against the literal's
/// actual value by [`int_fits`], which is strictly more permissive.
pub fn int_widens(from: (bool, u32), to: (bool, u32)) -> bool {
    let ((from_signed, from_bits), (to_signed, to_bits)) = (from, to);
    match (from_signed, to_signed) {
        (false, false) | (true, true) => to_bits >= from_bits,
        (false, true) => to_bits > from_bits,
        (true, false) => false,
    }
}

/// Whether `value` is representable in an integer type of this width and
/// signedness — the "coerces to any integer type **it fits**" rule for a
/// `comptime_int`.
///
/// Takes a resolved `bits` rather than an integer type, because resolving one
/// is where the interesting decision is: a symbolic width has no answer at all.
/// Callers get the pair from [`Ty::int_parts`] and say for themselves what
/// `None` means for them.
pub fn int_fits(value: &BigInt, signed: bool, bits: u32) -> bool {
    if bits == 0 {
        return false;
    }
    if signed {
        // -2^(n-1) ..= 2^(n-1) - 1
        let limit = BigInt::from(1) << (bits - 1);
        *value >= -&limit && *value < limit
    } else {
        // 0 ..= 2^n - 1
        *value >= BigInt::from(0) && *value < (BigInt::from(1) << bits)
    }
}

/// Whether `value` survives the conversion to a float of this width — the
/// float counterpart of [`int_fits`], and the rule an **implicit** conversion
/// has to satisfy (§1.5).
///
/// "Fits" is deliberately not "is represented exactly". Almost no decimal
/// fraction is exactly a binary float — `0.1` is not one in `f64` any more than
/// in `f32` — so requiring exactness would reject nearly every float literal
/// ever written. What is rejected is a conversion that loses the number
/// *entirely*: a finite value that overflows to infinity, and a non-zero value
/// that underflows to zero. Those two are the cases where the type cannot hold
/// the written number at all, and where silently continuing would make the
/// program mean something the source never said.
///
/// `f16` is range-checked against its extremes rather than rounded, and `f80` /
/// `f128` accept whatever an `f64` already holds: the compiler's own storage for
/// a compile-time float is an `f64` (see [`crate::parser::ast::Lit::Float`]), so
/// a literal needing more than that is caught earlier, by `WideFloat`.
pub fn float_fits(value: f64, width: FloatWidth) -> bool {
    if !value.is_finite() {
        // An `f64` that is already infinite means the literal overflowed on the
        // way in; no width the compiler can store recovers it.
        return false;
    }
    if value == 0.0 {
        return true;
    }
    match width {
        FloatWidth::F32 => {
            let narrowed = value as f32;
            narrowed.is_finite() && narrowed != 0.0
        }
        // The largest finite `f16` and the smallest positive subnormal one.
        FloatWidth::F16 => value.abs() <= F16_MAX && value.abs() >= F16_MIN_SUBNORMAL,
        FloatWidth::F64 | FloatWidth::F80 | FloatWidth::F128 => true,
    }
}

/// The largest finite `f16` (2^15 × (2 − 2^−10)).
const F16_MAX: f64 = 65504.0;

/// The smallest positive subnormal `f16` (2^−24). Anything smaller rounds to
/// zero.
const F16_MIN_SUBNORMAL: f64 = 5.960_464_477_539_063e-8;

/// Truncate `value` to `width` the way the machine does: keep the low bits and
/// reinterpret them with the target's signedness.
///
/// This is what an **explicit** `$cast` means. The program asked for the
/// narrowing and a run-time cast would do exactly this, so a compile-time one
/// that refused — or that produced some other number — would make a constant
/// disagree with the same expression evaluated at run time.
pub fn int_truncate(value: &BigInt, signed: bool, bits: u32) -> BigInt {
    if bits == 0 {
        return BigInt::from(0);
    }
    let modulus = BigInt::from(1) << bits;
    // `mod_floor`, not `%`: Rust's remainder keeps the sign of the dividend, and
    // the low bits of a negative number are its two's-complement ones.
    let mut low = value % &modulus;
    if low < BigInt::from(0) {
        low += &modulus;
    }
    if signed && low >= (BigInt::from(1) << (bits - 1)) {
        low -= modulus;
    }
    low
}

/// Parse a primitive type name (`i32`, `u7`, `usize`, `f64`, `bool`, …) into a
/// [`Ty`]. Returns `None` for a name that is not a primitive; width validity
/// mirrors the resolver's `synth_primitive` (§3.1). This is how a `TypePath`
/// whose head resolved to a [`DefKind::Primitive`] becomes a concrete [`Ty`].
pub fn primitive_ty(name: &str) -> Option<Ty> {
    match name {
        "bool" => return Some(Ty::Bool),
        "char" => return Some(Ty::Char),
        "void" => return Some(Ty::Void),
        // `never` was always the type of `return` / `break` / a `loop` with no
        // `break`; this is only the spelling. Writing it is what lets a
        // signature *promise* divergence — `abort :: func () -> never` — which
        // is what makes a diverging call sit in any expression position without
        // the type checker special-casing it (§3.1).
        "never" => return Some(Ty::Never),
        _ => {}
    }
    let (signed, digits) = match name.split_at(1) {
        ("i", d) => (Some(true), d),
        ("u", d) => (Some(false), d),
        ("f", d) => (None, d),
        _ => return None,
    };
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    let width: u32 = digits.parse().ok()?;
    match signed {
        Some(signed) => {
            // `u1` is `bool`; `i1` is not a type; widths cap at 65535 (§3.1).
            if !signed && width == 1 {
                return Some(Ty::Bool);
            }
            if signed && width == 1 {
                return None;
            }
            if (1..=65535).contains(&width) {
                Some(Ty::int(width as u16, signed))
            } else {
                None
            }
        }
        None => match width {
            16 => Some(Ty::Float(FloatWidth::F16)),
            32 => Some(Ty::Float(FloatWidth::F32)),
            64 => Some(Ty::Float(FloatWidth::F64)),
            80 => Some(Ty::Float(FloatWidth::F80)),
            128 => Some(Ty::Float(FloatWidth::F128)),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for `usize` in this module's tests.
    ///
    /// `usize` is a `core` declaration now (§3.1), so there is no `usize` to
    /// name without a session — and these tests are about the lattice, not about
    /// `core`. Anything nominal serves; the source-level behaviour is pinned by
    /// `sema::tests` instead, where a real `core` is loaded.
    fn a_nominal() -> Ty {
        Ty::Nominal {
            def: DefId(0),
            args: Vec::new(),
        }
    }

    #[test]
    fn a_const_arguments_type_is_half_its_identity() {
        // §5: `3usize` and `3u8` are **different** arguments. The values are
        // equal and the types are not, and comparing only the values would make
        // `Foo.<3u8>` and `Foo.<3usize>` one type — a collapse nothing would
        // catch until monomorphization tried to emit one body for two different
        // argument types.
        let three_usize = Const::len(3, a_nominal());
        let three_u8 = Const::known(Ty::u8(), ConstValue::Int(3.into()));
        let mut cx = InferCtxt::new();
        assert!(cx.unify_const(&three_usize, &three_usize).is_ok());
        assert!(cx.unify_const(&three_u8, &three_u8).is_ok());
        assert!(
            cx.unify_const(&three_usize, &three_u8).is_err(),
            "`3usize` and `3u8` must not unify"
        );

        // Different values at one type disagree too, which is the rule that
        // makes `[3]i32` and `[4]i32` different types.
        assert!(
            cx.unify_const(&Const::len(3, a_nominal()), &Const::len(4, a_nominal()))
                .is_err()
        );

        // A variable takes whichever it meets first, and keeps the type with it.
        let v = cx.fresh_const();
        assert!(cx.unify_const(&v, &three_u8).is_ok());
        assert_eq!(cx.shallow_const(&v), three_u8);
        assert!(cx.unify_const(&v, &three_usize).is_err());
    }

    /// A width is a bare number, not a value at a type — which is what keeps the
    /// representation finite: a width typed `u16` would be a `uint.<16>` whose
    /// width is typed `u16`, without end.
    #[test]
    fn a_width_is_a_bare_number() {
        assert_eq!(Const::bits_of(32), Const::Width(32));
        assert_eq!(Ty::int(32, true).int_parts(), Some((true, 32)));

        let mut cx = InferCtxt::new();
        assert!(cx.unify(&Ty::int(32, true), &Ty::int(32, true)).is_ok());
        assert!(cx.unify(&Ty::int(32, true), &Ty::int(64, true)).is_err());
        assert!(cx.unify(&Ty::int(32, true), &Ty::int(32, false)).is_err());
    }

    #[test]
    fn a_const_parameter_admits_every_primitive_and_no_aggregate() {
        // §5: the admissible set is the primitives. `bool` is load-bearing —
        // `<const B: bool>` is an ordinary thing to want — and an aggregate is
        // excluded because structural equality of arbitrary values is a much
        // larger promise than comparing two scalars.
        for name in ["u16", "u8", "i64", "f32", "bool", "char"] {
            let t = primitive_ty(name).expect(name);
            assert!(t.is_primitive(), "`{name}` should be admissible");
        }
        assert!(!Ty::Void.is_primitive());
        assert!(
            !Ty::Array {
                len: Const::len(3, a_nominal()),
                mutable: false,
                inner: Box::new(Ty::Bool),
            }
            .is_primitive()
        );
        assert!(!Ty::Tuple(vec![Ty::Bool, Ty::Bool]).is_primitive());
    }

    /// The sugar and the family spelling are **one type**, not two that convert:
    /// `i32` is `int.<32>` down to the representation.
    #[test]
    fn the_sugar_and_the_family_are_one_type() {
        assert_eq!(
            primitive_ty("i32"),
            Some(Ty::Int {
                signed: true,
                width: Const::Width(32),
            })
        );

        // And it renders back as the sugar, never as the argument the compiler
        // chose for it.
        let defs = super::super::def::DefTable::new();
        assert_eq!(Ty::int(32, true).display(&defs), "i32");
        assert_eq!(Ty::int(7, false).display(&defs), "u7");

        // A symbolic width has no sugar to print, so the family form shows.
        let mut cx = InferCtxt::new();
        let open = Ty::Int {
            signed: true,
            width: cx.fresh_const(),
        };
        assert_eq!(open.display(&defs), "int.<?c0>");
        let open_u = Ty::Int {
            signed: false,
            width: cx.fresh_const(),
        };
        assert_eq!(open_u.display(&defs), "uint.<?c1>");
    }

    #[test]
    fn primitive_parsing() {
        assert_eq!(
            primitive_ty("i32"),
            Some(Ty::int(32, true))
        );
        assert_eq!(
            primitive_ty("u7"),
            Some(Ty::int(7, false))
        );
        // `usize` is no longer a primitive: it is a `distinct` declared in
        // `core` (§3.1), reached by name through the prelude like `str`.
        assert_eq!(primitive_ty("usize"), None);
        assert_eq!(primitive_ty("isize"), None);
        assert_eq!(primitive_ty("f80"), Some(Ty::Float(FloatWidth::F80)));
        assert_eq!(primitive_ty("u1"), Some(Ty::Bool));
        assert_eq!(primitive_ty("i1"), None);
        assert_eq!(primitive_ty("f100"), None);
        assert_eq!(primitive_ty("Foo"), None);
    }

    #[test]
    fn unify_solves_variables() {
        let mut cx = InferCtxt::new();
        let v = cx.fresh();
        assert!(cx.unify(&v, &Ty::Bool).is_ok());
        assert_eq!(cx.resolve(&v), Ty::Bool);
    }

    /// An unconstrained integer literal defaults to `isize` — the one the
    /// context was told about, since `isize` is a `core` declaration and this
    /// module cannot name it.
    #[test]
    fn int_literal_defaults_to_isize() {
        let mut cx = InferCtxt::new();
        cx.set_ptr_int_tys(Some((a_nominal(), a_nominal())));
        let lit = cx.fresh_of(TyVarKind::Int);
        let mut amb = false;
        let out = cx.finalize(&lit, &mut || amb = true);
        assert_eq!(out, a_nominal());
        assert!(!amb);
    }

    #[test]
    fn int_literal_unifies_with_concrete_then_no_default() {
        let mut cx = InferCtxt::new();
        let lit = cx.fresh_of(TyVarKind::Int);
        let i32 = Ty::int(32, true);
        assert!(cx.unify(&lit, &i32).is_ok());
        assert_eq!(cx.resolve(&lit), i32);
    }

    #[test]
    fn int_literal_rejects_non_integer() {
        let mut cx = InferCtxt::new();
        let lit = cx.fresh_of(TyVarKind::Int);
        assert!(cx.unify(&lit, &Ty::Bool).is_err());
    }

    #[test]
    fn float_literal_defaults_to_f64() {
        let mut cx = InferCtxt::new();
        let lit = cx.fresh_of(TyVarKind::Float);
        let out = cx.finalize(&lit, &mut || {});
        assert_eq!(out, Ty::Float(FloatWidth::F64));
    }

    #[test]
    fn unsolved_general_var_is_ambiguous() {
        let mut cx = InferCtxt::new();
        let v = cx.fresh();
        let mut amb = false;
        let out = cx.finalize(&v, &mut || amb = true);
        assert_eq!(out, Ty::Error);
        assert!(amb);
    }

    #[test]
    fn occurs_check_blocks_infinite_type() {
        let mut cx = InferCtxt::new();
        let v = cx.fresh();
        let ptr = Ty::Ptr {
            mutable: false,
            inner: Box::new(v.clone()),
        };
        assert!(cx.unify(&v, &ptr).is_err());
    }

    #[test]
    fn mut_ptr_coerces_to_const_ptr() {
        let mut cx = InferCtxt::new();
        let m = Ty::Ptr {
            mutable: true,
            inner: Box::new(Ty::Bool),
        };
        let c = Ty::Ptr {
            mutable: false,
            inner: Box::new(Ty::Bool),
        };
        assert!(cx.unify(&m, &c).is_ok());
    }
}
