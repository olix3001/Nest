//! Abstract syntax tree for the Nest language.
//!
//! # Shape
//!
//! The tree is stored as a flat **arena** ([`Ast`]): every node lives in one
//! `Vec`, and a node refers to its children only by [`NodeId`] (a transparent
//! `usize` index). Nothing owns anything else structurally, so the tree is a
//! plain data table that is trivial to `Clone`, `serde`-serialize, and walk in
//! either direction.
//!
//! Each slot is a [`RefCell<Node>`]. That interior mutability is what lets a
//! [`MutVisitor`](super::visitor::MutVisitor) borrow the arena shared (`&Ast`)
//! and still rewrite one node at a time while recursing — no `&mut Ast`
//! threaded through the whole walk. Read-only traversal just uses
//! [`Ast::node`].
//!
//! Every node carries its source [`Span`] and the [`FileId`] it came from; the
//! latter matters because `import` pulls nodes in from other files, so a span
//! alone is ambiguous.

use std::any::Any;
use std::cell::{Ref, RefCell, RefMut};

use num_bigint::BigInt;
use serde::{Deserialize, Serialize};

use crate::common::meta::MetaStore;
use crate::common::span::Span;
use crate::common::symbol::Symbol;

/// Index of a [`Node`] inside an [`Ast`] arena. Cheap, `Copy`, transparent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub usize);

/// Identifies the source file a node originates from. Defined in
/// [`crate::common::source`] (alongside the [`SourceMap`] that assigns it) and
/// re-exported here for the parser's convenience.
///
/// [`SourceMap`]: crate::common::source::SourceMap
pub use crate::common::source::FileId;

// ===< Leaf payloads (embedded by value, never allocated as nodes) >===

/// A scalar literal value.
/// The `..` segment of a slice pattern: how many element patterns precede it
/// (the rest are the suffix), and the name it binds, if any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SliceRest {
    /// Index into `elems` the `..` sits at: `elems[..at]` is the prefix and
    /// `elems[at..]` the suffix.
    pub at: usize,
    /// `.. name` binds the skipped middle; a bare `..` discards it.
    pub name: Option<Symbol>,
}

/// Marks a float-literal node whose source text needs more than an `f64`: the
/// `comptime_float` → `f64` collapse would lose it, so it types only as an
/// explicit `f80` / `f128`. Attached by the parser, enforced by inference.
#[derive(Debug, Clone, Copy)]
pub struct WideFloat;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Lit {
    /// An integer literal, held at **arbitrary precision**: a literal is a
    /// `comptime_int` and keeps its exact value until it is cast to a runtime
    /// integer type, so nothing may truncate it on the way in.
    Int(BigInt),
    Float(f64),
    Str(String),
    Char(char),
    Bool(bool),
}

/// Binary operators, in every precedence band the grammar defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinOp {
    // logical (short-circuit): `&&`/`and`, `||`/`or`
    And,
    Or,
    // comparison (non-associating)
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    // bitwise
    BitOr,
    BitXor,
    BitAnd,
    Shl,
    Shr,
    // arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// Prefix unary operators. Note deref is postfix (`.*`), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnOp {
    /// `&expr` — address-of.
    Ref,
    /// `&mut expr` — mutable address-of.
    RefMut,
    /// `-expr`
    Neg,
    /// `!expr` / `not expr`
    Not,
    /// `~expr`
    BitNot,
}

/// Assignment operators for [`NodeKind::Assign`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssignOp {
    Assign,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// Which `Try` postfix operator was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TryKind {
    /// `.?` — unwrap or return the failure from the enclosing function.
    Propagate,
    /// `.!` — unwrap or abort.
    Abort,
}

/// The endpoint style of a range expression or pattern. Either endpoint may be
/// absent (unbounded), tracked separately on the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RangeKind {
    /// `a..<b` — half-open.
    HalfOpen,
    /// `a..=b` — closed.
    Closed,
    /// `a..` — unbounded above (only valid as an expression range).
    Open,
}

/// Body shape of a `struct` type-forming expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StructKind {
    /// `struct { field, $assert(...), ... }` — record; ids are [`NodeKind::Field`]
    /// or comptime item nodes.
    Record(Vec<NodeId>),
    /// `struct (T, U)` — tuple struct; ids are type nodes.
    Tuple(Vec<NodeId>),
    /// `struct` with no body — unit struct.
    Unit,
}

/// Payload attached to an `enum` variant declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VariantPayload {
    /// No payload.
    None,
    /// `.v(T, U)` — ids are type nodes.
    Tuple(Vec<NodeId>),
    /// `.v { a: T }` — ids are [`NodeKind::Field`] nodes.
    Record(Vec<NodeId>),
}

