//! LIR — the low-level IR, and the last representation before code generation.
//!
//! `design/lir.md` is the specification; this module is what it describes. The
//! IR above it is a *typed tree* with the source's control flow still in it —
//! `if`, `match`, a `loop` with `break`. LIR gives that up and keeps everything
//! else: a function is a list of **basic blocks** connected by explicit jumps,
//! in the spirit of Rust's MIR.
//!
//! What is deliberately *not* given up is the type system, the GC model and the
//! `defer` semantics, none of which LLVM has a notion of. LIR is where those are
//! made explicit rather than where they are discarded.
//!
//! # What this stage adds
//!
//! - **Basic blocks and terminators** (§1). Structured control flow is gone; a
//!   two-way `if` is a `switch` on a `bool`, a `match` is a decision tree over a
//!   discriminant, a `loop` is a back edge.
//! - **Places** (§1). An lvalue is a local plus a chain of projections, and a
//!   projection is always "member *n*", "element *i*", "the pointee", or "this
//!   variant". Nothing else.
//! - **`defer` bodies placed on every exit path** (§3), as ordinary blocks. A
//!   deferred body is not a construct at this level and does not get a structure
//!   of its own: it is a block, reached by a jump, like everything else.
//! - **Flattened aggregates** (§7b). A tuple, an enum, a slice, a `distinct` and
//!   a trait object are all plain structs by the time they get here; only the
//!   array keeps a shape of its own, for the four reasons §7b lists. Every pass
//!   after this one asks structural questions, and each aggregate that kept its
//!   own shape would be one more case for every one of them.
//! - **Names** (§7). Every function carries the symbol the linker will see —
//!   decided by monomorphization, because that is where identity is decided —
//!   and the unmangled name beside it, for dumps and for the debugger.
//! - **Everything a debugger needs** (§7c), carried rather than reconstructed: a
//!   span on every statement, a source name on every local that had one, the
//!   variant names and array lengths that flattening would otherwise lose.
//! - **The build's settings, applied to the shape of the graph** (§7d).
//!   `overflow=trap` is not a flag on an instruction here: it is a checked
//!   operation, an extra edge, and a block that panics.
//!
//! # What it no longer has
//!
//! Generics and `const` generic parameters (monomorphization ran first, so every
//! type here is concrete), `impl` blocks, traits as a *concept* — a `dyn` call is
//! an indirect call through a vtable slot, and a vtable is a constant of function
//! pointers — structured control flow, and the distinction between a `match`, an
//! `if` and a `while`.
//!
//! **Intrinsics are gone as calls.** A `#intrinsic` declared in `core` has no
//! body; where the IR has a call to one, LIR has the operation it denotes.
//! `size_of.<T>()` is the number layout computed, and it arrives here already
//! folded.
//!
//! # Identity, and why this module does not use [`Meta`](crate::ir::Meta)
//!
//! The IR keys every per-node fact by an [`IrId`](crate::ir::IrId) in a side
//! table, because the facts a pass computes about an IR node are open-ended: a
//! type, a span, directives, a layout, a const value, an instantiation. LIR's
//! are not. A local has a name, a type and a span and will never have anything
//! else; a statement has a span. So they are **fields**, and the structures
//! below are self-contained — which is what lets a backend consume a
//! [`Function`] without also being handed the compiler's side tables.
//!
//! The one thing LIR does still reach for is the [`Ty`] of a local, which is the
//! same `Ty` the rest of the compiler uses. It is concrete here, and the
//! definitions behind every nominal one are in [`Program::types`].

use crate::common::source::FileSpan;
use crate::common::symbol::Symbol;
use crate::ir::ConstValue;
use crate::ir::layout::Layout;
use crate::parser::ast::{BinOp, UnOp};
use crate::sema::builtins::BuiltinOp;
use crate::sema::def::{DefId, Directive};
use crate::sema::ty::Ty;

