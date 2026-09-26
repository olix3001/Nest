//! Promotion: which locals live on the collected heap because their address
//! outlives the frame (spec §6.6, "Where `&` points: escape and promotion").
//!
//! `&x` is a pointer into the frame, and a frame ends when its function
//! returns. A pointer that is still reachable after that would read whatever
//! the next call left there, so a local whose address **escapes** is placed in
//! a cell the collector owns instead — the same cell a local a closure shares
//! lives in ([`crate::ir::Boxed`]) — and `&x` is the cell's address. A local
//! whose address does not escape stays in the frame: promotion costs an
//! allocation, and it is only paid where it is needed.
//!
//! The same holds for a **temporary**: `&P { x: 1 }` and `&{ x in x + 1 }`
//! take the address of a value that has no name, and one that escapes is
//! built in a cell as well.
//!
//! ### What escaping is
//!
//! An address escapes when a copy of it can be reached from anywhere but this
//! frame — when it, or a pointer derived from it (`&x.field`, `&arr[i]`, a
//! sub-slice, a cast, a `*dyn` made from it), is:
//!
//! - **returned** from the function the local belongs to, or the value of a
//!   `break`;
//! - **stored through a pointer** — into memory that is not one of this
//!   function's own locals;
//! - **passed to a call that keeps it**: a parameter keeps what it is given
//!   when, in the callee's own body, the parameter escapes by these same
//!   rules. A call the compiler cannot see into — through a `*dyn`, a function
//!   pointer, or to a function with no body here — keeps everything.
//!
//! A callee that only **returns** a parameter does not keep it: the call's
//! result is then a pointer derived from the argument, and is followed like
//! one. `const q := first(&x)` leaves `x` in the frame while `q` stays in it
//! too, and `return first(&x)` promotes `x`.
//!
//! A value that **holds** a derived pointer is followed like the pointer: a
//! struct, tuple or enum value built with one (`Holder { p: &x }`), a closure
//! that copies one, a local such a value is bound or assigned to (`h.p = &x`
//! makes `h` a holder), a field read out of a holder, and `&` of a holder.
//! So `const h := Holder { p: &tmp }; h.p.x` leaves `tmp` in the frame, and
//! `return h` promotes it. Likewise `const p := &x; return p` escapes and
//! `const p := &x; p.y = 1` does not. Reading through a pointer (`p.x`,
//! `p.*`), comparing it, and handing it to a call that does not keep it are
//! not escapes.
//!
//! ### Parameters: per-function summaries
//!
//! "Does this callee keep its `i`-th argument" is answered by a summary per
//! function, computed over the whole program before any promotion is decided.
//! Every summary starts at "keeps nothing" and is raised as bodies are
//! analyzed, until nothing changes — which is what lets a recursive function
//! be analyzed in terms of itself. Libraries carry their IR, so a call into
//! `std` is seen through like any other.
//!
//! ### The collector
//!
//! A promoted local is a pointer-typed local holding its cell, and a promoted
//! temporary's cell is one too; everything that later holds the address holds
//! a pointer. Safepoints ([`super::safepoint`]) take their roots from exactly
//! those — the live locals whose type can hold a reference — so a promoted
//! object is traced from wherever its address is still live, with nothing
//! special here. [`super::escape`]'s drops never see one: they are only for
//! a `new`/`make` a program wrote.
//!
//! ### Why it is flow-insensitive
//!
//! A local that holds a derived pointer on one path holds it on all of them as
//! far as this analysis is concerned, and an address that escapes on one path
//! escapes. That over-promotes a local now and then, which costs an
//! allocation; the opposite mistake is a dangling pointer.

use std::collections::{HashMap, HashSet};

use super::escape::{each_block, each_child};
use crate::ir::meta::IrId;
use crate::ir::{Block, Dispatch, Expr, ExprKind, Function, Pattern, PatternKind, Stmt, StmtKind};
use crate::sema::def::DefId;

/// What is promoted: per function (by its id), the locals that live in cells,
/// and every `&` of a temporary whose value is built in a cell.
#[derive(Debug, Default)]
pub struct Promotions {
    pub locals: HashMap<IrId, HashSet<DefId>>,
    pub temps: HashSet<IrId>,
}