/// Body of a composite literal (`Type { ... }`, `.{ ... }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompositeBody {
    /// `{ a: x, b: y }` — ids are [`NodeKind::FieldInit`] nodes -> record/struct.
    Named(Vec<NodeId>),
    /// `{ x, y, z }` — positional entries -> array or tuple (chosen by type).
    Positional(Vec<NodeId>),
    /// `{ value; count }` — array repeat.
    Repeat { value: NodeId, count: NodeId },
}

/// Payload of an enum-variant *value* (`.variant(...)` / `.variant { ... }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VariantArgs {
    /// `.v` — no payload.
    None,
    /// `.v(x, y)` — ids are [`NodeKind::Arg`] nodes.
    Tuple(Vec<NodeId>),
    /// `.v { a: x }` — ids are [`NodeKind::FieldInit`] nodes.
    Record(Vec<NodeId>),
}

/// Payload of an enum-variant *pattern*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VariantPatArgs {
    /// `.v` — no payload.
    None,
    /// `.v(p, q)` — ids are pattern nodes.
    Tuple(Vec<NodeId>),
    /// `.v { a: p, .. }` — ids are [`NodeKind::FieldPat`] nodes.
    Record { fields: Vec<NodeId>, rest: bool },
}

/// Resolved target of an `import`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ImportPath {
    /// `<a/b/c>` — a package path split on `/`.
    Package(Vec<Symbol>),
    /// `"path/to/file"` — a file path resolved on the file system.
    File(String),
}

// ===< Node >===

/// A single AST node: identity, source location, and a kind-tagged payload
/// whose children are all [`NodeId`]s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// This node's own index in the arena (its slot in [`Ast::nodes`]).
    pub id: NodeId,
    /// Source range within [`Node::file`].
    pub span: Span,
    /// Which file this node came from (see [`FileId`]).
    pub file: FileId,
    /// The node's syntactic category and children.
    pub kind: NodeKind,
}

