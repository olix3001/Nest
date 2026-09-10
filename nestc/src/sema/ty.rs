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
//! really is an `f80` / `f128`). A general variable that is never solved is a
//! "type annotations needed" error.

use num_bigint::BigInt;
use std::collections::HashMap;

use crate::common::symbol::Symbol;
use crate::parser::ast::{BinOp, NodeId};

use super::def::DefId;

/// Bit width of an integer primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntWidth {
    /// A fixed width `N` (`i8` = `Fixed(8)`, `u4096` = `Fixed(4096)`).
    Fixed(u16),
    /// Pointer-sized (`isize` / `usize`).
    Ptr,
}

impl IntWidth {
    /// The number of value bits, taking a pointer-sized width as 64 (the only
    /// target the bootstrap compiles for).
    pub fn bits(self) -> u32 {
        match self {
            IntWidth::Fixed(n) => n as u32,
            IntWidth::Ptr => 64,
        }
    }
}

/// The five legal float widths (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FloatWidth {
    F16,
    F32,
    F64,
    F80,
    F128,
}

/// An inference variable: an index into [`InferCtxt::subst`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TyVar(pub u32);

/// What a fresh [`TyVar`] may unify with, and how it defaults if left unsolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TyVarKind {
    /// Any type; an unsolved one is a "type annotations needed" error.
    General,
    /// An integer literal: unifies only with an integer type; defaults `isize`.
    Int,
    /// A float literal (`comptime_float`, conceptually `f128`): unifies only
    /// with a float type, and collapses to `f64` when left unconstrained.
    Float,
}

/// A **const-generic** inference variable: an index into
/// [`InferCtxt::const_subst`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConstVar(pub u32);

/// A compile-time *value* appearing inside a type — today only an array length
/// (§3.2 `[N]T`, §5 `<const N: usize>`).
///
/// Lengths take part in type identity: `[3]i32` and `[4]i32` are different
/// types, so they need their own tiny unification lattice alongside [`Ty`]'s.
/// A [`Const::Param`] is the symbolic stand-in for a `const` generic parameter
/// that survives all the way to monomorphization; a [`Const::Var`] is the
/// inference variable a call site instantiates it to, or the hole `[_]T` leaves
/// for a literal to fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Const {
    /// A known value.
    Value(u64),
    /// An as-yet-uninstantiated `const` generic parameter, by its [`DefId`].
    Param(DefId),
    /// An unsolved inference variable.
    Var(ConstVar),
    /// A value that could not be determined; a diagnostic was already reported.
    /// Unifies with anything so one error does not cascade.
    Error,
}

impl Const {
    /// The value, when it is already known.
    pub fn value(self) -> Option<u64> {
        match self {
            Const::Value(n) => Some(n),
            _ => None,
        }
    }

    /// A short, human-readable rendering (`3`, `N`, `?c1`, `_`).
    pub fn display(&self, defs: &super::def::DefTable) -> String {
        match self {
            Const::Value(n) => n.to_string(),
            Const::Param(d) => defs.get(*d).name.to_string(),
            Const::Var(v) => format!("?c{}", v.0),
            Const::Error => "_".into(),
        }
    }
}

