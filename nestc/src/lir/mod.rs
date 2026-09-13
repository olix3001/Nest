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
//!   tag, a `loop` is a back edge.
//! - **Places** (§1). An lvalue is a local plus a chain of projections, and a
//!   projection is always "member *n*", "element *i*", "the pointee", or "read
//!   these bytes as that type". Nothing else.
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
//! an indirect call through a member of an ordinary struct of function pointers
//! — structured control flow, and the distinction between a `match`, an `if` and
//! a `while`.
//!
//! **Intrinsics are gone as calls.** A `#intrinsic` declared in `core` has no
//! body; where the IR has a call to one, LIR has the operation it denotes.
//! `size_of.<T>()` is the number layout computed, and it arrives here already
//! folded.
//!
//! # A unit is self-contained (§11)
//!
//! Nothing below refers to the compiler's own tables. A type is a [`Ty`], whose
//! only non-obvious case is [`Ty::Named`] — an index into the [`Unit`]'s own
//! type table. A function is a [`FuncId`] and a global is a [`GlobalId`], both
//! indices into the same unit. So a [`Unit`] can be written out, handed to a
//! backend in another process, or compiled on its own thread, and every
//! question it raises it also answers.
//!
//! That is what makes the codegen-unit split (§11) a filter rather than a
//! redesign: a unit gets the functions it defines, a **declaration** for every
//! function it calls, and copies of the types and globals it names.
//!
//! # Identity, and why this module does not use [`Meta`](crate::ir::Meta)
//!
//! The IR keys every per-node fact by an [`IrId`](crate::ir::IrId) in a side
//! table, because the facts a pass computes about an IR node are open-ended: a
//! type, a span, directives, a layout, a const value, an instantiation. LIR's
//! are not. A local has a name, a type and a span and will never have anything
//! else; a statement has a span. So they are **fields**.

use num_bigint::BigInt;

use crate::common::source::FileSpan;
use crate::common::symbol::Symbol;
use crate::ir::layout::Layout;

pub mod entry;
pub mod escape;
pub mod lower;
pub mod pretty;
pub mod safepoint;
pub mod unit;

pub use lower::lower;

/// A whole program, lowered, in one or more codegen units (§11).
///
/// One unit is the default and is the whole program; `-C codegen-units=N` asks
/// for a split. The units are *independent* — see the module note — so what the
/// number buys is parallel code generation, and what it costs is a declaration
/// in one unit for every definition in another.
#[derive(Debug, Clone, Default)]
pub struct Program {
    pub units: Vec<Unit>,
}

impl Program {
    /// The single unit a program has when nothing asked for a split.
    ///
    /// Panics on an empty program, which is not one this compiler produces: the
    /// split always emits at least one unit.
    pub fn unit(&self) -> &Unit {
        &self.units[0]
    }
}

/// One codegen unit: a self-contained slice of the program (§11).
#[derive(Debug, Clone, Default)]
pub struct Unit {
    /// What the unit is called — the source file it came from, or the merged
    /// name of several. It is the object file's name and the dump's heading.
    pub name: String,
    /// Every aggregate this unit's code mentions, flattened to a struct (§7b),
    /// indexed by [`TypeId`].
    pub types: Vec<TypeDef>,
    /// The program-lifetime regions (`#static`, §2.6) and the read-only data
    /// this unit's code refers to, indexed by [`GlobalId`]. A global defined in
    /// another unit appears here as [`Linkage::Imported`], with no initializer.
    pub globals: Vec<Global>,
    /// The functions, indexed by [`FuncId`]. One with no blocks is a
    /// **declaration** — an `extern` function, or a function another unit
    /// defines.
    pub funcs: Vec<Function>,
}

impl Unit {
    pub fn ty(&self, id: TypeId) -> &TypeDef {
        &self.types[id.0 as usize]
    }

    pub fn func(&self, id: FuncId) -> &Function {
        &self.funcs[id.0 as usize]
    }

    pub fn global(&self, id: GlobalId) -> &Global {
        &self.globals[id.0 as usize]
    }
}

/// A local slot: a parameter, a `let` binding, or a temporary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u32);

/// A basic block's index in its function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// An aggregate's index in [`Unit::types`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(pub u32);

/// A function's index in [`Unit::funcs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FuncId(pub u32);