/// Every syntactic form in the language. Children are referenced by [`NodeId`];
/// only leaf data ([`Symbol`], [`Lit`], operators, flags) is stored inline.
///
/// The enum is intentionally flat and untyped in its child references: a
/// `NodeId` can point at any kind. Category invariants (e.g. "the `cond` of an
/// `If` is an expression") are upheld by the parser, not the type system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeKind {
    // ===< Root >===
    /// A whole source file: a sequence of items (an anonymous namespace).
    File { items: Vec<NodeId> },

    // ===< Decorations >===
    /// `@name(args...)` — `args` are [`NodeKind::Arg`] nodes (positional or named).
    Attribute { name: Symbol, args: Vec<NodeId> },
    /// `#name(args...)` — one directive. Directives never nest; when several stack
    /// (`#packed #align(4)`) each is its own node in the modified item's
    /// `directives` list (or in [`NodeKind::Decl`]`::directives`).
    Directive { name: Symbol, args: Vec<NodeId> },

    // ===< Declarations / bindings >===
    /// `{attr} {directive} (const_bind | local_decl)` — a decorated declaration.
    /// Only emitted when at least one attribute or declaration-level directive is
    /// present; an undecorated binding is stored as the bare `item` node. The
    /// directives here are the ones written *before* the bound name (e.g.
    /// `#static let ...`); directives written before a `func`/`struct`/`enum`/
    /// `trait`/`namespace` keyword on the RHS live on that literal node instead.
    Decl {
        attrs: Vec<NodeId>,
        directives: Vec<NodeId>,
        item: NodeId,
    },
    /// `pattern :: rhs` — the single `::` binding form. The RHS category (value,
    /// type, func, trait, namespace, import) is whatever node `rhs` points at.
    ConstBind { pattern: NodeId, rhs: NodeId },
    /// `(let | const) pattern [: ty] := value`
    LocalDecl {
        is_const: bool,
        pattern: NodeId,
        ty: Option<NodeId>,
        value: NodeId,
    },

    // ===< Statements >===
    /// `place op= value`
    Assign {
        op: AssignOp,
        place: NodeId,
        value: NodeId,
    },
    /// `defer (expr | block)`
    Defer { body: NodeId },
    /// `return [expr]`
    Return { value: Option<NodeId> },
    /// `break [expr]` (value only meaningful inside `loop`).
    Break { value: Option<NodeId> },
    /// `continue`
    Continue,
    /// `loop block`
    Loop { body: NodeId },
    /// `while cond block`
    While { cond: NodeId, body: NodeId },
    /// `for pattern in iter block`
    For {
        pattern: NodeId,
        iter: NodeId,
        body: NodeId,
    },

    // ===< Expressions >===
    /// `{ stmts...; tail }` — a block; evaluates to `tail` (or void).
    Block {
        stmts: Vec<NodeId>,
        tail: Option<NodeId>,
    },
    /// A scalar literal.
    Lit(Lit),
    /// `f"...{e}..."` — interpolated string; `parts` interleaves the embedded
    /// expression nodes (the literal chunks are recoverable from spans).
    InterpolatedStr { parts: Vec<NodeId> },
    /// A dotted name: `a.b.c`. A bare identifier is a one-segment path.
    ///
    /// `self` and `Self` are **not** special AST nodes: `self` is the ordinary
    /// path `["self"]` (a receiver parameter binding) and `Self` the ordinary
    /// path `["Self"]` (resolved to the implementing type inside a `trait`/`impl`).
    /// Name resolution reserves both names.
    Path { segments: Vec<Symbol> },
    /// `op operand` — prefix unary.
    Unary { op: UnOp, operand: NodeId },
    /// `lhs op rhs` — binary.
    Binary { op: BinOp, lhs: NodeId, rhs: NodeId },
    /// `(a, b, c)` — tuple (also `()` for the unit value: empty `elems`).
    Tuple { elems: Vec<NodeId> },
    /// `base.name`
    FieldAccess { base: NodeId, name: Symbol },
    /// `base.0` — tuple element access.
    TupleIndex { base: NodeId, index: u64 },
    /// `callee(args...)`
    Call { callee: NodeId, args: Vec<NodeId> },
    /// `base.<T, _, ...>` — postfix generic instantiation; args are types or
    /// [`NodeKind::TypeHole`].
    GenericApply { base: NodeId, args: Vec<NodeId> },
    /// `base[index]`
    Index { base: NodeId, index: NodeId },
    /// `base[range]` — slicing; `range` is a [`NodeKind::Range`].
    Slice { base: NodeId, range: NodeId },
    /// `base.*` — pointer dereference.
    Deref { base: NodeId },
    /// `base.?` / `base.!`
    Try { base: NodeId, kind: TryKind },
    /// `scrutinee.match { arms }`
    MatchExpr {
        scrutinee: NodeId,
        arms: Vec<NodeId>,
    },
    /// `[name:] value` — one call argument.
    Arg { name: Option<Symbol>, value: NodeId },
    /// `$name[.<T,...>](args...)` — a compiler intrinsic call.
    IntrinsicCall {
        name: Symbol,
        generic_args: Vec<NodeId>,
        args: Vec<NodeId>,
    },
    /// `[Type] { body }` / `.{ body }` — record, array, or repeat literal.
    ///
    /// The typed tuple-struct form `Type(args...)` is syntactically identical to a
    /// call and is parsed as [`NodeKind::Call`]; whether the callee is a type
    /// (construction) or a function is settled during name resolution.
    CompositeLit {
        ty: Option<NodeId>,
        body: CompositeBody,
    },
    /// `.variant[payload]` — enum variant value (enum inferred from context).
    VariantLit { name: Symbol, args: VariantArgs },
    /// `name: value` — one entry of a named composite body.
    FieldInit { name: Symbol, value: NodeId },
    /// `if cond then [else els]` — `then`/`els` are blocks (or a chained `if`).
    If {
        cond: NodeId,
        then: NodeId,
        els: Option<NodeId>,
    },
    /// `if match pattern := value then [else els]`
    IfMatch {
        pattern: NodeId,
        value: NodeId,
        then: NodeId,
        els: Option<NodeId>,
    },
    /// `pattern [if guard] => body` — one match arm.
    MatchArm {
        pattern: NodeId,
        guard: Option<NodeId>,
        body: NodeId,
    },
    /// A range expression (`a..<b`, `..=b`, `lo..`, `..`).
    Range {
        start: Option<NodeId>,
        end: Option<NodeId>,
        kind: RangeKind,
    },

    // ===< Type-forming expressions >===
    /// `path[.<args>]` — a named type, optionally instantiated. A `generic_args`
    /// entry is a type, a [`NodeKind::TypeHole`], or a [`NodeKind::AssocBinding`]
    /// (an `Item = T` associated-type constraint).
    TypePath {
        path: NodeId,
        generic_args: Vec<NodeId>,
    },
    /// `_` — an inferred generic argument.
    TypeHole,
    /// `name = type` inside a `.<...>` argument list — an associated-type
    /// equality constraint, e.g. `Iterator.<Item = int32>`. Appears only among
    /// the `generic_args` of a [`NodeKind::TypePath`] / [`NodeKind::GenericApply`].
    AssocBinding { name: Symbol, ty: NodeId },
    /// `*[mut] T`
    PtrType { mutable: bool, inner: NodeId },
    /// `[directives] [][mut] T` — directives select layout/repr (e.g. `#soa`).
    SliceType {
        directives: Vec<NodeId>,
        mutable: bool,
        inner: NodeId,
    },
    /// `[directives] [len][mut] T` — directives select layout/repr
    /// (e.g. `#simd`, `#soa`).
    ArrayType {
        directives: Vec<NodeId>,
        len: NodeId,
        mutable: bool,
        inner: NodeId,
    },
    /// `(T, U, ...)` — tuple type; empty is the `void`/unit type.
    TupleType { elems: Vec<NodeId> },
    /// `dyn T` — a trait object type.
    DynType { inner: NodeId },
    /// `distinct T` — a fresh nominal type over `T`.
    /// `[directives] distinct T` — a new nominal type with `T`'s representation
    /// (§2.4). It carries directives for the same reason a `struct` does: a
    /// `distinct` type is a type *declaration*, so `#lang` must be able to name
    /// it. `str` is exactly this — `#lang("str") distinct []u8`.
    DistinctType {
        directives: Vec<NodeId>,
        inner: NodeId,
    },
    /// `func [<g>] (param_types) [-> ret]` — a function *type*.
    FuncType {
        generics: Vec<NodeId>,
        params: Vec<NodeId>,
        ret: Option<NodeId>,
    },
    /// `[directives] struct [<g>] [body]`
    StructType {
        directives: Vec<NodeId>,
        generics: Vec<NodeId>,
        kind: StructKind,
    },
    /// `[attrs] name: ty` — a struct/enum record field.
    Field {
        attrs: Vec<NodeId>,
        /// `#align(4)`, `#raw`, … written before the field (§9).
        directives: Vec<NodeId>,
        name: Symbol,
        ty: NodeId,
    },
    /// `[directives] enum [<g>] { variants }`
    EnumType {
        directives: Vec<NodeId>,
        generics: Vec<NodeId>,
        variants: Vec<NodeId>,
    },
    /// `[attrs] name [payload]` — one enum variant declaration.
    Variant {
        attrs: Vec<NodeId>,
        name: Symbol,
        payload: VariantPayload,
    },
    /// `[directives] trait { members }` — members are `::` bindings
    /// ([`NodeKind::ConstBind`]) and comptime items. A method signature is a
    /// `ConstBind` whose RHS is a bodyless [`NodeKind::FuncExpr`]; an associated
    /// type is a `ConstBind` whose RHS is [`NodeKind::AssocType`].
    TraitType {
        directives: Vec<NodeId>,
        generics: Vec<NodeId>,
        members: Vec<NodeId>,
    },
    /// The `type` RHS of an associated-type binding (`Item :: type [: bounds]`).
    /// Nameless — `Item` is the enclosing [`NodeKind::ConstBind`] pattern.
    /// `bounds` are `+`-separated trait/type bounds the implementing type's
    /// choice must satisfy; empty means unconstrained.
    AssocType { bounds: Vec<NodeId> },

    /// The RHS of an associated **constant** in a trait: `MAX :: i32 [:= 100]`.
    /// Nameless — `MAX` is the enclosing [`NodeKind::ConstBind`] pattern.
    ///
    /// `ty` is the declared type every impl's value must have. `default` is the
    /// value an impl may omit, exactly as a method may omit a body the trait
    /// supplies; it uses `:=` rather than `=` for the same reason a parameter
    /// default does — every `=` in the grammar is assignment to an existing
    /// place or an associated-**type** constraint, and this *introduces* what a
    /// binding holds (§2.3, §5.2).
    AssocConst { ty: NodeId, default: Option<NodeId> },

    // ===< Functions / generics >===
    /// `[directives] [extern(abi)] func [<g>] (params) [-> ret] [block]`.
    /// A missing `body` is an external (bodyless) declaration.
    FuncExpr {
        directives: Vec<NodeId>,
        extern_abi: Option<Symbol>,
        generics: Vec<NodeId>,
        params: Vec<NodeId>,
        ret: Option<NodeId>,
        body: Option<NodeId>,
    },
    /// `name [: constraint]` — a generic type parameter (bare `T` is
    /// unconstrained; `constraint` is trait bounds, there is no `type` kind).
    GenericTypeParam {
        name: Symbol,
        constraint: Option<NodeId>,
    },
    /// `const name: ty` — a compile-time value parameter.
    GenericConstParam { name: Symbol, ty: NodeId },
    /// `T + U + ...` — a `+`-separated list of trait bounds; ids are type nodes.
    Bounds { bounds: Vec<NodeId> },
    /// `name: ty [':=' default]` — a function parameter (`ty` optional for
    /// inferred closures and for a bare `self` receiver, whose type defaults to
    /// `Self`).
    ///
    /// A `default` makes the parameter optional at the call site (§5.2). It is
    /// `:=`, not `=`, because it *introduces* what the binding holds and is
    /// evaluated per call — the same role `:=` plays for a local — where every
    /// `=` in the grammar is either assignment to an existing place or an
    /// associated-type constraint.
    Param {
        name: Symbol,
        ty: Option<NodeId>,
        default: Option<NodeId>,
    },

    // ===< Namespaces / impls / imports >===
    /// `[directives] namespace { items }` — directives select repr (e.g. `#c`).
    NamespaceExpr {
        directives: Vec<NodeId>,
        items: Vec<NodeId>,
    },
    /// `impl [<g>] Type [for Target] { items }`
    ImplBlock {
        generics: Vec<NodeId>,
        ty: NodeId,
        for_ty: Option<NodeId>,
        items: Vec<NodeId>,
    },
    /// `import <pkg>` / `import "file"`
    Import { path: ImportPath },

    // ===< Patterns >===
    /// `_`
    WildcardPat,
    /// `*` — glob (namespace-import patterns only).
    GlobPat,
    /// `[mut] name`
    BindingPat { mutable: bool, name: Symbol },
    /// `name @ pattern`
    AtPat { name: Symbol, pattern: NodeId },
    /// A literal pattern.
    LitPat(Lit),
    /// A range pattern (`a..<b`, `..=b`, ...).
    RangePat {
        start: Option<NodeId>,
        end: Option<NodeId>,
        kind: RangeKind,
    },
    /// `.variant[payload]` — enum-variant pattern.
    VariantPat { name: Symbol, args: VariantPatArgs },
    /// `[.]{ field_pats [, ..] }` — struct/namespace pattern. `path` names the
    /// type when present.
    StructPat {
        path: Option<NodeId>,
        fields: Vec<NodeId>,
        rest: bool,
    },
    /// `Type(p, q [, ..])` — tuple-struct pattern.
    TupleStructPat {
        path: NodeId,
        elems: Vec<NodeId>,
        rest: bool,
    },
    /// `(p, q, ...)` — tuple pattern.
    TuplePat { elems: Vec<NodeId> },
    /// `[a, b, .. [rest], c]` — slice pattern. `elems` holds the element patterns
    /// on both sides of the `..`; `rest` says where that `..` sits and what (if
    /// anything) it binds.
    SlicePat {
        elems: Vec<NodeId>,
        rest: Option<SliceRest>,
    },
    /// `&pattern` — dereference pattern.
    RefPat { pattern: NodeId },
    /// `a | b | c` — or-pattern.
    OrPat { alternatives: Vec<NodeId> },
    /// `[mut] name` or `name: pattern` — one field of a struct pattern.
    FieldPat {
        mutable: bool,
        name: Symbol,
        pattern: Option<NodeId>,
    },

    // ===< Recovery >===
    /// A placeholder produced by error recovery so parsing can continue.
    Error,
}

