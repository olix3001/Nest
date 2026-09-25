//! Putting what each `impl` return type is in its place (§5.4).
//!
//! `make_adder :: func (n: i32) -> impl Func(i32) -> i32` promises its callers
//! one thing: whatever it returns can be called with an `i32`. Inference holds
//! them to that — a caller sees a type parameter bounded by `Func`, and nothing
//! it writes can depend on more — while the function's own body decides what
//! the type is, and inference records it ([`ParamDecl::revealed`]).
//!
//! Past inference there is nothing left to hide. Layout needs the closure's
//! size, monomorphization needs its `call`, and a type parameter that is really
//! one known type would reach both as a question with no answer. So this runs
//! once, after the per-file programs are linked and before anything else reads
//! them, and replaces every opaque type with the type it stands for — with the
//! caller's arguments substituted for the function's own parameters, since what
//! a generic function returns depends on what it was called with.
//!
//! It runs before the IR checks too, so they judge the program with its real
//! types; what the program could *write* was already settled by inference.

use std::collections::HashMap;

use crate::sema::decl::{Decl, DeclTable};
use crate::sema::def::{DefId, DefTable};
use crate::sema::infer::{GenericArg, Instantiation};
use crate::sema::ty::Ty;

use super::{Dispatch, Expr, ExprKind, Linked, Meta, VisitorMut, walk_expr_mut};

/// How many opaque types may stand inside one another's revealed types before
/// the chain is taken to be a cycle. A cycle is a type that is its own
/// return — `f :: func () -> impl Func() { return f }` — which inference already
/// rejects; the bound keeps a defect here from being a hang.
const DEPTH: u32 = 64;

/// Replace every opaque type in `linked`, and in every type `meta` records.
pub fn run(defs: &DefTable, decls: &DeclTable, meta: &Meta, linked: &mut Linked) {
    let mut table: HashMap<DefId, (Vec<DefId>, Ty)> = HashMap::new();
    for d in defs.iter().filter(|d| d.opaque) {
        if let Some(Decl::Param(p)) = decls.get(&d.id)
            && let Some(r) = p.revealed.clone()
        {
            table.insert(d.id, r);
        }
    }
    if table.is_empty() {
        return;
    }
    let r = Revealer { table: &table };
    for (id, ty) in meta.store().entries::<Ty>() {
        if r.mentions(&ty) {
            meta.set_ty(id, r.ty(&ty, 0));
        }
    }
    for (id, Instantiation(args)) in meta.store().entries::<Instantiation>() {
        let args = args
            .into_iter()
            .map(|a| match a {
                GenericArg::Ty(t) => GenericArg::Ty(r.ty(&t, 0)),
                other => other,
            })
            .collect();
        meta.set(id, Instantiation(args));
    }
    let funcs: Vec<DefId> = linked.defs().collect();
    let mut walker = Walker { r };
    for d in funcs {
        if let Some(f) = linked.get_mut(d) {
            walker.visit_function(f);
        }
    }
}

struct Revealer<'a> {
    table: &'a HashMap<DefId, (Vec<DefId>, Ty)>,
}

impl Revealer<'_> {
    /// Whether `ty` names an opaque type anywhere in it.
    fn mentions(&self, ty: &Ty) -> bool {
        let mut found = false;
        visit(ty, &mut |t| {
            if let Ty::Nominal { def, .. } = t {
                found |= self.table.contains_key(def);
            }
        });
        found
    }

    /// `ty`, with every opaque type in it replaced by what it is.
    fn ty(&self, ty: &Ty, depth: u32) -> Ty {
        if depth > DEPTH {
            return Ty::Error;
        }
        map(ty, &mut |t| match t {
            Ty::Nominal { def, args } => {
                let (params, concrete) = self.table.get(def)?;
                let args: Vec<Ty> = args.iter().map(|a| self.ty(a, depth + 1)).collect();
                let subst: HashMap<DefId, Ty> = params.iter().copied().zip(args).collect();
                let here = map(concrete, &mut |t| match t {
                    Ty::Nominal { def, args } if args.is_empty() => subst.get(def).cloned(),
                    _ => None,
                });
                Some(self.ty(&here, depth + 1))
            }
            _ => None,
        })
    }
}

/// The types written inside expressions rather than recorded beside them.
struct Walker<'a> {
    r: Revealer<'a>,
}

impl VisitorMut for Walker<'_> {
    fn visit_expr(&mut self, expr: &mut Expr) {
        match &mut expr.kind {
            ExprKind::DynCast { concrete, .. } => *concrete = self.r.ty(concrete, 0),
            ExprKind::Call { dispatch, .. } => match dispatch {
                Dispatch::Generic {
                    self_ty,
                    trait_args,
                    ..
                } => {
                    *self_ty = self.r.ty(self_ty, 0);
                    for a in trait_args.iter_mut() {
                        *a = self.r.ty(a, 0);
                    }
                }
                Dispatch::Func { self_ty } => *self_ty = self.r.ty(self_ty, 0),
                Dispatch::Static | Dispatch::Virtual { .. } => {}
            },
            _ => {}
        }
        walk_expr_mut(self, expr);
    }
}

/// Call `f` on `ty` and every type inside it.
fn visit(ty: &Ty, f: &mut impl FnMut(&Ty)) {
    f(ty);
    match ty {
        Ty::Nominal { args, .. } | Ty::Tuple(args) => args.iter().for_each(|a| visit(a, f)),
        Ty::Ptr { inner, .. }
        | Ty::Slice { inner, .. }
        | Ty::Array { inner, .. }
        | Ty::Spread(inner) => visit(inner, f),
        Ty::Struct(fields) => fields.iter().for_each(|(_, t)| visit(t, f)),
        Ty::Func { params, ret, .. } => {
            params.iter().for_each(|p| visit(p, f));
            visit(ret, f);
        }
        _ => {}
    }
}

/// `ty`, rebuilt bottom-up, with each type `f` answers for replaced by its
/// answer. A replacement is not looked into again.
fn map(ty: &Ty, f: &mut impl FnMut(&Ty) -> Option<Ty>) -> Ty {
    if let Some(t) = f(ty) {
        return t;
    }
    match ty {
        Ty::Nominal { def, args } => Ty::Nominal {
            def: *def,
            args: args.iter().map(|a| map(a, f)).collect(),
        },
        Ty::Tuple(elems) => Ty::tuple(elems.iter().map(|e| map(e, f)).collect()),
        Ty::Spread(inner) => Ty::Spread(Box::new(map(inner, f))),
        Ty::Dyn { def, assoc } => Ty::Dyn {
            def: *def,
            assoc: assoc
                .iter()
                .map(|(n, t)| (n.clone(), (|e| map(e, f))(t)))
                .collect(),
        },
        Ty::Ptr { mutable, inner } => Ty::Ptr {
            mutable: *mutable,
            inner: Box::new(map(inner, f)),
        },
        Ty::Slice { mutable, inner } => Ty::Slice {
            mutable: *mutable,
            inner: Box::new(map(inner, f)),
        },
        Ty::Array {
            len,
            mutable,
            inner,
        } => Ty::Array {
            len: len.clone(),
            mutable: *mutable,
            inner: Box::new(map(inner, f)),
        },
        Ty::Struct(fields) => {
            Ty::Struct(fields.iter().map(|(n, t)| (n.clone(), map(t, f))).collect())
        }
        Ty::Func { params, ret, c } => Ty::Func {
            params: params.iter().map(|p| map(p, f)).collect(),
            ret: Box::new(map(ret, f)),
            c: *c,
        },
        other => other.clone(),
    }
}