pub mod escape;
pub mod lower;
pub mod pretty;
pub mod safepoint;

pub use lower::lower;

/// A whole program, lowered.
#[derive(Debug, Clone, Default)]
pub struct Program {
    /// Every aggregate the program uses, flattened to a struct (§7b), in the
    /// order they were first reached.
    pub types: Vec<TypeDef>,
    /// The program-lifetime regions (`#static`). A `::` constant is **not** here:
    /// it is its value (§2.5), and its uses carry that value directly.
    pub globals: Vec<Global>,
    /// One per `(trait, concrete type)` pair some `*T` → `*dyn Trait` coercion
    /// asked for (§7b). A vtable is data, and data is a struct.
    pub vtables: Vec<Vtable>,
    pub funcs: Vec<Function>,
}

/// A local slot: a parameter, a `let` binding, or a temporary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u32);

/// A basic block's index in its function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// A vtable's index in [`Program::vtables`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VtableId(pub u32);

/// One lowered function.
///
/// A **declaration** — an `extern("c") func` with no body — is a `Function` with
/// no blocks. It is here because a call to it is an ordinary call and codegen
/// still needs its signature and its symbol.
#[derive(Debug, Clone)]
pub struct Function {
    pub def: DefId,
    /// The unmangled name, with concrete arguments written out
    /// (`core.Vec.<i32>.push`). What a stack frame is labelled, and what a dump
    /// prints so a reader does not have to demangle by hand (§7, §7c).
    pub name: String,
    /// The name the linker sees. **This** is what the program is keyed by from
    /// here on (§7).
    pub symbol: Symbol,
    /// Every local, parameter slots first, indexed by [`LocalId`].
    pub locals: Vec<Local>,
    /// How many of the leading locals are parameters.
    pub params: usize,
    pub ret: Ty,
    /// The blocks, indexed by [`BlockId`]. Empty for a declaration. Block 0 is
    /// the entry.
    pub blocks: Vec<Block>,
    /// The ABI of an `extern("c") func`, or `None` for a Nest function.
    pub extern_abi: Option<Symbol>,
    /// Where the function was declared — "step into" and breakpoint resolution
    /// (§7c).
    pub span: Option<FileSpan>,
    /// The directives that survive to this level (§7): `#section`, `#offset`,
    /// `#inline`, `#unsafe`, and the layout ones kept for debug info and FFI.
    pub directives: Vec<Directive>,
}

/// One local slot.
///
/// The **name** is the part that has to be carried rather than recomputed: LIR
/// renumbers everything into slots, so a debugger printing `xs` instead of `_7`
/// needs the binding's name attached when the binding was lowered (§7c). A
/// temporary no source name produced simply has none, which is honest — better
/// than inventing one.
#[derive(Debug, Clone)]
pub struct Local {
    pub id: LocalId,
    pub name: Option<Symbol>,
    pub ty: Ty,
    pub span: Option<FileSpan>,
}

/// A program-lifetime region (`#static`, §2.6).
///
/// Its initial contents are a value the const evaluator already produced, or
/// `None` for a region that is simply zeroed. Nothing in a function refers to it
/// by anything but its [`DefId`] — see [`Base::Global`].
#[derive(Debug, Clone)]
pub struct Global {
    pub def: DefId,
    pub name: Symbol,
    pub ty: Ty,
    pub init: Option<ConstValue>,
    pub span: Option<FileSpan>,
}

/// A vtable: the addresses of one impl's methods, in the trait's declaration
/// order (§7b).
///
/// It is a **constant**, and its type is a struct of function pointers, which is
/// the whole reason to represent it this way: a vtable has an address, a layout
/// and a member at an offset, and every pass that already handles those handles
/// it for free. The trait disappears at this level; the ordering it fixed does
/// not.
#[derive(Debug, Clone)]
pub struct Vtable {
    pub id: VtableId,
    /// The trait whose slot order this follows.
    pub trait_def: DefId,
    /// The concrete type whose impl fills the slots.
    pub concrete: Ty,
    /// The symbol the vtable constant itself is emitted under.
    pub symbol: Symbol,
    /// One entry per trait method, in declaration order. `None` is a slot no
    /// impl and no default body fills — which object safety should have made
    /// impossible, and which is printed as a hole rather than silently skipped.
    pub slots: Vec<Option<VtableSlot>>,
}