impl NodeKind {
    /// Collect every child [`NodeId`] this node references, in source order,
    /// into `out`. This is the single source of truth for tree traversal —
    /// both visitor directions build on it.
    pub fn collect_children(&self, out: &mut Vec<NodeId>) {
        use NodeKind::*;
        match self {
            Continue
            | Lit(_)
            | Path { .. }
            | TypeHole
            | Import { .. }
            | WildcardPat
            | GlobPat
            | BindingPat { .. }
            | LitPat(_)
            | Error => {}

            AssocType { bounds } => out.extend_from_slice(bounds),

            AssocConst { ty, default } => {
                out.push(*ty);
                push_opt(out, default);
            }

            File { items: elems }
            | Tuple { elems }
            | TupleType { elems }
            | TuplePat { elems }
            | SlicePat { elems, .. }
            | OrPat {
                alternatives: elems,
            }
            | InterpolatedStr { parts: elems }
            | Bounds { bounds: elems } => out.extend_from_slice(elems),

            Attribute { args, .. } | Directive { args, .. } => out.extend_from_slice(args),

            Decl {
                attrs,
                directives,
                item,
            } => {
                out.extend_from_slice(attrs);
                out.extend_from_slice(directives);
                out.push(*item);
            }
            ConstBind { pattern, rhs } => out.extend_from_slice(&[*pattern, *rhs]),
            LocalDecl {
                pattern, ty, value, ..
            } => {
                out.push(*pattern);
                push_opt(out, ty);
                out.push(*value);
            }

            Assign { place, value, .. } => out.extend_from_slice(&[*place, *value]),
            Defer { body } | Loop { body } => out.push(*body),
            Return { value } | Break { value } => push_opt(out, value),
            While { cond, body } => out.extend_from_slice(&[*cond, *body]),
            For {
                pattern,
                iter,
                body,
            } => out.extend_from_slice(&[*pattern, *iter, *body]),

            Block { stmts, tail } => {
                out.extend_from_slice(stmts);
                push_opt(out, tail);
            }
            Unary { operand, .. } => out.push(*operand),
            Binary { lhs, rhs, .. } => out.extend_from_slice(&[*lhs, *rhs]),
            FieldAccess { base, .. }
            | TupleIndex { base, .. }
            | Deref { base }
            | Try { base, .. } => out.push(*base),
            Call { callee, args } => {
                out.push(*callee);
                out.extend_from_slice(args);
            }
            GenericApply { base, args } => {
                out.push(*base);
                out.extend_from_slice(args);
            }
            Index { base, index } => out.extend_from_slice(&[*base, *index]),
            Slice { base, range } => out.extend_from_slice(&[*base, *range]),
            MatchExpr { scrutinee, arms } => {
                out.push(*scrutinee);
                out.extend_from_slice(arms);
            }
            Arg { value, .. } | FieldInit { value, .. } => out.push(*value),
            IntrinsicCall {
                generic_args, args, ..
            } => {
                out.extend_from_slice(generic_args);
                out.extend_from_slice(args);
            }
            CompositeLit { ty, body } => {
                push_opt(out, ty);
                body.collect_children(out);
            }
            VariantLit { args, .. } => args.collect_children(out),
            If { cond, then, els } => {
                out.extend_from_slice(&[*cond, *then]);
                push_opt(out, els);
            }
            IfMatch {
                pattern,
                value,
                then,
                els,
            } => {
                out.extend_from_slice(&[*pattern, *value, *then]);
                push_opt(out, els);
            }
            MatchArm {
                pattern,
                guard,
                body,
            } => {
                out.push(*pattern);
                push_opt(out, guard);
                out.push(*body);
            }
            Range { start, end, .. } | RangePat { start, end, .. } => {
                push_opt(out, start);
                push_opt(out, end);
            }

            TypePath { path, generic_args } => {
                out.push(*path);
                out.extend_from_slice(generic_args);
            }
            AssocBinding { ty, .. } => out.push(*ty),
            PtrType { inner, .. } | DynType { inner } => out.push(*inner),
            DistinctType { directives, inner } => {
                out.extend_from_slice(directives);
                out.push(*inner);
            }
            SliceType {
                directives, inner, ..
            } => {
                out.extend_from_slice(directives);
                out.push(*inner);
            }
            ArrayType {
                directives,
                len,
                inner,
                ..
            } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(&[*len, *inner]);
            }
            FuncType {
                generics,
                params,
                ret,
            } => {
                out.extend_from_slice(generics);
                out.extend_from_slice(params);
                push_opt(out, ret);
            }
            StructType {
                directives,
                generics,
                kind,
            } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(generics);
                kind.collect_children(out);
            }
            Field {
                attrs,
                directives,
                ty,
                ..
            } => {
                out.extend_from_slice(attrs);
                out.extend_from_slice(directives);
                out.push(*ty);
            }
            EnumType {
                directives,
                generics,
                variants,
            } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(generics);
                out.extend_from_slice(variants);
            }
            Variant { attrs, payload, .. } => {
                out.extend_from_slice(attrs);
                payload.collect_children(out);
            }
            TraitType {
                directives,
                generics,
                members,
            } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(generics);
                out.extend_from_slice(members);
            }

            FuncExpr {
                directives,
                generics,
                params,
                ret,
                body,
                ..
            } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(generics);
                out.extend_from_slice(params);
                push_opt(out, ret);
                push_opt(out, body);
            }
            GenericTypeParam { constraint, .. } => push_opt(out, constraint),
            GenericConstParam { ty, .. } => out.push(*ty),
            Param { ty, default, .. } => {
                push_opt(out, ty);
                push_opt(out, default);
            }

            NamespaceExpr { directives, items } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(items);
            }
            ImplBlock {
                generics,
                ty,
                for_ty,
                items,
            } => {
                out.extend_from_slice(generics);
                out.push(*ty);
                push_opt(out, for_ty);
                out.extend_from_slice(items);
            }

            AtPat { pattern, .. } | RefPat { pattern } => out.push(*pattern),
            VariantPat { args, .. } => args.collect_children(out),
            StructPat { path, fields, .. } => {
                push_opt(out, path);
                out.extend_from_slice(fields);
            }
            TupleStructPat { path, elems, .. } => {
                out.push(*path);
                out.extend_from_slice(elems);
            }
            FieldPat { pattern, .. } => push_opt(out, pattern),
        }
    }

    /// Convenience wrapper over [`NodeKind::collect_children`].
    pub fn children(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_children(&mut out);
        out
    }
}

