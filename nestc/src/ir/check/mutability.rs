//! The mutability check: nothing is written that did not grant permission.
//!
//! Inference deliberately skips this. It is not a typing question — every
//! program this rejects is already well-typed — and doing it here rather than on
//! the AST means every write is *explicit*: compound assignment has been
//! desugared to `place = place op x`, `a[i]` on a user type is already a call to
//! `IndexMut.index_mut`, and a `*mut self` method call already carries the
//! `&mut` that the surface `x.m()` left implicit. So there are exactly **two**
//! things to look at, and no surface form can hide either one:
//!
//! 1. an assignment, `place = value`;
//! 2. an `&mut place`.
//!
//! # The rule
//!
//! §2.3 draws a line this pass has to keep straight: **binding** mutability
//! (`let` vs `const`) and **reference** mutability (`*mut` vs `*`, `[]mut` vs
//! `[]`) are independent.
//!
//! ```text
//! const p := &mut x
//! p.* = 1        // legal: the pointer grants the write
//! p   = &mut y   // illegal: the binding does not
//! ```
//!
//! So permission is not one flag carried down a path. Walking a place from the
//! outside in, each projection either **inherits** its base's permission or
//! **restarts** the question from a type:
//!
//! | Projection | Where permission comes from |
//! |---|---|
//! | `x` | the binding's own mutability |
//! | `.field`, `.0` | the base — a field of an immutable value is immutable |
//! | `.*` | the **pointer's** type: `*mut T` grants, `*T` does not |
//! | `[i]` on a slice | the **slice's** type: `[]mut T` grants, `[]T` does not |
//! | `[i]` on an array | the base — an array is a value, its elements are part of it |
//!
//! The two restarting rows are what "the mutability of a path is the mutability
//! of its weakest link" means in practice, and they are also why a naive
//! left-to-right `&&` of flags gets this wrong in both directions.
//!
//! # One diagnostic per mistake
//!
//! A rejected write reports the **link that denied it**, not the whole path, and
//! reports it once. `a.b.c = 1` where `a` is a `const` is one error about `a`,
//! not three about `a`, `a.b` and `a.b.c`.

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::{DefKind, DefTable};
use crate::sema::ty::Ty;

use crate::ir::{Block, Expr, ExprKind, IrId, Linked, Meta, Stmt, StmtKind, Visitor, walk_expr};

/// Report every write the program was not given permission for.
pub fn check(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    for func in linked.funcs() {
        let Some(body) = &func.body else { continue };
        let mut w = Walk { defs, meta, out };
        w.block(body);
    }
}

struct Walk<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    out: &'a mut Vec<Diagnostic>,
}