/// One filled vtable slot.
#[derive(Debug, Clone)]
pub struct VtableSlot {
    pub method: Symbol,
    pub def: DefId,
    pub symbol: Symbol,
}

/// A basic block: a straight run of statements, then exactly one terminator.
#[derive(Debug, Clone)]
pub struct Block {
    pub id: BlockId,
    pub stmts: Vec<Stmt>,
    pub term: Terminator,
    /// What this block is *for*, printed as a trailing comment.
    ///
    /// It is metadata for a reader and nothing else — `then`, `loop body`,
    /// `overflow`, the variant a decision-tree node tested. A CFG is the one
    /// representation where a reader genuinely cannot reconstruct intent from
    /// the shape, because the shape is the same for every construct.
    pub label: Option<String>,
}

/// One instruction.
#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    /// The source position this came from. On **every** statement, because the
    /// line table is address → source position and a span reconstructed at the
    /// end is a span that is wrong (§7c).
    pub span: Option<FileSpan>,
    /// Set when collection may happen here (§6): a call, an allocation.
    pub safepoint: Option<Safepoint>,
}

impl Stmt {
    pub fn new(kind: StmtKind, span: Option<FileSpan>) -> Stmt {
        Stmt {
            kind,
            span,
            safepoint: None,
        }
    }
}

/// A point where the collector may run, and what it must be told (§6).
///
/// `live` is both halves of the answer at once. It is the **root set** — the
/// locals holding references the collector has to trace — and it is the list of
/// `reloc` definitions, because with a moving collector every one of those
/// locals holds a different address afterwards. They are one list rather than
/// two for the reason the rest of LIR keeps one of everything: `p := reloc p`
/// beside `live: [p]` is the same fact written twice, and two copies of a fact
/// are two things that can disagree.
///
/// What makes this a *definition* site rather than an annotation is the whole
/// argument of §6: a pass that does not know what a safepoint is still must not
/// move a load of `p` across one, and it will not, because `p` is redefined
/// here and every pass respects a definition.
///
/// Turning this into a shadow stack, an LLVM statepoint, or nothing at all (a
/// non-moving collector drops the relocs as identity) is **codegen's** choice.
/// The expensive part — knowing precisely which locals are live — is the same
/// for all of them, so it is computed once, here.
#[derive(Debug, Clone)]
pub struct Safepoint {
    pub live: Vec<LocalId>,
}