/// Decide promotion for every function.
pub fn analyze<'a>(funcs: impl Iterator<Item = &'a Function>) -> Promotions {
    let funcs: Vec<&Function> = funcs.collect();
    let by_def: HashMap<DefId, &Function> = funcs.iter().map(|f| (f.def, *f)).collect();
    let sums = summaries(&funcs, &by_def);
    let mut out = Promotions::default();
    for f in &funcs {
        let Some(body) = &f.body else { continue };
        let mut seeds = Vec::new();
        addressed_block(body, &mut seeds);
        let mut seen = HashSet::new();
        for seed in seeds {
            if !seen.insert(seed) {
                continue;
            }
            let mut flow = Flow::new(seed, &by_def, &sums);
            if !flow.escapes(body) {
                continue;
            }
            match seed {
                Seed::Local(d) => {
                    out.locals.entry(f.id).or_default().insert(d);
                }
                Seed::Temp(id) => {
                    out.temps.insert(id);
                }
                Seed::Param(_) => {}
            }
        }
    }
    out
}

/// What a function does with one parameter: keeps it (it escapes), or hands
/// it back as (part of) its result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Summary {
    keeps: bool,
    returns: bool,
}

/// Each function's [`Summary`] per parameter, raised to a fixpoint. A function
/// with no body here keeps all of them: nothing says otherwise.
fn summaries(
    funcs: &[&Function],
    by_def: &HashMap<DefId, &Function>,
) -> HashMap<DefId, Vec<Summary>> {
    let mut sums: HashMap<DefId, Vec<Summary>> = funcs
        .iter()
        .map(|f| {
            let unknown = Summary {
                keeps: f.body.is_none(),
                returns: false,
            };
            (f.def, vec![unknown; f.params.len()])
        })
        .collect();
    loop {
        let mut changed = false;
        for f in funcs {
            let Some(body) = &f.body else { continue };
            for (i, p) in f.params.iter().enumerate() {
                if sums[&f.def][i].keeps {
                    continue;
                }
                let mut flow = Flow::new(Seed::Param(p.def), by_def, &sums);
                let now = Summary {
                    keeps: flow.escapes(body),
                    returns: flow.returned,
                };
                let old = &mut sums.get_mut(&f.def).unwrap()[i];
                let merged = Summary {
                    keeps: old.keeps || now.keeps,
                    returns: old.returns || now.returns,
                };
                if merged != *old {
                    *old = merged;
                    changed = true;
                }
            }
        }
        if !changed {
            return sums;
        }
    }
}

/// The address being followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Seed {
    /// A parameter's value: whatever the caller passed.
    Param(DefId),
    /// `&x` of a local or parameter `x`.
    Local(DefId),
    /// `&value` of a temporary, by the `&` expression.
    Temp(IrId),
}

struct Flow<'a> {
    seed: Seed,
    /// Locals holding a pointer derived from the seed.
    holders: HashSet<DefId>,
    escaped: bool,
    /// Whether a parameter seed is handed back as the function's result.
    returned: bool,
    funcs: &'a HashMap<DefId, &'a Function>,
    sums: &'a HashMap<DefId, Vec<Summary>>,
}

impl<'a> Flow<'a> {
    fn new(
        seed: Seed,
        funcs: &'a HashMap<DefId, &'a Function>,
        sums: &'a HashMap<DefId, Vec<Summary>>,
    ) -> Self {
        Flow {
            seed,
            holders: HashSet::new(),
            escaped: false,
            returned: false,
            funcs,
            sums,
        }
    }

    /// The seed, or a pointer derived from it, leaves as the function's result:
    /// a parameter's is the caller's to follow, anything else's escapes.
    fn leaves(&mut self) {
        match self.seed {
            Seed::Param(_) => self.returned = true,
            _ => self.escaped = true,
        }
    }

    /// Whether the seed escapes `body`, following the locals that come to hold
    /// it until there are no more.
    fn escapes(&mut self, body: &Block) -> bool {
        loop {
            let before = self.holders.len();
            self.block(body, true);
            if self.escaped {
                return true;
            }
            if self.holders.len() == before {
                return false;
            }
        }
    }

    // ===< Where a derived pointer goes >===

