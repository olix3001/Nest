//! Match exhaustiveness and arm reachability — Maranget's usefulness algorithm.
//!
//! Two diagnostics come out of one computation:
//!
//! - **non-exhaustive**: a value the `match` does not cover, reported *with a
//!   witness*. "`match` is not exhaustive" alone sends the reader back to
//!   enumerate the variants by hand; ``pattern `.none` is not covered`` tells
//!   them what to write.
//! - **unreachable arm**: an arm that no value can reach — one after a `_`, a
//!   duplicated variant, a range already covered.
//!
//! Both are the same question, "is this pattern *useful* against the ones before
//! it", asked with a different last row.
//!
//! # The algorithm
//!
//! `useful(P, q)` — given a matrix `P` of pattern rows and a candidate row `q`,
//! is there a value matched by `q` and by no row of `P`? Recurse on the first
//! column:
//!
//! - `q` is empty: useful exactly when `P` has no rows. (No columns left means
//!   every row matches everything, so one existing row already covers it.)
//! - `q[0]` is a constructor `c`: specialize both to `c` — keep only the rows
//!   that can match `c`, replacing `c`'s arguments with new columns — and
//!   recurse.
//! - `q[0]` is a wildcard: look at which constructors the first column of `P`
//!   mentions. If they are **complete** for the type, a wildcard is no better
//!   than trying each in turn, so recurse per constructor. If any is missing,
//!   the wildcard reaches a value no row does, so recurse on the *default*
//!   matrix — the rows whose first pattern is itself a wildcard — and report a
//!   missing constructor as the witness.
//!
//! # Guards
//!
//! An arm with a guard **contributes nothing to coverage**: its pattern may
//! match and the guard still send control to the next arm. So the exhaustiveness
//! matrix is built from unguarded arms only. Reachability uses the same matrix
//! for the same reason — an arm after a guarded one is reachable precisely
//! because the guard may fail.
//!
//! # Where it is not complete, and why that is the safe direction
//!
//! Integers and `char` are handled by **interval arithmetic**, so a `match`
//! covered by ranges is accepted. Floats, strings and dynamically-sized slices
//! are treated as never exhaustible except by a wildcard or a rest-only slice
//! pattern: there is no finite set of constructors to enumerate, and claiming
//! coverage the compiler cannot verify would turn the decision tree's final
//! `_ => unreachable` — which Phase 5 emits on the strength of *this* pass —
//! into a guess.

use std::collections::HashSet;

use num_bigint::BigInt;

use crate::common::diagnostic::Diagnostic;
use crate::common::symbol::Symbol;
use crate::parser::ast::Lit;
use crate::sema::def::DefTable;
use crate::sema::ty::Ty;

use crate::ir::{
    Arm, Block, Expr, ExprKind, Linked, Member, Meta, Pattern, PatternKind, Stmt, StmtKind,
    TypeDefKind,
};

/// Report every non-exhaustive `match` and every unreachable arm.
pub fn check(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    for func in linked.funcs() {
        let Some(body) = &func.body else { continue };
        let mut w = Walk {
            cx: Cx { defs, meta, linked },
            out,
        };
        w.block(body);
    }
}

/// What every step of the algorithm needs: the def table for names, the side
/// table for types, and the linked program for what is inside a nominal type.
#[derive(Clone, Copy)]
struct Cx<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    linked: &'a Linked,
}