/// A global's index in [`Unit::globals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalId(pub u32);

// ===< Types (§7b) >===

/// A type, as a machine has to hold it.
///
/// This is **not** [`crate::sema::ty::Ty`]. The front end's type is about what a
/// program may say; this one is about what a register or a field holds, so
/// everything that was a rule rather than a representation is already gone:
/// generics (monomorphization), `distinct` (§9), mutability (§9 — no target has
/// two kinds of address), and the name of a nominal type, which is an index into
/// the unit's own table.
///
/// The one thing it keeps that a machine does not have is [`Ty::Never`], on the
/// return of a function that does not come back. Every backend has a way to say
/// that and none of them agree on how, so LIR says it in the signature and lets
/// them each spell it.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    /// `int.<N>` / `uint.<N>` (§3.1). A `char` arrives as `uint.<32>` and a
    /// `bool` does not: see [`Ty::Bool`].
    Int { bits: u16, signed: bool },
    /// `f16` / `f32` / `f64` / `f128`.
    Float { bits: u16 },
    /// One bit of truth in at least one byte of storage. It is a case of its own
    /// rather than `uint.<1>` because every backend has a distinct notion of it
    /// — LLVM's `i1`, C's `_Bool`, wasm's `i32` in a specific 0/1 range — and
    /// each of the three wants to know which one this is.
    Bool,
    /// A pointer, with the pointee kept: a load through one needs the size, and
    /// a projection through one needs the layout.
    Ptr(Box<Ty>),
    /// `[N]T` — the one aggregate that does not flatten (§7b), because the
    /// element index is a value rather than a name.
    Array { len: u64, elem: Box<Ty> },
    /// The type of a function *pointer*: what a vtable slot holds and what an
    /// indirect call goes through. There is no way to have a `func` by value.
    Func {
        params: Vec<Ty>,
        ret: Box<Ty>,
    },
    /// A struct in this unit's type table — which after §7b's flattening is
    /// every aggregate the program has: a struct, a tuple, an enum, a slice, a
    /// trait object's fat pointer, a vtable.
    Named(TypeId),
    /// No value. The return of a function that returns nothing.
    Void,
    /// Control does not come back. Only ever a return type (§1): a *local* of
    /// this type is a slot no machine has, and an invariant test says so.
    Never,
}

impl Ty {
    pub fn ptr(inner: Ty) -> Ty {
        Ty::Ptr(Box::new(inner))
    }

    /// Whether a value of this type can hold a reference the collector traces
    /// (§6). A pointer is one; so is anything containing one.
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            Ty::Int { .. } | Ty::Float { .. } | Ty::Bool | Ty::Ptr(_) | Ty::Func { .. }
        )
    }
}

/// One aggregate, flattened to a struct (§7b).
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub id: TypeId,
    /// The type's mangled encoding — its identity.
    ///
    /// It is what the table is keyed by while it is being built, and it is what
    /// says that two units' copies of one type *are* one type: the split gives
    /// each unit its own numbering (§11), so the key is the only thing that
    /// survives it. A backend merging debug info across units needs exactly
    /// that.
    pub key: String,
    /// How the type is written, in full: `core.Vec.<i32>`, `Shape.circle`,
    /// `vtable.Draw`. A dump names a type the way the source does, so a reader
    /// never has to demangle (§7c).
    pub name: String,
    pub members: Vec<TypeMember>,
    pub layout: Layout,
    /// What this was before flattening.
    ///
    /// It is kept because flattening is a change of *representation*, not of
    /// information (§7b): a debugger showing `2` where the source says `.green`
    /// is a worse debugger, so the variant names have to survive the tag they
    /// became. A backend never matches on it.
    pub origin: Origin,
}

/// One member of a flattened struct.
#[derive(Debug, Clone)]
pub struct TypeMember {
    pub name: Symbol,
    pub ty: Ty,
    pub offset: u64,
}

/// What a [`TypeDef`] was before §7b flattened it. Debug metadata.
#[derive(Debug, Clone)]
pub enum Origin {
    /// It always was one.
    Struct,
    /// An enum: a `tag` member and a `payload` member, plus the variants the
    /// payload stands for.
    Enum { variants: Vec<VariantDef> },
    /// `(A, B)` — members named by position.
    Tuple,
    /// `[]T` / `[]mut T` — `{ ptr, len }`.
    Slice,
    /// `dyn Trait` behind a pointer — `{ data, vtable }`.
    Dyn { trait_name: String },
    /// One enum variant's payload, as a struct of its own: the type a
    /// [`Projection::Cast`] reads the shared payload bytes as.
    Variant { parent: String },
    /// A trait's vtable: one member per method, in declaration order (§7b).
    Vtable { trait_name: String },
}

