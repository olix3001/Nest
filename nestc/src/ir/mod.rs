//! The Nest intermediate representation.
//!
//! The IR is what the AST becomes once name resolution, desugaring, and type
//! inference are done: a **typed, structured, much smaller tree** built by
//! [`super::sema::lower`]. It keeps the AST's structured control flow — `if`,
//! `match`, and a single infinite [`ExprKind::Loop`] with `break` — because the
//! code generator and the analyses that run on the IR want that shape, but it
//! drops everything the earlier stages resolved away:
//!
//! - Surface sugar is gone (`for`, `while`, `.?`/`.!`, compound assignment) —
//!   `for`/`.?`/`.!` were desugared on the AST; `while` becomes a `loop` with a
//!   leading `if !cond { break }` here.
//! - Names are **bound**: every leaf is an [`ExprKind::Local`] /
//!   [`ExprKind::Global`] carrying the [`DefId`] it refers to, never a string
//!   path to re-resolve.
//! - Every node is **typed**; nothing is left to infer. The type is not a field
//!   but a [`Meta`] fact (`meta.ty(node.id)`) — see below.
//! - Auto-deref is **explicit**: a field/index access through a pointer gets an
//!   [`ExprKind::Deref`] inserted.
//! - `defer` is a **statement, where it was written**: [`StmtKind::Defer`] holds
//!   the body once, at the point control registers it. Every exit from the block
//!   *below* that point runs it, in reverse of registration, which the CFG stage
//!   emits as one epilogue per scope and registration count. Hoisting the bodies
//!   to the block would lose the position, and a `defer` control never reached
//!   would run (spec §8.4).
//! - implicit coercions are **explicit**: an `@using` upcast is the field access
//!   it stands for, and a `*T` → `*dyn Trait` unsizing is an
//!   [`ExprKind::DynCast`] carrying the erased pointee.
//!
//! Three things the surface language hides behind one syntax become one node
//! with a tag, so a consumer that does not care about the distinction can ignore
//! it and one that does gets the answer in O(1):
//!
//! - **Every call is [`ExprKind::Call`]** — a free call, an operator, and a
//!   method call alike. A method's receiver is `args[0]`, already adjusted (the
//!   `&` / `&mut` / `.*` the call site implied is written out), and
//!   [`Dispatch`] says how the callee is reached: directly, through a trait
//!   object's vtable, or through a bound that monomorphization will resolve.
//! - **Every operator is a call too** (§6.13), with [`BuiltinOp`] marking the
//!   ones that are machine instructions. Indexing included: there is no `Index`
//!   node, because `a[i]` is `Index.index(&a, i).*` for a `[]T` exactly as it is
//!   for a user container — `core` supplies the impl and its member is
//!   `#intrinsic`, so the call *is* the address computation. `&&` / `||`, `!`, and comparisons on
//!   the numeric core are the exceptions: they dispatch on nothing and stay
//!   [`ExprKind::Binary`] / [`ExprKind::Unary`].
//! - **Pointers keep their permission in the type**: `*T` and `*mut T` are one
//!   [`Ty::Ptr`] with a `mutable` flag, and [`Function::recv`] /
//!   [`Function::mutating`] say what a callee may write through.
//!
//! # Node identity and metadata
//!
//! Every node — [`Function`], [`Param`], [`Block`], [`Stmt`], [`Expr`], [`Arm`],
//! [`Pattern`], [`Binding`] — carries an [`IrId`], and per-node facts live in a
//! type-indexed [`Meta`] side table keyed by it, exactly as the AST keys its own
//! store by `NodeId`. See [`meta`] for why identity is a field here rather than
//! an arena slot, and why ids are unique across the whole compilation rather
//! than per [`Program`].
//!
//! The dividing line: **a node holds what is particular to its own shape;
//! anything that recurs across shapes is metadata.** A `Call`'s [`Dispatch`] and
//! a `Field`'s name belong to those nodes and nowhere else, so they are fields.
//! A **span**, a **type** and a node's **directives** are the same fact on an
//! expression, a block, a parameter, a type and a function alike, so they are
//! metadata — a field per node type would be several places to keep in step,
//! and "what is this node's type" would be a different question depending on
//! which node you were holding.
//! Every pass that computes a new per-node fact — layout, const-safety, escape,
//! liveness — adds it the same way, without touching these definitions.
//!
//! What is deliberately *not* done here — each a documented next layer:
//! generic monomorphization, `match` exhaustiveness and decision trees (arms
//! stay structured, and [`Pattern`] keeps every form's full shape for the pass
//! that builds them), vtable layout and object-safety checking for
//! [`Dispatch::Virtual`], and the mutability check that reads the pointer
//! permissions above. Closures are the one *expression* with no IR yet: they
//! need a captured environment, which is a representation decision this layer
//! does not make.
//!
//! Traversal is via the [`Visitor`] / [`VisitorMut`] traits, whose default
//! methods walk every child so an implementation overrides only the nodes it
//! cares about.

