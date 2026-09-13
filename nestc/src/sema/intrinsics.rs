//! The registry of intrinsics the compiler knows how to supply (§6.4, §9).
//!
//! An intrinsic is an ordinary bodyless function declared in `core` and marked
//! `#intrinsic`. Nothing about the *declaration* is special — it has a
//! signature, it takes turbofish type arguments, it infers, and it is documented
//! beside every other function core provides. What is special is the **body**,
//! which the compiler supplies: an instruction, a constant, or nothing at all.
//!
//! ### Identity is the tag, not the name
//!
//! `#intrinsic("size_of")` names *which* intrinsic a declaration is. The
//! function's own name and path do not, for the same reason a `#lang` item's do
//! not: core must stay renameable and replaceable, and a compiler that keyed on
//! `core.mem.size_of` would make a library decision into a compiler change. The
//! shorthand `#intrinsic` — no argument — means "the tag is the declared name",
//! which is what every declaration in `core` happens to want; the explicit form
//! exists so a rename never has to break one.
//!
//! An `#intrinsic` tag this table does not list is an error **at the
//! declaration**, not a link failure later: core declaring a body the compiler
//! cannot fill is a mistake to report where it is written.
//!
//! ### What the compiler does with each
//!
//! Most of these need nothing beyond their signature. A call to one type-checks
//! as an ordinary call, and lowering emits an [`Intrinsic`] IR node in place of
//! the [`Call`] it would otherwise build, keyed by this tag. The two exceptions
//! carry a [`Special`], and both are exceptions for the same reason: their rule
//! is one no signature in the language can state.
//!
//! [`Intrinsic`]: crate::ir::ExprKind::Intrinsic
//! [`Call`]: crate::ir::ExprKind::Call

/// The one rule an intrinsic needs beyond its declared signature.
///
/// This is deliberately a *short* list. The temptation with intrinsics is to
/// give each one a special case in inference; the whole point of declaring them
/// in `core` is that the signature carries the shape, so a row here has to earn
/// its place by naming something the signature genuinely cannot say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    /// `make.<[]T>(n)` is written with the slice already and yields `[]mut T`.
    /// The *mutability* is what the allocation adds, and there is no bound that
    /// means "the same type, made mutable".
    MutableArg,
    /// `len(x)` accepts an array or a slice and nothing else. `T` is unbounded
    /// in the declaration because the language has no bound meaning "one of the
    /// two built-in sequences", so the check lives here.
    SequenceArg,
}

/// One intrinsic: the tag that identifies it, and the rule (if any) the compiler
/// applies beyond the declared signature.
#[derive(Debug, Clone, Copy)]
pub struct IntrinsicRow {
    pub tag: &'static str,
    pub special: Option<Special>,
}

const fn plain(tag: &'static str) -> IntrinsicRow {
    IntrinsicRow { tag, special: None }
}

const fn special(tag: &'static str, special: Special) -> IntrinsicRow {
    IntrinsicRow {
        tag,
        special: Some(special),
    }
}

/// Every intrinsic the compiler recognizes. Extensible: a new one is a row here
/// and a declaration in `core`, in that order, because the declaration is
/// rejected until the row exists.
pub const INTRINSICS: &[IntrinsicRow] = &[
    // Conversion (§6.5). Both take the target type *first* so a turbofish can
    // supply it — `cast.<u8>(n)` — and leave the source to be inferred from the
    // argument. `cast(n)` with no turbofish takes the target from context, which
    // is ordinary return-position inference and needs no special case.
    plain("cast"),
    plain("transmute"),
    // Layout queries (§12). Constants once the type argument is concrete.
    plain("size_of"),
    plain("align_of"),
    // Allocation (§6.9), and the one explicit release. `drop` is the same
    // instruction the escape analysis emits on its own (`design/lir.md` §5); a
    // program writing it takes on the question that analysis would have
    // answered, which is why `check::dropped` then refuses a later use.
    plain("new"),
    special("make", Special::MutableArg),
    plain("drop"),
    // Sequences (§3.2). `core`'s `.len()` methods **are** these — the members
    // are marked `#intrinsic` rather than given a body that forwards to one.
    special("len", Special::SequenceArg),
    // Indexing a built-in sequence (§3.2, §6.13). This is the body of `core`'s
    // `Index` impls on `[]T` and `[N]T`, so `a[i]` on a sequence goes through
    // the same trait a user type does and the compiler carries no special case
    // for what indexing *means*.
    //
    // It hands back a **pointer** to the element, because that is what the trait
    // promises: `a[i]` is `index(&a, i).*`, and the indirection is what makes
    // `a[i] = v` a place rather than a value. What that pointer *permits* is the
    // one thing the declared signature cannot say — see
    // [`crate::sema::lower::Lowerer::lower_index_call`].
    plain("index"),
    // Compile-time data.
    plain("embed_file"),
    // Failing, at run time and at compile time (§6.10, §8).
    //
    // `panic` is **not** here: it is an ordinary function in `core` that calls
    // the `#lang("panic_handler")` item, and the compiler's own failures (a
    // trapped overflow, an index out of bounds) lower to a call to it like any
    // other. What no library can write is the last instruction, so that — and
    // only that — is the intrinsic.
    plain("trap"),
    plain("assert"),
    // Integer arithmetic with a stated overflow behaviour (§6.6). These are the
    // inherent methods on the two integer families in `core/num.nest`, and they
    // need no `Special`: `func (self: Self, rhs: Self) -> Self` inside
    // `impl <const N: usize> int.<N>` says everything — same family, same width,
    // no widening — because `Self` is the family member being implemented.
    plain("wrapping_add"),
    plain("wrapping_sub"),
    // Reflection (§9's addition). All three are constants or one instruction:
    // `type_info` and `type_id` are read-only data the compiler already has by
    // the time it mangles a symbol, and `member_ptr` is the byte offset every
    // static field access already computes.
    plain("type_info"),
    plain("type_id"),
    plain("member_ptr"),
    // The collector (§6.4.1). All three yield `void`.
    plain("gc_collect"),
    plain("gc_keep_alive"),
    plain("gc_pin"),
];

/// The row for `tag`, or `None` if the compiler has never heard of it.
pub fn lookup(tag: &str) -> Option<&'static IntrinsicRow> {
    INTRINSICS.iter().find(|r| r.tag == tag)
}