    /// `outer` is the function's body, whose value is what it returns.
    fn block(&mut self, b: &Block, outer: bool) {
        for s in &b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &b.tail {
            if outer && self.derived(t) {
                self.leaves();
            }
            self.walk(t);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { pattern, init } => {
                if self.derived(init) {
                    bindings(pattern, &mut self.holders);
                }
                self.walk(init);
            }
            StmtKind::Assign { place, value } => {
                // Into this frame's own local — itself, or a field of it — the
                // local becomes a holder; through a pointer, it is stored
                // somewhere this function does not own.
                if self.derived(value) {
                    match own_local(place) {
                        Some(d) => {
                            self.holders.insert(d);
                        }
                        None => self.escaped = true,
                    }
                }
                self.walk(place);
                self.walk(value);
            }
            StmtKind::Return(Some(e)) => {
                if self.derived(e) {
                    self.leaves();
                }
                self.walk(e);
            }
            StmtKind::Break(Some(e)) => {
                if self.derived(e) {
                    self.escaped = true;
                }
                self.walk(e);
            }
            StmtKind::Expr(e) | StmtKind::Defer(e) => self.walk(e),
            StmtKind::Return(None) | StmtKind::Break(None) | StmtKind::Continue => {}
        }
    }

    fn walk(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Call {
                callee,
                args,
                builtin,
                dispatch,
            } if builtin.is_none() => {
                for (i, a) in args.iter().enumerate() {
                    if self.derived(a) && self.summary(callee, dispatch, i, args.len()).keeps {
                        self.escaped = true;
                    }
                }
            }
            ExprKind::Intrinsic { name, args } if !harmless(name.as_str()) => {
                if args.iter().any(|a| self.derived(a)) {
                    self.escaped = true;
                }
            }
            ExprKind::Match { scrutinee, arms } if self.derived(scrutinee) => {
                for a in arms {
                    bindings(&a.pattern, &mut self.holders);
                }
            }
            _ => {}
        }
        each_block(e, &mut |b| self.block(b, false));
        each_child(e, &mut |c| self.walk(c));
    }

    /// What the callee does with its `i`-th argument of `n`. One it cannot see
    /// keeps it.
    fn summary(&self, callee: &Expr, dispatch: &Dispatch, i: usize, n: usize) -> Summary {
        let unknown = Summary {
            keeps: true,
            returns: true,
        };
        let (Dispatch::Static, ExprKind::Global(def)) = (dispatch, &callee.kind) else {
            return unknown;
        };
        match (self.funcs.get(def), self.sums.get(def)) {
            (Some(f), Some(sums)) if f.params.len() == n => sums[i],
            _ => unknown,
        }
    }

    // ===< What a derived pointer is >===

    /// Whether `e`'s value is the seed or a pointer derived from it.
    fn derived(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Local(d) => self.holders.contains(d) || self.seed == Seed::Param(*d),
            ExprKind::Ref { place, .. } => self.addresses(place, e.id),
            ExprKind::Intrinsic { name, args } if preserves(name.as_str()) => {
                args.first().is_some_and(|a| self.derived(a))
            }
            ExprKind::DynCast { value, .. } => self.derived(value),
            // An aggregate built with one holds it, and so does a field read
            // out of a holder. A field read *through* a pointer is a value of
            // the pointee's, which is not derived.
            ExprKind::Construct { fields, .. } => fields.iter().any(|(_, f)| self.derived(f)),
            ExprKind::Tuple { elems: args } | ExprKind::Variant { args, .. } => {
                args.iter().any(|a| self.derived(a))
            }
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => self.derived(base),
            // A call that hands an argument back returns what it was given.
            ExprKind::Call {
                callee,
                args,
                builtin: None,
                dispatch,
            } => args.iter().enumerate().any(|(i, a)| {
                self.derived(a) && self.summary(callee, dispatch, i, args.len()).returns
            }),
            ExprKind::Block(b) => b.tail.as_deref().is_some_and(|t| self.derived(t)),
            ExprKind::If { then, els, .. } => {
                then.tail.as_deref().is_some_and(|t| self.derived(t))
                    || els
                        .as_ref()
                        .and_then(|b| b.tail.as_deref())
                        .is_some_and(|t| self.derived(t))
            }
            ExprKind::Match { arms, .. } => arms.iter().any(|a| self.derived(&a.body)),
            _ => false,
        }
    }

    /// Whether `&place` (the `&` being `at`) is derived from the seed: its
    /// place is inside the seeded local or a holder, inside a temporary that is
    /// the seed or holds it, or reached through a derived pointer.
    fn addresses(&self, place: &Expr, at: IrId) -> bool {
        match &place.kind {
            ExprKind::Local(d) => self.seed == Seed::Local(*d) || self.holders.contains(d),
            ExprKind::Global(_) => false,
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
                self.addresses(base, at)
            }
            ExprKind::Deref { base } => self.derived(base),
            _ => self.seed == Seed::Temp(at) || self.derived(place),
        }
    }
}