use crate::common::symbol::Symbol;
use crate::parser::ast::{BinOp, Lit, UnOp};

use crate::sema::def::DefId;
use crate::sema::ty::Ty;

pub use crate::sema::builtins::BuiltinOp;

pub mod check;
pub mod const_eval;
pub mod layout;
pub mod link;
pub mod meta;
pub mod mono;
pub mod pretty;

pub use const_eval::ConstValue;
pub use link::{Linked, link};
pub use meta::{IrId, Meta};

/// A parameter's lowered **default** (§5.2), attached to the [`Param`] as
/// metadata.
///
/// The default belongs to the parameter, not to any call. It is lowered once —
/// against its own declaration, in its own file — and the finished expression is
/// *cloned* into each call site that omits it, so by the time a call reaches the
/// IR its filled-in arguments are indistinguishable from written ones. Keeping
/// the original here is what lets a pass talk about the default itself: the
/// `#const` check has to reject `y: i32 := read_config()` **once, at the
/// declaration**, including for a function nothing calls yet.
///
/// Note the clones share their [`IrId`]s with the original, which is right for
/// everything the side table holds about a default: its span, its type and its
/// const-safety are properties of the one expression that was written, not of
/// the places it was pasted.
#[derive(Debug, Clone)]
pub struct DefaultValue(pub Expr);

/// Marks a `$cast` the **compiler** inserted, rather than one the program wrote
/// (§1.5, §2.5). Set by lowering on the node it builds for a
/// [`Coercion`](crate::sema::infer::Coercion); read by the const evaluator.
///
/// The two are the same operation and different *promises*. A written
/// `$cast.<u8>(x)` is the program saying "narrow this, I know what that costs" —
/// it may lose precision, exactly as it does at run time. An inserted one is the
/// compiler settling an untyped literal on the type its use site asked for, and
/// that conversion has to be **exact**: nothing in the source said `300` should
/// become `44`, so a literal that does not fit is a mistake rather than a
/// truncation.
///
/// It is a side-table fact rather than a distinct [`ExprKind`] because the
/// distinction is spent by the time the check has run: to everything downstream
/// of the const evaluator a cast is a cast.
#[derive(Debug, Clone, Copy)]
pub struct ImplicitCast;

/// A whole lowered program: the type definitions it declares, the constants and
/// static regions it declares, and every function that had a body.
#[derive(Debug, Clone)]
pub struct Program {
    pub types: Vec<TypeDef>,
    pub globals: Vec<Global>,
    pub funcs: Vec<Function>,
}

/// One namespace- or impl-scope value definition: a constant `A :: 5`, or the
/// program-lifetime region `#static count: u32 := 0` (§2.6).
///
/// These reach the IR for the same reason [`TypeDef`]s do: a use of one lowers
/// to an [`ExprKind::Global`], which is only a *name*. Everything downstream
/// needs the value behind it — the const evaluator has to run the initializer,
/// and codegen has to emit an initialized `.data` region or fold the constant
/// into its uses — and reaching back into the AST for the initializer would mean
/// re-deriving a lowering that has already been done once.
///
/// The type is `meta.ty(id)`; the span and the directives are read the same way
/// as on every other node. Its evaluated value, once the const evaluator has
/// run, is `meta.get::<ConstValue>(id)`.
#[derive(Debug, Clone)]
pub struct Global {
    pub id: IrId,
    pub def: DefId,
    pub name: Symbol,
    /// The lowered initializer.
    ///
    /// `None` only for a `#static` with no `:=`, whose region is zeroed — the
    /// one binding form in the language that needs no initializer, because
    /// static storage is zeroed anyway (§2.6). A constant always has one.
    pub init: Option<Expr>,
    /// Whether this names a **mutable region** (`#static`) rather than a
    /// constant. The two are one node because they are one declaration form and
    /// every consumer wants both: the const evaluator runs either initializer,
    /// and only the flag decides whether the result is a value to fold into use
    /// sites or the initial contents of a region that code may then write.
    pub mutable: bool,
}