/// One enum variant, as it survives flattening: the tag value that selects it,
/// its name, and the struct type its payload is read as.
#[derive(Debug, Clone)]
pub struct VariantDef {
    pub name: Symbol,
    pub tag: i128,
    /// The variant's own fields, as a struct type in this unit's table, at
    /// offsets **relative to the payload**.
    ///
    /// This is what makes reading a variant an ordinary field access: the enum
    /// keeps one payload big and aligned enough for every variant (§7b), and
    /// this type says how to read those bytes. A backend gets the offsets from
    /// the table like every other member's, instead of being handed a byte array
    /// and a rule.
    pub ty: TypeId,
    /// Whether the payload was written positionally (`.b(T)`), which is what a
    /// dump needs in order to print the variant the way it was written.
    pub tuple: bool,
}

// ===< Functions, globals >===

/// One lowered function.
///
/// A **declaration** — an `extern("c") func` with no body, or a function this
/// unit calls and another unit defines — is a `Function` with no blocks. It is
/// here because a call to it is an ordinary call and codegen still needs its
/// signature and its symbol.
#[derive(Debug, Clone)]
pub struct Function {
    /// The unmangled name, with concrete arguments written out
    /// (`core.Vec.<i32>.push`). What a stack frame is labelled, and what a dump
    /// prints so a reader does not have to demangle by hand (§7, §7c).
    pub name: String,
    /// The name the linker sees. **This** is what the program is keyed by from
    /// here on (§7), and what ties a declaration in one unit to the definition
    /// in another.
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
    /// Where the function was declared — "step into", breakpoint resolution
    /// (§7c), and which codegen unit it belongs to (§11).
    pub span: Option<FileSpan>,
    /// The decided facts a backend needs, rather than the directive list they
    /// were decided from (§7).
    pub attrs: FunctionAttrs,
}

/// What the front end's directives *mean* to a backend (§7).
///
/// The directives themselves are an AST-shaped list, and re-interpreting one is
/// work every backend would do identically and could do differently. They are
/// decided once, here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunctionAttrs {
    /// `#section("name")` — which object-file section the code goes in.
    pub section: Option<Symbol>,
    /// `#inline` / `#inline(never)`.
    pub inline: Inline,
    /// `#offset(N)` — the symbol is fixed at this position in the binary, for
    /// an interrupt vector or a boot header (§9).
    pub offset: Option<i128>,
    /// `@public` — visible outside its unit and outside the program (§9).
    /// Everything else may be given internal linkage, which is the whole reason
    /// a backend is told: a symbol only one unit uses can be made local to it.
    pub public: bool,
    /// `#unsafe` — the checks this body was compiled without. A backend does not
    /// act on it; it is carried because a profiler and a debugger both want to
    /// say so (§9).
    pub unchecked: bool,
}

/// What `#inline` asked for. A hint, and a backend may ignore it (§9).
///
/// `Never` has no spelling in the language today; it is here because the shape
/// of the question is "which of the three", and a backend that matches on it now
/// does not have to change when one appears.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Inline {
    #[default]
    Default,
    Always,
    Never,
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

/// A program-lifetime region: a `#static` (§2.6), or the read-only storage a
/// constant needs (§2.5).
///
/// **A constant that does not fit in a register is one of these.** A string's
/// bytes, a byte string's, an array constant's contents and a vtable are all
/// data with an address, and an operand that carried the blob itself would ask
/// every backend to invent read-only data emission on its own, differently. So
/// the blob is a global and the operand is its address.
#[derive(Debug, Clone)]
pub struct Global {
    /// The name a dump prints, in full.
    pub name: String,
    /// The name the linker sees.
    pub symbol: Symbol,
    pub ty: Ty,
    /// Its initial contents, or `None` for a region that is simply zeroed — and
    /// for a global this unit only *refers* to ([`Linkage::Imported`]).
    pub init: Option<Constant>,
    /// Whether anything may write to it. A `#static` may; the storage behind a
    /// constant may not, and a backend is free to put it in `.rodata` and to
    /// merge two of them that hold the same bytes.
    pub mutable: bool,
    /// Who else can see it, and which unit defines it (§11).
    pub linkage: Linkage,
    pub span: Option<FileSpan>,
}

