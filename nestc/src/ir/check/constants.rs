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
use crate::sema::def::DefTable;
use crate::sema::infer::RangeReported;
use crate::sema::ty::Ty;

use crate::ir::const_eval::{ConstEval, ConstValue};
use crate::ir::layout::Layouts;
use crate::ir::{Expr, ExprKind, Linked, Meta, Visitor, walk_expr};

/// Evaluate every global initializer, recording what it produced and reporting
/// what it could not.
pub fn check(
    defs: &DefTable,
    meta: &Meta,
    linked: &Linked,
    layouts: &Layouts,
    out: &mut Vec<Diagnostic>,
) {
    let mut cx = ConstEval::new(defs, meta, linked, layouts);
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

        // A constant declared in a **generic impl** has no single value:
        // `MAX` on `impl <const N: u16> uint.<N>` is 255 read through `u8` and
        // 65535 read through `u16`. There is nothing to record here and nothing
        // to report — each *read* is evaluated at what it bound the impl's
        // parameters to, by `use_sites` below.
        if meta
            .get::<crate::sema::infer::Generics>(global.id)
            .is_some_and(|g| !g.params.is_empty())
        {
            continue;
        }

        match cx.eval(init) {
            Ok(value) => {
                meta.set(global.id, value);
            }
            Err(err) => {
                // Inference already reported this exact conversion, at the
                // literal itself and with a better span. One mistake, one
                // diagnostic (see [`RangeReported`]).
                if meta.get::<RangeReported>(err.at).is_some() {
                    continue;
                }
                // The same rule, said by the evaluator rather than found here:
                // the constant genuinely has no value, and the reason it has
                // none is already on the screen (see [`ConstError::reported`]).
                if err.reported {
                    continue;
                }
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
    comptime_assertions(defs, meta, linked, layouts, out);
}

/// Evaluate every **read** of a constant that a generic impl declares, and
/// record the value that read produces.
///
/// `MAX` on `impl <const N: u16> uint.<N>` is one declaration and a value per
/// width, so there is nothing to fold once and put on the declaration — the
/// value belongs to the read. `u8.MAX` is 255 and `u16.MAX` is 65535, and both
/// are the same `Global` naming the same def; what separates them is the
/// [`Instantiation`](crate::sema::infer::Instantiation) inference stamped on
/// the read, which is what the evaluator zips against the declaration's
/// parameters.
///
/// It runs **after** monomorphization, and has to: a read inside a generic body
/// names that body's parameters until an instantiation replaces them, and
/// `Cloner` is what carries the substituted arguments onto the copy.
pub fn use_sites(
    defs: &DefTable,
    meta: &Meta,
    linked: &Linked,
    layouts: &Layouts,
    out: &mut Vec<Diagnostic>,
) {
    let mut v = GenericReads {
        cx: ConstEval::new(defs, meta, linked, layouts),
        defs,
        meta,
        linked,
        out,
    };
    for func in linked.funcs() {
        if let Some(body) = &func.body {
            v.visit_block(body);
        }
    }
}

struct GenericReads<'a, 'b> {
    cx: ConstEval<'a>,
    defs: &'a DefTable,
    meta: &'a Meta,
    linked: &'a Linked,
    out: &'b mut Vec<Diagnostic>,
}

impl Visitor for GenericReads<'_, '_> {
    fn visit_expr(&mut self, e: &Expr) {
        if let ExprKind::Global(def) = &e.kind {
            let def = self.defs.resolve_alias(*def);
            let generic = self
                .linked
                .global(def)
                .map(|g| g.id)
                .and_then(|id| self.meta.get::<crate::sema::infer::Generics>(id))
                .is_some_and(|g| !g.params.is_empty());
            if generic && self.meta.get::<ConstValue>(e.id).is_none() {
                match self.cx.eval_global_at(def, e.id) {
                    // Recorded on the **read**, not on the declaration: that is
                    // the node that has one answer.
                    Ok(value) => {
                        self.meta.set(e.id, value);
                    }
                    Err(err) if err.reported => {}
                    Err(err) => {
                        let what = format!("the constant `{}`", self.defs.get(def).name);
                        self.out.push(err.to_diagnostic(self.meta, &what).with_note(
                            "a constant an `impl`'s generics appear in is evaluated where it is \
                             read, at whatever that read makes them (§2.5)",
                        ));
                    }
                }
            }
        }
        walk_expr(self, e);
    }
}

/// Judge every `comptime_assert(cond)` in the program (§6.10).
///
/// This is the whole of what the intrinsic does. The condition is evaluated
/// here and the call lowers to nothing at all, so a false one stops the build
/// and a true one costs the program nothing — which is the difference between
/// it and the run-time `assert`, an ordinary `core` function that panics.
///
/// A condition that cannot be evaluated is its own error, and a different one:
/// "this assertion is false" and "I could not tell" are not the same thing to
/// report, and only the first is the program saying something untrue.
fn comptime_assertions(
    defs: &DefTable,
    meta: &Meta,
    linked: &Linked,
    layouts: &Layouts,
    out: &mut Vec<Diagnostic>,
) {
    let mut v = Assertions {
        cx: ConstEval::new(defs, meta, linked, layouts),
        meta,
        out,
    };
    for func in linked.funcs() {
        if let Some(body) = &func.body {
            v.visit_block(body);
        }
    }
}

struct Assertions<'a, 'b> {
    cx: ConstEval<'a>,
    meta: &'a Meta,
    out: &'b mut Vec<Diagnostic>,
}

impl Visitor for Assertions<'_, '_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::Intrinsic { name, args } = &expr.kind
            && name.as_str() == "comptime_assert"
            && let Some(cond) = args.first()
        {
            match self.cx.eval(cond) {
                Ok(ConstValue::Bool(true)) => {}
                Ok(_) => {
                    let mut d = Diagnostic::error("assertion failed at compile time");
                    if let Some(span) = self.meta.span(expr.id) {
                        d = d.with_primary(span, "this is false");
                    }
                    self.out.push(d.with_note(
                        "`comptime_assert` is checked while compiling; the run-time assertion is \
                         `assert`, which panics instead",
                    ));
                }
                // Already somebody's diagnostic — the same rule the globals
                // above follow.
                Err(err) if err.reported || self.meta.get::<RangeReported>(err.at).is_some() => {}
                Err(err) => self
                    .out
                    .push(err.to_diagnostic(self.meta, "a `comptime_assert` condition")),
            }
        }
        walk_expr(self, expr);
    }
}