/// What a [`Stmt`] does.
///
/// Three, and the interesting one is the call. A call is an *instruction* rather
/// than a terminator, which is the single largest simplification in LIR and is
/// bought by the panic model (§2): a panic does not unwind, so a call has one
/// successor and the CFG stays roughly the size of the source.
#[derive(Debug, Clone)]
pub enum StmtKind {
    /// Compute a value and put it somewhere.
    Assign { place: Place, value: Rvalue },
    /// Call something, optionally keeping the result.
    ///
    /// `dest` is `None` for a call whose value is discarded and for one that
    /// does not return (`-> never`), whose block ends in
    /// [`TermKind::Unreachable`].
    Call {
        dest: Option<Place>,
        callee: Callee,
        args: Vec<Operand>,
    },
    /// A named machine operation with effects and, sometimes, a value (§9).
    ///
    /// It mirrors [`StmtKind::Call`] deliberately, down to the optional
    /// destination, because the two differ in exactly one way: a call transfers
    /// to a symbol and this does not. Everything else about them — arguments
    /// evaluated into operands, a result that may be discarded, a block that
    /// ends when the operation does not return — is the same question, and a
    /// backend answers it once.
    ///
    /// `dest` is `None` for an operation with no value (`$gc_collect()`) and for
    /// one that does not return (`$trap()`, whose block ends in
    /// [`TermKind::Unreachable`]). That is the whole reason it is a statement
    /// rather than an [`Rvalue`]: a slot typed `void` or `never` is a slot no
    /// machine has, and giving one to every `$trap()` made the representation
    /// claim something false.
    Intrinsic {
        dest: Option<Place>,
        name: Symbol,
        args: Vec<Operand>,
    },
    /// Free the allocation this pointer names, without involving the collector
    /// (§5).
    ///
    /// It arrives here two ways and they are the same instruction. The compiler
    /// emits one where escape analysis proved an object does not outlive its
    /// scope, on the same cleanup ladder `defer` rides — an allocation in a
    /// scope with three exits is freed once, in a block all three reach. A
    /// program emits one by writing `drop(p)` (§6.9), having taken on the
    /// question the analysis would otherwise have answered.
    ///
    /// It takes an **operand** rather than a local because the second way needs
    /// it to: `drop(node.*.next)` frees a pointer no local names. For a backend
    /// the two are one case — a pointer value, and a free.
    ///
    /// The operand is a **pointer**, always. A `make`d slice is `{ ptr, len }`
    /// by this level (§7b), and the lowering projects the member rather than
    /// handing over the header: an instruction whose operand is sometimes an
    /// address and sometimes a struct is one a backend has to switch on.
    Drop(Operand),
}

/// How a call reaches its code.
#[derive(Debug, Clone)]
pub enum Callee {
    /// A direct call to a known symbol.
    Static {
        def: DefId,
        name: String,
        symbol: Symbol,
    },
    /// A call through a pointer — a function value, or a vtable slot already
    /// loaded into a local. Dynamic dispatch is *this*, plus the two ordinary
    /// projections that fetched the slot (§9).
    Indirect(Operand),
}

/// An lvalue: a base plus a chain of projections (§1).
///
/// A place is never a value. `t := p.x` **loads** from one and `p.x = t`
/// **stores** into one; only the store form appears on the left of `=`.
#[derive(Debug, Clone)]
pub struct Place {
    pub base: Base,
    pub projection: Vec<Projection>,
}

impl Place {
    /// The whole of a local, with nothing projected out of it.
    pub fn local(id: LocalId) -> Place {
        Place {
            base: Base::Local(id),
            projection: Vec::new(),
        }
    }

    /// This place with one more projection on the end.
    pub fn then(mut self, p: Projection) -> Place {
        self.projection.push(p);
        self
    }

    /// Whether this names a whole local — what decides `:=` from `=` in a dump,
    /// because that is exactly the distinction the source language draws.
    pub fn is_whole_local(&self) -> bool {
        matches!(self.base, Base::Local(_)) && self.projection.is_empty()
    }
}

/// What a [`Place`] starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    Local(LocalId),
    /// A `#static` region (§2.6). It is a base rather than a local because its
    /// storage is the program's rather than the frame's: there is one of it for
    /// the whole run, and no function owns it.
    Global(DefId),
}

/// One step of a place path.
///
/// Every aggregate is a struct by now (§7b), so a field projection is "member
/// *n*" and nothing else — the `name` beside the index is for the reader. After
/// `#packed` / `#align` have had their say the index alone does not tell anyone
/// which field was meant, which is why a dump prints the name (§1).
#[derive(Debug, Clone)]
pub enum Projection {
    /// Member `index` of a struct.
    Field { index: u32, name: Symbol },
    /// Element `index` of an **array**, or of the pointee a pointer addresses.
    /// The index is a **value**, which is what keeps arrays from flattening
    /// (§7b): this is `base + i * stride`, not a constant offset.
    ///
    /// Never a slice: a slice flattened into `{ ptr, len }`, and a struct has
    /// members rather than elements. Indexing one goes through the pointer it
    /// holds, which is what §7b means by calling the slice the interesting
    /// near-miss.
    Index(Operand),
    /// `base.*` — the pointee.
    Deref,
    /// Read the enum's payload as this variant's fields. The tag is a separate
    /// member and is tested before this ever runs; §4's decision tree is what
    /// guarantees that.
    Variant { index: u32, name: Symbol },
}

