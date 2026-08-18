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

use std::cell::{Ref, RefCell, RefMut};

use serde::{Deserialize, Serialize};

use crate::common::span::Span;
use crate::common::symbol::Symbol;

/// Index of a [`Node`] inside an [`Ast`] arena. Cheap, `Copy`, transparent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub usize);

/// Identifies the source file a node originates from. Because `import` splices
/// members from other files into a namespace, a node's file is tracked
/// independently of the arena it ends up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(pub u32);

// ===< Leaf payloads (embedded by value, never allocated as nodes) >===

/// A scalar literal value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Lit {
    Int(i128),
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
    /// `@name(args...)`
    Attribute { name: Symbol, args: Vec<NodeId> },
    /// `#name(args...) ...` — trailing `nested` holds directives chained after.
    Directive {
        name: Symbol,
        args: Vec<NodeId>,
        nested: Vec<NodeId>,
    },

    // ===< Declarations / bindings >===
    /// `{attr} [directive] (const_bind | local_decl)` — a decorated declaration.
    Decl {
        attrs: Vec<NodeId>,
        directive: Option<NodeId>,
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
    Path { segments: Vec<Symbol> },
    /// `self`
    SelfValue,
    /// `Self`
    SelfType,
    /// `op operand` — prefix unary.
    Unary { op: UnOp, operand: NodeId },
    /// `lhs op rhs` — binary.
    Binary {
        op: BinOp,
        lhs: NodeId,
        rhs: NodeId,
    },
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
    CompositeLit {
        ty: Option<NodeId>,
        body: CompositeBody,
    },
    /// `Type(args...)` — typed tuple-struct literal.
    TupleStructLit { ty: NodeId, args: Vec<NodeId> },
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
    /// `path[.<args>]` — a named type, optionally instantiated.
    TypePath {
        path: NodeId,
        generic_args: Vec<NodeId>,
    },
    /// `_` — an inferred generic argument.
    TypeHole,
    /// `*[mut] T`
    PtrType { mutable: bool, inner: NodeId },
    /// `[][mut] T`
    SliceType { mutable: bool, inner: NodeId },
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
    DistinctType { inner: NodeId },
    /// `func [<g>] (param_types) [-> ret]` — a function *type*.
    FuncType {
        generics: Vec<NodeId>,
        params: Vec<NodeId>,
        ret: Option<NodeId>,
    },
    /// `[directives] struct [body]`
    StructType {
        directives: Vec<NodeId>,
        kind: StructKind,
    },
    /// `[attrs] name: ty` — a struct/enum record field.
    Field {
        attrs: Vec<NodeId>,
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
        members: Vec<NodeId>,
    },
    /// The `type` RHS of an associated-type binding (`Item :: type [: bounds]`).
    /// Nameless — `Item` is the enclosing [`NodeKind::ConstBind`] pattern.
    /// `bounds` are `+`-separated trait/type bounds the implementing type's
    /// choice must satisfy; empty means unconstrained.
    AssocType { bounds: Vec<NodeId> },

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
    /// `name: ty` — a function parameter (`ty` optional for inferred closures).
    Param { name: Symbol, ty: Option<NodeId> },
    /// `self [: ty]` — the receiver parameter.
    SelfParam { ty: Option<NodeId> },

    // ===< Namespaces / impls / imports >===
    /// `namespace { items }`
    NamespaceExpr { items: Vec<NodeId> },
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
    /// `[a, b, .. [rest]]` — slice pattern. `rest` is `Some(name?)` when a `..`
    /// segment is present, carrying its optional binding.
    SlicePat {
        elems: Vec<NodeId>,
        rest: Option<Option<Symbol>>,
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
            Continue | Lit(_) | Path { .. } | SelfValue | SelfType | TypeHole
            | Import { .. } | WildcardPat | GlobPat | BindingPat { .. }
            | LitPat(_) | Error => {}

            AssocType { bounds } => out.extend_from_slice(bounds),

            File { items: elems }
            | Tuple { elems }
            | TupleType { elems }
            | NamespaceExpr { items: elems }
            | TuplePat { elems }
            | SlicePat { elems, .. }
            | OrPat { alternatives: elems }
            | InterpolatedStr { parts: elems }
            | Bounds { bounds: elems } => out.extend_from_slice(elems),

            Attribute { args, .. } => out.extend_from_slice(args),
            Directive { args, nested, .. } => {
                out.extend_from_slice(args);
                out.extend_from_slice(nested);
            }

            Decl { attrs, directive, item } => {
                out.extend_from_slice(attrs);
                push_opt(out, directive);
                out.push(*item);
            }
            ConstBind { pattern, rhs } => out.extend_from_slice(&[*pattern, *rhs]),
            LocalDecl { pattern, ty, value, .. } => {
                out.push(*pattern);
                push_opt(out, ty);
                out.push(*value);
            }

            Assign { place, value, .. } => out.extend_from_slice(&[*place, *value]),
            Defer { body } | Loop { body } => out.push(*body),
            Return { value } | Break { value } => push_opt(out, value),
            While { cond, body } => out.extend_from_slice(&[*cond, *body]),
            For { pattern, iter, body } => out.extend_from_slice(&[*pattern, *iter, *body]),

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
            IntrinsicCall { generic_args, args, .. } => {
                out.extend_from_slice(generic_args);
                out.extend_from_slice(args);
            }
            CompositeLit { ty, body } => {
                push_opt(out, ty);
                body.collect_children(out);
            }
            TupleStructLit { ty, args } => {
                out.push(*ty);
                out.extend_from_slice(args);
            }
            VariantLit { args, .. } => args.collect_children(out),
            If { cond, then, els } => {
                out.extend_from_slice(&[*cond, *then]);
                push_opt(out, els);
            }
            IfMatch { pattern, value, then, els } => {
                out.extend_from_slice(&[*pattern, *value, *then]);
                push_opt(out, els);
            }
            MatchArm { pattern, guard, body } => {
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
            PtrType { inner, .. }
            | SliceType { inner, .. }
            | DynType { inner }
            | DistinctType { inner } => out.push(*inner),
            ArrayType { directives, len, inner, .. } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(&[*len, *inner]);
            }
            FuncType { generics, params, ret } => {
                out.extend_from_slice(generics);
                out.extend_from_slice(params);
                push_opt(out, ret);
            }
            StructType { directives, kind } => {
                out.extend_from_slice(directives);
                kind.collect_children(out);
            }
            Field { attrs, ty, .. } => {
                out.extend_from_slice(attrs);
                out.push(*ty);
            }
            EnumType { directives, generics, variants } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(generics);
                out.extend_from_slice(variants);
            }
            Variant { attrs, payload, .. } => {
                out.extend_from_slice(attrs);
                payload.collect_children(out);
            }
            TraitType { directives, members } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(members);
            }

            FuncExpr { directives, generics, params, ret, body, .. } => {
                out.extend_from_slice(directives);
                out.extend_from_slice(generics);
                out.extend_from_slice(params);
                push_opt(out, ret);
                push_opt(out, body);
            }
            GenericTypeParam { constraint, .. } => push_opt(out, constraint),
            GenericConstParam { ty, .. } => out.push(*ty),
            Param { ty, .. } | SelfParam { ty } => push_opt(out, ty),

            ImplBlock { generics, ty, for_ty, items } => {
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

// ===< Arena >===

/// The AST arena: a flat table of nodes plus an optional root.
///
/// Slots are [`RefCell`]-wrapped so a [`MutVisitor`](super::visitor::MutVisitor)
/// can rewrite nodes through a shared `&Ast`. Allocate with [`Ast::alloc`],
/// read with [`Ast::node`], mutate with [`Ast::node_mut`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ast {
    nodes: Vec<RefCell<Node>>,
    root: Option<NodeId>,
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
        let a = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(1)));
        let b = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(2)));
        assert_eq!(a, NodeId(0));
        assert_eq!(b, NodeId(1));
        assert_eq!(ast.node(a).id, a);
        assert_eq!(ast.len(), 2);
    }

    #[test]
    fn children_are_collected_in_order() {
        let mut ast = Ast::new();
        let l = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(1)));
        let r = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(2)));
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
        let n = ast.alloc(sp(), f(), NodeKind::Lit(Lit::Int(1)));
        // A shared &Ast still allows rewriting a node — the RefCell property a
        // MutVisitor relies on.
        let ast_ref = &ast;
        ast_ref.node_mut(n).kind = NodeKind::Lit(Lit::Int(99));
        assert!(matches!(
            ast.node(n).kind,
            NodeKind::Lit(Lit::Int(99))
        ));
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