impl StructKind {
    fn collect_children(&self, out: &mut Vec<NodeId>) {
        match self {
            StructKind::Record(ids) | StructKind::Tuple(ids) => out.extend_from_slice(ids),
            StructKind::Unit => {}
        }
    }
}

impl VariantPayload {
    fn collect_children(&self, out: &mut Vec<NodeId>) {
        match self {
            VariantPayload::Tuple(ids) | VariantPayload::Record(ids) => out.extend_from_slice(ids),
            VariantPayload::None => {}
        }
    }
}

impl CompositeBody {
    fn collect_children(&self, out: &mut Vec<NodeId>) {
        match self {
            CompositeBody::Named(ids) | CompositeBody::Positional(ids) => {
                out.extend_from_slice(ids)
            }
            CompositeBody::Repeat { value, count } => out.extend_from_slice(&[*value, *count]),
        }
    }
}

impl VariantArgs {
    fn collect_children(&self, out: &mut Vec<NodeId>) {
        match self {
            VariantArgs::Tuple(ids) | VariantArgs::Record(ids) => out.extend_from_slice(ids),
            VariantArgs::None => {}
        }
    }
}

impl VariantPatArgs {
    fn collect_children(&self, out: &mut Vec<NodeId>) {
        match self {
            VariantPatArgs::Tuple(ids) => out.extend_from_slice(ids),
            VariantPatArgs::Record { fields, .. } => out.extend_from_slice(fields),
            VariantPatArgs::None => {}
        }
    }
}

