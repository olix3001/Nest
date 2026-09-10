//! Evaluating every constant and static initializer, and reporting the ones
//! that cannot be.
//!
//! This is where [`ConstEval`](crate::ir::const_eval::ConstEval) meets the
//! program. `check::constness` already asked the *structural* question — may
//! this construct run at compile time — and this pass asks the one only an
//! evaluator can answer: **what value does it produce**. The two are separate on
//! purpose. `A :: factorial(5)` passes the structural check the moment
//! `factorial` is `#const`; whether it terminates, and what it returns, is not a
//! property of the syntax.
//!
//! Both declaration forms are checked by the same rule, because both make the
//! same promise:
//!
//! - A `::` constant (§2.5) *is* its value. There is no run-time moment at which
//!   it could be computed — every use site expects the number.
//! - A `#static` region's initializer must be `#const` (§2.6), because the value
//!   is baked into the program's initialized data. Nest is garbage-collected and
//!   a static is not a place the collector can trace, which is why the rule is a
//!   rule rather than a convenience: a global that needed to allocate could not
//!   be represented at all. A static with no initializer is zeroed and has
//!   nothing to evaluate.
//!
//! A value that evaluates is recorded on the global's node as a
//! [`ConstValue`](crate::ir::const_eval::ConstValue), the same way every other
//! per-node fact is recorded. That is what makes it available to the consumers
//! that come later — monomorphization naming `h.<4>`, and codegen emitting an
//! initialized region — without any of them re-running the interpreter.

use crate::common::diagnostic::Diagnostic;
use crate::common::target::Target;
use crate::sema::def::DefTable;
use crate::sema::ty::Ty;

use crate::ir::const_eval::ConstEval;
use crate::ir::{Linked, Meta};

/// Evaluate every global initializer, recording what it produced and reporting
/// what it could not.
pub fn check(
    defs: &DefTable,
    meta: &Meta,
    linked: &Linked,
    target: Target,
    out: &mut Vec<Diagnostic>,
) {
    let mut cx = ConstEval::new(defs, meta, linked, target);
    for global in linked.globals() {
        // A `#static` with no `:=` is zeroed; there is no expression to run.
        let Some(init) = &global.init else { continue };

        // A constant that names a **function** is a symbol, not a value: it is
        // as fixed as any number, but what it stands for is an address the
        // linker chooses. Asking the evaluator for its value would report a
        // failure on a binding that is perfectly well formed.
        if matches!(meta.ty_or_error(global.id), Ty::Func { .. }) {
            continue;
        }

        match cx.eval(init) {
            Ok(value) => {
                meta.set(global.id, value);
            }
            Err(err) => {
                let what = if global.mutable {
                    format!("the initializer of `#static {}`", global.name)
                } else {
                    format!("the constant `{}`", global.name)
                };
                out.push(err.to_diagnostic(meta, &what).with_note(if global.mutable {
                    "a `#static` region's initial contents are written into the program's data, \
                     so its initializer must be computable now (§2.6)"
                } else {
                    "a `::` binding is its value: it has no run-time moment at which it could \
                     be computed (§2.5)"
                }));
            }
        }
    }
}
