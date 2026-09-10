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
        for d in &b.defers {
            self.expr(d);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Assign { place, value } => {
                if let Some(why) = self.denial(place) {
                    self.report(place.id, &why, Use::Assign);
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
            StmtKind::Continue => {}
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
            // A dereference restarts the question at the pointer's permission.
            // The *binding* holding the pointer has nothing to say about it.
            ExprKind::Deref { base } => match self.meta.ty_or_error(base.id) {
                Ty::Ptr { mutable: true, .. } => None,
                Ty::Ptr { mutable: false, .. } => Some(Denial::Pointer { at: base.id }),
                // Not a pointer: already diagnosed as a type error.
                _ => None,
            },
            ExprKind::Index { base, .. } => match self.meta.ty_or_error(base.id) {
                // A slice is a *view*: its own permission decides, exactly as a
                // pointer's does.
                Ty::Slice { mutable: true, .. } => None,
                Ty::Slice { mutable: false, .. } => Some(Denial::Slice { at: base.id }),
                // An array is a value. Its elements are part of whatever holds
                // it, so the base decides.
                Ty::Array { .. } => self.denial(base),
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
}

impl Denial {
    fn at(&self) -> IrId {
        match self {
            Denial::Binding { at, .. } | Denial::Pointer { at } | Denial::Slice { at } => *at,
        }
    }
}