/// A value an instruction reads: something already in memory, or a constant.
#[derive(Debug, Clone)]
pub enum Operand {
    /// Read a place.
    Copy(Place),
    Const(Constant),
}

impl Operand {
    pub fn local(id: LocalId) -> Operand {
        Operand::Copy(Place::local(id))
    }
}

/// A value that needs no code to produce.
#[derive(Debug, Clone)]
pub enum Constant {
    /// A scalar, a string, or a whole aggregate the const evaluator folded.
    Value(ConstValue),
    /// The address of a function — what a `::` constant bound to one denotes,
    /// and what fills a vtable slot.
    Func {
        def: DefId,
        name: String,
        symbol: Symbol,
    },
    /// The address of a vtable constant (§7b).
    Vtable(VtableId),
    /// Nothing in particular: the value of a `void`, and the contents of a slot
    /// that is about to be written field by field.
    Undef,
}

/// What a [`StmtKind::Assign`] computes.
#[derive(Debug, Clone)]
pub enum Rvalue {
    /// Move a value across unchanged.
    Use(Operand),
    /// `&place` / `&mut place`.
    Ref { mutable: bool, place: Place },
    /// A primitive machine operation (§6.13's [`BuiltinOp`] set).
    ///
    /// `checked` is the build's `overflow=` setting made real (§7d). It is
    /// decided **here** and not in codegen because the trap form is not a flag
    /// on an instruction — it is a second block, an extra edge and a call that
    /// diverges, and every pass after this one has to see that edge to be
    /// correct.
    Builtin {
        op: BuiltinOp,
        args: Vec<Operand>,
        checked: bool,
    },
    /// A comparison or a boolean operation — the primitive core that dispatches
    /// on nothing.
    Binary {
        op: BinOp,
        lhs: Operand,
        rhs: Operand,
    },
    Unary { op: UnOp, operand: Operand },
    /// A `$cast` between primitives. `from` is kept beside `to` because what the
    /// conversion *is* — a truncation, a sign extension, a float rounding —
    /// depends on both, and recovering `from` from the operand would mean codegen
    /// re-deriving a type this stage already had.
    Cast {
        value: Operand,
        from: Ty,
        to: Ty,
    },
    /// Build an aggregate out of its parts.
    Aggregate {
        kind: AggregateKind,
        fields: Vec<Operand>,
    },
    /// `ptr + index * stride(elem)` — **pointer arithmetic**, in elements.
    ///
    /// The source language has none, deliberately: an address you can move is an
    /// address you can move wrongly, and every sequence the language has carries
    /// its own bounds. LIR needs it anyway, because a slice flattened into
    /// `{ ptr, len }` (§7b) and reaching its element `i` is exactly this — the
    /// struct has members, not elements, so the arithmetic has to be somewhere
    /// and this is where.
    ///
    /// It is in **elements** rather than bytes, and carries the element type
    /// rather than a byte stride, for the reason every other type in LIR is
    /// carried: the stride is layout's answer and there should be one of it. A
    /// byte offset computed here would be a second one.
    Offset {
        ptr: Operand,
        index: Operand,
        elem: Ty,
    },
}

/// Which aggregate an [`Rvalue::Aggregate`] builds.
///
/// Each of these is a struct by §7b; the tag says which struct, so that a dump
/// and debug info can still print what was written.
#[derive(Debug, Clone)]
pub enum AggregateKind {
    Struct(DefId),
    Tuple,
    Array,
    /// One variant of an enum: the tag and this variant's payload.
    Variant {
        def: DefId,
        index: u32,
        name: Symbol,
    },
    /// `(ptr, len)`.
    Slice,
    /// `(data, vtable)` — a trait object's fat pointer.
    Dyn,
}