struct Walk<'a> {
    cx: Cx<'a>,
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
            StmtKind::Let { init, .. } => self.expr(init),
            StmtKind::Assign { place, value } => {
                self.expr(place);
                self.expr(value);
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Return(v) | StmtKind::Break(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
            }
            // Walked with the block's, after the statements.
            StmtKind::Continue | StmtKind::Defer(_) => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                let ty = self.cx.meta.ty_or_error(scrutinee.id);
                self.check_match(e, &ty, arms);
                for a in arms {
                    if let Some(g) = &a.guard {
                        self.expr(g);
                    }
                    self.expr(&a.body);
                }
            }
            ExprKind::Block(b) | ExprKind::Loop { body: b } => self.block(b),
            ExprKind::If { cond, then, els } => {
                self.expr(cond);
                self.block(then);
                if let Some(e) = els {
                    self.block(e);
                }
            }
            _ => {
                let mut kids = |c: &Expr| self.expr(c);
                super::children_of(e, &mut kids);
            }
        }
    }

    fn check_match(&mut self, at: &Expr, ty: &Ty, arms: &[Arm]) {
        // A `match` with an error-typed scrutinee has already been diagnosed;
        // anything said here would be about a type that does not exist.
        if matches!(ty, Ty::Error) {
            return;
        }

        // Reachability, arm by arm. A guarded arm is *checked* — an arm nothing
        // can match is a mistake whether or not it has a guard — but it does not
        // *cover* anything, so it never joins the matrix.
        let mut matrix: Matrix = Vec::new();
        for arm in arms {
            let row = vec![arm.pattern.clone()];
            if !self
                .cx
                .useful(&matrix, &row, std::slice::from_ref(ty))
                .is_useful()
            {
                let mut d = Diagnostic::error("unreachable `match` arm");
                if let Some(span) = self.cx.meta.span(arm.pattern.id) {
                    d = d.with_primary(span, "no value can reach this arm");
                }
                self.out
                    .push(d.with_note("an earlier arm already matches everything this one would"));
            } else if let PatternKind::Or(alts) = &arm.pattern.kind {
                // The arm is reachable, but one of its *alternatives* may not
                // be: `.a | .c` where `.a` is already covered still reaches this
                // arm through `.c`, so the mistake hides behind the arm being
                // useful as a whole. Only worth asking once the arm itself has
                // passed, or every alternative of a dead arm would be reported
                // on top of the arm.
                let mut seen = matrix.clone();
                for alt in alts {
                    let row = vec![alt.clone()];
                    if !self
                        .cx
                        .useful(&seen, &row, std::slice::from_ref(ty))
                        .is_useful()
                    {
                        let mut d = Diagnostic::error("unreachable alternative in an or-pattern");
                        if let Some(span) = self.cx.meta.span(alt.id) {
                            d = d.with_primary(span, "no value can reach this alternative");
                        }
                        self.out.push(d.with_note(
                            "an earlier arm, or an earlier alternative, already covers it",
                        ));
                    }
                    seen.push(row);
                }
            }
            if arm.guard.is_none() {
                matrix.push(row);
            }
        }

        // Exhaustiveness: is a bare wildcard still useful against everything
        // that covers?
        let wild = vec![Pattern {
            id: at.id,
            kind: PatternKind::Wildcard,
        }];
        if let Useful::Yes(witness) = self.cx.useful(&matrix, &wild, std::slice::from_ref(ty)) {
            let shown = witness
                .first()
                .map(|w| w.render(self.cx.defs))
                .unwrap_or_else(|| "_".to_string());
            let mut d = Diagnostic::error(format!(
                "`match` is not exhaustive: `{shown}` is not covered"
            ));
            if let Some(span) = self.cx.meta.span(at.id) {
                d = d.with_primary(span, format!("pattern `{shown}` not covered"));
            }
            self.out.push(d.with_note(
                "add an arm for it, or a `_` arm — every value the scrutinee can hold must \
                 reach some arm",
            ));
        }
    }
}

// ===< Constructors >===

/// A constructor a value of some type can have been built with.
///
/// This is the axis the algorithm splits on. Two constructors are the same iff
/// they are equal here, and a set of them is *complete* when a value of the type
/// must have been built with one of them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Ctor {
    /// A named enum variant.
    Variant(Symbol),
    /// The only way to build the type: a struct, a tuple, a `distinct`.
    Single,
    /// An inclusive interval of integers or `char`s. A single literal is the
    /// degenerate interval `[n, n]`, so one rule serves both.
    Range(BigInt, BigInt),
    /// A slice or array of exactly `len` elements.
    Slice(usize),
    /// A slice pattern with a `..`, matching every length **at or above** the
    /// elements it fixes. It is a *set* of lengths, not one of them, so it never
    /// appears as a split target — only as what a row was written with, which
    /// [`Ctor::covers`] compares against.
    SliceMin(usize),
    /// A value with no enumerable structure — a float, a string. Each distinct
    /// literal is its own constructor and the set is never complete.
    Opaque(String),
    /// Stands for "everything the rows did not mention". Never appears in a
    /// pattern; it is what a wildcard specializes to when the set is incomplete,
    /// and what a witness prints as.
    Missing,
}

impl Ctor {
    /// How many columns this constructor contributes when a row is specialized
    /// to it — the arity of its payload.
    fn arity(&self, cx: Cx, ty: &Ty) -> usize {
        match self {
            Ctor::Variant(name) => cx.variant_members(ty, name).map_or(0, |m| m.len()),
            Ctor::Single => cx.single_members(ty).len(),
            Ctor::Slice(n) => *n,
            Ctor::SliceMin(n) => *n,
            Ctor::Range(..) | Ctor::Opaque(_) | Ctor::Missing => 0,
        }
    }

    /// The types of those columns.
    fn field_tys(&self, cx: Cx, ty: &Ty) -> Vec<Ty> {
        match self {
            Ctor::Variant(name) => cx
                .variant_members(ty, name)
                .map(|ms| cx.member_tys(ty, &ms))
                .unwrap_or_default(),
            Ctor::Single => {
                let ms = cx.single_members(ty);
                cx.member_tys(ty, &ms)
            }
            Ctor::Slice(n) | Ctor::SliceMin(n) => {
                let elem = cx.element_ty(ty);
                vec![elem; *n]
            }
            Ctor::Range(..) | Ctor::Opaque(_) | Ctor::Missing => Vec::new(),
        }
    }