/// Where a global is defined and who may see the name (§11).
///
/// The distinction only exists because the program is cut into units. A
/// `#static` is written in one file and read from anywhere, so it is one
/// definition and a declaration in every other unit. The storage behind a
/// *constant* is not like that: no file wrote it, nothing can refer to it by
/// name, and it is immutable — so each unit that needs one gets its own copy,
/// under a name the linker never sees, and a unit stays independent of its
/// neighbours' anonymous data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linkage {
    /// Defined here, and the name is the linker's: exactly one unit defines it.
    External,
    /// Defined here, and private to this unit. Another unit that needs the same
    /// contents has its own copy.
    Internal,
    /// Defined in another unit. The linker resolves it.
    Imported,
}

// ===< Code >===

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
    /// Run something, optionally keeping the result.
    ///
    /// A symbol, a pointer and an intrinsic are three *callees*, not three
    /// statements: what differs between them is how the code is reached, and
    /// everything else — arguments evaluated into operands, a result that may be
    /// discarded, a block that ends when the operation does not return — is the
    /// same question, answered once.
    ///
    /// `dest` is `None` for a call whose value is discarded and for one that
    /// does not return (`-> never`), whose block ends in
    /// [`TermKind::Unreachable`]. That is why an intrinsic is a statement rather
    /// than an [`Rvalue`]: a slot typed `void` or `never` is a slot no machine
    /// has, and giving one to every `$trap()` made the representation claim
    /// something false.
    Call {
        dest: Option<Place>,
        callee: Callee,
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
    /// A direct call to a function this unit declares or defines.
    Static(FuncId),
    /// A call through a pointer — a function value, or a vtable slot already
    /// loaded into a local. Dynamic dispatch is *this*, plus the two ordinary
    /// projections that fetched the slot (§9).
    Indirect(Operand),
    /// A machine operation with effects, named by the compiler rather than by
    /// the linker (§9).
    ///
    /// It is an **enum** rather than a symbol so that a backend's match is
    /// exhaustive: an intrinsic added upstream is then a compile error in every
    /// backend rather than a silent fall-through.
    ///
    /// Every member is one instruction or one runtime call, and that is a rule
    /// this list has to keep earning. `slice` and `array` were once here and are
    /// not any more: a slice is an `Offset` and an `Aggregate` over a pointer
    /// and a length, and a slice literal is a `make` and a store per element.
    /// Both were built out of instructions a backend already had, so both
    /// belonged in the lowering — the `slice` one especially, since it used to
    /// take a `Range`, which is a six-variant enum and would have made every
    /// backend switch on a tag to recover what the syntax already knew. An
    /// intrinsic that needs a branch is not an intrinsic.
    Intrinsic(Intrinsic),
}

/// Every operation that reaches a backend by name (§9).
///
/// The set is closed and small: what is here is what no library can write and
/// what the lowering does not turn into ordinary instructions. `size_of`,
/// `align_of`, `cast`, `drop`, `index`, `len`, `wrapping_add` and `wrapping_sub`
/// are all *gone* by this point — folded to a constant, to a projection, to an
/// [`Rvalue::Op`] or to [`StmtKind::Drop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intrinsic {
    /// Allocate one object of the destination's pointee type, collected (§6.9).
    New,
    /// Allocate a slice of `n` elements, collected (§6.9).
    Make,
    /// Stop the processor: `ud2`, `brk`, `unreachable`. The one operation no
    /// library can write, which is why it is the only unconditional one here.
    Trap,
    /// Fail the run unless the argument is true (§6.10).
    Assert,
    /// Reinterpret the bits as the destination's type. The one intrinsic whose
    /// type is not in the instruction: it is on the destination, because "become
    /// whatever this slot holds" is what the operation *is*.
    Transmute,
    /// The file's bytes, at compile time.
    EmbedFile,
    /// Run a collection now (§6.4.1).
    GcCollect,
    /// Keep this reference reachable across the point (§6.4.1).
    GcKeepAlive,
    /// Pin this object for the duration (§6.4.1).
    GcPin,
    /// An intrinsic this lowering has no case for.
    ///
    /// It exists so that adding a row to `sema::intrinsics` cannot silently
    /// produce a call to a function that does not exist; a test asserts that
    /// every declared intrinsic maps to one of the cases above, so a program
    /// never contains this.
    Unknown(Symbol),
}

