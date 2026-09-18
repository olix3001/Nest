//! An error type that reached the linked IR with **nothing reported**.
//!
//! [`Ty::Error`](crate::sema::ty::Ty::Error) is inference's placeholder for
//! "already said something about this", and every other pass asks
//! [`mentions_error`](crate::sema::ty::Ty::mentions_error) before it speaks so
//! one mistake stays one diagnostic. That contract has an unguarded half: when
//! a path *produces* an error type without reporting, nothing downstream
//! notices. An error type unifies with everything, so the program type-checks
//! against nothing at all, and the first thing that objects is the **backend** —
//! `Void is not a type a value can have`, raised in whatever function *called*
//! the one with the mistake in it, with no span and no source line.
//!
//! So this pass is not about the program. It runs only when nothing else had
//! anything to say, and what it reports is a defect in the compiler: an error
//! type survived, here is where, and here is the span that would have carried
//! the missing diagnostic. That is the same treatment monomorphization already
//! gives a bound that vanished between inference and instantiation.
//!
//! **One per function.** An error type propagates to every parent expression,
//! so the node that has it is rarely alone; saying it once per body names the
//! function to look at without burying it.

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::DefTable;

use crate::ir::{Expr, ExprKind, IrId, Linked, Meta};

/// Report any surviving error type, at most once per function body.
pub fn check(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    for func in linked.funcs() {
        let Some(body) = &func.body else { continue };
        let mut found: Option<IrId> = None;
        super::block_children(body, &mut |e| walk(meta, e, &mut found));
        let Some(id) = found else { continue };
        let name = defs.get(func.def).name.clone();
        let mut d = Diagnostic::error(format!(
            "internal: an error type reached code generation in `{name}`"
        ));
        if let Some(span) = meta.span(id) {
            d = d.with_primary(span, "this expression has no type");
        }
        out.push(
            d.with_note(
                "nothing was reported about it, so inference produced an error type without \
             a diagnostic \u{2014} this is a compiler defect"
                    .to_string(),
            ),
        );
    }
}

/// The **innermost** erroneous expression, which is the one worth pointing at:
/// an error type propagates outwards, so the outermost node carrying one is
/// usually the whole statement and the innermost is the mistake.
fn walk(meta: &Meta, e: &Expr, found: &mut Option<IrId>) {
    super::children_of(e, &mut |c| walk(meta, c, found));
    if found.is_some() {
        return;
    }
    if matches!(e.kind, ExprKind::Error) || meta.ty_or_error(e.id).mentions_error() {
        *found = Some(e.id);
    }
}