fn push_opt(out: &mut Vec<NodeId>, id: &Option<NodeId>) {
    if let Some(id) = id {
        out.push(*id);
    }
}

// ===< Side-table metadata >===
//
// The store itself is [`crate::common::meta::MetaStore`], shared with the IR,
// which keys the same structure by `IrId`. Access goes through the arena —
// [`Ast::set_meta`], [`Ast::meta`], [`Ast::with_meta`], [`Ast::has_meta`],
// [`Ast::take_meta`] — so a caller never names the store type, and it lives
// behind interior mutability so a shared `&Ast` can annotate during a walk,
// mirroring the per-node [`RefCell`] design.

// ===< Arena >===

/// The AST arena: a flat table of nodes, an optional root, and a type-indexed
/// [`MetaStore`] of per-node metadata for later passes.
///
/// Slots are [`RefCell`]-wrapped so a [`MutVisitor`](super::visitor::MutVisitor)
/// can rewrite nodes through a shared `&Ast`. Allocate with [`Ast::alloc`],
/// read with [`Ast::node`], mutate with [`Ast::node_mut`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ast {
    nodes: Vec<RefCell<Node>>,
    root: Option<NodeId>,
    #[serde(skip)]
    meta: MetaStore<NodeId>,
}

impl Ast {
    /// An empty arena.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a node built from `(span, file, kind)` and return its [`NodeId`].
    /// The node's own `id` field is filled in to match its slot.
    pub fn alloc(&mut self, span: Span, file: FileId, kind: NodeKind) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(RefCell::new(Node {
            id,
            span,
            file,
            kind,
        }));
        id
    }

    /// Number of nodes in the arena.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the arena holds no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The tree root, if one has been set.
    pub fn root(&self) -> Option<NodeId> {
        self.root
    }

    /// Set the tree root.
    pub fn set_root(&mut self, root: NodeId) {
        self.root = Some(root);
    }

    /// The raw cell for `id`, for callers that want explicit borrow control.
    pub fn cell(&self, id: NodeId) -> &RefCell<Node> {
        &self.nodes[id.0]
    }

    /// Shared borrow of the node at `id`. Panics if `id` is out of range or the
    /// node is already mutably borrowed.
    pub fn node(&self, id: NodeId) -> Ref<'_, Node> {
        self.nodes[id.0].borrow()
    }

    /// Mutable borrow of the node at `id`. Panics on an out-of-range `id` or an
    /// outstanding borrow.
    pub fn node_mut(&self, id: NodeId) -> RefMut<'_, Node> {
        self.nodes[id.0].borrow_mut()
    }

    /// Copy out `id`'s children without keeping a borrow open — the traversal
    /// primitive used by both visitor directions.
    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        self.node(id).kind.children()
    }

    /// Iterate all node ids in allocation order.
    pub fn ids(&self) -> impl Iterator<Item = NodeId> {
        (0..self.nodes.len()).map(NodeId)
    }

    // ===< Metadata side table >===

    /// Attach a metadata value of type `T` to `id`, replacing any previous `T`
    /// for that node. Values are keyed by `(NodeId, TypeId::of::<T>())`, so
    /// different `T`s coexist on the same node. Returns the displaced value, if
    /// any.
    ///
    /// ```ignore
    /// ast.set_meta(node, Resolution::Local(binding));   // in name resolution
    /// ast.set_meta(node, ty);                           // in the type checker
    /// ```
    pub fn set_meta<T: Any>(&self, id: NodeId, value: T) -> Option<T> {
        self.meta.set(id, value)
    }

    /// Clone out the `T` metadata attached to `id`, if present. Convenient for
    /// small `Copy`/`Clone` payloads; use [`Ast::with_meta`] to avoid a clone.
    pub fn meta<T: Any + Clone>(&self, id: NodeId) -> Option<T> {
        self.with_meta::<T, _>(id, T::clone)
    }

    /// Borrow the `T` metadata attached to `id` and run `f` on it, returning
    /// `f`'s result (or `None` when no `T` is attached). The borrow of the store
    /// is released before `f`'s result is returned.
    pub fn with_meta<T: Any, R>(&self, id: NodeId, f: impl FnOnce(&T) -> R) -> Option<R> {
        self.meta.with(&id, f)
    }

    /// Whether any `T` metadata is attached to `id`.
    pub fn has_meta<T: Any>(&self, id: NodeId) -> bool {
        self.meta.has::<T>(&id)
    }

    /// Remove and return the `T` metadata attached to `id`, if present.
    pub fn take_meta<T: Any>(&self, id: NodeId) -> Option<T> {
        self.meta.take::<T>(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp() -> Span {
        Span::new(0, 0)
    }

    fn f() -> FileId {
        FileId(0)
    }

    #[test]
    fn alloc_assigns_sequential_ids() {
        let mut ast = Ast::new();
        let a = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(1.into())));
        let b = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(2.into())));
        assert_eq!(a, NodeId(0));
        assert_eq!(b, NodeId(1));
        assert_eq!(ast.node(a).id, a);
        assert_eq!(ast.len(), 2);
    }

    #[test]
    fn children_are_collected_in_order() {
        let mut ast = Ast::new();
        let l = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(1.into())));
        let r = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(2.into())));
        let add = ast.alloc(
            sp(),
            f(),
            NodeKind::Binary {
                op: BinOp::Add,
                lhs: l,
                rhs: r,
            },
        );
        assert_eq!(ast.children(add), vec![l, r]);
    }

    #[test]
    fn mutate_through_shared_ref() {
        let mut ast = Ast::new();
        let n = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(1.into())));
        // A shared &Ast still allows rewriting a node — the RefCell property a
        // MutVisitor relies on.
        let ast_ref = &ast;
        ast_ref.node_mut(n).kind = NodeKind::Lit(Lit::Int(99.into()));
        assert!(matches!(&ast.node(n).kind, NodeKind::Lit(Lit::Int(v)) if *v == 99.into()));
    }

    #[test]
    fn metadata_is_keyed_by_node_and_type() {
        // Two distinct Rust types coexist on one node; each round-trips.
        #[derive(Debug, Clone, PartialEq)]
        struct Ty(u32);
        #[derive(Debug, Clone, PartialEq)]
        struct Res(&'static str);

        let mut ast = Ast::new();
        let n = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(1.into())));

        assert!(!ast.has_meta::<Ty>(n));
        assert_eq!(ast.set_meta(n, Ty(7)), None);
        ast.set_meta(n, Res("local"));

        assert!(ast.has_meta::<Ty>(n));
        assert_eq!(ast.meta::<Ty>(n), Some(Ty(7)));
        assert_eq!(ast.meta::<Res>(n), Some(Res("local")));

        // Overwrite returns the old value; different node has nothing.
        assert_eq!(ast.set_meta(n, Ty(9)), Some(Ty(7)));
        assert_eq!(ast.with_meta::<Ty, _>(n, |t| t.0), Some(9));
        assert_eq!(ast.take_meta::<Ty>(n), Some(Ty(9)));
        assert!(!ast.has_meta::<Ty>(n));
        assert_eq!(ast.meta::<Res>(n), Some(Res("local")));
    }

    #[test]
    fn metadata_dropped_on_clone_and_serde() {
        let mut ast = Ast::new();
        let n = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(1.into())));
        ast.set_meta(n, 42u32);
        // Derived state: not carried by clone.
        assert_eq!(ast.clone().meta::<u32>(n), None);
        assert_eq!(ast.meta::<u32>(n), Some(42)); // original untouched
    }

    #[test]
    fn round_trips_through_serde() {
        let mut ast = Ast::new();
        let l = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Str("hi".into())));
        let file = ast.alloc(sp(), f(), NodeKind::File { items: vec![l] });
        ast.set_root(file);

        let json = serde_json::to_string(&ast).expect("serialize");
        let back: Ast = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.len(), ast.len());
        assert_eq!(back.root(), Some(file));
        assert!(matches!(back.node(l).kind, NodeKind::Lit(Lit::Str(_))));
    }
}
