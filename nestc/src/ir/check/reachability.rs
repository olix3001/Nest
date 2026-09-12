//! Unreachable code.
//!
//! A statement after one that does not return — a `return`, a `break`, a
//! `continue`, a call to a `-> never` function, a `loop` with no `break` — can
//! never run. It is almost always a mistake rather than a deliberate choice:
//! code left behind after an early return, a `break` that moved, a line added to
//! the wrong side of a `panic`.
//!
//! The reachability question is the divergence check's, and it is asked the same
//! way here; this pass only reports what falls out of it. That is deliberate —
//! two passes disagreeing about what "reachable" means would be worse than
//! either being imprecise, and both tighten together when the CFG arrives.
//!
//! Only the **first** unreachable statement in a block is reported. Everything
//! after it is unreachable for the same reason, and listing all of it turns one
//! stray `return` into a page of diagnostics.
//!
//! # A warning, not an error
//!
//! Unreachable code is not unsound: the program means exactly what it says, and
//! deleting the dead lines changes nothing about how it runs. It is a *lint* —
//! evidence that the author probably meant something else — so it is reported at
//! [`Severity::Warning`](crate::common::diagnostic::Severity::Warning) and does
//! not fail the build. Rejecting it outright would turn a work-in-progress
//! function with a temporary early `return` into a compile error, which is
//! exactly when a person least wants one.

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::DefTable;

use crate::ir::{Block, Expr, ExprKind, Linked, Meta, Stmt};

use super::divergence::{diverges_expr, diverges_stmt};

/// Report the first unreachable statement in every block that has one.
pub fn check(_defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    for func in linked.funcs() {
        let Some(body) = &func.body else { continue };
        block(meta, body, out);
    }
}

fn block(meta: &Meta, b: &Block, out: &mut Vec<Diagnostic>) {
    // The statement control never continues past, once one is found.
    let mut cause: Option<&Stmt> = None;
    for s in &b.stmts {
        match cause {
            // The first statement after it is the one to report; everything
            // beyond is unreachable for the same reason, and listing all of it
            // turns one stray `return` into a page of diagnostics.
            Some(c) => {
                push(meta, c.id, s.id, out);
                cause = None;
            }
            None if diverges_stmt(meta, s) => cause = Some(s),
            None => {}
        }
        walk_stmt(meta, s, out);
    }
    // Nothing followed it, but the block has a tail: that tail is the block's
    // *value*, so a reader looking for where the value comes from is looking
    // right at code that never runs.
    if let (Some(c), Some(t)) = (cause, &b.tail) {
        push(meta, c.id, t.id, out);
    }
    if let Some(t) = &b.tail {
        walk_expr(meta, t, out);
    }
    for d in crate::ir::defer_bodies(b) {
        walk_expr(meta, d, out);
    }
}

fn walk_stmt(meta: &Meta, s: &Stmt, out: &mut Vec<Diagnostic>) {
    super::stmt_children(s, &mut |e| walk_expr(meta, e, out));
}

fn walk_expr(meta: &Meta, e: &Expr, out: &mut Vec<Diagnostic>) {
    match &e.kind {
        ExprKind::Block(b) | ExprKind::Loop { body: b } => block(meta, b, out),
        ExprKind::If { cond, then, els } => {
            walk_expr(meta, cond, out);
            block(meta, then, out);
            if let Some(e) = els {
                block(meta, e, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(meta, scrutinee, out);
            for a in arms {
                if let Some(g) = &a.guard {
                    walk_expr(meta, g, out);
                }
                walk_expr(meta, &a.body, out);
            }
        }
        _ => super::children_of(e, &mut |c| walk_expr(meta, c, out)),
    }
}

fn push(meta: &Meta, cause: crate::ir::IrId, at: crate::ir::IrId, out: &mut Vec<Diagnostic>) {
    let mut d = Diagnostic::warning("unreachable code");
    if let Some(span) = meta.span(at) {
        d = d.with_primary(span, "this can never run");
    }
    if let Some(span) = meta.span(cause) {
        d = d.with_label(crate::common::diagnostic::Label::secondary(
            span,
            "control never continues past here",
        ));
    }
    out.push(d);
}

/// Re-exported so the pass's dependency on the divergence rule is explicit: the
/// two must agree about what "reachable" means, so there is one definition.
const _: fn(&Meta, &Expr) -> bool = diverges_expr;