impl Intrinsic {
    /// How the dump names it, and how `sema` spells it.
    pub fn name(&self) -> &str {
        match self {
            Intrinsic::New => "new",
            Intrinsic::Make => "make",
            Intrinsic::Trap => "trap",
            Intrinsic::Assert => "assert",
            Intrinsic::Transmute => "transmute",
            Intrinsic::EmbedFile => "embed_file",
            Intrinsic::GcCollect => "gc_collect",
            Intrinsic::GcKeepAlive => "gc_keep_alive",
            Intrinsic::GcPin => "gc_pin",
            Intrinsic::Unknown(s) => s.as_str(),
        }
    }

    /// The intrinsic `sema` spells this way, or [`Intrinsic::Unknown`].
    pub fn from_name(name: &Symbol) -> Intrinsic {
        match name.as_str() {
            "new" => Intrinsic::New,
            "make" => Intrinsic::Make,
            "trap" => Intrinsic::Trap,
            "assert" => Intrinsic::Assert,
            "transmute" => Intrinsic::Transmute,
            "embed_file" => Intrinsic::EmbedFile,
            "gc_collect" => Intrinsic::GcCollect,
            "gc_keep_alive" => Intrinsic::GcKeepAlive,
            "gc_pin" => Intrinsic::GcPin,
            _ => Intrinsic::Unknown(name.clone()),
        }
    }
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