    /// Whether a row built with `other` can match a value built with `self`.
    fn covers(&self, other: &Ctor) -> bool {
        match (self, other) {
            (Ctor::Range(a1, b1), Ctor::Range(a2, b2)) => a2 <= a1 && b1 <= b2,
            // A `..` pattern covers every length from the one it fixes upward.
            (Ctor::Slice(n), Ctor::SliceMin(m)) => n >= m,
            _ => self == other,
        }
    }
}

// ===< The matrix >===

type Matrix = Vec<Vec<Pattern>>;

/// The answer to "is this row useful", carrying a witness when it is.
enum Useful {
    No,
    Yes(Vec<Witness>),
}

impl Useful {
    fn is_useful(&self) -> bool {
        matches!(self, Useful::Yes(_))
    }
}

/// A value the candidate row matches and the matrix does not, in pattern shape.
#[derive(Debug, Clone)]
enum Witness {
    Wild,
    Variant(Symbol, Vec<Witness>),
    Tuple(Vec<Witness>),
    Struct(Symbol, Vec<(Symbol, Witness)>),
    Int(BigInt),
    Bool(bool),
    Char(char),
    Slice(Vec<Witness>),
}

impl Witness {
    fn render(&self, defs: &DefTable) -> String {
        match self {
            Witness::Wild => "_".into(),
            Witness::Variant(name, sub) if sub.is_empty() => format!(".{name}"),
            Witness::Variant(name, sub) => format!(
                ".{name}({})",
                sub.iter()
                    .map(|w| w.render(defs))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Witness::Tuple(elems) => format!(
                "({})",
                elems
                    .iter()
                    .map(|w| w.render(defs))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Witness::Struct(name, fields) => format!(
                "{name} {{ {} }}",
                fields
                    .iter()
                    .map(|(n, w)| format!("{n}: {}", w.render(defs)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Witness::Int(n) => n.to_string(),
            Witness::Bool(b) => b.to_string(),
            Witness::Char(c) => format!("{c:?}"),
            Witness::Slice(elems) => format!(
                "[{}]",
                elems
                    .iter()
                    .map(|w| w.render(defs))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl Cx<'_> {
    /// Is `q` useful against `P`? See the module docs for the recursion.
    ///
    /// `tys` gives the type of each column, which is what makes "is this set of
    /// constructors complete" answerable.
    fn useful(&self, p: &Matrix, q: &[Pattern], tys: &[Ty]) -> Useful {
        // No columns left: `q` is useful exactly when nothing already matches.
        if q.is_empty() {
            return if p.is_empty() {
                Useful::Yes(Vec::new())
            } else {
                Useful::No
            };
        }

        let ty = self.resolve(&tys[0]);

        // An or-pattern is several rows wearing one hat. It is useful if any
        // alternative is — and each alternative is tried against the matrix
        // *plus the alternatives before it*, so `a | a` reports the second.
        if let PatternKind::Or(alts) = &q[0].kind {
            let mut p = p.clone();
            for alt in alts {
                let mut row = vec![alt.clone()];
                row.extend_from_slice(&q[1..]);
                if let Useful::Yes(w) = self.useful(&p, &row, tys) {
                    return Useful::Yes(w);
                }
                p.push(row);
            }
            return Useful::No;
        }

        match self.ctor_of(&q[0], &ty) {
            // A concrete constructor: narrow everything to it.
            Some(ctor) => {
                let sub_tys = ctor.field_tys(*self, &ty);
                let arity = sub_tys.len();
                let p = self.specialize(p, &ctor, arity, &ty);
                let q = self.specialize_row(q, &ctor, arity, &ty);
                let mut tys2 = sub_tys;
                tys2.extend_from_slice(&tys[1..]);
                match self.useful(&p, &q, &tys2) {
                    Useful::No => Useful::No,
                    Useful::Yes(w) => Useful::Yes(self.rebuild(&ctor, arity, &ty, w)),
                }
            }
            // A wildcard. Everything turns on whether the first column's
            // constructors already exhaust the type.
            None => {
                let seen = self.column_ctors(p, &ty);
                let split = match self.split(&ty, &seen) {
                    // No row names a constructor here, so every row already
                    // matches whatever is in the column and trying each
                    // constructor learns nothing. It must not be tried anyway:
                    // a struct reaching itself through a pointer field would be
                    // expanded forever.
                    Split::Complete(ctors) if seen.is_empty() && !ctors.is_empty() => {
                        Split::Missing(None)
                    }
                    split => split,
                };
                match split {
                    // Complete: a wildcard is only as good as trying each.
                    Split::Complete(ctors) => {
                        for ctor in ctors {
                            let sub_tys = ctor.field_tys(*self, &ty);
                            let arity = sub_tys.len();
                            let pm = self.specialize(p, &ctor, arity, &ty);
                            let qr = self.specialize_row(q, &ctor, arity, &ty);
                            let mut tys2 = sub_tys;
                            tys2.extend_from_slice(&tys[1..]);
                            if let Useful::Yes(w) = self.useful(&pm, &qr, &tys2) {
                                return Useful::Yes(self.rebuild(&ctor, arity, &ty, w));
                            }
                        }
                        Useful::No
                    }
                    // Something is missing: the wildcard reaches it. Drop the
                    // column and carry a witness naming what was missed.
                    Split::Missing(missing) => {
                        let p = self.default_matrix(p);
                        match self.useful(&p, &q[1..], &tys[1..]) {
                            Useful::No => Useful::No,
                            Useful::Yes(mut w) => {
                                let head = missing
                                    .map(|c| self.witness_for(&c, &ty))
                                    .unwrap_or(Witness::Wild);
                                w.insert(0, head);
                                Useful::Yes(w)
                            }
                        }
                    }
                }
            }
        }
    }

    /// Keep the rows that can match `ctor`, replacing the first column with the
    /// constructor's arguments.
    fn specialize(&self, p: &Matrix, ctor: &Ctor, arity: usize, ty: &Ty) -> Matrix {
        p.iter()
            .filter_map(|row| self.specialize_one(row, ctor, arity, ty))
            .collect()
    }

    fn specialize_row(&self, row: &[Pattern], ctor: &Ctor, arity: usize, ty: &Ty) -> Vec<Pattern> {
        self.specialize_one(row, ctor, arity, ty)
            .unwrap_or_else(|| row[1..].to_vec())
    }

    fn specialize_one(
        &self,
        row: &[Pattern],
        ctor: &Ctor,
        arity: usize,
        ty: &Ty,
    ) -> Option<Vec<Pattern>> {
        let head = &row[0];
        let rest = &row[1..];

        // An or-pattern in the matrix expands: each alternative is its own row,
        // and any that survives specialization keeps the row alive. Taking the
        // first that matches is enough for usefulness — they cover the same
        // columns.
        if let PatternKind::Or(alts) = &head.kind {
            for alt in alts {
                let mut r = vec![alt.clone()];
                r.extend_from_slice(rest);
                if let Some(s) = self.specialize_one(&r, ctor, arity, ty) {
                    return Some(s);
                }
            }
            return None;
        }

        match self.ctor_of(head, ty) {
            // A wildcard matches any constructor, contributing wildcards for
            // each of its arguments.
            None => {
                let mut out = vec![
                    Pattern {
                        id: head.id,
                        kind: PatternKind::Wildcard,
                    };
                    arity
                ];
                out.extend_from_slice(rest);
                Some(out)
            }
            Some(row_ctor) if ctor.covers(&row_ctor) => {
                let mut out = self.sub_patterns(head, arity, ty);
                out.extend_from_slice(rest);
                Some(out)
            }
            Some(_) => None,
        }
    }

    /// The rows whose first pattern matches everything, with that column
    /// dropped — the matrix a wildcard is checked against when the constructor
    /// set is incomplete.
    fn default_matrix(&self, p: &Matrix) -> Matrix {
        let mut out = Matrix::new();
        for row in p {
            match &row[0].kind {
                PatternKind::Wildcard | PatternKind::Binding { .. } => out.push(row[1..].to_vec()),
                PatternKind::At { pattern, .. } | PatternKind::Deref(pattern) => {
                    let mut r = vec![(**pattern).clone()];
                    r.extend_from_slice(&row[1..]);
                    out.extend(self.default_matrix(&vec![r]));
                }
                PatternKind::Or(alts) => {
                    for alt in alts {
                        let mut r = vec![alt.clone()];
                        r.extend_from_slice(&row[1..]);
                        out.extend(self.default_matrix(&vec![r]));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Put a constructor back on the front of a witness list.
    fn rebuild(&self, ctor: &Ctor, arity: usize, ty: &Ty, mut w: Vec<Witness>) -> Vec<Witness> {
        let args: Vec<Witness> = w.drain(..arity.min(w.len())).collect();
        let head = match ctor {
            Ctor::Variant(name) => Witness::Variant(name.clone(), args),
            Ctor::Single => self.single_witness(ty, args),
            Ctor::Range(lo, _) => self.scalar_witness(ty, lo.clone()),
            Ctor::Opaque(_) | Ctor::Missing => Witness::Wild,
            Ctor::Slice(_) | Ctor::SliceMin(_) => Witness::Slice(args),
        };
        let mut out = vec![head];
        out.extend(w);
        out
    }

    /// A witness for a constructor nothing in the matrix mentioned.
    fn witness_for(&self, ctor: &Ctor, ty: &Ty) -> Witness {
        let arity = ctor.arity(*self, ty);
        let args = vec![Witness::Wild; arity];
        match ctor {
            Ctor::Variant(name) => Witness::Variant(name.clone(), args),
            Ctor::Single => self.single_witness(ty, args),
            Ctor::Range(lo, _) => self.scalar_witness(ty, lo.clone()),
            Ctor::Slice(_) | Ctor::SliceMin(_) => Witness::Slice(args),
            Ctor::Opaque(_) | Ctor::Missing => Witness::Wild,
        }
    }

    /// The witness shape for a type with one constructor: a tuple prints as a
    /// tuple, a named struct as a struct with its field names.
    fn single_witness(&self, ty: &Ty, args: Vec<Witness>) -> Witness {
        match self.resolve(ty) {
            Ty::Tuple(_) => Witness::Tuple(args),
            Ty::Nominal { def, .. } => {
                let members = self.single_members(ty);
                // A tuple struct's members are positions; print it like a tuple
                // so the witness is something the reader can paste back.
                let positional = members
                    .iter()
                    .all(|m| m.name.as_str().chars().all(|c| c.is_ascii_digit()));
                if positional && !members.is_empty() {
                    return Witness::Tuple(args);
                }
                let name = self.defs.get(def).name.clone();
                Witness::Struct(
                    name,
                    members.iter().map(|m| m.name.clone()).zip(args).collect(),
                )
            }
            _ => Witness::Wild,
        }
    }

    fn scalar_witness(&self, ty: &Ty, v: BigInt) -> Witness {
        match self.resolve(ty) {
            Ty::Bool => Witness::Bool(v != BigInt::from(0)),
            Ty::Char => u32::try_from(&v)
                .ok()
                .and_then(char::from_u32)
                .map(Witness::Char)
                .unwrap_or(Witness::Wild),
            _ => Witness::Int(v),
        }
    }

    // ===< Reading a pattern >===

    /// The constructor `p` tests for, or `None` if it tests nothing (a wildcard,
    /// a plain binding, an unbounded range).
    fn ctor_of(&self, p: &Pattern, ty: &Ty) -> Option<Ctor> {
        match &p.kind {
            PatternKind::Wildcard | PatternKind::Binding { .. } => None,
            PatternKind::At { pattern, .. } | PatternKind::Deref(pattern) => {
                self.ctor_of(pattern, ty)
            }
            // Handled by the caller, which expands it into rows.
            PatternKind::Or(_) => None,
            PatternKind::Variant { name, .. } => Some(Ctor::Variant(name.clone())),
            PatternKind::Tuple(_)
            | PatternKind::Struct { .. }
            | PatternKind::TupleStruct { .. } => Some(Ctor::Single),
            PatternKind::Lit(l) => Some(lit_ctor(l)),
            PatternKind::Range {
                start,
                end,
                inclusive,
            } => {
                let (lo, hi) = self.bounds(ty);
                let start = start.as_ref().and_then(lit_int).unwrap_or(lo);
                let mut stop = end.as_ref().and_then(lit_int).unwrap_or(hi);
                if !*inclusive && end.is_some() {
                    stop -= 1;
                }
                Some(Ctor::Range(start, stop))
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let fixed = prefix.len() + suffix.len();
                Some(match rest {
                    Some(_) => Ctor::SliceMin(fixed),
                    None => Ctor::Slice(fixed),
                })
            }
        }
    }

    /// The sub-patterns `p` contributes when specialized to a constructor of
    /// `arity` arguments, in member order.
    fn sub_patterns(&self, p: &Pattern, arity: usize, ty: &Ty) -> Vec<Pattern> {
        let wild = |id| Pattern {
            id,
            kind: PatternKind::Wildcard,
        };
        match &p.kind {
            PatternKind::At { pattern, .. } | PatternKind::Deref(pattern) => {
                self.sub_patterns(pattern, arity, ty)
            }
            PatternKind::Variant { sub, .. } => fill(sub.clone(), arity, p.id),
            PatternKind::Tuple(elems) => fill(elems.clone(), arity, p.id),
            PatternKind::TupleStruct { elems, .. } => fill(elems.clone(), arity, p.id),
            // A struct pattern names its fields, so the sub-patterns have to be
            // put back in *member* order — a pattern may list them in any order
            // and may leave some out under a `..`.
            PatternKind::Struct { fields, .. } => {
                let members = self.single_members(ty);
                members
                    .iter()
                    .map(|m| {
                        fields
                            .iter()
                            .find(|(n, _)| *n == m.name)
                            .map(|(_, sub)| sub.clone())
                            .unwrap_or_else(|| wild(p.id))
                    })
                    .collect()
            }
            // The `..` is the *middle*: the prefix matches from the front and
            // the suffix from the back, so the wildcards standing for the
            // skipped elements go between them. Appending the suffix and then
            // padding would line the last elements up against the wrong columns.
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let mut out = prefix.clone();
                if rest.is_some() {
                    let skipped = arity.saturating_sub(prefix.len() + suffix.len());
                    out.extend(std::iter::repeat_n(wild(p.id), skipped));
                }
                out.extend(suffix.iter().cloned());
                fill(out, arity, p.id)
            }
            _ => vec![wild(p.id); arity],
        }
    }

    // ===< Completeness >===

    /// Every constructor mentioned in the first column of `p`.
    fn column_ctors(&self, p: &Matrix, ty: &Ty) -> Vec<Ctor> {
        let mut out = Vec::new();
        for row in p {
            self.collect_ctors(&row[0], ty, &mut out);
        }
        out
    }

    fn collect_ctors(&self, p: &Pattern, ty: &Ty, out: &mut Vec<Ctor>) {
        match &p.kind {
            PatternKind::Or(alts) => {
                for a in alts {
                    self.collect_ctors(a, ty, out);
                }
            }
            _ => {
                if let Some(c) = self.ctor_of(p, ty) {
                    out.push(c);
                }
            }
        }
    }

    /// Whether `seen` exhausts `ty`, and if not, one constructor it misses.
    fn split(&self, ty: &Ty, seen: &[Ctor]) -> Split {
        match self.resolve(ty) {
            Ty::Bool => {
                let ints: Vec<(BigInt, BigInt)> = seen.iter().filter_map(as_range).collect();
                match first_gap(&ints, BigInt::from(0), BigInt::from(1)) {
                    Some(v) => Split::Missing(Some(Ctor::Range(v.clone(), v))),
                    None => Split::Complete(vec![
                        Ctor::Range(BigInt::from(0), BigInt::from(0)),
                        Ctor::Range(BigInt::from(1), BigInt::from(1)),
                    ]),
                }
            }
            Ty::Int { .. } | Ty::Char | Ty::ComptimeInt => {
                let (lo, hi) = self.bounds(ty);
                let ints: Vec<(BigInt, BigInt)> = seen.iter().filter_map(as_range).collect();
                // Nothing constrained the column at all, so *every* value is
                // uncovered. Naming one of them — the type's minimum — would
                // read as if that single value were the omission.
                if ints.is_empty() {
                    return Split::Missing(None);
                }
                match first_gap(&ints, lo.clone(), hi.clone()) {
                    Some(v) => Split::Missing(Some(Ctor::Range(v.clone(), v))),
                    // Covered end to end. Split into the intervals the rows drew
                    // so each is checked on its own.
                    None => Split::Complete(split_intervals(&ints, lo, hi)),
                }
            }
            Ty::Tuple(_) => Split::Complete(vec![Ctor::Single]),
            Ty::Nominal { def, .. } => {
                let def = self.defs.resolve_alias(def);
                match self.linked.ty(def).map(|t| &t.kind) {
                    Some(TypeDefKind::Enum { variants }) => {
                        let seen: HashSet<&Symbol> = seen
                            .iter()
                            .filter_map(|c| match c {
                                Ctor::Variant(n) => Some(n),
                                _ => None,
                            })
                            .collect();
                        match variants.iter().find(|v| !seen.contains(&v.name)) {
                            Some(v) => Split::Missing(Some(Ctor::Variant(v.name.clone()))),
                            None if variants.is_empty() => {
                                // An enum with no variants has no values, so
                                // there is nothing left to cover.
                                Split::Complete(Vec::new())
                            }
                            None => Split::Complete(
                                variants
                                    .iter()
                                    .map(|v| Ctor::Variant(v.name.clone()))
                                    .collect(),
                            ),
                        }
                    }
                    Some(TypeDefKind::Struct { .. }) | Some(TypeDefKind::Distinct { .. }) => {
                        Split::Complete(vec![Ctor::Single])
                    }
                    // A trait object, a type parameter, an opaque or `#raw`
                    // type: nothing to enumerate, so only `_` covers it.
                    _ => Split::Missing(None),
                }
            }
            // A fixed-length array has exactly one length: listing that many
            // element patterns, or any `..` that fits, is exhaustive.
            Ty::Array { len, .. } => match len.value() {
                Some(n) => {
                    let n = n as usize;
                    let target = Ctor::Slice(n);
                    if seen.iter().any(|c| target.covers(c)) {
                        Split::Complete(vec![target])
                    } else {
                        Split::Missing(Some(target))
                    }
                }
                // The length is still a `const` parameter; monomorphization has
                // not run. Nothing can be claimed about it.
                None => Split::Missing(None),
            },
            // A slice's length is a runtime value, so there is no finite set of
            // lengths to enumerate — but the patterns themselves bound the
            // problem. Every length up to the longest one written behaves
            // differently; every length past it behaves the same, so one
            // representative stands for all of them.
            Ty::Slice { .. } => {
                let longest = seen
                    .iter()
                    .filter_map(|c| match c {
                        Ctor::Slice(n) | Ctor::SliceMin(n) => Some(*n),
                        _ => None,
                    })
                    .max();
                let Some(longest) = longest else {
                    // No slice pattern at all: length 0 is as good a witness as
                    // any, and nothing covers it.
                    return Split::Missing(Some(Ctor::Slice(0)));
                };
                let lengths: Vec<Ctor> = (0..=longest + 1).map(Ctor::Slice).collect();
                match lengths
                    .iter()
                    .find(|target| !seen.iter().any(|c| target.covers(c)))
                {
                    Some(missing) => Split::Missing(Some(missing.clone())),
                    None => Split::Complete(lengths),
                }
            }
            // Floats and strings have no enumerable structure: only `_` covers.
            _ => Split::Missing(None),
        }
    }

    // ===< Type queries >===

    /// The type a pattern in this column is actually matched against.
    ///
    /// Pointers are peeled first. Matching through a pointer matches the pointee
    /// — that is §3.2's auto-deref, and inference already does it (see
    /// `variant_payload`, which calls `autoderef` before looking a variant up).
    /// Without the same step here, `self: *Shape` scrutinized against `.circle`
    /// would look like a match on a type with no constructors at all, and every
    /// such `match` — every method that matches on its own receiver — would be
    /// reported as non-exhaustive.
    fn resolve(&self, ty: &Ty) -> Ty {
        let mut ty = ty;
        while let Ty::Ptr { inner, .. } = ty {
            ty = inner;
        }
        match ty {
            Ty::Nominal { def, args } => {
                let d = self.defs.resolve_alias(*def);
                // A `distinct` matches as its representation would, except that
                // it is still one nominal constructor. Keep it nominal.
                Ty::Nominal {
                    def: d,
                    args: args.clone(),
                }
            }
            other => other.clone(),
        }
    }

    /// The members of a one-constructor type, in declaration order.
    fn single_members(&self, ty: &Ty) -> Vec<Member> {
        match self.resolve(ty) {
            Ty::Tuple(elems) => elems
                .iter()
                .enumerate()
                .map(|(i, _)| Member {
                    // A tuple's elements have no nodes of their own; the id is
                    // never looked up, only the name and the position matter.
                    id: crate::ir::IrId(u32::MAX),
                    def: None,
                    name: Symbol::new(&i.to_string()),
                })
                .collect(),
            Ty::Nominal { def, .. } => match self.linked.ty(def).map(|t| &t.kind) {
                Some(TypeDefKind::Struct { members }) => members.clone(),
                Some(TypeDefKind::Distinct { repr }) => vec![repr.clone()],
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn variant_members(&self, ty: &Ty, name: &Symbol) -> Option<Vec<Member>> {
        let Ty::Nominal { def, .. } = self.resolve(ty) else {
            return None;
        };
        let TypeDefKind::Enum { variants } = &self.linked.ty(def)?.kind else {
            return None;
        };
        variants
            .iter()
            .find(|v| v.name == *name)
            .map(|v| v.members.clone())
    }

    /// The types of `members` as seen through `ty`'s generic arguments.
    ///
    /// A member is recorded definition-relative — a field of `Box.<T>` declared
    /// `T` is the parameter — so a `match` on a `Box.<i32>` has to substitute
    /// before it can decide what its sub-patterns are matching against.
    fn member_tys(&self, ty: &Ty, members: &[Member]) -> Vec<Ty> {
        match self.resolve(ty) {
            Ty::Tuple(elems) => elems,
            Ty::Nominal { def, args } => {
                let params = self.type_params(def);

                members
                    .iter()
                    .map(|m| {
                        let t = self.meta.ty_or_error(m.id);
                        subst(&t, &params, &args)
                    })
                    .collect()
            }
            _ => vec![Ty::Error; members.len()],
        }
    }

    /// The generic parameter defs of a type, in order, read off the type its own
    /// definition node is stamped with.
    fn type_params(&self, def: crate::sema::def::DefId) -> Vec<crate::sema::def::DefId> {
        let Some(t) = self.linked.ty(def) else {
            return Vec::new();
        };
        match self.meta.ty_or_error(t.id) {
            Ty::Nominal { args, .. } => args
                .iter()
                .filter_map(|a| match a {
                    Ty::Nominal { def, args } if args.is_empty() => Some(*def),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn element_ty(&self, ty: &Ty) -> Ty {
        match self.resolve(ty) {
            Ty::Slice { inner, .. } | Ty::Array { inner, .. } => *inner,
            _ => Ty::Error,
        }
    }

    /// The inclusive value range of a scalar type.
    fn bounds(&self, ty: &Ty) -> (BigInt, BigInt) {
        match self.resolve(ty) {
            Ty::Bool => (BigInt::from(0), BigInt::from(1)),
            Ty::Char => (BigInt::from(0), BigInt::from(0x10_FFFF_u32)),
            // A symbolic `int.<N, S>` has no finite range, so it falls to the
            // same answer a `comptime_int` gets: nothing but a wildcard covers
            // it, which is the only honest reading of a width not yet chosen.
            ty @ Ty::Int { .. } => match ty.int_parts() {
                Some((signed, bits)) => int_bounds(signed, bits),
                None => (BigInt::from(i128::MIN), BigInt::from(i128::MAX)),
            },
            // A `comptime_int` is arbitrary precision, so nothing finite covers
            // it. Give it a range no finite set of patterns can fill.
            _ => (BigInt::from(i128::MIN), BigInt::from(i128::MAX)),
        }
    }
}

/// Whether a set of constructors exhausts a type.
enum Split {
    /// It does; these are the constructors to check one by one.
    Complete(Vec<Ctor>),
    /// It does not. Carries one missing constructor for the witness, or `None`
    /// when the type has no enumerable constructors to name.
    Missing(Option<Ctor>),
}

// ===< Small helpers >===

fn fill(mut ps: Vec<Pattern>, arity: usize, id: crate::ir::IrId) -> Vec<Pattern> {
    while ps.len() < arity {
        ps.push(Pattern {
            id,
            kind: PatternKind::Wildcard,
        });
    }
    ps.truncate(arity);
    ps
}

fn lit_ctor(l: &Lit) -> Ctor {
    match l {
        Lit::Int(n) => Ctor::Range(n.clone(), n.clone()),
        Lit::Bool(b) => {
            let v = BigInt::from(*b as u8);
            Ctor::Range(v.clone(), v)
        }
        Lit::Char(c) => {
            let v = BigInt::from(*c as u32);
            Ctor::Range(v.clone(), v)
        }
        Lit::Float(f) => Ctor::Opaque(f.to_string()),
        Lit::Str(s) => Ctor::Opaque(s.clone()),
        Lit::Bytes(b) => Ctor::Opaque(crate::parser::ast::bytes_repr(b)),
    }
}

fn lit_int(l: &Lit) -> Option<BigInt> {
    match l {
        Lit::Int(n) => Some(n.clone()),
        Lit::Bool(b) => Some(BigInt::from(*b as u8)),
        Lit::Char(c) => Some(BigInt::from(*c as u32)),
        _ => None,
    }
}

fn as_range(c: &Ctor) -> Option<(BigInt, BigInt)> {
    match c {
        Ctor::Range(a, b) => Some((a.clone(), b.clone())),
        _ => None,
    }
}

fn int_bounds(signed: bool, bits: u32) -> (BigInt, BigInt) {
    if signed {
        let limit = BigInt::from(1) << (bits - 1);
        (-limit.clone(), limit - 1)
    } else {
        ((BigInt::from(0)), (BigInt::from(1) << bits) - 1)
    }
}

/// The smallest value in `[lo, hi]` that no interval covers, or `None` if they
/// cover it end to end.
fn first_gap(ranges: &[(BigInt, BigInt)], lo: BigInt, hi: BigInt) -> Option<BigInt> {
    let mut sorted: Vec<&(BigInt, BigInt)> = ranges.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut next = lo;
    for (a, b) in sorted {
        if *a > next {
            return Some(next);
        }
        if *b >= next {
            next = b + 1;
        }
        if next > hi {
            return None;
        }
    }
    if next > hi { None } else { Some(next) }
}

/// Cut `[lo, hi]` at every interval boundary the rows drew.
///
/// Checking one interval per row would double-count where two overlap; checking
/// the pieces they cut the line into checks each distinct set of covering rows
/// exactly once, which is what makes `0..<5 | 3..<10` behave.
fn split_intervals(ranges: &[(BigInt, BigInt)], lo: BigInt, hi: BigInt) -> Vec<Ctor> {
    let mut cuts: Vec<BigInt> = vec![lo.clone()];
    for (a, b) in ranges {
        if *a > lo && *a <= hi {
            cuts.push(a.clone());
        }
        if *b >= lo && *b < hi {
            cuts.push(b + 1);
        }
    }
    cuts.sort();
    cuts.dedup();

    let mut out = Vec::new();
    for (i, start) in cuts.iter().enumerate() {
        let end = cuts.get(i + 1).map(|n| n - 1).unwrap_or_else(|| hi.clone());
        if *start <= end {
            out.push(Ctor::Range(start.clone(), end));
        }
    }
    out
}

/// Substitute a definition-relative type against a use site's type arguments.
fn subst(ty: &Ty, params: &[crate::sema::def::DefId], args: &[Ty]) -> Ty {
    match ty {
        Ty::Nominal { def, args: inner } if inner.is_empty() => {
            match params.iter().position(|p| p == def) {
                Some(i) => args.get(i).cloned().unwrap_or(Ty::Error),
                None => ty.clone(),
            }
        }
        Ty::Nominal { def, args: inner } => Ty::Nominal {
            def: *def,
            args: inner.iter().map(|t| subst(t, params, args)).collect(),
        },
        Ty::Ptr { mutable, inner } => Ty::Ptr {
            mutable: *mutable,
            inner: Box::new(subst(inner, params, args)),
        },
        Ty::Slice { mutable, inner } => Ty::Slice {
            mutable: *mutable,
            inner: Box::new(subst(inner, params, args)),
        },
        Ty::Array {
            len,
            mutable,
            inner,
        } => Ty::Array {
            len: len.clone(),
            mutable: *mutable,
            inner: Box::new(subst(inner, params, args)),
        },
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|t| subst(t, params, args)).collect()),
        Ty::Func { params: p, ret } => Ty::Func {
            params: p.iter().map(|t| subst(t, params, args)).collect(),
            ret: Box::new(subst(ret, params, args)),
        },
        other => other.clone(),
    }
}
