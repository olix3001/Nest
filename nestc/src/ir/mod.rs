//! The Nest intermediate representation.
//!
//! The IR is what the AST becomes once name resolution, desugaring, and type
//! inference are done: a **typed, structured, much smaller tree** built by
//! [`super::lower`]. It keeps the AST's structured control flow — `if`, `match`,
//! and a single infinite [`Expr::Loop`] with `break` — because the code
//! generator and the analyses that run on the IR want that shape, but it drops
//! everything the earlier stages resolved away:
//!
//! - Surface sugar is gone (`for`, `while`, `.?`/`.!`, compound assignment) —
//!   `for`/`.?`/`.!` were desugared on the AST; `while` becomes a `loop` with a
//!   leading `if !cond { break }` here.
//! - Names are **bound**: every leaf is an [`Expr::Local`] / [`Expr::Global`]
//!   carrying the [`DefId`] it refers to, never a string path to re-resolve.
//! - Every node carries its [`Ty`] (see [`Expr::ty`]); nothing is left to infer.
//! - Auto-deref is **explicit**: a field/index access through a pointer gets an
//!   [`Expr::Deref`] inserted.
//! - `defer` is **explicit**: its body is copied to each point control leaves the
//!   defining block (block end and every enclosed `return`), innermost-last.
//!
//! What is deliberately *not* lowered yet (each a documented next layer, see
//! [`super::lower`]): operator-trait selection (arithmetic stays a primitive
//! [`Expr::Binary`]), generic monomorphization, `match` exhaustiveness / decision
//! trees (arms stay structured), and `@using` upcasts.
//!
//! Traversal is via the [`Visitor`] / [`VisitorMut`] traits, whose default
//! methods walk every child so an implementation overrides only the nodes it
//! cares about.

use crate::common::symbol::Symbol;
use crate::parser::ast::{BinOp, Lit, UnOp};

use crate::sema::def::DefId;
use crate::sema::ty::Ty;

pub use crate::sema::builtins::BuiltinOp;

pub mod pretty;

/// A whole lowered program: every function that had a body.
#[derive(Debug, Clone)]
pub struct Program {
    pub funcs: Vec<Function>,
}

/// One lowered function.
#[derive(Debug, Clone)]
pub struct Function {
    /// The function's definition id (its canonical name lives in the def table).
    pub def: DefId,
    pub name: Symbol,
    pub params: Vec<Param>,
    pub ret: Ty,
    pub body: Block,
}

/// A bound function parameter.
#[derive(Debug, Clone)]
pub struct Param {
    pub def: DefId,
    pub name: Symbol,
    pub ty: Ty,
}

/// A sequence of statements and an optional trailing value expression. The
/// block's type is the tail's type, or `void`.
#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
    pub ty: Ty,
}

/// A statement: an effect with no value contribution to its block.
#[derive(Debug, Clone)]
pub enum Stmt {
    /// `let`/`const` binding (mutability is not tracked in the IR: the checker
    /// already enforced it).
    Let {
        def: DefId,
        name: Symbol,
        ty: Ty,
        init: Expr,
    },
    /// A place assignment (`place = value`); compound forms were desugared.
    Assign { place: Expr, value: Expr },
    /// An expression evaluated for effect.
    Expr(Expr),
    /// `return [value]`. Any `defer`s in scope have already been spliced in
    /// before this by lowering.
    Return(Option<Expr>),
    /// `break [value]` out of the enclosing `loop`.
    Break(Option<Expr>),
    /// `continue` the enclosing `loop`.
    Continue,
}

/// One `match` arm; patterns stay structured (no decision tree yet).
#[derive(Debug, Clone)]
pub struct Arm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
}

/// A (simplified) pattern. Field/slice-rest details the AST carried are dropped;
/// what remains is enough for a later decision-tree pass and for binding.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// `_` and any pattern the bootstrap does not model bind nothing.
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
}