    /// The whole of a global.
    pub fn global(id: GlobalId) -> Place {
        Place {
            base: Base::Global(id),
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
    /// A global's storage (§2.6). It is a base rather than a local because the
    /// storage is the program's rather than the frame's: there is one of it for
    /// the whole run, and no function owns it.
    Global(GlobalId),
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
    /// Read these bytes as another type: the same address, a different shape.
    ///
    /// It is how a variant is read (§7b). An enum is `{ tag, payload }` with one
    /// payload big and aligned enough for every variant, and a variant's fields
    /// are a struct type of their own — so `s.payload as Shape.circle` is the
    /// whole of what reading a variant is, and the field offsets under it come
    /// from the type table like every other. A backend emits a pointer cast,
    /// which every target has.
    Cast(TypeId),
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

    pub fn int(n: impl Into<BigInt>) -> Operand {
        Operand::Const(Constant::Int(n.into()))
    }

}

/// A value that needs no code to produce.
///
/// An **operand**'s constant is a scalar, an address or `undef` — never a blob:
/// a string's bytes and an aggregate's contents are [`Global`]s by the time they
/// get here, and what the operand holds is the address. The composite cases
/// below appear only in a global's own initializer, which is the one place that
/// describes data rather than a value in a register. An invariant test says so.
#[derive(Debug, Clone)]
pub enum Constant {
    /// An integer, or a `char`'s scalar value. The type is the destination's —
    /// a literal does not carry one, the same way no machine's immediate does.
    ///
    /// Arbitrary precision, because the language has arbitrary integer widths
    /// (§3.1): a `u4096` constant does not fit in any machine integer, and the
    /// front end already carries it exactly. A backend reads as many bits as the
    /// destination type has.
    Int(BigInt),
    Float(f64),
    Bool(bool),
    /// The address of a function — what a `::` constant bound to one denotes,
    /// and what fills a vtable slot.
    Func(FuncId),
    /// The address of a global.
    Global(GlobalId),
    /// An aggregate's contents, member by member, in declaration order.
    /// Initializers only.
    Aggregate(Vec<Constant>),
    /// Raw bytes: a string's contents, a byte string's. Initializers only.
    Bytes(Vec<u8>),
    /// One variant of an enum, as data: the tag, and the payload's fields in the
    /// variant type's order. Initializers only.
    Variant {
        tag: i128,
        name: Symbol,
        payload: Vec<Constant>,
    },
    /// Nothing in particular: the value of a `void`, and the contents of a slot
    /// that is about to be written member by member.
    Undef,
}

/// What a [`StmtKind::Assign`] computes.
#[derive(Debug, Clone)]
pub enum Rvalue {
    /// Move a value across unchanged.
    Use(Operand),
    /// `&place`. Mutability is not carried: no target distinguishes two kinds of
    /// address, and the rule that needed the distinction was enforced in sema
    /// (§9).
    Ref(Place),
    /// A primitive machine operation.
    ///
    /// One shape for all of them — unary, binary, checked — because the arity is
    /// the opcode's business and a backend that matches one enum once is a
    /// backend that cannot forget a case. `ty` is the type the operation runs
    /// *at*: `lt.u64` and `lt.i64` are different instructions on every target,
    /// and the operands may be constants, which carry no type of their own.
    ///
    /// The checked opcodes are the build's `overflow=` setting made real (§7d).
    /// They are decided **here** and not in codegen because the trap form is not
    /// a flag on an instruction — it is a second block, an extra edge and a call
    /// that diverges, and every pass after this one has to see that edge to be
    /// correct.
    Op {
        op: Op,
        ty: Ty,
        args: Vec<Operand>,
    },
    /// A conversion between primitives, and `kind` says **which** conversion.
    ///
    /// The two types travel beside it and are not redundant with it: `kind` is
    /// the instruction, and `from`/`to` are the widths it runs at, which a
    /// backend needs anyway to name the LLVM or C type. What the pair is *not*
    /// is a derivation — deciding that `i32 -> i64` sign-extends while
    /// `u32 -> i64` zero-extends is a rule about the source's signedness, and a
    /// backend that re-derived it would be a second copy of that rule, able to
    /// disagree with this one. There are many conversions between two numbers
    /// and the instruction is not recoverable from the destination alone, so it
    /// is written down.
    Cast {
        value: Operand,
        kind: CastKind,
        from: Ty,
        to: Ty,
    },
    /// Build an aggregate out of its parts.
    Aggregate {
        kind: Aggregate,
        fields: Vec<Operand>,
    },
    /// `ptr + index * stride` — **pointer arithmetic**, in bytes per element.
    ///
    /// The source language has none, deliberately: an address you can move is an
    /// address you can move wrongly, and every sequence the language has carries
    /// its own bounds. LIR needs it anyway, because a slice flattened into
    /// `{ ptr, len }` (§7b) and reaching its element `i` is exactly this — the
    /// struct has members, not elements, so the arithmetic has to be somewhere
    /// and this is where.
    Offset {
        ptr: Operand,
        index: Operand,
        /// **Bytes** between one element and the next, tail padding included.
        ///
        /// A number rather than the element type, because every other size in
        /// LIR is one — a `TypeDef`'s layout, a member's offset — and a type
        /// here would send a backend back through the layout engine for an
        /// answer this compiler has already computed.
        stride: u64,
    },
}

/// **Which** conversion a [`Rvalue::Cast`] performs.
///
/// One case per machine instruction, decided here rather than in a backend. The
/// same argument [`Op`] makes: a backend that matches one enum once cannot
/// forget a case, and the alternative — every backend re-deriving the
/// conversion from the type pair — is the same rule written as many times as
/// there are backends, each able to get a corner wrong. The corners are real:
/// an integer widening sign-extends or zero-extends by the **source's**
/// signedness, a float-to-integer rounds toward zero by the **destination's**,
/// and a same-width integer change is no instruction at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastKind {
    /// Integer to a **narrower** integer: keep the low bits (spec §6.5 — a
    /// written `cast` is allowed to lose).
    Truncate,
    /// Integer to a **wider** integer, source unsigned: fill with zeroes. Also
    /// what a `bool` widens by, since a `bool` is one unsigned bit.
    ZeroExtend,
    /// Integer to a **wider** integer, source signed: fill with the sign bit.
    SignExtend,
    /// Float to a narrower float: round to nearest. `3.5e40` to `f32` is `inf`,
    /// which is a value and not a trap.
    FloatTruncate,
    /// Float to a wider float: exact, always.
    FloatExtend,
    /// Float to integer, rounding **toward zero**. `signed` is the
    /// destination's, because that is what decides the instruction.
    FloatToInt { signed: bool },
    /// Integer to float, rounding to nearest. `signed` is the **source's**, for
    /// the same reason.
    IntToFloat { signed: bool },
    /// Two types of the same width, reinterpreted: `i32` to `u32`, `u64` to
    /// `f64`. No instruction on any target — a register is a register — but it
    /// is a case rather than an absence so that a backend handles it
    /// deliberately instead of falling through to one that shifts bits.
    Reinterpret,
    /// A pointer to an integer of pointer width.
    PtrToInt,
    /// An integer of pointer width to a pointer.
    IntToPtr,
    /// A pointer to a pointer. Mutability is erased by this level (§9) and an
    /// address is an address, so this is always an identity a backend folds.
    PtrCast,
    /// A pair this stage has no case for.
    ///
    /// It exists for the same reason [`Intrinsic::Unknown`] does: a conversion
    /// that reaches a backend as "figure it out" is worse than one that fails a
    /// test here. `no_program_contains_an_unknown_cast` says no program holds
    /// one, so this is a bug report and not a fallback.
    Unknown,
}

impl CastKind {
    /// How the dump names it.
    pub fn name(self) -> &'static str {
        match self {
            CastKind::Truncate => "trunc",
            CastKind::ZeroExtend => "zext",
            CastKind::SignExtend => "sext",
            CastKind::FloatTruncate => "fptrunc",
            CastKind::FloatExtend => "fpext",
            CastKind::FloatToInt { signed: true } => "fptosi",
            CastKind::FloatToInt { signed: false } => "fptoui",
            CastKind::IntToFloat { signed: true } => "sitofp",
            CastKind::IntToFloat { signed: false } => "uitofp",
            CastKind::Reinterpret => "reinterpret",
            CastKind::PtrToInt => "ptrtoint",
            CastKind::IntToPtr => "inttoptr",
            CastKind::PtrCast => "ptrcast",
            CastKind::Unknown => "<unknown cast>",
        }
    }