/// One lowered type definition.
///
/// The IR carries these because a [`Ty::Nominal`] is only a *name*: it says
/// which type, never what is in it. Everything downstream needs the contents —
/// layout needs field types and order, exhaustiveness needs an enum's variants,
/// LIR needs all of it to flatten aggregates into plain structs — and reaching
/// back into the [`DefTable`](crate::sema::def::DefTable) and the AST for it
/// would mean every one of those passes re-deriving what one pass already
/// worked out.
///
/// The type this defines is `meta.ty(id)`, the same way every other node's type
/// is read: `Ty::Nominal { def, args }` over its own generic parameters. Member
/// types are `meta.ty(member.id)`, and are **definition-relative** — a field of
/// `Pair.<T>` declared `T` is the type parameter, not anything a use site
/// substituted, because substituting is monomorphization's job.
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub id: IrId,
    pub def: DefId,
    pub name: Symbol,
    pub kind: TypeDefKind,
}

/// What a [`TypeDef`] defines.
#[derive(Debug, Clone)]
pub enum TypeDefKind {
    /// `struct { a: T, b: U }`, and a tuple struct alike — a tuple struct's
    /// members are named by position (`"0"`, `"1"`, …), which is what they
    /// already are everywhere else in the IR (§3.3).
    Struct { members: Vec<Member> },
    /// `enum { a, b(T), c { x: U } }`.
    Enum { variants: Vec<Variant> },
    /// `distinct T`. The representation is the single [`Member`]: a `distinct`
    /// is structurally a newtype over one thing, so giving it the same shape a
    /// one-field struct has means the passes that flatten aggregates need no
    /// special case for it.
    Distinct { repr: Member },
    /// `trait { ... }`. A trait is not a runtime type, but its **members in
    /// declaration order** are the layout of every vtable built for it, so they
    /// have to survive to the stage that builds one.
    Trait {
        methods: Vec<TraitMethod>,
        /// Associated constants (`MAX :: i32`), in declaration order. They are
        /// what an impl must supply and what makes a trait *not* object-safe —
        /// a vtable holds code, not values.
        consts: Vec<AssocConst>,
    },
}

/// One associated constant a trait declares: `MAX :: i32 [:= 100]`.
///
/// Its declared type is `meta.ty(id)`; the default the trait supplies, if any,
/// is `meta.get::<DefaultValue>(id)` — the same place a parameter's default
/// lives, for the same reason. An impl may omit a constant exactly when the
/// trait gave it one, which is how a default method body already works.
#[derive(Debug, Clone)]
pub struct AssocConst {
    pub id: IrId,
    pub def: DefId,
    pub name: Symbol,
}

/// One method a trait declares: a vtable slot.
///
/// Its signature is `meta.ty(id)`, a [`Ty::Func`] in which `Self` appears as the
/// trait's own [`Ty::Nominal`] — which is how "takes or returns `Self` by value"
/// is recognized without any name matching.
#[derive(Debug, Clone)]
pub struct TraitMethod {
    pub id: IrId,
    pub def: DefId,
    pub name: Symbol,
    /// How the method takes `self` (§3.4). [`Recv::None`] means it declares no
    /// receiver at all, which is what makes it un-callable through a trait
    /// object.
    pub recv: Recv,
    /// Whether the method declares generic parameters of its own. One vtable
    /// slot cannot stand for an unbounded family of instantiations.
    pub generic: bool,
    /// Whether the trait itself supplies a body — a default method. It still
    /// occupies a slot; it only changes what fills it when an impl is silent.
    pub has_default: bool,
}

/// One member of a struct, of a variant's payload, or the representation of a
/// `distinct` type. Its type is `meta.ty(id)`.
#[derive(Debug, Clone)]
pub struct Member {
    pub id: IrId,
    /// The member's own definition, or `None` where the front end gives it none
    /// — a variant's payload elements and a `distinct`'s representation are
    /// positions in a declaration rather than named items.
    pub def: Option<DefId>,
    /// The member's name; a positional member is named by its index.
    pub name: Symbol,
}

/// One `enum` variant.
#[derive(Debug, Clone)]
pub struct Variant {
    pub id: IrId,
    pub def: DefId,
    pub name: Symbol,
    /// The payload, empty for a bare variant. A tuple payload's members are
    /// named by position, exactly as a tuple struct's are.
    pub members: Vec<Member>,
    /// Whether the payload was written positionally (`.b(T)`) rather than as a
    /// record (`.c { x: U }`). It does not affect layout — it is what a pattern
    /// and a dump need in order to print the variant the way it was written.
    pub tuple: bool,
}

