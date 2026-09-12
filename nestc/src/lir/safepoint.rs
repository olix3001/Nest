//! GC safepoints, and the live pointer set each one carries (§6).
//!
//! The collector needs two things: to know when it may run, and to know which
//! stack slots hold references when it does. This pass answers both, over the
//! finished CFG, because both are questions about the graph — where control can
//! reach a collection, and what a later block will still read.
//!
//! ### Where a safepoint goes
//!
//! Three places, and §6 names all three:
//!
//! - **A call.** Collection may happen inside the callee.
//! - **An allocation.** `new` / `make` are the ordinary reason a collection
//!   starts, and `gc_collect` asks for one outright.
//! - **A loop's back edge.** Without it a loop that calls nothing and allocates
//!   nothing is a region the collector can never interrupt.
//!
//! Each is a point control *passes through*, so the safepoint sits on the
//! statement or the terminator itself rather than becoming a block of its own:
//! the association between "this call" and "the collection that may happen in
//! it" is what an LLVM statepoint needs, and a separate block loses it.
//!
//! ### What it carries, and why it is a definition
//!
//! The live set: the locals holding traceable references that something *after*
//! this point still reads. It is the root set the collector traces, and it is
//! also the list of relocations, because a moving collector leaves every one of
//! those locals holding a different address (see [`Safepoint`]).
//!
//! Precision is not only a performance question. An over-approximate live set
//! keeps garbage alive, and with a moving collector it also means relocating
//! objects nothing will ever read again (§6). So this is real liveness — a
//! backward dataflow to a fixed point — rather than "every pointer local in the
//! frame".
//!
//! ### The one over-approximation left
//!
//! [`is_root`] asks whether a **type** can contain a reference, and a raw `*T`
//! answers yes whatever `T` is. A vtable pointer is a `*void` by then (§7b) and
//! is counted with the rest. Narrowing that wants a distinction between a
//! managed reference and a machine address which the type system does not draw
//! today; counting a code pointer as a root is safe and costs a word in a stack
//! map, so it waits for the language to have the distinction rather than for
//! this pass to guess at it.

use std::collections::{HashMap, HashSet};

use crate::sema::def::DefTable;
use crate::sema::ty::Ty;

use super::{
    Base, BlockId, Function, LocalId, Operand, Place, Program, Rvalue, Safepoint, StmtKind,
    TermKind,
};

/// Annotate every safepoint in the program with its live set.
pub fn annotate(defs: &DefTable, program: &mut Program) {
    // The flattened type table is what says whether a named type holds a
    // reference, so it is indexed once and asked per local (§7b).
    let types: HashMap<String, usize> = program
        .types
        .iter()
        .enumerate()
        .map(|(i, t)| (t.key.clone(), i))
        .collect();
    let cx = Roots {
        defs,
        types: &types,
        defined: &program.types,
    };
    for f in &mut program.funcs {
        annotate_function(&cx, f);
    }
}

/// What the root question needs: the def table for a type's key, and the
/// flattened definitions for its members.
struct Roots<'a> {
    defs: &'a DefTable,
    types: &'a HashMap<String, usize>,
    defined: &'a [super::TypeDef],
}

fn annotate_function(cx: &Roots, f: &mut Function) {
    // Which locals are worth tracing at all. A non-pointer local is never a
    // root (§6), so it never enters the dataflow and never reaches a stack map.
    let roots: HashSet<LocalId> = f
        .locals
        .iter()
        .filter(|l| cx.is_root(&l.ty, 0))
        .map(|l| l.id)
        .collect();
    if roots.is_empty() {
        return;
    }

    let back = back_edges(f);
    let live_out = liveness(f, &roots);

    for b in &mut f.blocks {
        // Walk the block backwards, carrying the set live *after* the point in
        // hand. A safepoint's roots are exactly that set: what the collector
        // must trace is what somebody is still going to read.
        let mut live = live_out.get(&b.id).cloned().unwrap_or_default();
        b.term.safepoint = back.contains(&b.id).then(|| point(&live));
        term_reads(&b.term.kind, &mut live, &roots);
        for s in b.stmts.iter_mut().rev() {
            let is_point = match &s.kind {
                StmtKind::Call { .. } => true,
                StmtKind::Assign { value, .. } => allocates(value),
                StmtKind::Drop(_) => false,
            };
            // The set is the one live **before** the statement, not after it.
            // Collection happens while the statement is running — inside the
            // callee, inside the allocator — and at that moment the destination
            // has not been written, so relocating it would relocate whatever the
            // slot happened to hold. What is live going in is exactly the
            // arguments plus everything a later block still reads.
            stmt_effect(&s.kind, &mut live, &roots);
            s.safepoint = is_point.then(|| point(&live));
        }
    }
}