impl Walk<'_> {
    fn block(&mut self, b: &Block) {
        for s in &b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &b.tail {
            self.expr(t);
        }
        for d in crate::ir::defer_bodies(b) {
            self.expr(d);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Assign { place, value } => {
                // A place first, and only then the question of permission: `f()
                // = 3` is not a read-only binding, it is not a binding at all.
                // Without this it lowered — the IR happily holds an `Assign`
                // whose left side is a call — and the program was accepted.
                if is_place(place) {
                    if let Some(why) = self.denial(place) {
                        self.report(place.id, &why, Use::Assign);
                    }
                } else if !self.meta.ty_or_error(place.id).mentions_error() {
                    let mut d = Diagnostic::error(
                        "cannot assign to this expression: it is not a place".to_string(),
                    );
                    if let Some(span) = self.meta.span(place.id) {
                        d = d.with_primary(span, "this computes a value, it does not name one");
                    }
                    self.out.push(
                        d.with_note(
                            "the left side of an assignment has to be a binding, a field, a \
                         tuple element, an index or a dereference"
                                .to_string(),
                        ),
                    );
                }
                // The place's own sub-expressions still hold writes of their
                // own: `a[f(&mut b)] = 1` has one inside the index.
                self.expr(value);
                self.subexprs(place);
            }
            StmtKind::Let { init, .. } => self.expr(init),
            StmtKind::Return(v) | StmtKind::Break(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
            }
            // Walked with the block's, after the statements.
            StmtKind::Continue | StmtKind::Defer(_) => {}
            StmtKind::Expr(e) => self.expr(e),
        }
    }

    fn expr(&mut self, e: &Expr) {
        if let ExprKind::Ref {
            mutable: true,
            place,
        } = &e.kind
            && let Some(why) = self.denial(place)
        {
            self.report(place.id, &why, Use::MutRef);
        }
        match &e.kind {
            ExprKind::Block(b) | ExprKind::Loop { body: b } => self.block(b),
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
            _ => self.subexprs(e),
        }
    }

    /// Visit `e`'s children without re-examining `e` itself.
    fn subexprs(&mut self, e: &Expr) {
        struct Kids<'w, 'a>(&'w mut Walk<'a>);
        impl Visitor for Kids<'_, '_> {
            fn visit_expr(&mut self, e: &Expr) {
                self.0.expr(e);
            }
        }
        walk_expr(&mut Kids(self), e);
    }

    // ===< The rule >===

    /// Why `place` may not be written, or `None` if it may.
    ///
    /// Returns the **innermost** reason: the walk stops at the first link that
    /// denies permission, and everything further out is irrelevant once one
    /// link has said no.
    fn denial(&self, place: &Expr) -> Option<Denial> {
        match &place.kind {
            ExprKind::Local(def) | ExprKind::Global(def) => {
                let d = self.defs.get(*def);
                if d.mutable {
                    return None;
                }
                Some(Denial::Binding {
                    at: place.id,
                    name: d.name.to_string(),
                    param: d.kind == DefKind::Param,
                })
            }
            // A projection out of a value: whatever the value allows.
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => self.denial(base),
            // `a[i]` is `index(&a, i).*` (§6.13), so by the time a place
            // reaches here the sequence the program wrote is three nodes down.
            // Ask the question the surface form asked, rather than the one the
            // pointer in between happens to answer — the two agree, and only one
            // of them can say "a read-only slice".
            ExprKind::Deref { base } if sequence_index(base).is_some() => {
                let recv = sequence_index(base).expect("checked");
                match self.meta.ty_or_error(recv.id) {
                    // A slice is a *view*: its own permission decides, exactly
                    // as a pointer's does.
                    Ty::Ptr { inner, .. } => match *inner {
                        Ty::Slice { mutable: true, .. } => None,
                        Ty::Slice { mutable: false, .. } => Some(Denial::Slice { at: recv.id }),
                        // An array is a value. Its elements are part of whatever
                        // holds it, so the base decides.
                        Ty::Array { .. } => match &recv.kind {
                            ExprKind::Ref { place, .. } => self.denial(place),
                            _ => None,
                        },
                        _ => None,
                    },
                    _ => None,
                }
            }
            // A dereference restarts the question at the pointer's permission.
            // The *binding* holding the pointer has nothing to say about it.
            ExprKind::Deref { base } => match self.meta.ty_or_error(base.id) {
                Ty::Ptr { mutable: true, .. } => None,
                // A closure reaching into itself: what it holds by value is a
                // `[n]` copy, which is read-only (§5.5).
                Ty::Ptr { inner, .. }
                    if matches!(&*inner, Ty::Nominal { def, .. }
                        if self.defs.get(*def).kind == DefKind::Closure) =>
                {
                    Some(Denial::Capture { at: base.id })
                }
                Ty::Ptr { mutable: false, .. } => Some(Denial::Pointer { at: base.id }),
                // Not a pointer: already diagnosed as a type error.
                _ => None,
            },
            // Not a place at all — assigning to one is a different error, and
            // `&mut` of a temporary is legal (it addresses the temporary).
            _ => None,
        }
    }

    fn report(&mut self, place: IrId, why: &Denial, used: Use) {
        let what = match used {
            Use::Assign => "assign to",
            Use::MutRef => "take a mutable pointer to",
        };
        let (message, label, note) = match why {
            Denial::Binding { name, param, .. } => (
                format!("cannot {what} `{name}`: it is not a mutable binding"),
                "declared immutable here".to_string(),
                if *param {
                    "a parameter is an immutable binding (§5.2); rebind it with `let` for a \
                     mutable copy"
                        .to_string()
                } else {
                    "declare it with `let` instead of `const`, or write `mut` in the pattern \
                     that binds it"
                        .to_string()
                },
            ),
            Denial::Pointer { .. } => (
                format!("cannot {what} the pointee of a read-only pointer"),
                "this is a `*T`".to_string(),
                "writing through a pointer requires `*mut T` (§3.2)".to_string(),
            ),
            Denial::Capture { .. } => (
                format!("cannot {what} a copy the closure captured"),
                "copied here, when the closure was made".to_string(),
                "a name in a closure's capture list (`[n]`) is a read-only copy; leave it out of \
                 the list to share the local itself (§5.5)"
                    .to_string(),
            ),
            Denial::Slice { .. } => (
                format!("cannot {what} an element of a read-only slice"),
                "this is a `[]T`".to_string(),
                "writing an element requires `[]mut T` (§3.2)".to_string(),
            ),
        };

        let mut d = Diagnostic::error(message);
        // Point at the write, and mark the link that denied it.
        if let Some(span) = self.meta.span(place) {
            d = d.with_primary(span, "");
        }
        if let Some(span) = self.meta.span(why.at())
            && Some(why.at()) != Some(place)
        {
            d = d.with_label(crate::common::diagnostic::Label::secondary(span, label));
        } else if let Some(span) = self.meta.span(why.at()) {
            d = Diagnostic::error(d.message).with_primary(span, label);
        }
        self.out.push(d.with_note(note));
    }
}

/// What the program was trying to do, which only changes the wording.
#[derive(Clone, Copy)]
enum Use {
    Assign,
    MutRef,
}

/// The link in a place path that refused the write.
enum Denial {
    /// A binding declared without `let` / `mut`.
    Binding { at: IrId, name: String, param: bool },
    /// A `.*` through a `*T`.
    Pointer { at: IrId },
    /// An `[i]` into a `[]T`.
    Slice { at: IrId },
    /// A closure's `[n]` copy.
    Capture { at: IrId },
}

impl Denial {
    fn at(&self) -> IrId {
        match self {
            Denial::Binding { at, .. }
            | Denial::Pointer { at }
            | Denial::Slice { at }
            | Denial::Capture { at } => *at,
        }
    }
}

/// The receiver of an `index` intrinsic call, when `e` is one.
///
/// `a[i]` lowers to `index(&a, i).*` for every type (§6.13), and for the two
/// built-in sequences the member that resolves to is `#intrinsic`, so what the
/// IR holds is the operation rather than a call. This is how a place walk finds
/// the sequence again.
fn sequence_index(e: &Expr) -> Option<&Expr> {
    match &e.kind {
        ExprKind::Intrinsic { name, args } if name.as_str() == "index" && args.len() == 2 => {
            Some(&args[0])
        }
        _ => None,
    }
}

/// Whether `e` **names** storage rather than computing a value.
///
/// The five forms are the ones [`Checker::denial`] knows how to ask a
/// permission question about, and that is not a coincidence: a place is exactly
/// what has an address to write through. `a[i]` is absent because it is already
/// `index(&a, i).*` by this point (§6.13) — a [`ExprKind::Deref`].
fn is_place(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Local(_)
            | ExprKind::Global(_)
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Deref { .. }
    )
}