/// One lowered function.
#[derive(Debug, Clone)]
pub struct Function {
    pub id: IrId,
    /// The function's definition id (its canonical name lives in the def table).
    pub def: DefId,
    pub name: Symbol,
    pub params: Vec<Param>,
    /// The lowered body, or `None` for a **declaration** — an
    /// `extern("c") func` with no body, or a trait method that only states a
    /// signature. A declaration is still a [`Function`] because a call to it is
    /// an ordinary call and codegen still needs its signature; what it has no
    /// code of its own.
    pub body: Option<Block>,
    /// The ABI of an `extern("c") func`, or `None` for a Nest function. Present
    /// on definitions too: `extern("c") func f() { … }` is a Nest body that is
    /// *emitted* with the C ABI and calling convention (§11.3).
    pub extern_abi: Option<Symbol>,
    /// How this function takes its receiver, if it is a method (§3.4). This is
    /// what a vtable slot needs to know about a `dyn` call and what the later
    /// mutability check reads to decide whether `x.m()` requires a mutable `x`.
    pub recv: Recv,
    /// Whether the function can write through **any** of its parameters — one
    /// of them is a `*mut T` or a `[]mut T` (transitively, through a pointer or
    /// slice element). A `false` here is the strong statement: nothing the
    /// caller lent this function can come back changed.
    ///
    /// It is deliberately a *syntactic* fact about the signature, not a summary
    /// of the body: the IR-level mutability check needs the permission the type
    /// grants, and computing what the body really touches is an analysis that
    /// runs on the CFG.
    pub mutating: bool,
}

/// How a function takes its `self` (§3.4). Recorded per [`Function`] because
/// the receiver's shape — value, pointer, mutable pointer — is what decides
/// whether a call site must own or may only borrow, and what a vtable slot's
/// first argument is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recv {
    /// Not a method: no `self` parameter.
    None,
    /// `self: Self` — the receiver is passed by value.
    Value,
    /// `self: *Self` — a read-only borrow.
    Ptr,
    /// `self: *mut Self` — a mutable borrow; this is the "mutating method".
    MutPtr,
}

impl Recv {
    /// Whether a call through this receiver can modify the callee's `self`.
    pub fn mutates(self) -> bool {
        matches!(self, Recv::MutPtr)
    }
}

/// How a [`ExprKind::Call`] finds the code it runs.
///
/// Every call is one `ExprKind::Call`; this tag is the *only* thing that
/// separates a direct jump from a vtable load, so a consumer that does not care
/// about dispatch can ignore it entirely. Neither non-static form is resolved
/// here on purpose: picking the vtable slot (and generating the vtable) belongs
/// to the IR → LIR lowering, and picking the impl for a generic belongs to
/// monomorphization. What this stage owes them is the *inputs* to those choices,
/// which is exactly what each variant carries.
#[derive(Debug, Clone)]
pub enum Dispatch {
    /// A direct call: [`callee`](ExprKind::Call::callee) is the function itself
    /// — a [`ExprKind::Global`] naming it, or any expression of function type.
    Static,
    /// A **virtual** call through a trait object's vtable. The receiver
    /// (`args[0]`) is the `*dyn Trait` fat pointer, `trait_def` is that trait,
    /// and `method` is the trait's own declaration the call selected. The pair
    /// names the slot; which concrete function sits in it is a property of the
    /// vtable the LIR builds, not of this call.
    Virtual { trait_def: DefId, method: DefId },
    /// A call on a **type parameter's** bound — `<T: Summing>` makes `t.total()`
    /// mean `Summing.total` with no impl chosen yet. `self_ty` is the receiver
    /// type as inference left it (a rigid type parameter, or something built
    /// over one); monomorphization substitutes it and re-selects, turning this
    /// into a [`Dispatch::Static`] call.
    Generic {
        trait_def: DefId,
        method: DefId,
        self_ty: Ty,
        /// The trait's own generic arguments, **as the bound wrote them**:
        /// `<T: Add.<f64>>` gives `[f64]`.
        ///
        /// The trait alone does not name an impl. `impl Add.<f64> for Vec3` and
        /// `impl Add.<i32> for Vec3` do not overlap (§4.9) and both apply to
        /// `Vec3`, so the arguments are the only thing that says which one this
        /// call meant. They are in the *declaration's* terms, like `self_ty`,
        /// and are substituted with it.
        trait_args: Vec<Ty>,
    },
}

/// A bound function parameter.
#[derive(Debug, Clone)]
pub struct Param {
    pub id: IrId,
    pub def: DefId,
    pub name: Symbol,
}

/// A sequence of statements and an optional trailing value expression. The
/// block's type — its tail's, or `void` — is `meta.ty(block.id)`.
#[derive(Debug, Clone)]
pub struct Block {
    pub id: IrId,
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
}

/// A statement: an effect with no value contribution to its block.
#[derive(Debug, Clone)]
pub struct Stmt {
    pub id: IrId,
    pub kind: StmtKind,
}