/// The local a place is part of, when it is one of this frame's: `h`, `h.p`,
/// `h.0.q` — not anything reached through a pointer.
fn own_local(place: &Expr) -> Option<DefId> {
    match &place.kind {
        ExprKind::Local(d) => Some(*d),
        ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => own_local(base),
        _ => None,
    }
}

/// Intrinsics whose result points where their first argument does.
fn preserves(name: &str) -> bool {
    matches!(
        name,
        "cast" | "transmute" | "index" | "index_mut" | "slice" | "member_ptr" | "member_dyn"
    )
}

/// Intrinsics that may be handed a derived pointer without keeping it: the
/// pointer-preserving ones (their result is followed instead), and those that
/// only read through it or measure it.
fn harmless(name: &str) -> bool {
    preserves(name)
        || matches!(
            name,
            "len"
                | "memcpy"
                | "memset"
                | "variant_tag"
                | "drop"
                | "gc_keep_alive"
                | "type_id"
                | "size_of"
                | "align_of"
        )
}

/// Every name `p` binds.
fn bindings(p: &Pattern, out: &mut HashSet<DefId>) {
    match &p.kind {
        PatternKind::Binding { def, .. } => {
            out.insert(*def);
        }
        PatternKind::Variant { sub, .. }
        | PatternKind::Tuple(sub)
        | PatternKind::Or(sub)
        | PatternKind::TupleStruct { elems: sub, .. } => sub.iter().for_each(|s| bindings(s, out)),
        PatternKind::Struct { fields, .. } => fields.iter().for_each(|(_, s)| bindings(s, out)),
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            prefix.iter().chain(suffix).for_each(|s| bindings(s, out));
            if let Some(Some(b)) = rest {
                out.insert(b.def);
            }
        }
        PatternKind::At { binding, pattern } => {
            out.insert(binding.def);
            bindings(pattern, out);
        }
        PatternKind::Deref(inner) => bindings(inner, out),
        PatternKind::Wildcard | PatternKind::Lit(_) | PatternKind::Range { .. } => {}
    }
}

// ===< Candidates >===

/// Every address a body takes of something in its own frame: a local's (the
/// place under the `&` is rooted in it, through fields) or a temporary's.
fn addressed_block(b: &Block, out: &mut Vec<Seed>) {
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Let { init: e, .. }
            | StmtKind::Expr(e)
            | StmtKind::Defer(e)
            | StmtKind::Return(Some(e))
            | StmtKind::Break(Some(e)) => addressed(e, out),
            StmtKind::Assign { place, value } => {
                addressed(place, out);
                addressed(value, out);
            }
            StmtKind::Return(None) | StmtKind::Break(None) | StmtKind::Continue => {}
        }
    }
    if let Some(t) = &b.tail {
        addressed(t, out);
    }
}

fn addressed(e: &Expr, out: &mut Vec<Seed>) {
    if let ExprKind::Ref { place, .. } = &e.kind {
        let mut root = &**place;
        while let ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } = &root.kind {
            root = base;
        }
        match &root.kind {
            ExprKind::Local(d) => out.push(Seed::Local(*d)),
            ExprKind::Deref { .. } | ExprKind::Global(_) => {}
            _ => out.push(Seed::Temp(e.id)),
        }
    }
    each_block(e, &mut |b| addressed_block(b, out));
    each_child(e, &mut |c| addressed(c, out));
}
