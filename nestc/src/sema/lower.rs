//! AST → IR lowering — the stage that turns the resolved, desugared, typed AST
//! into the small typed [`ir`](crate::ir) tree.
//!
//! It runs after [`super::infer`], so every expression node already carries a
//! [`Ty`] in the arena metadata; lowering reads that back rather than
//! recomputing anything. What it *does* is make structure explicit:
//!
//! - `while cond { body }` → `loop { if !cond { break }; body }`.
//! - `if match p := v { .. }` → `match v { p => then, _ => els }`.
//! - auto-deref: a field/index access whose base is a pointer gets an explicit
//!   [`ir::Expr::Deref`].
//! - `defer`: each block's defer bodies are collected once into
//!   [`ir::Block::defers`], in written order, rather than copied to every exit.
//!   Running them in reverse on each way out — and unwinding the enclosing
//!   blocks' on a `return` — is left to the CFG stage, which emits one epilogue
//!   per scope instead of one per exit path.
//!
//! - implicit coercions become explicit: an `@using` upcast turns into the field
//!   access it stands for (`e.t`, or `&e.t` through a pointer), and a `*T` →
//!   `*dyn Trait` unsizing into an [`ir::Expr::DynCast`] that keeps the erased
//!   pointee for vtable selection.
//!
//! Not lowered (documented stubs, matching [`super::infer`]): bitwise / shift
//! operators (they stay [`ir::Expr::Binary`]) and closures / nested-function
//! values. `match` arms stay structured — patterns keep their full shape, but
//! decision-tree compilation is a later, CFG-level pass.

use std::collections::HashMap;

use crate::common::source::FileId;
use crate::common::symbol::Symbol;
use crate::parser::ast::{
    Ast, CompositeBody, Lit, NodeId, NodeKind, RangeKind, UnOp, VariantArgs, VariantPatArgs,
};

use super::def::{DefId, DefKind, DefTable};
use super::infer::{DynCoerce, Upcast};
use super::infer::OpResolution;
use super::ty::Ty;
use super::{DefMeta, Resolution};
use crate::ir::{Arm, Binding, Block, Expr, Function, Param, Pattern, Program, Stmt};

/// Lower every function body in `file` to IR.
pub fn lower_file(defs: &DefTable, asts: &HashMap<FileId, Ast>, file: FileId) -> Program {
    let ast = &asts[&file];
    let mut lo = Lowerer {
        defs,
        ast,
        defers: Vec::new(),
    };
    // Iterate the `Func` defs of this file: each carries the name/DefId and its
    // node is the `ConstBind` whose RHS is the `FuncExpr` (a bodyless one — a
    // trait method signature or an `extern` decl — is skipped).
    let mut funcs = Vec::new();
    for def in defs.iter() {
        if def.kind != DefKind::Func || def.file != Some(file) {
            continue;
        }
        let Some(node) = def.node else { continue };
        let func = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            NodeKind::FuncExpr { .. } => node,
            _ => continue,
        };
        if let Some(f) = lo.lower_function(def.id, func) {
            funcs.push(f);
        }
    }
    Program { funcs }
}

struct Lowerer<'a> {
    defs: &'a DefTable,
    ast: &'a Ast,
    /// Stack of pending `defer` bodies, one frame per open block (innermost last).
    defers: Vec<Vec<Expr>>,
}