/// What a [`Stmt`] does.
#[derive(Debug, Clone)]
pub enum StmtKind {
    /// A `let` / `const` binding. The bound name is a [`PatternKind::Binding`]
    /// in the common case; a destructuring `let (a, b) := p` keeps its whole
    /// pattern here, so the bindings it introduces survive into the IR instead
    /// of collapsing into an expression evaluated for effect.
    ///
    /// A `let` pattern is irrefutable, so unlike a `match` arm it never needs a
    /// fallback. The type the pattern is matched against is the initializer's,
    /// `meta.ty(init.id)` — it was a separate field once and was always a copy
    /// of exactly that.
    Let { pattern: Pattern, init: Expr },
    /// A place assignment (`place = value`); compound forms were desugared.
    Assign { place: Expr, value: Expr },
    /// An expression evaluated for effect.
    Expr(Expr),
    /// `return [value]`. Running the [`Defer`](StmtKind::Defer) bodies already
    /// registered in this block and in every enclosing one (innermost first) is
    /// the CFG stage's job — they are not spliced in here.
    Return(Option<Expr>),
    /// `break [value]` out of the enclosing `loop`.
    Break(Option<Expr>),
    /// `continue` the enclosing `loop`.
    Continue,
    /// `defer <body>`: register `body` to run on every exit from the enclosing
    /// block that happens **after** this statement. The body is held here, at
    /// the position control registers it, because "a `defer` never reached does
    /// not run" (spec §8.4) is a fact about that position.
    Defer(Expr),
}

/// One `match` arm; patterns stay structured (no decision tree yet).
#[derive(Debug, Clone)]
pub struct Arm {
    pub id: IrId,
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
}

/// A (simplified) pattern. Field/slice-rest details the AST carried are dropped;
/// what remains is enough for a later decision-tree pass and for binding.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub id: IrId,
    pub kind: PatternKind,
}

/// What a [`Pattern`] tests and binds.
#[derive(Debug, Clone)]
pub enum PatternKind {
    /// `_`, and any pattern that binds and tests nothing.
    Wildcard,
    /// A name binding.
    Binding { def: DefId, name: Symbol },
    /// A scalar literal pattern.
    Lit(Lit),
    /// `.variant(sub...)` — an enum-variant pattern.
    Variant { name: Symbol, sub: Vec<Pattern> },
    /// `(a, b, ...)`.
    Tuple(Vec<Pattern>),
    /// `a | b | ...`.
    Or(Vec<Pattern>),
    /// `[Type] { field: p, ... [, ..] }` — a struct pattern. `def` is the named
    /// struct, or `None` for the inferred `.{ ... }` form. `rest` records a
    /// trailing `..`, so the untested fields stay distinguishable from an
    /// exhaustive listing.
    Struct {
        def: Option<DefId>,
        fields: Vec<(Symbol, Pattern)>,
        rest: bool,
    },
    /// `Type(p, q [, ..])` — a tuple-struct pattern.
    TupleStruct {
        def: Option<DefId>,
        elems: Vec<Pattern>,
        rest: bool,
    },
    /// `[a, b, .. [name], y, z]` — a slice pattern. The `..` splits the tested
    /// elements into a `prefix` matched from the front and a `suffix` matched
    /// from the back; without one, everything is `prefix` and `rest` is `None`.
    Slice {
        prefix: Vec<Pattern>,
        /// The `..` segment: present when the pattern has one, carrying the
        /// binding for the skipped middle if it named one.
        rest: Option<Option<Binding>>,
        suffix: Vec<Pattern>,
    },
    /// `a..<b` / `..=b` — a literal range pattern. `inclusive` distinguishes
    /// `..=` from `..<`; an absent bound is unbounded on that side.
    Range {
        start: Option<Lit>,
        end: Option<Lit>,
        inclusive: bool,
    },
    /// `name @ pattern` — bind the whole value *and* keep testing it.
    At {
        binding: Binding,
        pattern: Box<Pattern>,
    },
    /// `&pattern` — match through a reference.
    Deref(Box<Pattern>),
}

/// A name a pattern binds, and the definition it introduces.
#[derive(Debug, Clone)]
pub struct Binding {
    pub id: IrId,
    pub def: DefId,
    pub name: Symbol,
}

/// An expression: its identity, and what it does. Its type is `meta.ty(id)`.
#[derive(Debug, Clone)]
pub struct Expr {
    pub id: IrId,
    pub kind: ExprKind,
}