    /// Which conversion takes `from` to `to`.
    ///
    /// The one place the rule lives. It runs at lowering, and the answer is
    /// recorded in the instruction; nothing downstream asks again.
    pub fn of(from: &Ty, to: &Ty) -> CastKind {
        // A `bool` is an unsigned one-bit integer here, which makes
        // `bool -> u8` an ordinary zero-extension rather than its own case.
        let int = |t: &Ty| match t {
            Ty::Int { bits, signed } => Some((*bits, *signed)),
            Ty::Bool => Some((1, false)),
            _ => None,
        };
        match (int(from), int(to)) {
            (Some((fb, fs)), Some((tb, _))) => {
                return match fb.cmp(&tb) {
                    std::cmp::Ordering::Greater => CastKind::Truncate,
                    std::cmp::Ordering::Equal => CastKind::Reinterpret,
                    std::cmp::Ordering::Less if fs => CastKind::SignExtend,
                    std::cmp::Ordering::Less => CastKind::ZeroExtend,
                };
            }
            (Some((_, fs)), None) if matches!(to, Ty::Float { .. }) => {
                return CastKind::IntToFloat { signed: fs };
            }
            (None, Some((_, ts))) if matches!(from, Ty::Float { .. }) => {
                return CastKind::FloatToInt { signed: ts };
            }
            _ => {}
        }
        match (from, to) {
            (Ty::Float { bits: f }, Ty::Float { bits: t }) => match f.cmp(t) {
                std::cmp::Ordering::Greater => CastKind::FloatTruncate,
                std::cmp::Ordering::Less => CastKind::FloatExtend,
                std::cmp::Ordering::Equal => CastKind::Reinterpret,
            },
            // A function pointer is an address too, so it casts like one.
            (Ty::Ptr(_) | Ty::Func { .. }, Ty::Ptr(_) | Ty::Func { .. }) => CastKind::PtrCast,
            (Ty::Ptr(_) | Ty::Func { .. }, Ty::Int { .. }) => CastKind::PtrToInt,
            (Ty::Int { .. }, Ty::Ptr(_) | Ty::Func { .. }) => CastKind::IntToPtr,
            _ => CastKind::Unknown,
        }
    }
}

