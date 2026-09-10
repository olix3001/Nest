//! IR node identity, and the side table hung off it.
//!
//! The storage is [`common::meta::MetaStore`](crate::common::meta::MetaStore),
//! the same type-indexed table the AST keys by
//! [`NodeId`](crate::parser::ast::NodeId). What this module adds is the two
//! things that are specific to the IR: where ids come from, and named accessors
//! for the two facts every node has — its **span** and its **type**.
//!
//! Those two are metadata rather than fields for the same reason the AST treats
//! them that way: each is one fact computed by one stage and read by later ones,
//! and each is the *same* fact whether the node in hand is an expression, a
//! block, a parameter or a function. A field per node type would be four places
//! to keep in step. The rule the IR follows is that a node holds only what is
//! particular to its own shape, and anything that recurs across shapes lives
//! here.
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
use crate::sema::def::Directive;
use crate::sema::ty::Ty;

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

    // ===< Types >===
    //
    // A node's type lives here rather than in a field for the same reason the
    // AST keeps it here: it is a fact one stage computes and later ones read,
    // and it is the *same* fact on an expression, a block, a parameter and a
    // function. Repeating it as a field on each would make four places to keep
    // in step, and would make "what is this node's type" a different question
    // depending on which node you are holding.

    /// Record `id`'s type. Lowering sets one for every expression, block,
    /// parameter and function it builds.
    pub fn set_ty(&self, id: IrId, ty: Ty) {
        self.set(id, ty);
    }

    /// `id`'s type, cloned. Use [`Meta::with_ty`] when a borrow will do — a
    /// `Ty` is a tree, and cloning one to read its head is wasteful.
    ///
    /// `None` means no stage ever typed the node. Nothing reaches the IR
    /// untyped, so like a missing span this is a bug rather than a legitimate
    /// state; [`Meta::ty_or_error`] is the reading most callers want.
    pub fn ty(&self, id: IrId) -> Option<Ty> {
        self.get::<Ty>(id)
    }

    /// Borrow `id`'s type and run `f` on it.
    pub fn with_ty<R>(&self, id: IrId, f: impl FnOnce(&Ty) -> R) -> Option<R> {
        self.with::<Ty, R>(id, f)
    }

    /// `id`'s type, or [`Ty::Error`] if it somehow has none — so a consumer can
    /// keep walking instead of unwrapping. An untyped node is already a bug;
    /// making every reader panic on it turns one bug into a crash while
    /// reporting an unrelated diagnostic.
    pub fn ty_or_error(&self, id: IrId) -> Ty {
        self.ty(id).unwrap_or(Ty::Error)
    }

    // ===< Directives >===
    //
    // Directives are written on functions, on types, and on fields, so by the
    // same rule that put spans and types here they belong here too. They are
    // also the clearest case for a side table: the front end deliberately
    // *carries* directives it has no opinion about, and every stage that does
    // have one — layout for `#packed` / `#align` / `#soa`, codegen for
    // `#inline` / `#section` / `#offset`, the `#const` checker — is a different
    // stage from the one that wrote them down (§9).

    /// Record the `#...` directives written on `id`, in source order.
    pub fn set_directives(&self, id: IrId, directives: Vec<Directive>) {
        if !directives.is_empty() {
            self.set(id, directives);
        }
    }

    /// The directives written on `id`, in source order; empty if none were.
    pub fn directives(&self, id: IrId) -> Vec<Directive> {
        self.get::<Vec<Directive>>(id).unwrap_or_default()
    }

    /// Whether `id` carries `#name`.
    pub fn has_directive(&self, id: IrId, name: &str) -> bool {
        self.with::<Vec<Directive>, _>(id, |ds| ds.iter().any(|d| d.name.as_str() == name))
            .unwrap_or(false)
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
    fn types_round_trip_and_default_to_error() {
        let meta = Meta::new();
        let id = meta.fresh();
        assert_eq!(meta.ty(id), None);
        assert_eq!(meta.ty_or_error(id), Ty::Error);
        meta.set_ty(id, Ty::Bool);
        assert_eq!(meta.ty(id), Some(Ty::Bool));
        assert_eq!(meta.with_ty(id, |t| matches!(t, Ty::Bool)), Some(true));
        assert_eq!(meta.ty_or_error(meta.fresh()), Ty::Error);
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