/// What an [`Expr`] computes.
#[derive(Debug, Clone)]
pub enum ExprKind {
    /// A scalar literal.
    Lit(Lit),
    /// A reference to a local or parameter.
    Local(DefId),
    /// A reference to a top-level item (function / const / type used as a value).
    Global(DefId),
    /// A `<const N: usize>` generic parameter used as a value. It has no
    /// storage: monomorphization replaces it with the literal the instantiation
    /// chose, which is why it cannot be a [`ExprKind::Global`].
    ConstParam(DefId),
    /// `callee(args...)`.
    ///
    /// Operators lower to a `Call` too (§6: "int+int and Vec3+Vec3 are the same
    /// construct"), so both a primitive `i32 + i32` and a user `impl Add for
    /// Vec3` reach codegen as a call to the trait method they resolved to. The
    /// [`builtin`](ExprKind::Call::builtin) tag lets codegen recognize a
    /// primitive intrinsic op in **O(1)** — when it is `Some`, the call *is* the
    /// machine instruction and needs no function lookup; when `None`, it is an
    /// ordinary user call.
    /// A method call is this same node: the receiver is `args[0]` (adjusted to
    /// what the `self` parameter wants — lowering inserts the `&` or the `.*`),
    /// and [`dispatch`](ExprKind::Call::dispatch) says whether the callee is
    /// reached directly, through a vtable, or through a bound awaiting
    /// monomorphization.
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        /// `Some` iff this call is a builtin primitive operator (see
        /// [`crate::sema::builtins`]); codegen emits it inline. `None` for a
        /// normal function/method call.
        builtin: Option<BuiltinOp>,
        /// How the callee is reached (see [`Dispatch`]).
        dispatch: Dispatch,
    },
    /// A primitive binary operation (numeric / boolean core). Operator-trait
    /// dispatch is a later pass.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// A primitive prefix unary operation.
    Unary { op: UnOp, operand: Box<Expr> },
    /// `&place` / `&mut place`.
    Ref { mutable: bool, place: Box<Expr> },
    /// `base.*` — an **explicit** pointer dereference (inserted by lowering for
    /// auto-deref sites too).
    Deref { base: Box<Expr> },
    /// `base.name` — a struct field access (base is a value, never a pointer:
    /// lowering inserts a [`ExprKind::Deref`] first).
    Field {
        base: Box<Expr>,
        name: Symbol,
        /// The field this names, bound by [`crate::sema::fields`]. `None` only
        /// when the base type was already in error.
        def: Option<DefId>,
    },
    /// `base.N` — tuple element access.
    TupleIndex { base: Box<Expr>, index: u64 },
    /// `(a, b, ...)`.
    Tuple { elems: Vec<Expr> },
    /// A nested block expression.
    Block(Block),
    /// `if cond { then } else { els }`.
    If {
        cond: Box<Expr>,
        then: Block,
        els: Option<Block>,
    },
    /// `match scrutinee { arms }`.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    /// An infinite loop; exits only through a `break`.
    Loop { body: Block },
    /// A struct / record construction `Type { field: value, ... }`.
    Construct {
        def: DefId,
        fields: Vec<(Symbol, Expr)>,
    },
    /// An enum-variant value `.variant(args...)`.
    Variant { name: Symbol, args: Vec<Expr> },
    /// A compiler `$`-intrinsic call.
    Intrinsic { name: Symbol, args: Vec<Expr> },
    /// `*T` unsized to `*dyn Trait` — a fat pointer pairing `value` with `T`'s
    /// vtable for the trait the expression's type names. `concrete` is the
    /// erased pointee, kept because picking the vtable is exactly what the
    /// coerced type can no longer say.
    DynCast { value: Box<Expr>, concrete: Ty },
    /// A placeholder for an expression that could not be lowered (an error was
    /// already reported); its type is usually the error type.
    Error,
}

// ===< Visitors >===

/// A read-only IR walk. Every method defaults to visiting children, so an
/// implementor overrides only what it needs and calls the `walk_*` free
/// functions to recurse.
#[allow(unused_variables)]
pub trait Visitor: Sized {
    fn visit_function(&mut self, func: &Function) {
        walk_function(self, func);
    }
    fn visit_block(&mut self, block: &Block) {
        walk_block(self, block);
    }
    fn visit_stmt(&mut self, stmt: &Stmt) {
        walk_stmt(self, stmt);
    }
    fn visit_expr(&mut self, expr: &Expr) {
        walk_expr(self, expr);
    }
    fn visit_arm(&mut self, arm: &Arm) {
        walk_arm(self, arm);
    }
    fn visit_pattern(&mut self, pattern: &Pattern) {
        walk_pattern(self, pattern);
    }
}

