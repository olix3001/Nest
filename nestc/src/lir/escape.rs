//! Escape analysis: which allocations provably do not outlive their scope (§5).
//!
//! An allocation that cannot be reached from anywhere outside the scope that
//! made it can be **freed** at the scope's exits, without the collector ever
//! learning it existed. That is the whole of §5: a `drop` on the same cleanup
//! ladder `defer` rides, one rung per kind of exit, so an allocation in a scope
//! with three exits is freed once in a block all three reach.
//!
//! ### Why the analysis runs on the IR and the drops appear in LIR
//!
//! A scope is **lexical**, and after control flow becomes a graph there are no
//! scopes left to speak of — the ladder is what remains of them. Asking "does
//! this allocation escape its scope" is therefore a question about the tree, and
//! asking it there costs a walk instead of a dataflow. The answer is handed to
//! the lowering, which registers a drop the same way it registers a `defer`, and
//! the ladder machinery it already has does the rest.
//!
//! ### The rule, and why it is this blunt
//!
//! §5 states it: a value escapes if it is returned, stored into anything
//! reachable from outside the scope, **passed to any call**, or captured by an
//! escaping closure. "Passed to any call" is deliberate — without per-function
//! summaries there is no way to know whether a callee retains what it is given,
//! and guessing wrong frees memory something still references. The pass is
//! useful for short-lived local temporaries and honest about being useful for
//! nothing else. Extending it to real summaries later changes its *precision*,
//! not the shape of anything downstream.
//!
//! So the test here is a whitelist rather than a blacklist, which is the same
//! decision read from the other side: a candidate survives only if **every**
//! mention of it is the base of a place being read or written — `p.x`, `p.*`,
//! `p.x = 1`. Anything else disqualifies it, including forms that would be
//! provably fine, because a whitelist that is wrong leaks and a blacklist that
//! is wrong corrupts memory.
//!
//! Two consequences are worth knowing before reading the code and expecting
//! otherwise:
//!
//! - **A written `drop(p)`** (spec §6.9) disqualifies `p`, because passing a
//!   local to anything does and a `drop` is a call. That is not a special case,
//!   it is the rule doing exactly what it should: the program took the question
//!   on, so the compiler stops answering it, and the object is freed once.
//! - **A slice a program reads from is never a candidate.** Every use of one
//!   goes through `&xs` — `.len()` and `xs[i]` both do — and taking a local's
//!   address is not on the whitelist. So `make` allocations are in practice
//!   collected rather than dropped, and only a slice nothing touches gets a
//!   `drop`.
//!
//! ### The one ordering rule
//!
//! A rung of the ladder is built at the first exit that needs it and shared by
//! every later one (§3). A `let` that comes *after* an exit therefore has no
//! local yet when that exit's rung is built, and adding it later would put a
//! drop of an uninitialized slot on a path that never ran the `let`. Such a
//! candidate is disqualified: it is collected like anything else, which costs a
//! collection and is never wrong.

use std::collections::{HashMap, HashSet};

use crate::ir::meta::IrId;
use crate::ir::{Block, Expr, ExprKind, Function, Pattern, PatternKind, Stmt, StmtKind};
use crate::sema::def::DefId;

/// The scope-local allocations of one program, keyed by the block that owns
/// them. A block with no entry has nothing to drop, which is the usual case.
pub type Drops = HashMap<IrId, Vec<DefId>>;

/// Run the analysis over every function.
pub fn analyze<'a>(funcs: impl Iterator<Item = &'a Function>) -> Drops {
    let mut out = Drops::new();
    for f in funcs {
        let Some(body) = &f.body else { continue };
        let mut pass = Escape::default();
        pass.scan_block(body);
        pass.uses_block(body);
        let Escape { owned, escaped, .. } = pass;
        for (block, defs) in owned {
            let kept: Vec<DefId> = defs.into_iter().filter(|d| !escaped.contains(d)).collect();
            if !kept.is_empty() {
                out.insert(block, kept);
            }
        }
    }
    out
}

