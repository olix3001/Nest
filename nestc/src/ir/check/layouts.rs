//! Laying out every type the program declares.
//!
//! [`layout`](crate::ir::layout) is a *query*: it answers about a type when
//! something asks. This is the pass that asks about every type a program
//! declares, and keeps the answer — a concrete type's [`Layout`] is stamped on
//! its [`TypeDef`](crate::ir::TypeDef), so the LIR lowering, the root maps,
//! debug info and codegen read it rather than each re-asking. Four consumers
//! computing their own offsets would be four chances to disagree about where a
//! field is, and a disagreement about that is not a wrong answer, it is a wrong
//! program.
//!
//! A **generic** type is skipped, and not because it is hard: `Pair.<T>` has no
//! layout, in the same way and for the same reason that `T` does not. Its
//! instantiations do, and each of those arrives at the query when a use site
//! mentions it.
//!
//! # Why this pass reports almost nothing
//!
//! It would be natural for it to report every type it cannot lay out. It does
//! not, because **every one of those has already been reported** by the check
//! that owns the question:
//!
//! - a member that is `dyn Trait` by value is rejected where it is written, by
//!   the rule that a trait object is reached through a pointer (§3.4);
//! - a type that contains itself is rejected by `declarations::recursive_layouts`
//!   — and the reason it has no size is the cycle, which is a better sentence
//!   than "it nests too deeply";
//! - a member whose type is in error was reported by inference.
//!
//! So a failure here would be a second diagnostic for one mistake, which is the
//! thing the whole diagnostic discipline in this compiler is arranged to avoid.
//! A type that fails to lay out *and* has no diagnostic would be a defect in
//! this compiler rather than in the program; the place to catch that is a test,
//! and there is one.
//!
//! **Two failures are not like the others**, and they are the ones this pass
//! does report.
//!
//! The first is an `opaque` held by value. The resolver refuses it where it is
//! written, which is the diagnostic worth having; but an alias (`A :: opaque`)
//! and a generic argument both put one into a member without a spelling for the
//! resolver to look at, and an opaque type by value has no meaning at all. The
//! rule is enforced in both places deliberately: on the spelling for the
//! message, here for the guarantee.
//!
//! The second is a type the target cannot address
//! ([`LayoutError::TooLarge`](crate::ir::layout::LayoutError::TooLarge)).
//! `struct { a: [1 << 62]u64, b: [1 << 62]u64 }` is well-typed, its members are
//! well-typed, it contains itself nowhere, and nothing above this point has any
//! reason to look at it — being too big is a fact about the *size*, and the size
//! is computed here. No other check owns the question, so this one does.

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::{DefKind, DefTable};
use crate::sema::ty::Ty;

use crate::ir::layout::{LayoutError, Layouts};
use crate::ir::{Linked, Meta, TypeDef, TypeDefKind};

/// Lay out every concrete type, stamping what worked.
pub fn check(
    defs: &DefTable,
    meta: &Meta,
    linked: &Linked,
    layouts: &Layouts,
    out: &mut Vec<Diagnostic>,
) {
    for t in linked.types() {
        // A trait is not a run-time type; `dyn Trait` is what stands for one,
        // and it is unsized by design rather than by mistake.
        if matches!(t.kind, TypeDefKind::Trait { .. }) {
            continue;
        }
        // `Handle :: distinct opaque` is how a library mints a nominal handle,
        // so that its `*Handle` does not interchange with every other
        // `*opaque`. A `distinct` over an opaque type does not *hold* one — it
        // **is** one, with a name of its own — so it is as sizeless as what it
        // stands over, and refusing it here would refuse the sanctioned
        // spelling. Its own uses by value are refused wherever they are
        // written, by the same rule as `opaque`'s, because the layout of
        // `Handle` fails in exactly the same way.
        if is_distinct_opaque(meta, t) {
            continue;
        }
        soa_is_not_consumed_yet(meta, t, out);

        // A type that contains itself has no size, and the declaration check
        // has already said so. Laying it out would not produce a second
        // diagnostic so much as a worse one.
        if meta.has::<super::declarations::RecursiveLayout>(t.id) {
            continue;
        }
        let Some(ty) = meta.ty(t.id) else { continue };
        if is_generic(defs, &ty) {
            continue;
        }
        match layouts.of(&ty) {
            Ok(layout) => {
                meta.set(t.id, layout);
            }
            // `opaque` by value. The resolver refuses it where it is *written*
            // — `struct { f: opaque }`, a parameter, a slice element — and that
            // is the diagnostic a program normally gets, because it points at
            // the spelling. It cannot be the whole rule, though: a `::` binding
            // over `opaque` puts the name behind an alias the resolver sees as
            // an ordinary path, and a generic argument substitutes one in long
            // after resolution. The layout query sees through both, because by
            // the time a type is laid out every alias is gone.
            //
            // So the position rule is enforced twice on purpose: once on the
            // spelling, for the message, and once here, for the guarantee. An
            // opaque type has no size, so a value of one has no meaning — there
            // must be no way to write it.
            Err(err @ LayoutError::Opaque(_)) => {
                let mut d = Diagnostic::error(format!("`{}`: {}", t.name, err.message()));
                if let Some(span) = meta.span(t.id) {
                    d = d.with_primary(span, "holds an opaque type by value");
                }
                out.push(d.with_note(
                    "an opaque type has no size and no values; hold a pointer to it (`*opaque`)                      instead",
                ));
            }
            // Every other error is already somebody's diagnostic; this one is
            // nobody's until it is this one's (see the module docs).
            Err(err @ LayoutError::TooLarge(_)) => {
                let mut d = Diagnostic::error(format!("`{}`: {}", t.name, err.message()));
                if let Some(span) = meta.span(t.id) {
                    d = d.with_primary(span, "does not fit in the target's address space");
                }
                out.push(d.with_note(format!(
                    "the largest object this target can hold is {} bytes; a size is a `usize` \
                     and the distance between two addresses inside one object is an `isize`",
                    layouts.max_size()
                )));
            }
            Err(_) => {}
        }
    }
}