/// A primitive operation (§6.13).
///
/// The set is closed and every member is an instruction on every target this
/// could reach. Where two targets spell one of them differently — a signed
/// versus an unsigned divide, an integer versus a float add — the difference is
/// in [`Rvalue::Op::ty`], not in a second opcode, because it is the *type* that
/// differs and not the operation.
///
/// Overflow is the exception, and deliberately so: `add` and `add_checked` are
/// two different instructions, not one instruction and a flag. A flag that
/// changes the result type (`T` to `(T, bool)`) is not a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Arithmetic, **wrapping** on integer overflow and IEEE on floats — the
    /// machine instruction, with no undefined case.
    ///
    /// That is a definition rather than a default. `overflow=wrap` emits it,
    /// `#unsafe` emits it, and `wrapping_add` lowers to it, because all three
    /// mean the same instruction and the only alternative would be an opcode
    /// whose meaning depended on a build setting a backend cannot see. A backend
    /// whose target makes signed overflow undefined (C) does the operation at
    /// the unsigned width and converts back, which is what "wrapping" costs
    /// there and nowhere else.
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// Arithmetic that reports: `(T, bool)`, the value and whether it overflowed
    /// (§7d). What `overflow=trap` emits, beside a branch on member `1`.
    AddChecked,
    SubChecked,
    MulChecked,
    Neg,
    /// Bitwise.
    BitAnd,
    BitOr,
    BitXor,
    BitNot,
    Shl,
    Shr,
    /// Logical negation of a `bool`. `&&` and `||` are control flow (§1) and
    /// never reach here.
    Not,
    /// Comparison. The result is a `bool`; `ty` is what was compared.
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Op {
    /// How the dump names it.
    pub fn name(self) -> &'static str {
        match self {
            Op::Add => "add",
            Op::Sub => "sub",
            Op::Mul => "mul",
            Op::Div => "div",
            Op::Rem => "rem",
            Op::AddChecked => "add_checked",
            Op::SubChecked => "sub_checked",
            Op::MulChecked => "mul_checked",
            Op::Neg => "neg",
            Op::BitAnd => "bit_and",
            Op::BitOr => "bit_or",
            Op::BitXor => "bit_xor",
            Op::BitNot => "bit_not",
            Op::Shl => "shl",
            Op::Shr => "shr",
            Op::Not => "not",
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::Lt => "lt",
            Op::Le => "le",
            Op::Gt => "gt",
            Op::Ge => "ge",
        }
    }

    /// How many operands it takes.
    pub fn arity(self) -> usize {
        match self {
            Op::Neg | Op::BitNot | Op::Not => 1,
            _ => 2,
        }
    }

    /// Whether the result is `(T, bool)` rather than `T`.
    pub fn is_checked(self) -> bool {
        matches!(self, Op::AddChecked | Op::SubChecked | Op::MulChecked)
    }

    /// Whether the result is a `bool` regardless of the operand type.
    pub fn is_comparison(self) -> bool {
        matches!(self, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge)
    }
}

/// Which aggregate an [`Rvalue::Aggregate`] builds.
///
/// Two cases, not six. "Build a value of this struct type" is one operation
/// however the source spelled it — a struct literal, a tuple, a slice header, a
/// trait object's fat pointer — and the type says which struct, so the four
/// names for it were four names for one thing. What is left is the array, which
/// does not flatten (§7b), and the variant, which is the one aggregate whose
/// construction needs a value its fields do not carry: the tag.
#[derive(Debug, Clone)]
pub enum Aggregate {
    /// Build a value of this struct type from its members, in order.
    Struct(TypeId),
    /// Build an array from its elements. The element type is the destination's.
    Array,
    /// Build an enum value: `tag` into member 0, the operands into the variant's
    /// own fields inside the payload.
    Variant {
        /// The enum's type.
        ty: TypeId,
        /// The variant's fields, as a struct type (see [`VariantDef::ty`]).
        variant: TypeId,
        index: u32,
        tag: i128,
        name: Symbol,
    },
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
/// `bool`, and a `match` is a switch on a tag (§4). There is no `branch` /
/// `switch` pair, because two forms of the same edge would be two cases in every
/// pass that walks the graph.
#[derive(Debug, Clone)]
pub enum TermKind {
    Goto(BlockId),
    Switch {
        value: Operand,
        /// What the switched value is — the width the arms' numbers are read at.
        ty: Ty,
        /// The tested values and where each goes. Values are integers: a tag, a
        /// `bool` (0 / 1), a `char`'s scalar value.
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

#[cfg(test)]
mod tests;
