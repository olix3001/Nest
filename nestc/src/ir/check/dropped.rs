//! Using a value after `drop(...)` freed it.
//!
//! `drop(p)` (§6.9) releases an object before the collector would have. That is
//! the one operation in a collected language that can produce a dangling
//! pointer, so the language does not leave it to run time: after a `drop`, the
//! name that was dropped may not be used again.
//!
//! ```text
//! let p := new.<Node>()
//! drop(p)
//! return p.*.x        // error: `p` was dropped
//! ```
//!
//! ### What it tracks, and what it cannot
//!
//! One function, and a **local's own name**. `drop(p)` where the argument is the
//! local `p` is the tracked case; `drop(node.*.next)` frees a pointer no local
//! names, so there is nothing to forbid afterwards and nothing is said.
//!
//! An **alias** made before the drop is not tracked either:
//!
//! ```text
//! let q := p
//! drop(p)
//! q.*.x               // not caught
//! ```
//!
//! Catching that wants ownership, which this language does not have. The limit
//! is stated in `core`'s declaration of `drop` rather than hidden here: writing
//! the call is taking the question on, and this pass is the part of it a
//! compiler can answer, not the whole of it.
//!
//! ### Branches, loops, and coming back to life
//!
//! A drop on **either** side of an `if` counts afterwards. That is the
//! conservative direction and it is also the honest one: "dropped on some paths"
//! is not a state a program can be in, so the answer has to be one of the two
//! and only one of them is safe.
//!
//! A branch that **leaves** — `if c { drop(p) return 0 }` — is the exception,
//! and it has to be: there is no "afterwards" on that path, so merging its state
//! into the code below would refuse a program with nothing wrong with it. The
//! test for leaving is deliberately shallow (a `return`, `break` or `continue`
//! written as a statement of the branch, or an `if` both of whose sides leave),
//! because the two ways of being wrong are not symmetric — under-detecting
//! divergence refuses a correct program, and over-detecting it accepts a use
//! after free.
//!
//! A **loop** is the case worth naming. Dropping a local declared outside the
//! loop is a double free on the second iteration, so it is reported at the drop
//! itself rather than at a use — there is no use to point at, and the second
//! `drop` is the mistake.
//!
//! **Assigning to the local revives it.** `drop(p); p = new.<Node>()` leaves `p`
//! holding an object again, and refusing the next use would be refusing a
//! correct program.

use std::collections::{HashMap, HashSet};

use crate::common::diagnostic::{Diagnostic, Label};
use crate::common::source::FileSpan;
use crate::sema::def::{DefId, DefTable};

use crate::ir::{Arm, Block, Expr, ExprKind, Linked, Meta, Pattern, PatternKind, Stmt, StmtKind};

/// Report every use of a value after `drop` freed it.
pub fn check(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    for func in linked.funcs() {
        let Some(body) = &func.body else { continue };
        let mut cx = Dropped {
            defs,
            meta,
            out,
            depth: 0,
            declared: HashMap::new(),
            at: HashMap::new(),
        };
        for p in &func.params {
            cx.declared.insert(p.def, 0);
        }
        let mut state = HashSet::new();
        cx.block(body, &mut state);
    }
}

struct Dropped<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    out: &'a mut Vec<Diagnostic>,
    /// How many `loop` bodies deep the walk is.
    depth: usize,
    /// The loop depth each local was declared at, so a drop can tell "this one
    /// is new every iteration" from "this one is not".
    declared: HashMap<DefId, usize>,
    /// Where each dropped local was dropped, for the secondary span.
    at: HashMap<DefId, Option<FileSpan>>,
}

/// The set of locals dropped so far on the path in hand.
type State = HashSet<DefId>;

