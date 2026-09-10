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
//! - a **method call** becomes an ordinary [`ir::Expr::Call`] whose `args[0]` is
//!   the receiver: whatever the `self` parameter wanted — a value, a `*Self`, a
//!   `*mut Self` — is spelled out as an `&` / `&mut` / `.*` here, using the
//!   adjustment [`super::infer`] recorded, and the call carries the
//!   [`ir::Dispatch`] that says whether the callee is a known function, a vtable
//!   slot, or a bound awaiting monomorphization.
//! - an **operator** becomes the same kind of call to the method its `#lang`
//!   trait resolved to (§6.13) — including `a[i]`, which on a user type is
//!   `Index.index(&a, i).*`, and `a < b`, which on a user type tests the
//!   `Ordering` that `Ord.cmp` returns.
//!
//! Not lowered here: closures / nested-function values (they need a captured
//! environment, which is a representation decision the IR does not make), and
//! `match` decision trees — arms stay structured, patterns keep their full
//! shape, and compiling them to tests is a later, CFG-level pass.

use std::collections::HashMap;

use crate::common::source::FileId;
use crate::common::symbol::Symbol;
use crate::parser::ast::{
    Ast, BinOp, CompositeBody, Lit, NodeId, NodeKind, RangeKind, UnOp, VariantArgs, VariantPatArgs,
};

use super::def::{DefId, DefKind, DefTable, LangItems};
use super::infer::OpResolution;
use super::infer::{
    ArgOrder, Coercion, DistinctRecv, DynCoerce, MethodDispatch, MethodRes, RecvAdjust,
    SliceCoerce, Upcast,
};
use super::ty::Ty;
use super::{DefMeta, Resolution};
use crate::ir::{
    Arm, Binding, Block, Dispatch, Expr, Function, Param, Pattern, Program, Recv, Stmt,
};

/// Lower every function body in `file` to IR.
pub fn lower_file(
    defs: &DefTable,
    lang: &LangItems,
    asts: &HashMap<FileId, Ast>,
    file: FileId,
) -> Program {
    let ast = &asts[&file];
    let mut lo = Lowerer {
        defs,
        lang,
        ast,
        asts,
        defaults: HashMap::new(),
        defers: Vec::new(),
    };
    // Iterate the `Func` defs of this file: each carries the name/DefId and its
    // node is the `ConstBind` whose RHS is the `FuncExpr`. A **bodyless** one is
    // emitted too, with `body: None` — an `extern("c") func` is a real symbol
    // the linker must resolve and a call to it is an ordinary call, so dropping
    // it here would lose the only record of its signature and ABI.
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
            // A bodyless, non-`extern` function is a trait's *requirement* — a
            // signature an impl must satisfy, with no code and no symbol of its
            // own. It is reachable as a `Dispatch::Virtual` slot through its
            // def; emitting it here would put a body-less stub in the program
            // that nothing links.
            if f.body.is_some() || f.extern_abi.is_some() {
                funcs.push(f);
            }
        }
    }
    Program { funcs }
}

struct Lowerer<'a> {
    defs: &'a DefTable,
    /// The `#lang` registry, consulted for the types an operator's desugaring
    /// mentions but the surface syntax never wrote — `Ordering`, for the `cmp`
    /// a user-type comparison lowers to.
    lang: &'a LangItems,
    /// The file currently being lowered. Swapped, briefly, while a **default
    /// argument** declared in another file is lowered — the default's nodes and
    /// their inferred types live in that file's arena, not this one.
    ast: &'a Ast,
    /// The whole parsed program, so a call can reach the declaration of a callee
    /// in another file to lower its defaults (a call into `core` is the common
    /// case).
    asts: &'a HashMap<FileId, Ast>,
    /// Lowered default arguments, per callee, in **value-parameter** order.
    ///
    /// Each default is lowered exactly once and cloned into the call sites that
    /// omit it, rather than re-lowered per site: the default is one expression
    /// written in one place, and anything later that walks it — the `#const`
    /// check above all — should see it once.
    defaults: HashMap<DefId, Vec<Option<Expr>>>,
    /// Stack of pending `defer` bodies, one frame per open block (innermost last).
    defers: Vec<Vec<Expr>>,
}