impl Lowerer<'_> {
    fn lower_function(&mut self, def: DefId, func: NodeId) -> Option<Function> {
        let NodeKind::FuncExpr { params, body, .. } = self.ast.node(func).kind.clone() else {
            return None;
        };
        let name = self.defs.get(def).name.clone();
        let ret = self.ty(func);
        let params = params.iter().filter_map(|&p| self.lower_param(p)).collect();
        let body = self.lower_block(body?);
        Some(Function {
            def,
            name,
            params,
            ret,
            body,
        })
    }

    fn lower_param(&self, param: NodeId) -> Option<Param> {
        let NodeKind::Param { name, .. } = self.ast.node(param).kind.clone() else {
            return None;
        };
        let def = self.def_of(param)?;
        Some(Param {
            def,
            name,
            ty: self.ty(param),
        })
    }

    // ===< blocks / statements >===

    fn lower_block(&mut self, node: NodeId) -> Block {
        let (stmts, tail) = match self.ast.node(node).kind.clone() {
            NodeKind::Block { stmts, tail } => (stmts, tail),
            // A non-block body (an expression) becomes a tail-only block.
            _ => (vec![], Some(node)),
        };
        self.defers.push(Vec::new());
        let mut out = Vec::new();
        for s in stmts {
            self.lower_stmt(s, &mut out);
        }
        let tail = tail.map(|t| Box::new(self.lower_expr(t)));
        let defers = self.defers.pop().unwrap_or_default();
        let ty = tail.as_ref().map(|t| t.ty().clone()).unwrap_or(Ty::Void);
        Block {
            stmts: out,
            tail,
            ty,
            defers,
        }
    }

    /// Lower a pattern-binding statement (`let` / `const` / synthetic `::`) to
    /// an IR `Let`, or — for a destructuring pattern that binds no single def —
    /// keep the initializer for effect.
    fn lower_binding(&mut self, pattern: NodeId, value: NodeId, out: &mut Vec<Stmt>) {
        let init = self.lower_expr(value);
        match self.def_of(pattern) {
            Some(def) => out.push(Stmt::Let {
                def,
                name: self.defs.get(def).name.clone(),
                ty: init.ty().clone(),
                init,
            }),
            // A destructuring binding: keep the initializer for effect
            // (pattern-binding lowering is a later refinement).
            None => out.push(Stmt::Expr(init)),
        }
    }

    fn lower_stmt(&mut self, node: NodeId, out: &mut Vec<Stmt>) {
        match self.ast.node(node).kind.clone() {
            // A `let`/`const` local, or a `::` binding the desugarer introduced
            // in statement position (`__it`, `__try`): both bind a pattern to an
            // initializer and lower to an IR `Let`.
            NodeKind::LocalDecl { pattern, value, .. } => self.lower_binding(pattern, value, out),
            NodeKind::ConstBind { pattern, rhs } => self.lower_binding(pattern, rhs, out),
            NodeKind::Assign { place, value, op } => {
                let place = self.lower_expr(place);
                let value = self.lower_expr(value);
                // Compound assignment was desugared to simple `=` already; `op`
                // is `Assign` here (defensive: fall back to plain assign).
                let _ = op;
                out.push(Stmt::Assign { place, value });
            }
            // Control-flow exits carry no defer copies: the block that owns the
            // defers records them, and the CFG stage runs them on each way out.
            NodeKind::Return { value } => {
                let value = value.map(|v| self.lower_expr(v));
                out.push(Stmt::Return(value));
            }
            NodeKind::Break { value } => {
                let value = value.map(|v| self.lower_expr(v));
                out.push(Stmt::Break(value));
            }
            NodeKind::Continue => out.push(Stmt::Continue),
            NodeKind::Defer { body } => {
                let d = self.lower_expr(body);
                if let Some(frame) = self.defers.last_mut() {
                    frame.push(d);
                }
            }
            _ => {
                let e = self.lower_expr(node);
                out.push(Stmt::Expr(e));
            }
        }
    }

    // ===< expressions >===

    fn lower_expr(&mut self, node: NodeId) -> Expr {
        // An `@using` upcast is a coercion inference accepted at this node, not
        // anything the surface syntax wrote: make the "take `e.field`" explicit
        // around whatever the node itself lowers to.
        if let Some(up) = self.ast.meta::<Upcast>(node) {
            return self.lower_upcast(node, up);
        }
        // Likewise a `*T` → `*dyn Trait` unsizing: the fat pointer is built here,
        // not written anywhere in the source.
        if let Some(dc) = self.ast.meta::<DynCoerce>(node) {
            let value = self.lower_expr_inner(node);
            return Expr::DynCast {
                value: Box::new(value),
                concrete: dc.concrete,
                ty: Ty::Ptr {
                    mutable: matches!(self.ty(node), Ty::Ptr { mutable: true, .. }),
                    inner: Box::new(Ty::Dyn(dc.trait_def)),
                },
            };
        }
        self.lower_expr_inner(node)
    }

    /// Wrap `node`'s own lowering in the field access its `@using` coercion
    /// stands for: `e.t` for a value, `&e.t` (keeping mutability) for a pointer.
    fn lower_upcast(&mut self, node: NodeId, up: Upcast) -> Expr {
        let ty = up.target.clone();
        let name = self.defs.get(up.field).name.clone();
        let base = self.lower_expr_inner(node);
        if !up.through_ptr {
            return Expr::Field {
                base: Box::new(autoderef(base)),
                name,
                ty,
            };
        }
        // Through a pointer the sub-object's address is what coerces, so the
        // field is read off the pointee and re-addressed.
        let (mutable, inner) = match &ty {
            Ty::Ptr { mutable, inner } => (*mutable, (**inner).clone()),
            _ => (false, ty.clone()),
        };
        Expr::Ref {
            mutable,
            place: Box::new(Expr::Field {
                base: Box::new(autoderef(base)),
                name,
                ty: inner,
            }),
            ty,
        }
    }

    fn lower_expr_inner(&mut self, node: NodeId) -> Expr {
        let ty = self.ty(node);
        match self.ast.node(node).kind.clone() {
            NodeKind::Block { .. } => Expr::Block(self.lower_block(node)),
            NodeKind::Lit(lit) => Expr::Lit(lit, ty),
            NodeKind::InterpolatedStr { parts } => Expr::Intrinsic {
                name: Symbol::new("format"),
                args: parts.iter().map(|&p| self.lower_expr(p)).collect(),
                ty,
            },
            NodeKind::Path { .. } => self.lower_name(node, ty),
            NodeKind::FieldAccess { base, name } => {
                // A resolved namespace member is a global reference.
                if let Some(def) = self.resolved_def(node) {
                    return self.global_or_local(def, ty);
                }
                let base = autoderef(self.lower_expr(base));
                Expr::Field {
                    base: Box::new(base),
                    name,
                    ty,
                }
            }
            NodeKind::TupleIndex { base, index } => {
                let base = autoderef(self.lower_expr(base));
                Expr::TupleIndex {
                    base: Box::new(base),
                    index,
                    ty,
                }
            }
            NodeKind::Call { callee, args } => {
                let callee = Box::new(self.lower_expr(callee));
                let args = args.iter().map(|&a| self.lower_expr(a)).collect();
                Expr::Call {
                    callee,
                    args,
                    builtin: None,
                    ty,
                }
            }
            NodeKind::GenericApply { base, .. } => self.lower_expr(base),
            // An arithmetic operator that resolved through an operator trait
            // lowers to a **uniform** call to the chosen method — the same shape
            // for a primitive `i32 + i32` and a user `Vec3 + Vec3` (§6). The
            // `builtin` tag lets codegen recognize the primitive case in O(1).
            NodeKind::Binary { op, lhs, rhs } => match self.ast.meta::<OpResolution>(node) {
                Some(res) => self.lower_op_call(res, lhs, rhs, ty),
                None => Expr::Binary {
                    op,
                    lhs: Box::new(self.lower_expr(lhs)),
                    rhs: Box::new(self.lower_expr(rhs)),
                    ty,
                },
            },
            NodeKind::Unary { op, operand } => match op {
                UnOp::Ref | UnOp::RefMut => Expr::Ref {
                    mutable: matches!(op, UnOp::RefMut),
                    place: Box::new(self.lower_expr(operand)),
                    ty,
                },
                _ => Expr::Unary {
                    op,
                    operand: Box::new(self.lower_expr(operand)),
                    ty,
                },
            },
            NodeKind::Deref { base } => Expr::Deref {
                base: Box::new(self.lower_expr(base)),
                ty,
            },
            NodeKind::Index { base, index } => {
                let base = autoderef(self.lower_expr(base));
                Expr::Index {
                    base: Box::new(base),
                    index: Box::new(self.lower_expr(index)),
                    ty,
                }
            }
            NodeKind::Slice { base, range } => Expr::Intrinsic {
                name: Symbol::new("slice"),
                args: vec![self.lower_expr(base), self.lower_expr(range)],
                ty,
            },
            // A range is not an intrinsic: it is a value of the `#lang("range")`
            // enum, one variant per surface form so the bound count and the
            // `..<` / `..=` distinction survive lowering.
            NodeKind::Range { start, end, kind } => {
                let closed = kind == RangeKind::Closed;
                let (name, args) = match (start, end) {
                    (None, None) => ("full", Vec::new()),
                    (Some(s), None) => ("from", vec![self.lower_expr(s)]),
                    (None, Some(e)) => (
                        if closed { "to_inclusive" } else { "to" },
                        vec![self.lower_expr(e)],
                    ),
                    (Some(s), Some(e)) => (
                        if closed { "inclusive" } else { "exclusive" },
                        vec![self.lower_expr(s), self.lower_expr(e)],
                    ),
                };
                Expr::Variant {
                    name: Symbol::new(name),
                    args,
                    ty,
                }
            }
            NodeKind::Tuple { elems } => Expr::Tuple {
                elems: elems.iter().map(|&e| self.lower_expr(e)).collect(),
                ty,
            },
            NodeKind::If { cond, then, els } => Expr::If {
                cond: Box::new(self.lower_expr(cond)),
                then: self.lower_block(then),
                els: els.map(|e| self.lower_block(e)),
                ty,
            },
            NodeKind::IfMatch {
                pattern,
                value,
                then,
                els,
            } => {
                // `if match p := v { then } else { els }` -> a two-arm match.
                let scrutinee = Box::new(self.lower_expr(value));
                let then_block = self.lower_block(then);
                let then_ty = then_block.ty.clone();
                let mut arms = vec![Arm {
                    pattern: self.lower_pattern(pattern),
                    guard: None,
                    body: Expr::Block(then_block),
                }];
                let else_body = match els {
                    Some(e) => Expr::Block(self.lower_block(e)),
                    None => Expr::Tuple {
                        elems: vec![],
                        ty: Ty::Void,
                    },
                };
                arms.push(Arm {
                    pattern: Pattern::Wildcard,
                    guard: None,
                    body: else_body,
                });
                let _ = then_ty;
                Expr::Match {
                    scrutinee,
                    arms,
                    ty,
                }
            }
            NodeKind::MatchExpr { scrutinee, arms } => Expr::Match {
                scrutinee: Box::new(self.lower_expr(scrutinee)),
                arms: arms
                    .iter()
                    .filter_map(|&a| self.lower_match_arm(a))
                    .collect(),
                ty,
            },
            NodeKind::Loop { body } => Expr::Loop {
                body: self.lower_block(body),
                ty,
            },
            NodeKind::While { cond, body } => self.lower_while(cond, body, ty),
            NodeKind::VariantLit { name, args } => Expr::Variant {
                name,
                args: self.lower_variant_args(&args),
                ty,
            },
            NodeKind::CompositeLit { ty: ty_node, body } => {
                self.lower_composite(ty_node, &body, ty)
            }
            NodeKind::IntrinsicCall { name, args, .. } => Expr::Intrinsic {
                name,
                args: args.iter().map(|&a| self.lower_expr(a)).collect(),
                ty,
            },
            NodeKind::Arg { value, .. } => self.lower_expr(value),
            // Closures / nested-function values are not lowered in the bootstrap.
            _ => Expr::Error(ty),
        }
    }

    /// Lower an arithmetic operator to a uniform call to its resolved method.
    /// The callee is the trait/impl method as a global; its function type is
    /// reconstructed from the operand and result types so the IR stays fully
    /// typed. `builtin` carries through the primitive-op tag for codegen.
    fn lower_op_call(&mut self, res: OpResolution, lhs: NodeId, rhs: NodeId, ty: Ty) -> Expr {
        let lty = self.ty(lhs);
        let rty = self.ty(rhs);
        let callee_ty = Ty::Func {
            params: vec![lty, rty],
            ret: Box::new(ty.clone()),
        };
        let callee = Box::new(Expr::Global(res.method, callee_ty));
        let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
        Expr::Call {
            callee,
            args,
            builtin: res.builtin,
            ty,
        }
    }

    /// `while cond { body }` -> `loop { if <not cond> { break }; <body...> }`.
    fn lower_while(&mut self, cond: NodeId, body: NodeId, ty: Ty) -> Expr {
        let cond_expr = self.lower_expr(cond);
        let not_cond = Expr::Unary {
            op: UnOp::Not,
            operand: Box::new(cond_expr),
            ty: Ty::Bool,
        };
        let break_block = Block {
            stmts: vec![Stmt::Break(None)],
            tail: None,
            ty: Ty::Void,
            defers: Vec::new(),
        };
        let guard = Stmt::Expr(Expr::If {
            cond: Box::new(not_cond),
            then: break_block,
            els: None,
            ty: Ty::Void,
        });
        let mut body_block = self.lower_block(body);
        body_block.stmts.insert(0, guard);
        // A loop body yields nothing.
        if let Some(tail) = body_block.tail.take() {
            body_block.stmts.push(Stmt::Expr(*tail));
        }
        body_block.ty = Ty::Void;
        Expr::Loop {
            body: body_block,
            ty,
        }
    }

    fn lower_variant_args(&mut self, args: &VariantArgs) -> Vec<Expr> {
        match args {
            VariantArgs::None => vec![],
            VariantArgs::Tuple(ids) => ids.iter().map(|&a| self.lower_expr(a)).collect(),
            VariantArgs::Record(ids) => ids
                .iter()
                .map(|&f| match self.ast.node(f).kind.clone() {
                    NodeKind::FieldInit { value, .. } => self.lower_expr(value),
                    _ => self.lower_expr(f),
                })
                .collect(),
        }
    }

    fn lower_composite(&mut self, ty_node: Option<NodeId>, body: &CompositeBody, ty: Ty) -> Expr {
        match body {
            CompositeBody::Named(fields) => {
                let def = ty_node.and_then(|t| self.type_def(t));
                let fields = fields
                    .iter()
                    .filter_map(|&f| match self.ast.node(f).kind.clone() {
                        NodeKind::FieldInit { name, value } => Some((name, self.lower_expr(value))),
                        _ => None,
                    })
                    .collect();
                match def {
                    Some(def) => Expr::Construct { def, fields, ty },
                    // An inferred `.{ ... }` whose target type isn't a plain
                    // nominal: keep the field values as an intrinsic aggregate.
                    None => Expr::Intrinsic {
                        name: Symbol::new("aggregate"),
                        args: fields.into_iter().map(|(_, e)| e).collect(),
                        ty,
                    },
                }
            }
            CompositeBody::Positional(elems) => Expr::Intrinsic {
                name: Symbol::new("array"),
                args: elems.iter().map(|&e| self.lower_expr(e)).collect(),
                ty,
            },
            CompositeBody::Repeat { value, count } => Expr::Intrinsic {
                name: Symbol::new("repeat"),
                args: vec![self.lower_expr(*value), self.lower_expr(*count)],
                ty,
            },
        }
    }

    // ===< match arms / patterns >===

    fn lower_match_arm(&mut self, node: NodeId) -> Option<Arm> {
        let NodeKind::MatchArm {
            pattern,
            guard,
            body,
        } = self.ast.node(node).kind.clone()
        else {
            return None;
        };
        Some(Arm {
            pattern: self.lower_pattern(pattern),
            guard: guard.map(|g| self.lower_expr(g)),
            body: self.lower_expr(body),
        })
    }

    fn lower_pattern(&mut self, node: NodeId) -> Pattern {
        match self.ast.node(node).kind.clone() {
            NodeKind::WildcardPat => Pattern::Wildcard,
            NodeKind::BindingPat { name, .. } => match self.def_of(node) {
                Some(def) => Pattern::Binding { def, name },
                None => Pattern::Wildcard,
            },
            NodeKind::LitPat(lit) => Pattern::Lit(lit),
            NodeKind::VariantPat { name, args } => Pattern::Variant {
                name,
                sub: self.lower_variant_pat_args(&args),
            },
            NodeKind::TuplePat { elems } => {
                Pattern::Tuple(elems.iter().map(|&e| self.lower_pattern(e)).collect())
            }
            NodeKind::OrPat { alternatives } => Pattern::Or(
                alternatives
                    .iter()
                    .map(|&a| self.lower_pattern(a))
                    .collect(),
            ),
            NodeKind::AtPat { name, pattern } => match self.def_of(node) {
                Some(def) => Pattern::At {
                    binding: Binding { def, name },
                    pattern: Box::new(self.lower_pattern(pattern)),
                },
                None => self.lower_pattern(pattern),
            },
            NodeKind::RefPat { pattern } => {
                Pattern::Deref(Box::new(self.lower_pattern(pattern)))
            }
            NodeKind::StructPat { path, fields, rest } => Pattern::Struct {
                def: path.and_then(|p| self.resolved_def(p)),
                fields: fields.iter().filter_map(|&f| self.lower_field_pat(f)).collect(),
                rest,
            },
            NodeKind::TupleStructPat { path, elems, rest } => Pattern::TupleStruct {
                def: self.resolved_def(path),
                elems: elems.iter().map(|&e| self.lower_pattern(e)).collect(),
                rest,
            },
            NodeKind::SlicePat { elems, rest } => {
                // The `..` splits the element patterns: those before it match
                // from the front, those after it from the back.
                let split = rest.as_ref().map_or(elems.len(), |r| r.at.min(elems.len()));
                let lowered: Vec<Pattern> =
                    elems.iter().map(|&e| self.lower_pattern(e)).collect();
                let (prefix, suffix) = lowered.split_at(split);
                Pattern::Slice {
                    prefix: prefix.to_vec(),
                    // The rest's own binding lives on the `SlicePat` node.
                    rest: rest.map(|r| {
                        r.name
                            .zip(self.def_of(node))
                            .map(|(name, def)| Binding { def, name })
                    }),
                    suffix: suffix.to_vec(),
                }
            }
            NodeKind::RangePat { start, end, kind } => Pattern::Range {
                start: start.and_then(|s| self.lit_of(s)),
                end: end.and_then(|e| self.lit_of(e)),
                inclusive: kind == RangeKind::Closed,
            },
            // A glob pattern is an import form, not a value test.
            _ => Pattern::Wildcard,
        }
    }

    /// Lower one `FieldPat` of a struct pattern to its `name → sub-pattern`
    /// pair; the `{ name }` shorthand binds the field under its own name.
    fn lower_field_pat(&mut self, f: NodeId) -> Option<(Symbol, Pattern)> {
        let NodeKind::FieldPat { name, pattern, .. } = self.ast.node(f).kind.clone() else {
            return None;
        };
        let sub = match pattern {
            Some(p) => self.lower_pattern(p),
            None => match self.def_of(f) {
                Some(def) => Pattern::Binding {
                    def,
                    name: name.clone(),
                },
                None => Pattern::Wildcard,
            },
        };
        Some((name, sub))
    }

    /// The literal a range-pattern bound names, if it is one.
    fn lit_of(&self, node: NodeId) -> Option<Lit> {
        match self.ast.node(node).kind.clone() {
            NodeKind::LitPat(l) | NodeKind::Lit(l) => Some(l),
            _ => None,
        }
    }

    fn lower_variant_pat_args(&mut self, args: &VariantPatArgs) -> Vec<Pattern> {
        match args {
            VariantPatArgs::None => vec![],
            VariantPatArgs::Tuple(ids) => ids.iter().map(|&p| self.lower_pattern(p)).collect(),
            VariantPatArgs::Record { fields, .. } => fields
                .iter()
                .map(|&f| match self.ast.node(f).kind.clone() {
                    NodeKind::FieldPat {
                        pattern: Some(p), ..
                    } => self.lower_pattern(p),
                    NodeKind::FieldPat {
                        name,
                        pattern: None,
                        ..
                    } => match self.def_of(f) {
                        Some(def) => Pattern::Binding { def, name },
                        None => Pattern::Wildcard,
                    },
                    _ => Pattern::Wildcard,
                })
                .collect(),
        }
    }

    // ===< names / helpers >===

    fn lower_name(&mut self, node: NodeId, ty: Ty) -> Expr {
        match self.resolved_def(node) {
            Some(def) => self.global_or_local(def, ty),
            None => Expr::Error(ty),
        }
    }

    fn global_or_local(&self, def: DefId, ty: Ty) -> Expr {
        match self.defs.get(def).kind {
            DefKind::Local | DefKind::Param => Expr::Local(def, ty),
            _ => Expr::Global(def, ty),
        }
    }

    /// The type def a `TypePath`/`Path` type node names, if it is a struct/enum.
    fn type_def(&self, node: NodeId) -> Option<DefId> {
        // A composite head may be `Type` (Path/TypePath) or `Type.<args>`
        // (GenericApply); resolve through to the head's def either way.
        let head = match self.ast.node(node).kind.clone() {
            NodeKind::GenericApply { base, .. } => base,
            _ => node,
        };
        let def = self.resolved_def(head)?;
        matches!(self.defs.get(def).kind, DefKind::Struct | DefKind::Enum).then_some(def)
    }

    fn ty(&self, node: NodeId) -> Ty {
        self.ast.meta::<Ty>(node).unwrap_or(Ty::Error)
    }

    fn def_of(&self, node: NodeId) -> Option<DefId> {
        self.ast.meta::<DefMeta>(node).map(|m| m.0)
    }

    fn resolved_def(&self, node: NodeId) -> Option<DefId> {
        match self.ast.meta::<Resolution>(node)? {
            Resolution::Def(d) => Some(self.defs.resolve_alias(d)),
            _ => None,
        }
    }
}

/// Wrap `base` in an explicit [`Expr::Deref`] if its type is a pointer, so
/// auto-deref field/index access is spelled out in the IR (§3.2).
fn autoderef(base: Expr) -> Expr {
    if let Ty::Ptr { inner, .. } = base.ty().clone() {
        Expr::Deref {
            base: Box::new(base),
            ty: *inner,
        }
    } else {
        base
    }
}