impl Dropped<'_> {
    /// Walk a block. The answer is whether control **leaves** it rather than
    /// falling off the end, which is what decides if its state joins the code
    /// after the branch it belongs to.
    fn block(&mut self, b: &Block, state: &mut State) -> bool {
        let mut leaves = false;
        for s in &b.stmts {
            self.stmt(s, state);
            leaves |= diverges(s);
        }
        if let Some(t) = &b.tail {
            self.expr(t, state);
        }
        // A `defer` body runs on the way out, after everything above it, so it
        // sees the state the block ends in — including a drop the block did.
        for d in crate::ir::defer_bodies(b) {
            self.expr(d, state);
        }
        leaves
    }

    fn stmt(&mut self, s: &Stmt, state: &mut State) {
        match &s.kind {
            StmtKind::Let { pattern, init } => {
                self.expr(init, state);
                self.declare(pattern);
            }
            StmtKind::Assign { place, value } => {
                self.expr(value, state);
                // A store into the whole local gives it an object again. Any
                // other place is an ordinary use of the base.
                match &place.kind {
                    ExprKind::Local(def) => {
                        state.remove(def);
                        self.at.remove(def);
                    }
                    _ => self.expr(place, state),
                }
            }
            StmtKind::Expr(e) | StmtKind::Return(Some(e)) | StmtKind::Break(Some(e)) => {
                self.expr(e, state)
            }
            // The body is walked once per block, after the statements, because
            // that is the state it runs in.
            StmtKind::Return(None)
            | StmtKind::Break(None)
            | StmtKind::Continue
            | StmtKind::Defer(_) => {}
        }
    }

    fn expr(&mut self, e: &Expr, state: &mut State) {
        match &e.kind {
            ExprKind::Local(def) => self.use_of(*def, e, state),
            // The drop itself. Its argument is read *before* the free, so a
            // first drop is not a use-after-drop of its own operand — but a
            // second one is a double free, which is the message that fits.
            ExprKind::Intrinsic { name, args } if name.as_str() == "drop" && args.len() == 1 => {
                match &args[0].kind {
                    ExprKind::Local(def) => self.drop_of(*def, e, state),
                    _ => self.expr(&args[0], state),
                }
            }
            // The control-flow forms: each decides for itself what "afterwards"
            // means, which is the whole of this pass.
            ExprKind::Block(b) => {
                self.block(b, state);
            }
            ExprKind::If { cond, then, els } => {
                self.expr(cond, state);
                let mut a = state.clone();
                let a_leaves = self.block(then, &mut a);
                let mut b = state.clone();
                let b_leaves = match els {
                    Some(els) => self.block(els, &mut b),
                    None => false,
                };
                if !a_leaves {
                    state.extend(a);
                }
                if !b_leaves {
                    state.extend(b);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, state);
                let before = state.clone();
                for Arm {
                    pattern,
                    guard,
                    body,
                    ..
                } in arms
                {
                    self.declare(pattern);
                    let mut arm = before.clone();
                    if let Some(g) = guard {
                        self.expr(g, &mut arm);
                    }
                    let leaves = match &body.kind {
                        ExprKind::Block(b) => self.block(b, &mut arm),
                        _ => {
                            self.expr(body, &mut arm);
                            false
                        }
                    };
                    if !leaves {
                        state.extend(arm);
                    }
                }
            }
            // A loop body runs again, so a drop inside it of something declared
            // outside it is a second free with nothing between the two.
            ExprKind::Loop { body } => {
                self.depth += 1;
                self.block(body, state);
                self.depth -= 1;
            }
            // A `-> never` call ends the path the same way a `return` does, and
            // the enclosing block's own statement list is where that shows up.
            _ => super::children_of(e, &mut |c| self.expr(c, state)),
        }
    }

    /// A mention of `def` in a value position.
    fn use_of(&mut self, def: DefId, at: &Expr, state: &State) {
        if !state.contains(&def) {
            return;
        }
        let name = self.defs.get(def).name.clone();
        let mut d = Diagnostic::error(format!("`{name}` is used after it was dropped"));
        if let Some(span) = self.meta.span(at.id) {
            d = d.with_primary(span, "the object this points at has been freed");
        }
        if let Some(Some(span)) = self.at.get(&def) {
            d = d.with_label(Label::secondary(
                *span,
                format!("`{name}` was dropped here"),
            ));
        }
        self.out.push(
            d.with_note(
                "`drop` releases the object before the collector would have (§6.9); after it, the \
             pointer names memory that is gone"
                    .to_string(),
            ),
        );
    }

    /// A `drop(def)`.
    fn drop_of(&mut self, def: DefId, at: &Expr, state: &mut State) {
        let name = self.defs.get(def).name.clone();
        let span = self.meta.span(at.id);
        if state.contains(&def) {
            let mut d = Diagnostic::error(format!("`{name}` is dropped twice"));
            if let Some(span) = span {
                d = d.with_primary(span, "the object was already freed");
            }
            if let Some(Some(first)) = self.at.get(&def) {
                d = d.with_label(Label::secondary(
                    *first,
                    format!("`{name}` was dropped here"),
                ));
            }
            self.out.push(d);
            return;
        }
        // Declared outside this loop: the next iteration reaches this line with
        // the object already freed, and there is no use in between to blame.
        if self.declared.get(&def).is_some_and(|&d| d < self.depth) {
            let mut d = Diagnostic::error(format!("`{name}` is dropped inside a loop"));
            if let Some(span) = span {
                d = d.with_primary(span, "this runs again, and the object is freed once");
            }
            self.out.push(
                d.with_note(
                    "a value declared outside the loop is the same value on every iteration; \
                 declare it inside, or drop it after the loop"
                        .to_string(),
                ),
            );
            return;
        }
        state.insert(def);
        self.at.insert(def, span);
    }

    /// Record every name a pattern binds, at the loop depth it binds them at.
    fn declare(&mut self, p: &Pattern) {
        match &p.kind {
            PatternKind::Wildcard | PatternKind::Lit(_) | PatternKind::Range { .. } => {}
            PatternKind::Binding { def, .. } => {
                self.declared.insert(*def, self.depth);
            }
            PatternKind::Variant { sub: ps, .. }
            | PatternKind::Tuple(ps)
            | PatternKind::Or(ps)
            | PatternKind::TupleStruct { elems: ps, .. } => ps.iter().for_each(|s| self.declare(s)),
            PatternKind::Struct { fields, .. } => fields.iter().for_each(|(_, s)| self.declare(s)),
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                if let Some(Some(b)) = rest {
                    self.declared.insert(b.def, self.depth);
                }
                prefix.iter().chain(suffix).for_each(|s| self.declare(s));
            }
            PatternKind::At { binding, pattern } => {
                self.declared.insert(binding.def, self.depth);
                self.declare(pattern);
            }
            PatternKind::Deref(inner) => self.declare(inner),
        }
    }
}

/// Whether this statement takes control out of the block it is in.
///
/// Shallow on purpose — see the note at the top. An `if` whose two sides both
/// leave counts, because that is the shape a `match`-less early return takes and
/// it is common enough to be worth the two lines.
fn diverges(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Return(_) | StmtKind::Break(_) | StmtKind::Continue => true,
        StmtKind::Expr(e) => expr_diverges(e),
        _ => false,
    }
}

fn expr_diverges(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Block(b) => b.stmts.iter().any(diverges),
        ExprKind::If {
            then,
            els: Some(els),
            ..
        } => then.stmts.iter().any(diverges) && els.stmts.iter().any(diverges),
        _ => false,
    }
}