/// How a block ends. Exactly one per block.
#[derive(Debug, Clone)]
pub struct Terminator {
    pub kind: TermKind,
    pub span: Option<FileSpan>,
    /// Set on a loop's **back edge**, the third kind of safepoint (§6). A loop
    /// that calls nothing and allocates nothing would otherwise be a region the
    /// collector can never interrupt.
    pub safepoint: Option<Safepoint>,
}

impl Terminator {
    pub fn new(kind: TermKind, span: Option<FileSpan>) -> Terminator {
        Terminator {
            kind,
            span,
            safepoint: None,
        }
    }
}

/// What a [`Terminator`] does.
///
/// `switch` covers every branch there is: a two-way `if` is a switch on a
/// `bool`, and a `match` is a switch on a discriminant (§4). There is no
/// `branch` / `switch` pair, because two forms of the same edge would be two
/// cases in every pass that walks the graph.
#[derive(Debug, Clone)]
pub enum TermKind {
    Goto(BlockId),
    Switch {
        value: Operand,
        /// The tested values and where each goes. Values are integers: a
        /// discriminant, a `bool` (0 / 1), a `char`'s scalar value.
        arms: Vec<(i128, BlockId)>,
        /// Where everything else goes. Every switch has one — an exhaustive
        /// `match` sends it to a block that is [`TermKind::Unreachable`], which
        /// is a real guarantee rather than a hope because exhaustiveness was
        /// decided on the IR before this ran (§4).
        otherwise: BlockId,
    },
    Return(Option<Operand>),
    /// Control does not reach here. What follows a call to a `-> never` function
    /// (§2), and what an exhaustive `match`'s fallback is.
    Unreachable,
}

/// One aggregate, flattened to a struct (§7b).
#[derive(Debug, Clone)]
pub struct TypeDef {
    /// The type's mangled encoding — its identity, and what the table is keyed
    /// by.
    pub key: String,
    /// How the type is written, for dumps and debug info.
    pub name: String,
    pub members: Vec<TypeMember>,
    pub layout: Layout,
    /// What this was before flattening.
    ///
    /// It is kept because flattening is a change of *representation*, not of
    /// information (§7b): a debugger showing `2` where the source says `.green`
    /// is a worse debugger, so the variant names have to survive the tag they
    /// became.
    pub origin: Origin,
}

/// One member of a flattened struct.
#[derive(Debug, Clone)]
pub struct TypeMember {
    pub name: Symbol,
    pub ty: Ty,
    pub offset: u64,
}

/// What a [`TypeDef`] was before §7b flattened it.
#[derive(Debug, Clone)]
pub enum Origin {
    /// It always was one.
    Struct(DefId),
    /// `distinct T` — already a one-member struct in the IR.
    Distinct(DefId),
    /// An enum: a `tag` member and a `payload` member, plus the variants the
    /// payload stands for.
    Enum {
        def: DefId,
        variants: Vec<VariantDef>,
    },
    /// `(A, B)` — members named by position.
    Tuple,
    /// `[]T` / `[]mut T` — `{ ptr, len }`.
    Slice,
    /// `dyn Trait` behind a pointer — `{ data, vtable }`.
    Dyn(DefId),
}

/// One enum variant, as it survives flattening: the tag value that selects it,
/// its name, and where its payload's fields sit **within the payload**.
#[derive(Debug, Clone)]
pub struct VariantDef {
    pub name: Symbol,
    pub tag: i128,
    pub members: Vec<TypeMember>,
    /// Whether the payload was written positionally (`.b(T)`), which is what a
    /// dump needs in order to print the variant the way it was written.
    pub tuple: bool,
}
