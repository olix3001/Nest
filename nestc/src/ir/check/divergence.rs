//! The divergence check: a function declared `-> never` must really never
//! return.
//!
//! `never` earns its keep by converting implicitly into every other type, which
//! is what lets `panic(...)` sit wherever a value is expected. That conversion is
//! sound only because no value of the type can exist — reaching it would mean
//! holding one. A `-> never` function that *could* return would produce exactly
//! that: control arriving past a conversion that was supposed to be unreachable,
//! with whatever the callee left behind reinterpreted as the caller's type. So
//! the signature is a promise, and this pass is what makes the compiler check it
//! rather than take the author's word (§3.1).
//!
//! # The rule, and why it is the conservative one
//!
//! A `-> never` function is an error if a `return` in it is **reachable**, or if
//! the **end of its body** is reachable. Everything else follows from deciding,
//! for each construct, whether control can pass through it.
//!
//! Reachability here is structural, not a dataflow analysis: this runs on the
//! IR, which still has `if`, `match` and `loop` in source shape and no CFG. That
//! makes the answer approximate, and the approximation only ever runs one way —
//! **this pass claims divergence only when it is certain of it**. A body it
//! cannot prove diverges is rejected, which may reject a program that in fact
//! never returns; the opposite error would accept one that does, and quietly
//! unsound is much worse than occasionally strict. The obvious case is a
//! condition the pass cannot evaluate:
//!
//! ```text
//! f :: func () -> never { if true { loop {} } }   // rejected: the `if` has no
//!                                                 // `else`, so as far as this
//!                                                 // pass knows it completes
//! ```
//!
//! Once LIR exists the same question is answered on a real CFG and the rule can
//! tighten. The shape of the check does not change when it does — only how it
//! decides that a block is unreachable.
//!
//! # What counts as diverging
//!
//! - a `return`, `break` or `continue` — control does not continue past it;
//! - a call whose **own type is `never`**, which is exactly "the callee is
//!   declared `-> never`". Reading it off the call's type rather than chasing
//!   the callee means a virtual or generic call is handled for free;
//! - a `loop` with no `break` targeting it. It does not matter whether the body
//!   diverges: with no `break`, the only way out is to leave the function
//!   entirely;
//! - an `if` whose condition diverges, or whose two arms both do. **An `if` with
//!   no `else` never diverges** — the missing branch is the path that completes;
//! - a `match` whose scrutinee diverges, or all of whose arms do;
//! - any expression one of whose operands diverges, since the operand is
//!   evaluated first.

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::DefTable;
use crate::sema::ty::Ty;

use crate::ir::{Block, Expr, ExprKind, Function, IrId, Linked, Meta, Stmt, StmtKind};

/// Report every `-> never` function that could return.
pub fn check(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    for func in linked.funcs() {
        // A declaration has no body to check: `extern("c") func abort() -> never`
        // is a promise about code this compiler does not own.
        let Some(body) = &func.body else { continue };
        if !returns_never(meta, func) {
            continue;
        }
        report(defs, meta, func, body, out);
    }
}

/// Whether `func` is declared `-> never`. A function's node is typed with its
/// whole signature, so the return type is read out of that rather than kept
/// separately.
fn returns_never(meta: &Meta, func: &Function) -> bool {
    meta.with_ty(func.id, |t| match t {
        Ty::Func { ret, .. } => matches!(**ret, Ty::Never),
        _ => false,
    })
    .unwrap_or(false)
}