/// A typed expression. Every variant ends in its [`Ty`]; read it via
/// [`Expr::ty`].
#[derive(Debug, Clone)]
pub enum Expr {
    /// A scalar literal.
    Lit(Lit, Ty),
    /// A reference to a local or parameter.
    Local(DefId, Ty),
    /// A reference to a top-level item (function / const / type used as a value).
    Global(DefId, Ty),
    /// `callee(args...)`.
    ///
    /// Operators lower to a `Call` too (§6: "int+int and Vec3+Vec3 are the same
    /// construct"), so both a primitive `i32 + i32` and a user `impl Add for
    /// Vec3` reach codegen as a call to the trait method they resolved to. The
    /// [`builtin`](Expr::Call::builtin) tag lets codegen recognize a primitive
    /// intrinsic op in **O(1)** — when it is `Some`, the call *is* the machine
    /// instruction and needs no function lookup; when `None`, it is an ordinary
    /// user call.
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        /// `Some` iff this call is a builtin primitive operator (see
        /// [`crate::sema::builtins`]); codegen emits it inline. `None` for a
        /// normal function/method call.
        builtin: Option<BuiltinOp>,
        ty: Ty,
    },
    /// A primitive binary operation (numeric / boolean core). Operator-trait
    /// dispatch is a later pass.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        ty: Ty,
    },
    /// A primitive prefix unary operation.
    Unary {
        op: UnOp,
        operand: Box<Expr>,
        ty: Ty,
    },
    /// `&place` / `&mut place`.
    Ref {
        mutable: bool,
        place: Box<Expr>,
        ty: Ty,
    },
    /// `base.*` — an **explicit** pointer dereference (inserted by lowering for
    /// auto-deref sites too).
    Deref { base: Box<Expr>, ty: Ty },
    /// `base.name` — a struct field access (base is a value, never a pointer:
    /// lowering inserts a [`Expr::Deref`] first).
    Field {
        base: Box<Expr>,
        name: Symbol,
        ty: Ty,
    },
    /// `base.N` — tuple element access.
    TupleIndex { base: Box<Expr>, index: u64, ty: Ty },
    /// `base[index]`.
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
        ty: Ty,
    },
    /// `(a, b, ...)`.
    Tuple { elems: Vec<Expr>, ty: Ty },
    /// A nested block expression.
    Block(Block),
    /// `if cond { then } else { els }`.
    If {
        cond: Box<Expr>,
        then: Block,
        els: Option<Block>,
        ty: Ty,
    },
    /// `match scrutinee { arms }`.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
        ty: Ty,
    },
    /// An infinite loop; exits only through a `break`.
    Loop { body: Block, ty: Ty },
    /// A struct / record construction `Type { field: value, ... }`.
    Construct {
        def: DefId,
        fields: Vec<(Symbol, Expr)>,
        ty: Ty,
    },
    /// An enum-variant value `.variant(args...)`.
    Variant {
        name: Symbol,
        args: Vec<Expr>,
        ty: Ty,
    },
    /// A compiler `$`-intrinsic call.
    Intrinsic {
        name: Symbol,
        args: Vec<Expr>,
        ty: Ty,
    },
    /// A placeholder for an expression that could not be lowered (an error was
    /// already reported); carries its (usually error) type.
    Error(Ty),
}

impl Expr {
    /// This expression's type.
    pub fn ty(&self) -> &Ty {
        match self {
            Expr::Lit(_, ty)
            | Expr::Local(_, ty)
            | Expr::Global(_, ty)
            | Expr::Call { ty, .. }
            | Expr::Binary { ty, .. }
            | Expr::Unary { ty, .. }
            | Expr::Ref { ty, .. }
            | Expr::Deref { ty, .. }
            | Expr::Field { ty, .. }
            | Expr::TupleIndex { ty, .. }
            | Expr::Index { ty, .. }
            | Expr::Tuple { ty, .. }
            | Expr::If { ty, .. }
            | Expr::Match { ty, .. }
            | Expr::Loop { ty, .. }
            | Expr::Construct { ty, .. }
            | Expr::Variant { ty, .. }
            | Expr::Intrinsic { ty, .. }
            | Expr::Error(ty) => ty,
            Expr::Block(b) => &b.ty,
        }
    }
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
}

pub fn walk_function<V: Visitor>(v: &mut V, func: &Function) {
    v.visit_block(&func.body);
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
    match stmt {
        Stmt::Let { init, .. } => v.visit_expr(init),
        Stmt::Assign { place, value } => {
            v.visit_expr(place);
            v.visit_expr(value);
        }
        Stmt::Expr(e) => v.visit_expr(e),
        Stmt::Return(e) | Stmt::Break(e) => {
            if let Some(e) = e {
                v.visit_expr(e);
            }
        }
        Stmt::Continue => {}
    }
}