pub fn walk_function<V: Visitor>(v: &mut V, func: &Function) {
    if let Some(body) = &func.body {
        v.visit_block(body);
    }
}

/// The `defer` bodies a block registers, in written order.
///
/// Everything about a `defer` is about *when* it runs, and a consumer that cares
/// walks the statements and meets [`StmtKind::Defer`] at the point that
/// registers it. This is for the ones that only care *that* a body is in the
/// program — a check over every expression, say — and want it in the order the
/// block ends up running them in reverse of.
pub fn defer_bodies(block: &Block) -> impl Iterator<Item = &Expr> {
    block.stmts.iter().filter_map(|s| match &s.kind {
        StmtKind::Defer(e) => Some(e),
        _ => None,
    })
}

pub fn walk_block<V: Visitor>(v: &mut V, block: &Block) {
    for s in &block.stmts {
        v.visit_stmt(s);
    }
    if let Some(t) = &block.tail {
        v.visit_expr(t);
    }
}

pub fn walk_stmt<V: Visitor>(v: &mut V, stmt: &Stmt) {
    match &stmt.kind {
        StmtKind::Let { pattern, init, .. } => {
            v.visit_pattern(pattern);
            v.visit_expr(init);
        }
        StmtKind::Assign { place, value } => {
            v.visit_expr(place);
            v.visit_expr(value);
        }
        StmtKind::Expr(e) | StmtKind::Defer(e) => v.visit_expr(e),
        StmtKind::Return(e) | StmtKind::Break(e) => {
            if let Some(e) = e {
                v.visit_expr(e);
            }
        }
        StmtKind::Continue => {}
    }
}