/// Where a mention of a local sits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// The base of a place: `p.x`, `p.*`, `p.x = 1`. The pointer itself does not
    /// leave.
    Projected,
    /// Everything else — an argument, a returned value, a member of an
    /// aggregate, the right-hand side of a store.
    Escaping,
    /// A place whose **address** is taken: `&p.*.x`, or the array a sub-slice
    /// is cut from. A projection does not protect its base here, because the
    /// address it makes points into the same object — the object leaves with
    /// it.
    Addressed,
}

#[derive(Default)]
struct Escape {
    /// Candidates, by the block whose scope they belong to.
    owned: HashMap<IrId, Vec<DefId>>,
    /// Every candidate seen, so a mention can be recognized cheaply.
    candidates: HashSet<DefId>,
    escaped: HashSet<DefId>,
}

impl Escape {
    // ===< Finding candidates >===

    /// Collect the allocations each block declares, in order, stopping a block's
    /// list at its first exit (see the ordering rule above).
    fn scan_block(&mut self, b: &Block) {
        let mut past_exit = false;
        for s in &b.stmts {
            if let Some(def) = allocation(s).filter(|_| !past_exit) {
                self.owned.entry(b.id).or_default().push(def);
                self.candidates.insert(def);
            }
            past_exit |= exits(s);
            self.scan_stmt(s);
        }
        if let Some(t) = &b.tail {
            self.scan_expr(t);
        }
        for d in crate::ir::defer_bodies(b) {
            self.scan_expr(d);
        }
    }

    fn scan_stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { init, .. } => self.scan_expr(init),
            StmtKind::Assign { place, value } => {
                self.scan_expr(place);
                self.scan_expr(value);
            }
            StmtKind::Expr(e) | StmtKind::Return(Some(e)) | StmtKind::Break(Some(e)) => {
                self.scan_expr(e)
            }
            // Both walks reach a `defer` body through `defer_bodies`, once per
            // block, because that is where it runs.
            StmtKind::Return(None)
            | StmtKind::Break(None)
            | StmtKind::Continue
            | StmtKind::Defer(_) => {}
        }
    }

    fn scan_expr(&mut self, e: &Expr) {
        each_block(e, &mut |b| self.scan_block(b));
        each_child(e, &mut |c| self.scan_expr(c));
    }

    // ===< Disqualifying them >===

    fn uses_block(&mut self, b: &Block) {
        for s in &b.stmts {
            self.uses_stmt(s);
        }
        if let Some(t) = &b.tail {
            self.uses_expr(t, Ctx::Escaping);
        }
        for d in crate::ir::defer_bodies(b) {
            self.uses_expr(d, Ctx::Escaping);
        }
    }

    fn uses_stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { init, .. } => self.uses_expr(init, Ctx::Escaping),
            StmtKind::Assign { place, value } => {
                // Overwriting the pointer itself: the object the `let` made is
                // no longer what the slot names, so a drop at the scope's end
                // would free whatever replaced it.
                if let ExprKind::Local(def) = &place.kind {
                    self.escaped.insert(*def);
                }
                self.uses_expr(place, Ctx::Projected);
                self.uses_expr(value, Ctx::Escaping);
            }
            StmtKind::Expr(e) | StmtKind::Return(Some(e)) | StmtKind::Break(Some(e)) => {
                self.uses_expr(e, Ctx::Escaping)
            }
            StmtKind::Return(None)
            | StmtKind::Break(None)
            | StmtKind::Continue
            | StmtKind::Defer(_) => {}
        }
    }

    fn uses_expr(&mut self, e: &Expr, ctx: Ctx) {
        match &e.kind {
            ExprKind::Local(def) => {
                if ctx != Ctx::Projected && self.candidates.contains(def) {
                    self.escaped.insert(*def);
                }
            }
            ExprKind::Ref { place, .. } => self.uses_expr(place, Ctx::Addressed),
            // An element read or written in place, `p.*.xs[i]`: `$index` makes
            // an address into the object and the `.*` consumes it on the spot.
            // Under a `&` the address is kept, and the general arms see that.
            ExprKind::Deref { base } if ctx != Ctx::Addressed => match &base.kind {
                ExprKind::Intrinsic { name, args, .. }
                    if matches!(name.as_str(), "index" | "index_mut") && args.len() == 2 =>
                {
                    match &args[0].kind {
                        ExprKind::Ref { place, .. } => self.uses_expr(place, Ctx::Projected),
                        _ => self.uses_expr(&args[0], Ctx::Escaping),
                    }
                    self.uses_expr(&args[1], Ctx::Escaping);
                }
                _ => self.uses_expr(base, Ctx::Projected),
            },
            // A place path: the base is reached *through*, not handed out —
            // unless the path's address is what is being taken.
            ExprKind::Deref { base } | ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
                let inner = if ctx == Ctx::Addressed { Ctx::Addressed } else { Ctx::Projected };
                self.uses_expr(base, inner)
            }
            // A sub-slice of an array is the array's own storage.
            ExprKind::Intrinsic { name, args, .. } if name.as_str() == "slice" && !args.is_empty() => {
                self.uses_expr(&args[0], Ctx::Addressed);
                args[1..].iter().for_each(|a| self.uses_expr(a, Ctx::Escaping));
            }
            _ => {
                each_block(e, &mut |b| self.uses_block(b));
                each_child(e, &mut |c| self.uses_expr(c, Ctx::Escaping));
            }
        }
    }
}