/// A type. Cheap to clone; nested types are boxed.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    /// An unsolved inference variable.
    Var(TyVar),
    /// A signed / unsigned integer of the given width.
    Int {
        signed: bool,
        width: IntWidth,
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
    Bool,
    Char,
    /// The `string` type.
    Str,
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
    /// `isize` — the default for an unconstrained integer literal.
    pub fn isize() -> Ty {
        Ty::Int {
            signed: true,
            width: IntWidth::Ptr,
        }
    }

    /// `usize`.
    pub fn usize() -> Ty {
        Ty::Int {
            signed: false,
            width: IntWidth::Ptr,
        }
    }

    /// Whether this (already-resolved) type is an integer.
    pub fn is_int(&self) -> bool {
        matches!(self, Ty::Int { .. })
    }

    /// Whether this (already-resolved) type is a float.
    pub fn is_float(&self) -> bool {
        matches!(self, Ty::Float(_))
    }

    /// A short, human-readable rendering (`i32`, `*mut Foo`, `(A, B)`, `?3`).
    pub fn display(&self, defs: &super::def::DefTable) -> String {
        match self {
            Ty::Var(v) => format!("?{}", v.0),
            Ty::Int { signed, width } => {
                let p = if *signed { 'i' } else { 'u' };
                match width {
                    IntWidth::Fixed(n) => format!("{p}{n}"),
                    IntWidth::Ptr => (if *signed { "isize" } else { "usize" }).to_string(),
                }
            }
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
            Ty::Bool => "bool".into(),
            Ty::Char => "char".into(),
            Ty::Str => "string".into(),
            Ty::Void => "void".into(),
            Ty::Never => "never".into(),
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
        /// `Some` when this projection is an operator's `Output`; fulfillment
        /// stamps the resolved call onto `origin` so lowering emits a uniform
        /// [`crate::ir::Expr::Call`].
        op: Option<BinOp>,
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
}

impl InferCtxt {
    pub fn new() -> Self {
        Self::default()
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
        let mut cur = *k;
        while let Const::Var(v) = cur {
            match self.const_subst[v.0 as usize] {
                Some(bound) => cur = bound,
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
        match (a, b) {
            // An errored length absorbs, so one bad `[expr]T` does not cascade.
            (Const::Error, _) | (_, Const::Error) => Ok(()),
            (Const::Var(x), Const::Var(y)) if x == y => Ok(()),
            (Const::Var(v), other) | (other, Const::Var(v)) => {
                self.const_subst[v.0 as usize] = Some(other);
                Ok(())
            }
            (Const::Value(x), Const::Value(y)) if x == y => Ok(()),
            (Const::Param(x), Const::Param(y)) if x == y => Ok(()),
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
            Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|e| self.resolve(e)).collect()),
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
            (Ty::Error, _) | (_, Ty::Error) => Ok(()),
            (Ty::Never, _) | (_, Ty::Never) => Ok(()),

            (Ty::Var(x), Ty::Var(y)) if x == y => Ok(()),
            (Ty::Var(v), _) => self.bind(*v, &b),
            (_, Ty::Var(v)) => self.bind(*v, &a),

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
                if s1 == s2 && w1 == w2 {
                    Ok(())
                } else {
                    Err((a, b))
                }
            }
            (Ty::Float(x), Ty::Float(y)) if x == y => Ok(()),
            (Ty::Bool, Ty::Bool)
            | (Ty::Char, Ty::Char)
            | (Ty::Str, Ty::Str)
            | (Ty::Void, Ty::Void) => Ok(()),

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
                Ty::Var(w) if self.kind(*w) == TyVarKind::Float => {}
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
                    TyVarKind::Int => Some(Ty::isize()),
                    TyVarKind::Float => Some(Ty::Float(FloatWidth::F64)),
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
            Ty::Tuple(elems) => Ty::Tuple(
                elems
                    .iter()
                    .map(|e| self.finalize(e, on_ambiguous))
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

/// Whether `value` is representable in an integer type of this width and
/// signedness — the "coerces to any integer type **it fits**" rule for a
/// `comptime_int`.
pub fn int_fits(value: &BigInt, signed: bool, width: IntWidth) -> bool {
    let bits = width.bits();
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

/// Parse a primitive type name (`i32`, `u7`, `usize`, `f64`, `bool`, …) into a
/// [`Ty`]. Returns `None` for a name that is not a primitive; width validity
/// mirrors the resolver's `synth_primitive` (§3.1). This is how a `TypePath`
/// whose head resolved to a [`DefKind::Primitive`] becomes a concrete [`Ty`].
pub fn primitive_ty(name: &str) -> Option<Ty> {
    match name {
        "bool" => return Some(Ty::Bool),
        "char" => return Some(Ty::Char),
        "string" => return Some(Ty::Str),
        "void" => return Some(Ty::Void),
        "isize" => return Some(Ty::isize()),
        "usize" => return Some(Ty::usize()),
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
                Some(Ty::Int {
                    signed,
                    width: IntWidth::Fixed(width as u16),
                })
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

    #[test]
    fn primitive_parsing() {
        assert_eq!(
            primitive_ty("i32"),
            Some(Ty::Int {
                signed: true,
                width: IntWidth::Fixed(32)
            })
        );
        assert_eq!(
            primitive_ty("u7"),
            Some(Ty::Int {
                signed: false,
                width: IntWidth::Fixed(7)
            })
        );
        assert_eq!(primitive_ty("usize"), Some(Ty::usize()));
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

    #[test]
    fn int_literal_defaults_to_isize() {
        let mut cx = InferCtxt::new();
        let lit = cx.fresh_of(TyVarKind::Int);
        let mut amb = false;
        let out = cx.finalize(&lit, &mut || amb = true);
        assert_eq!(out, Ty::isize());
        assert!(!amb);
    }

    #[test]
    fn int_literal_unifies_with_concrete_then_no_default() {
        let mut cx = InferCtxt::new();
        let lit = cx.fresh_of(TyVarKind::Int);
        let i32 = Ty::Int {
            signed: true,
            width: IntWidth::Fixed(32),
        };
        assert!(cx.unify(&lit, &i32).is_ok());
        assert_eq!(cx.resolve(&lit), i32);
    }

    #[test]
    fn int_literal_rejects_non_integer() {
        let mut cx = InferCtxt::new();
        let lit = cx.fresh_of(TyVarKind::Int);
        assert!(cx.unify(&lit, &Ty::Str).is_err());
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