fn point(live: &HashSet<LocalId>) -> Safepoint {
    let mut live: Vec<LocalId> = live.iter().copied().collect();
    // Sorted, because a set has no order and a dump that reorders between runs
    // is a dump nothing can be tested against.
    live.sort_by_key(|l| l.0);
    Safepoint { live }
}

/// Whether this rvalue may start a collection.
fn allocates(v: &Rvalue) -> bool {
    match v {
        Rvalue::Intrinsic { name, .. } => matches!(name.as_str(), "new" | "make" | "gc_collect"),
        _ => false,
    }
}

// ===< Liveness >===

/// The set of roots live on the way *out* of each block, to a fixed point.
fn liveness(f: &Function, roots: &HashSet<LocalId>) -> HashMap<BlockId, HashSet<LocalId>> {
    let mut live_in: HashMap<BlockId, HashSet<LocalId>> = HashMap::new();
    let mut out: HashMap<BlockId, HashSet<LocalId>> = HashMap::new();
    let mut changed = true;
    while changed {
        changed = false;
        for b in f.blocks.iter().rev() {
            let mut live = HashSet::new();
            for succ in successors(&b.term.kind) {
                if let Some(s) = live_in.get(&succ) {
                    live.extend(s.iter().copied());
                }
            }
            out.insert(b.id, live.clone());
            term_reads(&b.term.kind, &mut live, roots);
            for s in b.stmts.iter().rev() {
                stmt_effect(&s.kind, &mut live, roots);
            }
            if live_in.get(&b.id) != Some(&live) {
                live_in.insert(b.id, live);
                changed = true;
            }
        }
    }
    out
}

fn successors(t: &TermKind) -> Vec<BlockId> {
    match t {
        TermKind::Goto(b) => vec![*b],
        TermKind::Switch {
            arms, otherwise, ..
        } => arms.iter().map(|(_, b)| *b).chain([*otherwise]).collect(),
        TermKind::Return(_) | TermKind::Unreachable => Vec::new(),
    }
}

/// Apply one statement to the live set, walking backwards: kills first, then
/// the reads it adds back.
fn stmt_effect(k: &StmtKind, live: &mut HashSet<LocalId>, roots: &HashSet<LocalId>) {
    match k {
        StmtKind::Assign { place, value } => {
            write(place, live, roots);
            rvalue_reads(value, live, roots);
        }
        StmtKind::Call { dest, callee, args } => {
            if let Some(d) = dest {
                write(d, live, roots);
            }
            if let super::Callee::Indirect(o) = callee {
                operand_reads(o, live, roots);
            }
            for a in args {
                operand_reads(a, live, roots);
            }
        }
        // A `drop` **reads** the pointer it frees, which is what keeps the
        // allocation traceable right up to the point it stops existing.
        StmtKind::Drop(l) => {
            if roots.contains(l) {
                live.insert(*l);
            }
        }
    }
}

/// A write to a *whole* local ends the previous value's live range — and is not
/// a read of it. A write **through** a projection is the other case entirely: it
/// reads the base to find out where to store.
fn write(p: &Place, live: &mut HashSet<LocalId>, roots: &HashSet<LocalId>) {
    match (&p.base, p.projection.is_empty()) {
        (Base::Local(l), true) => {
            live.remove(l);
        }
        _ => place_reads(p, live, roots),
    }
}

fn term_reads(t: &TermKind, live: &mut HashSet<LocalId>, roots: &HashSet<LocalId>) {
    match t {
        TermKind::Switch { value, .. } => operand_reads(value, live, roots),
        TermKind::Return(Some(v)) => operand_reads(v, live, roots),
        TermKind::Goto(_) | TermKind::Return(None) | TermKind::Unreachable => {}
    }
}

fn rvalue_reads(v: &Rvalue, live: &mut HashSet<LocalId>, roots: &HashSet<LocalId>) {
    match v {
        Rvalue::Use(o) => operand_reads(o, live, roots),
        Rvalue::Ref { place, .. } => place_reads(place, live, roots),
        Rvalue::Binary { lhs, rhs, .. } => {
            operand_reads(lhs, live, roots);
            operand_reads(rhs, live, roots);
        }
        Rvalue::Unary { operand, .. } => operand_reads(operand, live, roots),
        Rvalue::Cast { value, .. } => operand_reads(value, live, roots),
        Rvalue::Aggregate { fields, .. } => {
            for o in fields {
                operand_reads(o, live, roots);
            }
        }
        Rvalue::Offset { ptr, index, .. } => {
            operand_reads(ptr, live, roots);
            operand_reads(index, live, roots);
        }
        Rvalue::Builtin { args, .. } | Rvalue::Intrinsic { args, .. } => {
            for o in args {
                operand_reads(o, live, roots);
            }
        }
    }
}