pub fn walk_expr<V: Visitor>(v: &mut V, expr: &Expr) {
    match expr {
        Expr::Lit(..) | Expr::Local(..) | Expr::Global(..) | Expr::Error(_) => {}
        Expr::Call { callee, args, .. } => {
            v.visit_expr(callee);
            for a in args {
                v.visit_expr(a);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            v.visit_expr(lhs);
            v.visit_expr(rhs);
        }
        Expr::Unary { operand, .. } => v.visit_expr(operand),
        Expr::Ref { place, .. } => v.visit_expr(place),
        Expr::Deref { base, .. } | Expr::Field { base, .. } | Expr::TupleIndex { base, .. } => {
            v.visit_expr(base)
        }
        Expr::Index { base, index, .. } => {
            v.visit_expr(base);
            v.visit_expr(index);
        }
        Expr::Tuple { elems, .. } => {
            for e in elems {
                v.visit_expr(e);
            }
        }
        Expr::Block(b) => v.visit_block(b),
        Expr::If {
            cond, then, els, ..
        } => {
            v.visit_expr(cond);
            v.visit_block(then);
            if let Some(e) = els {
                v.visit_block(e);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            v.visit_expr(scrutinee);
            for a in arms {
                v.visit_arm(a);
            }
        }
        Expr::Loop { body, .. } => v.visit_block(body),
        Expr::Construct { fields, .. } => {
            for (_, e) in fields {
                v.visit_expr(e);
            }
        }
        Expr::Variant { args, .. } | Expr::Intrinsic { args, .. } => {
            for a in args {
                v.visit_expr(a);
            }
        }
    }
}

pub fn walk_arm<V: Visitor>(v: &mut V, arm: &Arm) {
    if let Some(g) = &arm.guard {
        v.visit_expr(g);
    }
    v.visit_expr(&arm.body);
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
}

pub fn walk_function_mut<V: VisitorMut>(v: &mut V, func: &mut Function) {
    v.visit_block(&mut func.body);
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
    match stmt {
        Stmt::Let { init, .. } => v.visit_expr(init),
        Stmt::Assign { place, value } => {
            v.visit_expr(place);
            v.visit_expr(value);
        }
        Stmt::Expr(e) => v.visit_expr(e),
        Stmt::Return(e) | Stmt::Break(e) => {
            if let Some(e) = e {
                v.visit_expr(e);
            }
        }
        Stmt::Continue => {}
    }
}

pub fn walk_expr_mut<V: VisitorMut>(v: &mut V, expr: &mut Expr) {
    match expr {
        Expr::Lit(..) | Expr::Local(..) | Expr::Global(..) | Expr::Error(_) => {}
        Expr::Call { callee, args, .. } => {
            v.visit_expr(callee);
            for a in args {
                v.visit_expr(a);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            v.visit_expr(lhs);
            v.visit_expr(rhs);
        }
        Expr::Unary { operand, .. } => v.visit_expr(operand),
        Expr::Ref { place, .. } => v.visit_expr(place),
        Expr::Deref { base, .. } | Expr::Field { base, .. } | Expr::TupleIndex { base, .. } => {
            v.visit_expr(base)
        }
        Expr::Index { base, index, .. } => {
            v.visit_expr(base);
            v.visit_expr(index);
        }
        Expr::Tuple { elems, .. } => {
            for e in elems {
                v.visit_expr(e);
            }
        }
        Expr::Block(b) => v.visit_block(b),
        Expr::If {
            cond, then, els, ..
        } => {
            v.visit_expr(cond);
            v.visit_block(then);
            if let Some(e) = els {
                v.visit_block(e);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            v.visit_expr(scrutinee);
            for a in arms {
                v.visit_arm(a);
            }
        }
        Expr::Loop { body, .. } => v.visit_block(body),
        Expr::Construct { fields, .. } => {
            for (_, e) in fields {
                v.visit_expr(e);
            }
        }
        Expr::Variant { args, .. } | Expr::Intrinsic { args, .. } => {
            for a in args {
                v.visit_expr(a);
            }
        }
    }
}

pub fn walk_arm_mut<V: VisitorMut>(v: &mut V, arm: &mut Arm) {
    if let Some(g) = &mut arm.guard {
        v.visit_expr(g);
    }
    v.visit_expr(&mut arm.body);
}
