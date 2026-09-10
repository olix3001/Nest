//! IR node identity, and the side table hung off it.
//!
//! The storage is [`common::meta::MetaStore`](crate::common::meta::MetaStore),
//! the same type-indexed table the AST keys by
//! [`NodeId`](crate::parser::ast::NodeId). What this module adds is the two
//! things that are specific to the IR: where ids come from, and the span
//! accessors.
//!
//! Identity differs from the AST's in one way that matters. An AST node's id is
//! its slot in the arena; an IR node is an owned tree node, so its [`IrId`] is a
//! field and there is no way to recover the node from the id. The store is
//! write-a-fact, read-a-fact — never a lookup table for the nodes themselves.
//!
//! Ids are unique **across the whole compilation**, not per
//! [`Program`](super::Program). Lowering is per file but everything after it is
//! whole-program: linking the per-file programs must not have to renumber
//! anything, and a diagnostic raised by a whole-program pass has to be able to
//! ask for one node's span without first knowing which file it came from. That
//! is why the allocator lives here, in a store the
//! [`Session`](crate::sema::session::Session) owns once, rather than on each
//! `Program`.

use std::any::Any;
use std::cell::Cell;
use std::fmt;

use crate::common::meta::MetaStore;
use crate::common::source::FileSpan;

/// The identity of one IR node, unique across the whole compilation.
///
/// Carried by [`Function`](super::Function), [`Param`](super::Param),
/// [`Block`](super::Block), [`Stmt`](super::Stmt), [`Expr`](super::Expr),
/// [`Arm`](super::Arm), [`Pattern`](super::Pattern) and
/// [`Binding`](super::Binding) — every node shape the IR has, with no
/// exceptions, so a pass never has to ask whether *this* kind of node can be
/// annotated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IrId(pub u32);

impl fmt::Display for IrId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

/// The IR's id allocator and per-node side table.
///
/// One per compilation, owned by the [`Session`](crate::sema::session::Session).
///
/// ```ignore
/// let id = meta.fresh();                 // lowering, allocating a node
/// meta.set_span(id, FileSpan::new(file, span));
/// meta.set(id, ConstSafety::Unsafe);     // the `#const` checker, later
/// ```
#[derive(Default, Clone)]
pub struct Meta {
    /// The next unused id. A [`Cell`] rather than a plain field for the same
    /// reason the store has interior mutability: lowering allocates through a
    /// shared borrow.
    next: Cell<u32>,
    store: MetaStore<IrId>,
}

impl fmt::Debug for Meta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Meta({} nodes, {:?})", self.next.get(), self.store)
    }
}

impl Meta {
    /// An empty store, numbering from zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next unused [`IrId`].
    pub fn fresh(&self) -> IrId {
        let id = IrId(self.next.get());
        self.next.set(id.0 + 1);
        id
    }

    /// How many ids have been handed out — the exclusive upper bound of every
    /// live `IrId`, which is what a dense per-node vector in a later pass wants
    /// for sizing.
    pub fn allocated(&self) -> u32 {
        self.next.get()
    }

    // ===< The type-indexed table >===

    /// Attach a `T` to `id`, replacing and returning any previous `T`.
    pub fn set<T: Any>(&self, id: IrId, value: T) -> Option<T> {
        self.store.set(id, value)
    }

    /// Clone out the `T` attached to `id`. Use [`Meta::with`] to avoid the clone.
    pub fn get<T: Any + Clone>(&self, id: IrId) -> Option<T> {
        self.store.get(&id)
    }

    /// Borrow the `T` attached to `id` and run `f` on it.
    pub fn with<T: Any, R>(&self, id: IrId, f: impl FnOnce(&T) -> R) -> Option<R> {
        self.store.with(&id, f)
    }

    /// Whether any `T` is attached to `id`.
    pub fn has<T: Any>(&self, id: IrId) -> bool {
        self.store.has::<T>(&id)
    }

    /// Remove and return the `T` attached to `id`.
    pub fn take<T: Any>(&self, id: IrId) -> Option<T> {
        self.store.take::<T>(&id)
    }

    // ===< Spans >===
    //
    // Spans get named accessors rather than being left to `set::<FileSpan>`
    // because every node has one and half the compiler asks for them: a bare
    // `meta.get::<FileSpan>(id)` at each site would turn the one universal fact
    // into the least discoverable one.

    /// Record where `id` came from. Lowering calls this for every node it
    /// allocates.
    pub fn set_span(&self, id: IrId, span: FileSpan) {
        self.set(id, span);
    }

    /// Where `id` came from.
    ///
    /// `None` means the node was allocated without a source position — which
    /// lowering never does deliberately, so an absent span is a bug rather than
    /// a synthetic node. It is still an `Option` rather than a panic because a
    /// diagnostic that loses its caret is a much smaller failure than a compiler
    /// that aborts while reporting one.
    pub fn span(&self, id: IrId) -> Option<FileSpan> {
        self.get::<FileSpan>(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::source::FileId;
    use crate::common::span::Span;

    #[derive(Debug, Clone, PartialEq)]
    struct Fact(u8);

    #[test]
    fn ids_are_unique_and_dense() {
        let meta = Meta::new();
        let ids: Vec<IrId> = (0..4).map(|_| meta.fresh()).collect();
        assert_eq!(ids, vec![IrId(0), IrId(1), IrId(2), IrId(3)]);
        assert_eq!(meta.allocated(), 4);
    }

    #[test]
    fn a_fact_reaches_the_underlying_store() {
        let meta = Meta::new();
        let (a, b) = (meta.fresh(), meta.fresh());
        meta.set(a, Fact(1));
        assert!(meta.has::<Fact>(a));
        assert_eq!(meta.get::<Fact>(a), Some(Fact(1)));
        assert_eq!(meta.get::<Fact>(b), None);
        assert_eq!(meta.take::<Fact>(a), Some(Fact(1)));
        assert!(!meta.has::<Fact>(a));
    }

    #[test]
    fn spans_round_trip_through_the_named_accessors() {
        let meta = Meta::new();
        let id = meta.fresh();
        let span = FileSpan::new(FileId(2), Span::new(10, 14));
        meta.set_span(id, span);
        assert_eq!(meta.span(id), Some(span));
        assert_eq!(meta.span(meta.fresh()), None);
    }

    #[test]
    fn a_shared_store_can_still_allocate_and_annotate() {
        // Lowering holds `&Meta`, never `&mut Meta`: both the counter and the
        // table have to work through a shared borrow.
        fn lower(meta: &Meta) -> IrId {
            let id = meta.fresh();
            meta.set(id, Fact(9));
            id
        }
        let meta = Meta::new();
        let id = lower(&meta);
        assert_eq!(meta.get::<Fact>(id), Some(Fact(9)));
        assert_eq!(meta.allocated(), 1);
    }
}