/// The local a `let` binds to a fresh allocation, if that is what it is.
///
/// `new` and `make` are the two intrinsics that produce one (§6.9). A
/// destructuring `let` is not a candidate: what it binds are the allocation's
/// parts, and freeing a part is not a thing.
fn allocation(s: &Stmt) -> Option<DefId> {
    let StmtKind::Let { pattern, init } = &s.kind else {
        return None;
    };
    let Pattern {
        kind: PatternKind::Binding { def, .. },
        ..
    } = pattern
    else {
        return None;
    };
    match &init.kind {
        ExprKind::Intrinsic { name, .. } if matches!(name.as_str(), "new" | "make") => Some(*def),
        _ => None,
    }
}

/// Whether this statement can leave the block it is in.
fn exits(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Return(_) | StmtKind::Break(_) | StmtKind::Continue => true,
        StmtKind::Let { init, .. } => expr_exits(init),
        StmtKind::Assign { place, value } => expr_exits(place) || expr_exits(value),
        StmtKind::Expr(e) => expr_exits(e),
        StmtKind::Defer(_) => false,
    }
}

/// Whether an expression contains an exit from the block *it* is in — a
/// `return` inside an `if`, say.
///
/// It over-counts: a `break` inside a nested `loop` leaves that loop rather than
/// this block, and is counted anyway. Over-counting here disqualifies an
/// allocation, which costs a collection; under-counting would place a drop of a
/// slot that was never written.
fn expr_exits(e: &Expr) -> bool {
    let mut found = false;
    each_block(e, &mut |b| {
        found |= b.stmts.iter().any(exits) || b.tail.as_deref().is_some_and(expr_exits);
    });
    each_child(e, &mut |c| found |= expr_exits(c));
    found
}

/// Every block directly inside `e`.
fn each_block(e: &Expr, f: &mut impl FnMut(&Block)) {
    match &e.kind {
        ExprKind::Block(b) | ExprKind::Loop { body: b } => f(b),
        ExprKind::If { then, els, .. } => {
            f(then);
            if let Some(e) = els {
                f(e);
            }
        }
        _ => {}
    }
}

/// Every expression directly inside `e`, blocks excluded.
fn each_child(e: &Expr, f: &mut impl FnMut(&Expr)) {
    match &e.kind {
        ExprKind::Lit(_)
        | ExprKind::Local(_)
        | ExprKind::Global(_)
        | ExprKind::ConstParam(_)
        | ExprKind::Error
        | ExprKind::Block(_)
        | ExprKind::Loop { .. } => {}
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
        ExprKind::Deref { base } | ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
            f(base)
        }
        ExprKind::Tuple { elems } => elems.iter().for_each(f),
        ExprKind::If { cond, .. } => f(cond),
        ExprKind::Match { scrutinee, arms } => {
            f(scrutinee);
            for a in arms {
                if let Some(g) = &a.guard {
                    f(g);
                }
                f(&a.body);
            }
        }
        ExprKind::Construct { fields, .. } => fields.iter().for_each(|(_, e)| f(e)),
        ExprKind::Variant { args, .. } | ExprKind::Intrinsic { args, .. } => args.iter().for_each(f),
        ExprKind::DynCast { value, .. } => f(value),
    }
}