/// Emit **at most one** diagnostic for `func`.
///
/// One mistake must read as one mistake. A body with a reachable `return` also
/// tends to have a reachable end, and reporting both would describe a single
/// wrong signature twice; the `return` is the more specific of the two and has a
/// place to point at, so it wins.
fn report(defs: &DefTable, meta: &Meta, func: &Function, body: &Block, out: &mut Vec<Diagnostic>) {
    let name = defs.canonical_string(func.def);
    let note = "a `-> never` function must not return: end the body in a call to another \
                `-> never` function, a `loop` with no `break`, or a `match` all of whose arms \
                diverge";

    let mut scan = Reachable {
        meta,
        returns: Vec::new(),
    };
    scan.block(body);

    if let Some(&at) = scan.returns.first() {
        let mut d = Diagnostic::error(format!(
            "`{name}` is declared `-> never`, but this `return` can be reached"
        ));
        if let Some(span) = meta.span(at) {
            d = d.with_primary(span, "control leaves the function here");
        }
        out.push(d.with_note(note));
        return;
    }

    if !diverges_block(meta, body) {
        let mut d = Diagnostic::error(format!(
            "`{name}` is declared `-> never`, but control can reach the end of its body"
        ));
        if let Some(span) = meta.span(func.id) {
            d = d.with_primary(span, "this body can complete");
        }
        out.push(d.with_note(note));
    }
}

// ===< Finding reachable `return`s >===

/// Collects the `return`s that control can actually get to.
///
/// The only thing that makes a statement unreachable at this level is an earlier
/// statement in the same block that diverges, so the walk is: visit each
/// statement, and stop at the first one control does not continue past.
struct Reachable<'a> {
    meta: &'a Meta,
    returns: Vec<IrId>,
}

