//! IR validation: the checks that run on the lowered, linked program.
//!
//! These are the rules deliberately deferred out of inference. They share a
//! shape — walk the IR, report, change nothing — and they run *here* rather than
//! on the AST because by this point all surface sugar is resolved: an operator
//! is an explicit call carrying its [`Dispatch`](super::Dispatch) and a
//! `builtin` tag, a `for` is a loop, `.?` and `.!` are matches. A walk over the
//! IR therefore sees every operation that actually happens, and nothing hides
//! behind a piece of syntax the pass would have to learn to recognize. On the
//! AST each check would have to handle every surface form *and* re-derive which
//! operator resolved to which trait method, redoing work inference already did.
//!
//! They run on the whole-program [`Linked`] view, not per file: reachability
//! starts at one entry point, exhaustiveness needs every variant of an enum that
//! may be declared elsewhere, and a call crosses files freely.
//!
//! Every pass appends to one diagnostic list and none of them stop the others,
//! so a single run reports everything wrong with a program rather than the first
//! thing.

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::DefTable;

use super::{Arm, Block, Expr, ExprKind, Linked, Meta, Stmt, StmtKind};

pub mod constness;
pub mod divergence;
pub mod exhaustive;
pub mod mutability;

/// Run every IR validation pass over `linked`, in order, collecting what they
/// report.
pub fn run(defs: &DefTable, meta: &Meta, linked: &Linked) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    divergence::check(defs, meta, linked, &mut out);
    mutability::check(defs, meta, linked, &mut out);
    exhaustive::check(defs, meta, linked, &mut out);
    constness::check(defs, meta, linked, &mut out);
    out
}

// ===< Shared traversal >===

/// The direct sub-expressions of `e`, in evaluation order.
///
/// Each walk above handles the control-flow forms — block, loop, `if`, `match` —
/// itself and reaches this only for the rest, because for those four "visit the
/// children" and "does control pass through" are different questions. They are
/// still covered here so that this stays a complete traversal on its own rather
/// than one that depends on its callers to be correct.
pub(super) fn children_of(e: &Expr, f: &mut impl FnMut(&Expr)) {
    match &e.kind {
        ExprKind::Lit(_)
        | ExprKind::Local(_)
        | ExprKind::Global(_)
        | ExprKind::ConstParam(_)
        | ExprKind::Error => {}
        ExprKind::Call { callee, args, .. } => {
            f(callee);
            args.iter().for_each(f);
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        ExprKind::Unary { operand, .. } => f(operand),
        ExprKind::Ref { place, .. } => f(place),
        ExprKind::Deref { base }
        | ExprKind::Field { base, .. }
        | ExprKind::TupleIndex { base, .. } => f(base),
        ExprKind::Index { base, index } => {
            f(base);
            f(index);
        }
        ExprKind::Tuple { elems } => elems.iter().for_each(f),
        ExprKind::Construct { fields, .. } => fields.iter().for_each(|(_, e)| f(e)),
        ExprKind::Variant { args, .. } | ExprKind::Intrinsic { args, .. } => {
            args.iter().for_each(f)
        }
        ExprKind::DynCast { value, .. } => f(value),
        ExprKind::Block(b) | ExprKind::Loop { body: b } => block_children(b, f),
        ExprKind::If { cond, then, els } => {
            f(cond);
            block_children(then, f);
            if let Some(els) = els {
                block_children(els, f);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            f(scrutinee);
            for Arm { guard, body, .. } in arms {
                if let Some(g) = guard {
                    f(g);
                }
                f(body);
            }
        }
    }
}

pub(super) fn block_children(b: &Block, f: &mut impl FnMut(&Expr)) {
    b.stmts.iter().for_each(|s| stmt_children(s, f));
    if let Some(t) = &b.tail {
        f(t);
    }
    b.defers.iter().for_each(|d| f(d));
}

pub(super) fn stmt_children(s: &Stmt, f: &mut impl FnMut(&Expr)) {
    match &s.kind {
        StmtKind::Return(v) | StmtKind::Break(v) => {
            if let Some(v) = v {
                f(v);
            }
        }
        StmtKind::Continue => {}
        StmtKind::Let { init, .. } => f(init),
        StmtKind::Assign { place, value } => {
            f(place);
            f(value);
        }
        StmtKind::Expr(e) => f(e),
    }
}
