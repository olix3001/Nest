//! The **one** place the primitive numeric operator impls live (§6: "int+int
//! and Vec3+Vec3 are the same construct").
//!
//! Nest has no source `impl Add for i32`: the integers and floats are
//! width-parameterized and synthesized on demand (see [`super::resolve`]), so
//! there is nowhere to hang a hand-written impl and far too many types to
//! pre-generate them. Instead every builtin arithmetic operator is one row of
//! [`BUILTIN_OPS`]. A row says: which operator-trait `#lang` tag it satisfies,
//! which primitive family it applies to, how it computes its `Output`, and the
//! [`BuiltinOp`] tag codegen keys on.
//!
//! The trait solver ([`super::infer`]) consults this table *uniformly with user
//! impls* while selecting the impl for an operator obligation: a primitive
//! `self` matches the row for its family, a nominal `self` falls through to the
//! user impls. Lowering then stamps the winning row's [`BuiltinOp`] onto the
//! resulting [`crate::ir::Expr::Call`] so codegen recognizes it in O(1).
//!
//! **Adding a new primitive intrinsic is one line here** — add a [`BuiltinRow`]
//! and every stage (selection, projection, codegen tagging) picks it up.

use serde::{Deserialize, Serialize};

use super::ty::Ty;

/// The machine-level operation a builtin operator call denotes. Stamped onto a
/// lowered [`crate::ir::Expr::Call`] so codegen can emit the instruction
/// directly instead of looking up a function body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuiltinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Neg,
    BitNot,
}

/// Which primitive family a [`BuiltinRow`] applies to.
///
/// `Int` / `Float` are part of the extensible contract (a future int-only shift
/// or float-only op is one more row); the current arithmetic set is all
/// `Numeric`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// Any integer type (`iN` / `uN`, `isize` / `usize`).
    Int,
    /// Any float type (`fN`).
    Float,
    /// Any integer *or* float.
    Numeric,
}

impl Applies {
    /// Whether a (shallow-resolved) primitive type is in this family.
    pub fn matches(self, ty: &Ty) -> bool {
        match self {
            Applies::Int => ty.is_int(),
            Applies::Float => ty.is_float(),
            Applies::Numeric => ty.is_int() || ty.is_float(),
        }
    }
}

/// How a builtin operator computes its associated `Output` from `self`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputRule {
    /// `Output = Self` — the result has the operand type (all arithmetic).
    SameAsSelf,
}

/// One builtin operator impl: the whole knowledge of a primitive intrinsic in a
/// single row.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinRow {
    /// The operator-trait `#lang` tag this row satisfies (`"add"`, `"sub"`, …).
    pub lang: &'static str,
    /// The trait method the operator sugar calls (`"add"`, `"sub"`, …); the
    /// resolved [`crate::ir::Expr::Call`] targets the `#lang` trait's method of
    /// this name.
    pub method: &'static str,
    /// The primitive family the operand must belong to.
    pub applies: Applies,
    /// How the result type is derived from the operand type.
    pub output: OutputRule,
    /// The codegen tag.
    pub op: BuiltinOp,
}

/// The complete set of builtin operator impls. Extend this to add a primitive
/// intrinsic; nothing else in the solver or codegen needs to change.
pub const BUILTIN_OPS: &[BuiltinRow] = &[
    BuiltinRow {
        lang: "add",
        method: "add",
        applies: Applies::Numeric,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::Add,
    },
    BuiltinRow {
        lang: "sub",
        method: "sub",
        applies: Applies::Numeric,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::Sub,
    },
    BuiltinRow {
        lang: "mul",
        method: "mul",
        applies: Applies::Numeric,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::Mul,
    },
    BuiltinRow {
        lang: "div",
        method: "div",
        applies: Applies::Numeric,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::Div,
    },
    BuiltinRow {
        lang: "rem",
        method: "rem",
        applies: Applies::Numeric,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::Rem,
    },
    // Bitwise and shift are integer-only (§6.7: "bitwise operators require
    // integer operands"), which is the whole reason [`Applies`] distinguishes
    // the families — a `f32 & f32` finds no row and is reported as a missing
    // impl rather than silently doing something.
    BuiltinRow {
        lang: "bitand",
        method: "bitand",
        applies: Applies::Int,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::BitAnd,
    },
    BuiltinRow {
        lang: "bitor",
        method: "bitor",
        applies: Applies::Int,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::BitOr,
    },
    BuiltinRow {
        lang: "bitxor",
        method: "bitxor",
        applies: Applies::Int,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::BitXor,
    },
    BuiltinRow {
        lang: "shl",
        method: "shl",
        applies: Applies::Int,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::Shl,
    },
    BuiltinRow {
        lang: "shr",
        method: "shr",
        applies: Applies::Int,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::Shr,
    },
    // The prefix unaries. They take no `rhs`, which changes nothing here: a row
    // describes the *self* family and the output rule, and the arity is the
    // trait method's business.
    BuiltinRow {
        lang: "neg",
        method: "neg",
        applies: Applies::Numeric,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::Neg,
    },
    BuiltinRow {
        lang: "bitnot",
        method: "bitnot",
        applies: Applies::Int,
        output: OutputRule::SameAsSelf,
        op: BuiltinOp::BitNot,
    },
];

/// The builtin row carrying `lang`, if any.
pub fn row_for_lang(lang: &str) -> Option<&'static BuiltinRow> {
    BUILTIN_OPS.iter().find(|r| r.lang == lang)
}