impl Reachable<'_> {
    fn block(&mut self, b: &Block) {
        for s in &b.stmts {
            self.stmt(s);
            if diverges_stmt(self.meta, s) {
                break;
            }
        }
        if let Some(t) = &b.tail {
            self.expr(t);
        }
        // A `defer` body runs on the way out of the scope, so it is reachable
        // whenever the scope is entered at all.
        for d in crate::ir::defer_bodies(b) {
            self.expr(d);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            // The body is walked once per block, after the statements: see
            // `Walk::block`.
            StmtKind::Defer(_) => {}
            StmtKind::Return(value) => {
                // `return diverge()` never actually returns: the value is
                // evaluated first, and control does not come back from it. This
                // is how a `-> never` function is written recursively, so
                // counting it would reject the most obvious way to write one.
                match value {
                    Some(v) => {
                        if !diverges_expr(self.meta, v) {
                            self.returns.push(s.id);
                        }
                        self.expr(v);
                    }
                    None => self.returns.push(s.id),
                }
            }
            StmtKind::Break(value) => {
                if let Some(v) = value {
                    self.expr(v);
                }
            }
            StmtKind::Continue => {}
            StmtKind::Let { init, .. } => self.expr(init),
            StmtKind::Assign { place, value } => {
                self.expr(place);
                self.expr(value);
            }
            StmtKind::Expr(e) => self.expr(e),
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop { body } => self.block(body),
            ExprKind::If { cond, then, els } => {
                self.expr(cond);
                self.block(then);
                if let Some(e) = els {
                    self.block(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms {
                    if let Some(g) = &a.guard {
                        self.expr(g);
                    }
                    self.expr(&a.body);
                }
            }
            _ => super::children_of(e, &mut |c| self.expr(c)),
        }
    }
}

// ===< Does control pass through? >===

/// Whether control can fall off the end of `b`.
pub fn diverges_block(meta: &Meta, b: &Block) -> bool {
    if b.stmts.iter().any(|s| diverges_stmt(meta, s)) {
        return true;
    }
    b.tail.as_ref().is_some_and(|t| diverges_expr(meta, t))
}

/// Whether control continues to the statement after `s`.
pub fn diverges_stmt(meta: &Meta, s: &Stmt) -> bool {
    match &s.kind {
        // `break` and `continue` do not leave the *function*, but they do leave
        // this block — which is the question every caller of this asks. A
        // `loop` that one of them targets is handled separately, by looking for
        // `break`s at its own level.
        StmtKind::Return(_) | StmtKind::Break(_) | StmtKind::Continue => true,
        StmtKind::Let { init, .. } => diverges_expr(meta, init),
        StmtKind::Assign { place, value } => {
            diverges_expr(meta, place) || diverges_expr(meta, value)
        }
        StmtKind::Expr(e) => diverges_expr(meta, e),
        // Registering a `defer` cannot diverge: the body runs on the way out of
        // the block, not here.
        StmtKind::Defer(_) => false,
    }
}

/// Whether control continues past `e`.
pub fn diverges_expr(meta: &Meta, e: &Expr) -> bool {
    match &e.kind {
        // A call to a `-> never` function. Its *own* type is the callee's return
        // type, so this covers a direct, virtual and generic call alike without
        // looking the callee up.
        ExprKind::Call { callee, args, .. } => {
            is_never(meta, e.id)
                || diverges_expr(meta, callee)
                || args.iter().any(|a| diverges_expr(meta, a))
        }
        ExprKind::Intrinsic { args, .. } => {
            is_never(meta, e.id) || args.iter().any(|a| diverges_expr(meta, a))
        }
        ExprKind::Block(b) => diverges_block(meta, b),
        // With no `break` targeting it the loop is never left except by leaving
        // the function, whatever its body does.
        ExprKind::Loop { body } => !has_break(body),
        // An `if` with no `else` has a path that completes: the one where the
        // condition was false.
        ExprKind::If { cond, then, els } => {
            diverges_expr(meta, cond)
                || match els {
                    Some(els) => diverges_block(meta, then) && diverges_block(meta, els),
                    None => false,
                }
        }
        // A guard is deliberately ignored: an arm whose guard fails falls
        // through to the next arm, so it cannot make the `match` complete on its
        // own, and treating a diverging guard as divergence would be a claim
        // this pass cannot back.
        ExprKind::Match { scrutinee, arms } => {
            diverges_expr(meta, scrutinee)
                || (!arms.is_empty() && arms.iter().all(|a| diverges_expr(meta, &a.body)))
        }
        // Everything else evaluates its operands and then itself, so it diverges
        // exactly when one of them does.
        _ => {
            let mut any = false;
            super::children_of(e, &mut |c| any |= diverges_expr(meta, c));
            any
        }
    }
}

fn is_never(meta: &Meta, id: IrId) -> bool {
    meta.with_ty(id, |t| matches!(t, Ty::Never))
        .unwrap_or(false)
}

/// Whether `body` contains a `break` targeting **this** loop — one not swallowed
/// by a nested `loop` of its own.
///
/// An unreachable `break` counts. That is the conservative direction: it makes
/// the pass say "this loop can be left" when perhaps it cannot, which rejects a
/// program rather than accepting an unsound one.
fn has_break(body: &Block) -> bool {
    fn in_block(b: &Block) -> bool {
        b.stmts.iter().any(in_stmt)
            || b.tail.as_deref().is_some_and(in_expr)
            || crate::ir::defer_bodies(b).any(in_expr)
    }
    fn in_stmt(s: &Stmt) -> bool {
        match &s.kind {
            StmtKind::Break(_) => true,
            StmtKind::Return(v) => v.as_ref().is_some_and(in_expr),
            StmtKind::Continue => false,
            StmtKind::Let { init, .. } => in_expr(init),
            StmtKind::Assign { place, value } => in_expr(place) || in_expr(value),
            StmtKind::Expr(e) => in_expr(e),
            // A `break` inside a `defer` body leaves the loop the body runs in,
            // which is this one — `crate::ir::defer_bodies` is where it is seen.
            StmtKind::Defer(_) => false,
        }
    }
    fn in_expr(e: &Expr) -> bool {
        match &e.kind {
            // A nested loop captures its own `break`s.
            ExprKind::Loop { .. } => false,
            ExprKind::Block(b) => in_block(b),
            ExprKind::If { cond, then, els } => {
                in_expr(cond) || in_block(then) || els.as_ref().is_some_and(in_block)
            }
            ExprKind::Match { scrutinee, arms } => {
                in_expr(scrutinee)
                    || arms
                        .iter()
                        .any(|a| a.guard.as_ref().is_some_and(in_expr) || in_expr(&a.body))
            }
            _ => {
                let mut any = false;
                super::children_of(e, &mut |c| any |= in_expr(c));
                any
            }
        }
    }
    in_block(body)
}