impl Lowerer<'_> {
    fn lower_function(&mut self, def: DefId, func: NodeId) -> Option<Function> {
        let NodeKind::FuncExpr {
            params,
            body,
            extern_abi,
            ..
        } = self.ast.node(func).kind.clone()
        else {
            return None;
        };
        let name = self.defs.get(def).name.clone();
        let ret = self.ty(func);
        let params: Vec<Param> = params.iter().filter_map(|&p| self.lower_param(p)).collect();
        let recv = recv_of(&params);
        let mutating = params.iter().any(|p| grants_mutation(&p.ty));
        let body = body.map(|b| self.lower_block(b));
        Some(Function {
            def,
            name,
            params,
            ret,
            body,
            extern_abi,
            directives: self.defs.get(def).directives.clone(),
            recv,
            mutating,
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
    /// an IR `Let`.
    ///
    /// The pattern travels whole: a plain `let x := e` is a
    /// [`Pattern::Binding`], and a destructuring `let (a, b) := p` keeps its
    /// tuple/struct shape so the names it introduces survive. A `let` pattern is
    /// irrefutable, so unlike a `match` arm there is nothing to fall back to.
    fn lower_binding(&mut self, pattern: NodeId, value: NodeId, out: &mut Vec<Stmt>) {
        let init = self.lower_expr(value);
        out.push(Stmt::Let {
            pattern: self.lower_pattern(pattern),
            ty: init.ty().clone(),
            init,
        });
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
        // A comptime literal converting into a runtime type: emit the `$cast`
        // the surface syntax left implicit.
        if let Some(c) = self.ast.meta::<Coercion>(node) {
            let value = self.lower_expr_inner(node);
            return Expr::Intrinsic {
                name: Symbol::new("cast"),
                args: vec![value],
                ty: c.to,
            };
        }
        // A `[N]T` reaching a `[]T`: the view is the whole sub-slice, so emit
        // exactly what `a[..]` emits.
        if let Some(sc) = self.ast.meta::<SliceCoerce>(node) {
            let value = self.lower_expr_inner(node);
            let full = Expr::Variant {
                name: Symbol::new("full"),
                args: Vec::new(),
                ty: sc.range,
            };
            return Expr::Intrinsic {
                name: Symbol::new("slice"),
                args: vec![value, full],
                ty: sc.to,
            };
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
                def: Some(up.field),
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
                name: name.clone(),
                def: Some(up.field),
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
                // A resolved namespace member is a global reference; a resolved
                // *field* is a projection out of the base value, and carries the
                // field's own def (see [`crate::sema::fields`]).
                let def = self.resolved_def(node);
                match def.map(|d| self.defs.get(d).kind) {
                    Some(DefKind::Field) | None => {}
                    Some(_) => return self.lower_name(node, ty),
                }
                let base = autoderef(self.lower_expr(base));
                Expr::Field {
                    base: Box::new(base),
                    name,
                    def,
                    ty,
                }
            }
            NodeKind::TupleIndex { base, index } => {
                let base = autoderef(self.lower_expr(base));
                // On a tuple struct this is a field projection out of a nominal
                // type, and `fields` bound it to the field's def: emit the same
                // `Expr::Field` a named access does, so every later stage reads
                // the offset off the struct's own definition (directives
                // included) rather than re-deriving it positionally.
                if let Some(def) = self.resolved_def(node) {
                    return Expr::Field {
                        base: Box::new(base),
                        name: Symbol::new(&index.to_string()),
                        def: Some(def),
                        ty,
                    };
                }
                Expr::TupleIndex {
                    base: Box::new(base),
                    index,
                    ty,
                }
            }
            NodeKind::Call { callee, args } => self.lower_call(callee, &args, ty),
            NodeKind::GenericApply { base, .. } => self.lower_expr(base),
            // An arithmetic operator that resolved through an operator trait
            // lowers to a **uniform** call to the chosen method — the same shape
            // for a primitive `i32 + i32` and a user `Vec3 + Vec3` (§6). The
            // `builtin` tag lets codegen recognize the primitive case in O(1).
            NodeKind::Binary { op, lhs, rhs } => match self.ast.meta::<OpResolution>(node) {
                // A comparison resolves to `Eq.eq` / `Ord.cmp`, whose results
                // are not the comparison's own — they need the test around them.
                Some(res) if is_comparison(op) => self.lower_cmp(res, op, lhs, rhs),
                Some(res) => {
                    let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
                    self.op_call(res, args, ty)
                }
                // `&&` / `||` and the comparisons of the numeric core dispatch
                // on nothing: they stay primitive (§6.13).
                None => Expr::Binary {
                    op,
                    lhs: Box::new(self.lower_expr(lhs)),
                    rhs: Box::new(self.lower_expr(rhs)),
                    ty,
                },
            },
            // `&x` / `&mut x` are the built-in pointer operations, not trait
            // calls (§6.13); `-x` and `~x` are, and lower to the same uniform
            // call shape a binary operator does.
            NodeKind::Unary { op, operand } => match op {
                UnOp::Ref | UnOp::RefMut => Expr::Ref {
                    mutable: matches!(op, UnOp::RefMut),
                    place: Box::new(self.lower_expr(operand)),
                    ty,
                },
                _ => match self.ast.meta::<OpResolution>(node) {
                    Some(res) => {
                        let args = vec![self.lower_expr(operand)];
                        self.op_call(res, args, ty)
                    }
                    None => Expr::Unary {
                        op,
                        operand: Box::new(self.lower_expr(operand)),
                        ty,
                    },
                },
            },
            NodeKind::Deref { base } => Expr::Deref {
                base: Box::new(self.lower_expr(base)),
                ty,
            },
            NodeKind::Index { base, index } => match self.ast.meta::<OpResolution>(node) {
                // A user type indexes through `Index` / `IndexMut`, which hand
                // back a *pointer* to the element: `a[i]` is `index(&a, i).*`.
                Some(res) => self.lower_index_call(res, base, index, ty),
                // Arrays and slices index directly.
                None => {
                    let base = autoderef(self.lower_expr(base));
                    Expr::Index {
                        base: Box::new(base),
                        index: Box::new(self.lower_expr(index)),
                        ty,
                    }
                }
            },
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
            NodeKind::CompositeLit { body, .. } => self.lower_composite(&body, ty),
            NodeKind::IntrinsicCall { name, args, .. } => {
                let mut lowered: Vec<Expr> =
                    args.iter().map(|&a| self.lower_expr(a)).collect();
                // `$len(a)` folds on a fixed array — the length is part of the
                // type, so there is nothing left to compute at run time. This is
                // the whole of `core`'s `Len` impls once they are inlined.
                if name.as_str() == "len" && lowered.len() == 1 {
                    return len_expr(autoderef(lowered.remove(0)), ty);
                }
                // An explicit `$cast.<*dyn Trait>(p)` builds the same fat
                // pointer the implicit coercion does; it is a spelling of the
                // unsizing, not a reinterpretation of bits, so it lowers to the
                // same node (§3.4).
                if name.as_str() == "cast" && lowered.len() == 1 {
                    if let Some(cast) = dyn_cast(lowered[0].clone(), &ty) {
                        return cast;
                    }
                }
                Expr::Intrinsic {
                    name,
                    args: lowered,
                    ty,
                }
            }
            NodeKind::Arg { value, .. } => self.lower_expr(value),
            // A nested item — a local `func`, `struct`, or `import` written
            // among a block's statements — is a *definition*, not a step the
            // block runs. It was collected and lowered on its own (a nested
            // `func` becomes its own [`Function`]), so here it contributes
            // nothing to the enclosing body.
            NodeKind::Decl { .. }
            | NodeKind::ConstBind { .. }
            | NodeKind::NamespaceExpr { .. }
            | NodeKind::ImplBlock { .. }
            | NodeKind::Import { .. } => Expr::Tuple {
                elems: Vec::new(),
                ty: Ty::Void,
            },
            // Closures and nested-function *values* are the one expression form
            // that has no IR yet: they need a captured environment, which is a
            // representation decision the IR does not make (see the module doc).
            _ => Expr::Error(ty),
        }
    }

    // ===< calls >===

    /// Lower a call. A method call (`recv.m(args)`) and a free call
    /// (`f(args)`) become the **same** [`Expr::Call`]: the difference is that
    /// the method's receiver is `args[0]`, adjusted to what its `self` parameter
    /// wants, and that its [`Dispatch`] may be virtual or generic.
    fn lower_call(&mut self, callee: NodeId, args: &[NodeId], ty: Ty) -> Expr {
        // `recv.m.<T>(x)` — the resolution sits on the field access the
        // turbofish wraps.
        let head = match self.ast.node(callee).kind.clone() {
            NodeKind::GenericApply { base, .. } => base,
            _ => callee,
        };
        if let Some(res) = self.ast.meta::<MethodRes>(head) {
            return self.lower_method_call(head, res, args, ty);
        }
        // `Pair(1, 2)` is not a call at all: a callee naming a type constructs
        // it (§3.3). It builds the same `Expr::Construct` the positional
        // composite literal `.{1, 2}` builds — a tuple struct has one
        // representation in the IR regardless of which syntax reached it.
        // Named arguments were bound to their parameters during inference; take
        // the order it recorded so the IR is positional, always.
        // Named arguments were bound and omitted defaults left as holes during
        // inference; take the slots it recorded, or the arguments as written when
        // the call needed neither.
        let written: Vec<Option<NodeId>>;
        let slots: &[Option<NodeId>] = match self.ast.meta::<ArgOrder>(head) {
            Some(o) => {
                written = o.args;
                &written
            }
            None => {
                written = args.iter().copied().map(Some).collect();
                &written
            }
        };
        if let Some(def) = self.construct_target(head, &ty) {
            // A tuple struct's fields are positions, not parameters: they take no
            // defaults and reject named arguments, so every slot is written.
            let fields = self
                .lower_args(None, slots)
                .into_iter()
                .enumerate()
                .map(|(i, e)| (Symbol::new(&i.to_string()), e))
                .collect();
            return Expr::Construct { def, fields, ty };
        }
        let target = self.resolved_def(head);
        let callee = Box::new(self.lower_expr(callee));
        let args = self.lower_args(target, slots);
        Expr::Call {
            callee,
            args,
            builtin: None,
            dispatch: Dispatch::Static,
            ty,
        }
    }

    /// Lower a call's argument slots, filling each `None` — a parameter the call
    /// left out — with the callee's default for that position (§5.2).
    ///
    /// This is where a default becomes real. Inference deliberately left the
    /// hole: filling it there would mean inferring the default expression once
    /// per call site, stamping conflicting types on the one set of AST nodes the
    /// declaration owns. Here there is no such conflict — the default is lowered
    /// once against its declaration and the result is *cloned* into each site,
    /// so a default like `.{}` still builds a fresh value per call.
    fn lower_args(&mut self, callee: Option<DefId>, slots: &[Option<NodeId>]) -> Vec<Expr> {
        let mut out = Vec::with_capacity(slots.len());
        for (i, slot) in slots.iter().enumerate() {
            match slot {
                Some(a) => out.push(self.lower_expr(*a)),
                None => {
                    let d = callee.and_then(|c| self.param_default(c, i));
                    // A hole with no default behind it means inference and
                    // lowering disagree about the signature; a typed `Error`
                    // keeps the IR well-formed rather than dropping an argument
                    // and silently changing the call's arity.
                    out.push(d.unwrap_or(Expr::Error(Ty::Error)));
                }
            }
        }
        out
    }

    /// The lowered default of `def`'s `i`-th **value** parameter, if it has one.
    ///
    /// `self` is excluded from the numbering, matching how inference counts the
    /// arguments of a method call.
    fn param_default(&mut self, def: DefId, i: usize) -> Option<Expr> {
        if let Some(cached) = self.defaults.get(&def) {
            return cached.get(i).cloned().flatten();
        }
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return None;
        };
        let ast = self.asts.get(&file)?;
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let NodeKind::FuncExpr { params, .. } = ast.node(rhs).kind.clone() else {
            return None;
        };
        let slots: Vec<Option<NodeId>> = params
            .iter()
            .filter_map(|&p| match &ast.node(p).kind {
                NodeKind::Param { name, default, .. } if name.as_str() != "self" => Some(*default),
                _ => None,
            })
            .collect();
        // Lower in the *declaring* file's context: the default's nodes, and the
        // types inference stamped on them, live in that arena.
        let saved = std::mem::replace(&mut self.ast, ast);
        let lowered: Vec<Option<Expr>> = slots
            .into_iter()
            .map(|s| s.map(|n| self.lower_expr(n)))
            .collect();
        self.ast = saved;
        let out = lowered.get(i).cloned().flatten();
        self.defaults.insert(def, lowered);
        out
    }

    /// The struct a call's callee names, when the callee is a type rather than a
    /// function — the construction form. `None` for an ordinary call.
    fn construct_target(&self, callee: NodeId, ty: &Ty) -> Option<DefId> {
        if !matches!(self.ast.node(callee).kind, NodeKind::Path { .. }) {
            return None;
        }
        let def = self.resolved_def(callee)?;
        if self.defs.get(def).kind != DefKind::Struct {
            return None;
        }
        // Name the def the *type* settled on, not the one the path resolved to:
        // an alias resolves to its target here the same way it does elsewhere.
        match ty {
            Ty::Nominal { def, .. } => Some(*def),
            _ => None,
        }
    }

    /// Lower `recv.m(args)` using the resolution inference stamped on the
    /// `recv.m` node.
    ///
    /// The receiver is not a field being read: it is the first *argument*, and
    /// whatever the `self` parameter asked for — a value, a `*Self`, a
    /// `*mut Self` — is spelled out here as an explicit `&` / `&mut` / `.*`, so
    /// nothing downstream has to re-derive an implicit adjustment.
    fn lower_method_call(
        &mut self,
        callee: NodeId,
        res: MethodRes,
        args: &[NodeId],
        ty: Ty,
    ) -> Expr {
        let NodeKind::FieldAccess { base, .. } = self.ast.node(callee).kind.clone() else {
            return Expr::Error(ty);
        };
        let mut recv = self.lower_expr(base);
        // The method was found on the type this `distinct` type is distinct from
        // (§2.4). Representations are identical, so reaching it is a
        // reinterpretation — but the method's `self` is typed as the
        // representation, so say so before the `&` / `.*` adjustment runs.
        if let Some(d) = self.ast.meta::<DistinctRecv>(base) {
            recv = Expr::Intrinsic {
                name: Symbol::new("cast"),
                args: vec![recv],
                ty: d.repr.clone(),
            };
        }
        let written: Vec<Option<NodeId>>;
        let slots: &[Option<NodeId>] = match self.ast.meta::<ArgOrder>(callee) {
            Some(o) => {
                written = o.args;
                &written
            }
            None => {
                written = args.iter().copied().map(Some).collect();
                &written
            }
        };
        let mut call_args = vec![adjust_recv(recv, res.adjust, &res.self_ty)];
        let lowered = self.lower_args(Some(res.method), slots);
        call_args.extend(lowered);
        // Inference recorded the instantiated signature on this node; falling
        // back to a reconstruction keeps the IR typed if it did not.
        let callee_ty = match self.ty(callee) {
            f @ Ty::Func { .. } => f,
            _ => Ty::Func {
                params: call_args.iter().map(|a| a.ty().clone()).collect(),
                ret: Box::new(ty.clone()),
            },
        };
        let dispatch = match res.dispatch {
            MethodDispatch::Static => Dispatch::Static,
            MethodDispatch::Virtual(trait_def) => Dispatch::Virtual {
                trait_def,
                method: res.method,
            },
            MethodDispatch::Generic(trait_def) => Dispatch::Generic {
                trait_def,
                method: res.method,
                self_ty: res.self_ty.clone(),
            },
        };
        Expr::Call {
            callee: Box::new(Expr::Global(res.method, callee_ty)),
            args: call_args,
            builtin: None,
            dispatch,
            ty,
        }
    }

    /// Lower an operator to a uniform call to its resolved method. The callee is
    /// the trait/impl method as a global; its function type is reconstructed
    /// from the (already lowered) argument and result types so the IR stays
    /// fully typed. `builtin` carries through the primitive-op tag for codegen.
    ///
    /// Operands are lowered by the caller on purpose: one that coerces (a
    /// `comptime_int` literal, say) presents its *converted* type to the call,
    /// so the reconstructed signature has to come from the lowered arguments.
    fn op_call(&mut self, res: OpResolution, args: Vec<Expr>, ty: Ty) -> Expr {
        let callee_ty = Ty::Func {
            params: args.iter().map(|a| a.ty().clone()).collect(),
            ret: Box::new(ty.clone()),
        };
        Expr::Call {
            callee: Box::new(Expr::Global(res.method, callee_ty)),
            args,
            builtin: res.builtin,
            dispatch: Dispatch::Static,
            ty,
        }
    }

    /// Lower a comparison that resolved to a user impl (§6.13).
    ///
    /// `==` is the method itself and `!=` its negation; the four relations all
    /// go through the *one* `Ord.cmp`, testing the `Ordering` it returns. That
    /// test is a `match`, not a primitive compare on the enum: the arms are what
    /// say which orderings count, and a decision-tree pass turns them into the
    /// discriminant check later.
    fn lower_cmp(&mut self, res: OpResolution, op: BinOp, lhs: NodeId, rhs: NodeId) -> Expr {
        let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            let call = self.op_call(res, args, Ty::Bool);
            return match op {
                BinOp::Ne => Expr::Unary {
                    op: UnOp::Not,
                    operand: Box::new(call),
                    ty: Ty::Bool,
                },
                _ => call,
            };
        }
        let ordering = match self.lang.get("ordering") {
            Some(d) => Ty::Nominal {
                def: self.defs.resolve_alias(d),
                args: Vec::new(),
            },
            None => Ty::Error,
        };
        let call = self.op_call(res, args, ordering);
        // Which single `Ordering` answers the relation, and whether landing on
        // it means `true`: `a < b` is "`.less`, yes"; `a >= b` is "`.less`, no".
        let (variant, hit) = match op {
            BinOp::Lt => ("less", true),
            BinOp::Ge => ("less", false),
            BinOp::Gt => ("greater", true),
            _ => ("greater", false),
        };
        Expr::Match {
            scrutinee: Box::new(call),
            arms: vec![
                Arm {
                    pattern: Pattern::Variant {
                        name: Symbol::new(variant),
                        sub: Vec::new(),
                    },
                    guard: None,
                    body: Expr::Lit(Lit::Bool(hit), Ty::Bool),
                },
                Arm {
                    pattern: Pattern::Wildcard,
                    guard: None,
                    body: Expr::Lit(Lit::Bool(!hit), Ty::Bool),
                },
            ],
            ty: Ty::Bool,
        }
    }

    /// Lower `a[i]` on a type that indexes through `Index` / `IndexMut`: the
    /// trait method takes the container by pointer and hands back a pointer to
    /// the element, so the surface `a[i]` is `index(&a, i).*` (§6.13).
    fn lower_index_call(
        &mut self,
        res: OpResolution,
        base: NodeId,
        index: NodeId,
        ty: Ty,
    ) -> Expr {
        // `IndexMut` is the write side; its `self` and its result are `*mut`.
        let mutable = self
            .lang
            .get("index_mut")
            .map(|t| self.defs.resolve_alias(t))
            == Some(res.trait_def);
        let base = self.lower_expr(base);
        let recv = match base.ty() {
            // Already a pointer (an auto-deref site): pass it straight through.
            Ty::Ptr { .. } => base,
            other => Expr::Ref {
                mutable,
                place: Box::new(base.clone()),
                ty: Ty::Ptr {
                    mutable,
                    inner: Box::new(other.clone()),
                },
            },
        };
        let index = self.lower_expr(index);
        let elem_ptr = Ty::Ptr {
            mutable,
            inner: Box::new(ty.clone()),
        };
        let call = self.op_call(res, vec![recv, index], elem_ptr);
        Expr::Deref {
            base: Box::new(call),
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

    /// Lower a composite literal to the construct its **type** calls for, not
    /// the syntax it was written with: `P { .. }` and `.{ .. }` both become an
    /// `Expr::Construct` for a struct, an `Expr::Tuple` for a tuple, `$array`
    /// for an array or slice. Inference has already checked the body fits, so
    /// the type is the authority here.
    fn lower_composite(&mut self, body: &CompositeBody, ty: Ty) -> Expr {
        match body {
            CompositeBody::Named(fields) => {
                let fields = fields
                    .iter()
                    .filter_map(|&f| match self.ast.node(f).kind.clone() {
                        NodeKind::FieldInit { name, value } => Some((name, self.lower_expr(value))),
                        _ => None,
                    })
                    .collect();
                match &ty {
                    Ty::Nominal { def, .. } => Expr::Construct {
                        def: *def,
                        fields,
                        ty,
                    },
                    // Named fields on a non-struct: already diagnosed.
                    _ => Expr::Error(ty),
                }
            }
            CompositeBody::Positional(elems) => {
                let elems: Vec<Expr> = elems.iter().map(|&e| self.lower_expr(e)).collect();
                match &ty {
                    Ty::Tuple(_) => Expr::Tuple { elems, ty },
                    // A tuple struct's members are positional but it is still a
                    // nominal construction; name the fields by their index.
                    Ty::Nominal { def, .. } => Expr::Construct {
                        def: *def,
                        fields: elems
                            .into_iter()
                            .enumerate()
                            .map(|(i, e)| (Symbol::new(&i.to_string()), e))
                            .collect(),
                        ty,
                    },
                    Ty::Array { .. } | Ty::Slice { .. } => Expr::Intrinsic {
                        name: Symbol::new("array"),
                        args: elems,
                        ty,
                    },
                    _ => Expr::Error(ty),
                }
            }
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
        // A static trait call (`Trait.member(args)`) resolves by name to the
        // trait's bodyless *declaration*; the solver stamped which impl won, so
        // point at that impl's member instead (see `infer::open_trait_self`).
        match (self.ast.meta::<OpResolution>(node), self.resolved_def(node)) {
            (Some(res), _) => self.global_or_local(res.method, ty),
            (None, Some(def)) => self.global_or_local(def, ty),
            (None, None) => Expr::Error(ty),
        }
    }

    fn global_or_local(&self, def: DefId, ty: Ty) -> Expr {
        match self.defs.get(def).kind {
            DefKind::Local | DefKind::Param => Expr::Local(def, ty),
            // A `<const N>` parameter has no storage to load from; it stands for
            // whatever value monomorphization substitutes.
            DefKind::ConstParam => Expr::ConstParam(def, ty),
            _ => Expr::Global(def, ty),
        }
    }

    /// The type def a `TypePath`/`Path` type node names, if it is a struct/enum.
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

/// The element count of an array or slice, however it was spelled (`a.len` or
/// `$len(a)`).
///
/// A fixed `[N]T` whose `N` is already known folds to the literal right here —
/// the length is part of the type, so there is nothing to compute. Everything
/// else keeps the `$len` intrinsic for a later stage to read (a slice header's
/// length) or substitute (`[N]T` still generic in `N`, resolved at
/// monomorphization).
fn len_expr(base: Expr, ty: Ty) -> Expr {
    if let Ty::Array { len, .. } = base.ty()
        && let Some(n) = len.value()
    {
        return Expr::Lit(Lit::Int(n.into()), ty);
    }
    Expr::Intrinsic {
        name: Symbol::new("len"),
        args: vec![base],
        ty,
    }
}

/// Apply the receiver adjustment a method call implies, spelling out in the IR
/// what the surface `x.m()` left to the type checker (§3.4).
fn adjust_recv(recv: Expr, adjust: RecvAdjust, self_ty: &Ty) -> Expr {
    match adjust {
        RecvAdjust::None => recv,
        RecvAdjust::Ref { mutable } => Expr::Ref {
            mutable,
            place: Box::new(recv),
            ty: self_ty.clone(),
        },
        RecvAdjust::Deref => Expr::Deref {
            base: Box::new(recv),
            ty: self_ty.clone(),
        },
    }
}

/// The [`Expr::DynCast`] an explicit `$cast.<*dyn Trait>(p)` denotes, when that
/// is what it is: a pointer value reaching a pointer-to-trait-object type. Any
/// other `$cast` is a real conversion and stays an intrinsic.
fn dyn_cast(value: Expr, to: &Ty) -> Option<Expr> {
    let Ty::Ptr { inner, .. } = to else {
        return None;
    };
    if !matches!(**inner, Ty::Dyn(_)) {
        return None;
    }
    let Ty::Ptr { inner: from, .. } = value.ty().clone() else {
        return None;
    };
    Some(Expr::DynCast {
        value: Box::new(value),
        concrete: *from,
        ty: to.clone(),
    })
}

/// How a lowered parameter list takes its receiver: the first parameter, if it
/// is named `self` (§3.4). Nest writes the receiver as an ordinary parameter, so
/// this is the one place that decides what counts as a method.
fn recv_of(params: &[Param]) -> Recv {
    let Some(first) = params.first() else {
        return Recv::None;
    };
    if first.name.as_str() != "self" {
        return Recv::None;
    }
    match &first.ty {
        Ty::Ptr { mutable: true, .. } => Recv::MutPtr,
        Ty::Ptr { mutable: false, .. } => Recv::Ptr,
        _ => Recv::Value,
    }
}

/// Whether a parameter of this type lets the callee write into the caller's
/// storage — a `*mut T` or a `[]mut T`, or one reached through a read-only
/// pointer or slice to such a thing (`*[]mut T` still hands out the mutable
/// view). Arrays, tuples and named types are *values*: their own `mut` marks
/// only the local copy.
fn grants_mutation(ty: &Ty) -> bool {
    match ty {
        Ty::Ptr { mutable, inner } | Ty::Slice { mutable, inner } => {
            *mutable || grants_mutation(inner)
        }
        _ => false,
    }
}

/// Whether a [`BinOp`] is one of the six comparisons, which reach `Eq` / `Ord`
/// by a method name that is not the operator's own.
fn is_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
    )
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
