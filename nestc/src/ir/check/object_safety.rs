//! Object safety: whether a trait can have a vtable at all.
//!
//! `dyn Trait` already coerces and dispatches, but nothing has been asking
//! whether the trait *can* be turned into a trait object. A `*dyn Trait` is a
//! fat pointer — the value, and a table of the trait's methods — and some
//! signatures have no slot they could occupy:
//!
//! | Rejected | Why there is no slot for it |
//! |---|---|
//! | no `self` receiver | there is no value to dispatch on; the table is reached *through* the receiver |
//! | a generic method | one slot cannot stand for an unbounded family of instantiations |
//! | `Self` taken or returned **by value** | the size of `Self` is erased, so the caller cannot lay out the argument or the result |
//! | an associated constant | a vtable holds code, not values, and every impl would want a different one |
//!
//! `self: *Self` and `self: *mut Self` are fine: a pointer is one word whatever
//! it points at. That is the whole distinction — erasing the type erases the
//! *size*, and everything above is a place where the size was still needed.
//!
//! # Reported at the coercion, not at the declaration
//!
//! A trait that is never turned into a trait object is under no obligation to be
//! object-safe, and most traits are not: `Add` returns `Self.Output`, `Iterator`
//! is generic, and both are perfectly good traits. The mistake is only ever
//! *asking for a vtable*, so that is where it is reported — pointing at the
//! coercion the program wrote, with the method that made it impossible named as
//! the reason.

use std::collections::HashSet;

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::{DefId, DefTable};
use crate::sema::ty::Ty;

use crate::ir::{Block, Expr, ExprKind, IrId, Linked, Meta, Recv, TraitMethod, TypeDefKind};

/// Report every `dyn` coercion whose trait cannot have a vtable.
pub fn check(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    let cx = Cx { defs, meta, linked };
    // A trait used in ten coercions is one unsafe trait, but each coercion is a
    // separate mistake at a separate place — so report per coercion site, and
    // dedupe only within one site.
    let mut seen: HashSet<IrId> = HashSet::new();
    for func in linked.funcs() {
        let Some(body) = &func.body else { continue };
        cx.walk(body, &mut |e| {
            let ExprKind::DynCast { .. } = &e.kind else {
                return;
            };
            if !seen.insert(e.id) {
                return;
            }
            let Some(trait_def) = cx.trait_of(e) else {
                return;
            };
            cx.check_coercion(e.id, trait_def, out);
        });
    }
}

#[derive(Clone, Copy)]
struct Cx<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    linked: &'a Linked,
}

impl Cx<'_> {
    /// The trait a `dyn` coercion erases to, read off the coerced type.
    fn trait_of(&self, e: &Expr) -> Option<DefId> {
        let mut ty = self.meta.ty(e.id)?;
        while let Ty::Ptr { inner, .. } = ty {
            ty = *inner;
        }
        match ty {
            Ty::Dyn(def) => Some(self.defs.resolve_alias(def)),
            _ => None,
        }
    }

    fn check_coercion(&self, at: IrId, trait_def: DefId, out: &mut Vec<Diagnostic>) {
        let Some(def) = self.linked.ty(trait_def) else {
            return;
        };
        let TypeDefKind::Trait {
            methods,
            assoc_consts,
        } = &def.kind
        else {
            return;
        };
        let name = self.defs.canonical_string(trait_def);

        // One diagnostic per coercion, naming the first obstacle. A trait with
        // three unsafe methods is still one thing the program cannot do.
        if let Some(c) = assoc_consts.first() {
            self.report(
                at,
                format!("`{name}` cannot be made into a trait object: it declares `{c}`"),
                "a vtable holds code, not values — an associated constant has no slot",
                out,
            );
            return;
        }
        for m in methods {
            let Some(why) = self.unsafe_reason(trait_def, m) else {
                continue;
            };
            self.report(
                at,
                format!(
                    "`{name}` cannot be made into a trait object: `{}` {}",
                    m.name, why.what
                ),
                why.note,
                out,
            );
            return;
        }
    }

    /// Why `m` has no vtable slot, or `None` if it has one.
    fn unsafe_reason(&self, trait_def: DefId, m: &TraitMethod) -> Option<Reason> {
        if m.recv == Recv::None {
            return Some(Reason {
                what: "takes no `self`",
                note: "a trait object dispatches *through* its receiver, so a method with none \
                       cannot be reached from one",
            });
        }
        if m.generic {
            return Some(Reason {
                what: "is generic",
                note: "one vtable slot cannot stand for every instantiation; take a `dyn` \
                       argument instead of a type parameter",
            });
        }
        let sig = self.meta.ty_or_error(m.id);
        let Ty::Func { params, ret } = &sig else {
            return None;
        };
        // The receiver itself is allowed to be `Self` by value in the signature
        // — `self: Self` is caught by the size rule below only for *other*
        // parameters, because `Recv::Value` is already the thing being asked
        // about. Checking it twice would give two names for one problem, so the
        // receiver is checked as a receiver and skipped here.
        if m.recv == Recv::Value {
            return Some(Reason {
                what: "takes `self` by value",
                note: "erasing the type erases its size, so the caller cannot lay out the \
                       receiver; take `self: *Self` or `self: *mut Self`",
            });
        }
        if params
            .iter()
            .skip(1)
            .any(|t| self.is_bare_self(trait_def, t))
        {
            return Some(Reason {
                what: "takes `Self` by value",
                note: "erasing the type erases its size, so the caller cannot lay out the \
                       argument; take a `*Self` instead",
            });
        }
        if self.is_bare_self(trait_def, ret) {
            return Some(Reason {
                what: "returns `Self` by value",
                note: "erasing the type erases its size, so the caller cannot lay out the \
                       result",
            });
        }
        None
    }

    /// Whether `ty` is `Self` itself, rather than a pointer or slice to it.
    ///
    /// Inside a trait declaration `Self` resolves to the trait's own def, so
    /// this is a `DefId` comparison rather than any matching on the name.
    fn is_bare_self(&self, trait_def: DefId, ty: &Ty) -> bool {
        matches!(ty, Ty::Nominal { def, .. } if self.defs.resolve_alias(*def) == trait_def)
    }

    fn report(&self, at: IrId, message: String, note: &str, out: &mut Vec<Diagnostic>) {
        let mut d = Diagnostic::error(message);
        if let Some(span) = self.meta.span(at) {
            d = d.with_primary(span, "a vtable cannot be built for this trait");
        }
        out.push(d.with_note(note));
    }

    fn walk(&self, b: &Block, f: &mut impl FnMut(&Expr)) {
        super::block_children(b, &mut |e| self.visit(e, f));
    }

    fn visit(&self, e: &Expr, f: &mut impl FnMut(&Expr)) {
        f(e);
        super::children_of(e, &mut |c| self.visit(c, f));
    }
}

struct Reason {
    /// Completes "`name` …" in the message.
    what: &'static str,
    note: &'static str,
}