pub fn walk_expr<V: Visitor>(v: &mut V, expr: &Expr) {
    match &expr.kind {
        ExprKind::Lit(_)
        | ExprKind::Local(_)
        | ExprKind::Global(_)
        | ExprKind::ConstParam(_)
        | ExprKind::Error => {}
        ExprKind::Call { callee, args, .. } => {
            v.visit_expr(callee);
            for a in args {
                v.visit_expr(a);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            v.visit_expr(lhs);
            v.visit_expr(rhs);
        }
        ExprKind::Unary { operand, .. } => v.visit_expr(operand),
        ExprKind::Ref { place, .. } => v.visit_expr(place),
        ExprKind::Deref { base }
        | ExprKind::Field { base, .. }
        | ExprKind::TupleIndex { base, .. } => v.visit_expr(base),
        ExprKind::Tuple { elems } => {
            for e in elems {
                v.visit_expr(e);
            }
        }
        ExprKind::Block(b) => v.visit_block(b),
        ExprKind::If { cond, then, els } => {
            v.visit_expr(cond);
            v.visit_block(then);
            if let Some(e) = els {
                v.visit_block(e);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            v.visit_expr(scrutinee);
            for a in arms {
                v.visit_arm(a);
            }
        }
        ExprKind::Loop { body } => v.visit_block(body),
        ExprKind::Construct { fields, .. } => {
            for (_, e) in fields {
                v.visit_expr(e);
            }
        }
        ExprKind::Variant { args, .. } | ExprKind::Intrinsic { args, .. } => {
            for a in args {
                v.visit_expr(a);
            }
        }
        ExprKind::DynCast { value, .. } => v.visit_expr(value),
    }
}

pub fn walk_arm<V: Visitor>(v: &mut V, arm: &Arm) {
    v.visit_pattern(&arm.pattern);
    if let Some(g) = &arm.guard {
        v.visit_expr(g);
    }
    v.visit_expr(&arm.body);
}

pub fn walk_pattern<V: Visitor>(v: &mut V, pattern: &Pattern) {
    match &pattern.kind {
        PatternKind::Wildcard
        | PatternKind::Binding { .. }
        | PatternKind::Lit(_)
        | PatternKind::Range { .. } => {}
        PatternKind::Variant { sub: ps, .. }
        | PatternKind::Tuple(ps)
        | PatternKind::Or(ps)
        | PatternKind::TupleStruct { elems: ps, .. } => {
            for p in ps {
                v.visit_pattern(p);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for (_, p) in fields {
                v.visit_pattern(p);
            }
        }
        PatternKind::Slice { prefix, suffix, .. } => {
            for p in prefix.iter().chain(suffix) {
                v.visit_pattern(p);
            }
        }
        PatternKind::At { pattern, .. } | PatternKind::Deref(pattern) => v.visit_pattern(pattern),
    }
}

/// A mutating IR walk, mirroring [`Visitor`].
#[allow(unused_variables)]
pub trait VisitorMut: Sized {
    fn visit_function(&mut self, func: &mut Function) {
        walk_function_mut(self, func);
    }
    fn visit_block(&mut self, block: &mut Block) {
        walk_block_mut(self, block);
    }
    fn visit_stmt(&mut self, stmt: &mut Stmt) {
        walk_stmt_mut(self, stmt);
    }
    fn visit_expr(&mut self, expr: &mut Expr) {
        walk_expr_mut(self, expr);
    }
    fn visit_arm(&mut self, arm: &mut Arm) {
        walk_arm_mut(self, arm);
    }
    fn visit_pattern(&mut self, pattern: &mut Pattern) {
        walk_pattern_mut(self, pattern);
    }
}

pub fn walk_function_mut<V: VisitorMut>(v: &mut V, func: &mut Function) {
    if let Some(body) = &mut func.body {
        v.visit_block(body);
    }
}

pub fn walk_block_mut<V: VisitorMut>(v: &mut V, block: &mut Block) {
    for s in &mut block.stmts {
        v.visit_stmt(s);
    }
    if let Some(t) = &mut block.tail {
        v.visit_expr(t);
    }
}

pub fn walk_stmt_mut<V: VisitorMut>(v: &mut V, stmt: &mut Stmt) {
    match &mut stmt.kind {
        StmtKind::Let { pattern, init, .. } => {
            v.visit_pattern(pattern);
            v.visit_expr(init);
        }
        StmtKind::Assign { place, value } => {
            v.visit_expr(place);
            v.visit_expr(value);
        }
        StmtKind::Expr(e) | StmtKind::Defer(e) => v.visit_expr(e),
        StmtKind::Return(e) | StmtKind::Break(e) => {
            if let Some(e) = e {
                v.visit_expr(e);
            }
        }
        StmtKind::Continue => {}
    }
}

pub fn walk_expr_mut<V: VisitorMut>(v: &mut V, expr: &mut Expr) {
    match &mut expr.kind {
        ExprKind::Lit(_)
        | ExprKind::Local(_)
        | ExprKind::Global(_)
        | ExprKind::ConstParam(_)
        | ExprKind::Error => {}
        ExprKind::Call { callee, args, .. } => {
            v.visit_expr(callee);
            for a in args {
                v.visit_expr(a);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            v.visit_expr(lhs);
            v.visit_expr(rhs);
        }
        ExprKind::Unary { operand, .. } => v.visit_expr(operand),
        ExprKind::Ref { place, .. } => v.visit_expr(place),
        ExprKind::Deref { base }
        | ExprKind::Field { base, .. }
        | ExprKind::TupleIndex { base, .. } => v.visit_expr(base),
        ExprKind::Tuple { elems } => {
            for e in elems {
                v.visit_expr(e);
            }
        }
        ExprKind::Block(b) => v.visit_block(b),
        ExprKind::If { cond, then, els } => {
            v.visit_expr(cond);
            v.visit_block(then);
            if let Some(e) = els {
                v.visit_block(e);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            v.visit_expr(scrutinee);
            for a in arms {
                v.visit_arm(a);
            }
        }
        ExprKind::Loop { body } => v.visit_block(body),
        ExprKind::Construct { fields, .. } => {
            for (_, e) in fields {
                v.visit_expr(e);
            }
        }
        ExprKind::Variant { args, .. } | ExprKind::Intrinsic { args, .. } => {
            for a in args {
                v.visit_expr(a);
            }
        }
        ExprKind::DynCast { value, .. } => v.visit_expr(value),
    }
}

pub fn walk_arm_mut<V: VisitorMut>(v: &mut V, arm: &mut Arm) {
    v.visit_pattern(&mut arm.pattern);
    if let Some(g) = &mut arm.guard {
        v.visit_expr(g);
    }
    v.visit_expr(&mut arm.body);
}

pub fn walk_pattern_mut<V: VisitorMut>(v: &mut V, pattern: &mut Pattern) {
    match &mut pattern.kind {
        PatternKind::Wildcard
        | PatternKind::Binding { .. }
        | PatternKind::Lit(_)
        | PatternKind::Range { .. } => {}
        PatternKind::Variant { sub: ps, .. }
        | PatternKind::Tuple(ps)
        | PatternKind::Or(ps)
        | PatternKind::TupleStruct { elems: ps, .. } => {
            for p in ps {
                v.visit_pattern(p);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for (_, p) in fields {
                v.visit_pattern(p);
            }
        }
        PatternKind::Slice { prefix, suffix, .. } => {
            for p in prefix.iter_mut().chain(suffix) {
                v.visit_pattern(p);
            }
        }
        PatternKind::At { pattern, .. } | PatternKind::Deref(pattern) => v.visit_pattern(pattern),
    }
}