fn operand_reads(o: &Operand, live: &mut HashSet<LocalId>, roots: &HashSet<LocalId>) {
    if let Operand::Copy(p) = o {
        place_reads(p, live, roots);
    }
}

/// Reading a place reads its base, and reads whatever a `[i]` projection
/// indexes with.
fn place_reads(p: &Place, live: &mut HashSet<LocalId>, roots: &HashSet<LocalId>) {
    if let Base::Local(l) = &p.base
        && roots.contains(l)
    {
        live.insert(*l);
    }
    for proj in &p.projection {
        if let super::Projection::Index(o) = proj {
            operand_reads(o, live, roots);
        }
    }
}

// ===< Back edges >===

/// The blocks whose terminator closes a cycle — a jump to a block already on the
/// current path through the graph.
///
/// A DFS with the path marked is enough and needs no dominator tree: an edge to
/// a block that is still open is a back edge by definition, and that is the same
/// answer dominators give on the reducible graphs this lowering produces.
fn back_edges(f: &Function) -> HashSet<BlockId> {
    let index: HashMap<BlockId, usize> = f
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id, i))
        .collect();
    let mut done = HashSet::new();
    let mut open = HashSet::new();
    let mut found = HashSet::new();
    let mut stack: Vec<(BlockId, usize)> = Vec::new();
    if f.blocks.is_empty() {
        return found;
    }
    stack.push((f.blocks[0].id, 0));
    open.insert(f.blocks[0].id);
    while let Some((id, next)) = stack.pop() {
        let Some(&i) = index.get(&id) else { continue };
        let succs = successors(&f.blocks[i].term.kind);
        if next < succs.len() {
            stack.push((id, next + 1));
            let s = succs[next];
            if open.contains(&s) {
                found.insert(id);
            } else if !done.contains(&s) {
                open.insert(s);
                stack.push((s, 0));
            }
        } else {
            open.remove(&id);
            done.insert(id);
        }
    }
    found
}

// ===< What counts as a root >===

impl Roots<'_> {
    /// Whether a value of this type can hold a reference the collector must know
    /// about.
    ///
    /// It is asked of the *type* rather than of the value, and it looks
    /// **through** aggregates: a struct with a pointer field is as much a root
    /// as the pointer is, because the frame slot holding it is where that
    /// pointer lives. A `str` is one for the same reason — a `distinct []u8` is
    /// a slice underneath (§2.4), and the flattened table is what says so.
    fn is_root(&self, ty: &Ty, depth: usize) -> bool {
        // A type that reaches this deep has a pointer somewhere above it or is
        // recursive, and a recursive type is recursive *through* a pointer.
        if depth > 8 {
            return true;
        }
        match ty {
            Ty::Ptr { .. } | Ty::Slice { .. } | Ty::Dyn(_) => true,
            Ty::Array { inner, .. } => self.is_root(inner, depth + 1),
            Ty::Tuple(elems) => elems.iter().any(|t| self.is_root(t, depth + 1)),
            Ty::Nominal { .. } => self.nominal_is_root(ty, depth),
            _ => false,
        }
    }

    /// A named type is a root when one of its members is.
    ///
    /// An **enum** is the case worth naming: its flattened members are a tag and
    /// `[N]u8` of shared payload, which holds no pointer as far as a type can
    /// tell, so the variants' own member types are what the question has to be
    /// asked of.
    fn nominal_is_root(&self, ty: &Ty, depth: usize) -> bool {
        let key = crate::ir::mono::type_key(self.defs, ty);
        let Some(&i) = self.types.get(&key) else {
            // A named type the program never used as a value has no flattened
            // definition here. Answering yes is the safe direction: a root
            // missed is a use after free, and a root invented is a word in a
            // stack map.
            return true;
        };
        let t = &self.defined[i];
        if let super::Origin::Enum { variants, .. } = &t.origin {
            return variants
                .iter()
                .any(|v| v.members.iter().any(|m| self.is_root(&m.ty, depth + 1)));
        }
        t.members.iter().any(|m| self.is_root(&m.ty, depth + 1))
    }
}