/// Whether `t` is a `distinct` standing directly over an opaque type.
///
/// Read off the `distinct`'s single member, which is the type it stands over —
/// the same place every other pass reads a `distinct`'s representation from.
fn is_distinct_opaque(meta: &Meta, t: &TypeDef) -> bool {
    let TypeDefKind::Distinct { repr } = &t.kind else {
        return false;
    };
    matches!(meta.ty(repr.id), Some(Ty::Opaque))
}

/// Whether `ty` mentions a generic parameter anywhere.
fn is_generic(defs: &DefTable, ty: &Ty) -> bool {
    match ty {
        Ty::Nominal { def, args } => {
            (args.is_empty() && defs.get(*def).kind == DefKind::TypeParam)
                || args.iter().any(|a| is_generic(defs, a))
        }
        Ty::Ptr { inner, .. }
            | Ty::Slice { inner, .. }
            | Ty::Array { inner, .. }
            | Ty::Spread(inner) => {
            is_generic(defs, inner)
        }
        Ty::Tuple(elems) => elems.iter().any(|e| is_generic(defs, e)),
        Ty::Dyn { assoc, .. } => assoc.iter().any(|(_, t)| (|e| is_generic(defs, e))(t)),
        Ty::Func { params, ret, .. } => {
            params.iter().any(|p| is_generic(defs, p)) || is_generic(defs, ret)
        }
        // `int.<N>` inside a family impl: the width is a parameter, so the type
        // is every bit as generic as one over a `T`.
        Ty::Int { width, .. } => width.bits().is_none(),
        _ => false,
    }
}

/// `#soa` is recognized, checked for where it may be written, and **not yet
/// acted on**. Saying so is the honest thing: a directive that is silently
/// ignored is worse than one that is not implemented.
///
/// What it is waiting for is not layout arithmetic. Storing a `[N]Particle`
/// column-wise means `&a[i]` no longer names a contiguous `Particle`, so a place
/// projection through a `#soa` array is a different operation — and what a place
/// projection *is* belongs to the LIR lowering (`design/lir.md` §1), which does
/// not exist yet. Laying the columns out before then would produce offsets that
/// nothing could use and that nothing would notice were wrong.
fn soa_is_not_consumed_yet(meta: &Meta, t: &TypeDef, out: &mut Vec<Diagnostic>) {
    if !meta.directives(t.id).iter().any(|d| d.is("soa")) {
        return;
    }
    let mut d = Diagnostic::warning(format!(
        "`#soa` on `{}` is not consumed yet; it is laid out array-of-structs",
        t.name
    ));
    if let Some(span) = meta.span(t.id) {
        d = d.with_primary(span, "");
    }
    out.push(
        d.with_note(
            "storing columns changes what `&a[i]` names, which is a decision the LIR lowering \
         has to make first"
                .to_string(),
        ),
    );
}
