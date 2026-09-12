//! Indexing past the end of a fixed-size array, caught where it is written.
//!
//! Out-of-bounds indexing **traps at run time** (§3.2), and the LIR lowering is
//! what emits that trap: a comparison against the length, an edge, and a block
//! that does not come back. This pass is about the cases where waiting for run
//! time would be absurd.
//!
//! A `[N]T` carries its length **in its type** (§3.2), so when the index is also
//! a compile-time value the comparison has two known numbers in it and an answer
//! that cannot change. `a[7]` on a `[3]i32` is not a program that might be fine
//! on some path — there is no path, no input and no build setting under which it
//! is anything but a trap. Reporting it here costs the program nothing it could
//! have used and saves a failure that only shows up when the line runs.
//!
//! # What it deliberately does not do
//!
//! - **Slices.** A `[]T`'s length is a run-time value, so there is nothing to
//!   compare against. The check is the one LIR emits.
//! - **Any index it cannot fold.** A loop counter, a parameter, an arithmetic
//!   result the evaluator gives up on — all of them are run-time checks, and
//!   guessing at them is how a bounds checker starts rejecting correct programs.
//!   The evaluator failing is not evidence of anything, so this pass says
//!   nothing about it.
//! - **Proving a run-time index in range.** That is an optimization, it belongs
//!   where the CFG is, and it is not a diagnostic either way.
//!
//! So the rule is narrow on purpose: **both** numbers known, and then it is an
//! error rather than a lint, because a lint is for code that might be right.

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::DefTable;
use crate::sema::ty::Ty;

use crate::ir::const_eval::ConstEval;
use crate::ir::layout::Layouts;
use crate::ir::{Expr, ExprKind, Linked, Meta};

/// Report every `a[i]` whose `a` is a fixed-size array and whose `i` is a
/// constant past its end.
pub fn check(
    defs: &DefTable,
    meta: &Meta,
    linked: &Linked,
    layouts: &Layouts,
    out: &mut Vec<Diagnostic>,
) {
    let mut cx = ConstEval::new(defs, meta, linked, layouts);
    for func in linked.funcs() {
        let Some(body) = &func.body else { continue };
        super::block_children(body, &mut |e| walk(defs, meta, &mut cx, e, out));
    }
}

fn walk(
    defs: &DefTable,
    meta: &Meta,
    cx: &mut ConstEval,
    e: &Expr,
    out: &mut Vec<Diagnostic>,
) {
    index_of(e, meta).inspect(|(len, index, ty)| {
        // A failure here means "not a compile-time value", which is the
        // overwhelmingly common case and is not a mistake: the check is the one
        // LIR emits. Only a value that *is* known and *is* out of range is
        // something to say.
        let Ok(value) = cx.eval(index) else { return };
        let Some(i) = value.as_u64() else { return };
        if i < *len {
            return;
        }
        let mut d = Diagnostic::error(format!(
            "index {i} is out of bounds for `{}`",
            ty.display(defs)
        ));
        if let Some(span) = meta.span(index.id) {
            d = d.with_primary(span, format!("this array has {len} elements"));
        }
        out.push(d.with_note(
            "a fixed-size array carries its length in its type (§3.2), so an index that is \
             also known cannot be in range on some other run"
                .to_string(),
        ));
    });
    super::children_of(e, &mut |c| walk(defs, meta, cx, c, out));
}

/// The length, the index expression and the array's type, when `e` is an
/// `index(&a, i)` call on a fixed-size array.
///
/// `a[i]` is `Index.index(&a, i).*` for every type (§6.13), and the sequences'
/// member is `#intrinsic`, so what the IR holds is the operation. A slice gives
/// `None`: its length is a run-time value.
fn index_of<'a>(e: &'a Expr, meta: &Meta) -> Option<(u64, &'a Expr, Ty)> {
    let ExprKind::Intrinsic { name, args } = &e.kind else {
        return None;
    };
    if name.as_str() != "index" || args.len() != 2 {
        return None;
    }
    let recv = meta.ty_or_error(args[0].id);
    let seq = match recv {
        Ty::Ptr { inner, .. } => *inner,
        other => other,
    };
    let Ty::Array { len, .. } = &seq else {
        return None;
    };
    Some((len.value()?, &args[1], seq.clone()))
}
