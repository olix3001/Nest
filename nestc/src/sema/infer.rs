//! Type inference (§3.7) — the stage that gives every expression a [`Ty`].
//!
//! It runs **after** desugaring, so `for` / `.?` / `.!` are already gone and the
//! tree is the core control-flow forms. Inference is per **function body**: each
//! `func` with a body gets a fresh [`InferCtxt`], its parameters typed from the
//! signature, its locals typed as they are bound, and every expression node
//! annotated with a resolved [`Ty`] in the arena's metadata side table (read
//! back by [`super::lower`] and the pretty-printer).
//!
//! The engine is Hindley–Milner unification (see [`super::ty`]) with two
//! bootstrap-scoped simplifications, each a place a later pass slots in:
//!
//! - **Operators are typed by the builtin numeric rule**, not by resolving the
//!   `Add`/`Ord`/… `#lang` trait and picking an `impl`. `a + b` unifies its
//!   operands and yields their type; comparisons yield `bool`. Operator
//!   *overloading* for user types is the job of the (future) trait-selection
//!   pass; this stage is correct for the primitive numeric core.
//! - **Generics are not monomorphized.** A generic type parameter is treated as
//!   a rigid opaque ([`Ty::Nominal`] over the param's own def), and turbofish /
//!   `<Assoc = T>` arguments are recorded but not yet propagated into a full
//!   substitution. Closures are typed by their signature; captures are not
//!   threaded into the enclosing body's variables.

use std::collections::HashMap;

use crate::common::diagnostic::Diagnostic;
use crate::common::source::{FileId, FileSpan};
use crate::parser::ast::{Ast, BinOp, Lit, NodeId, NodeKind, UnOp};

use super::def::{DefKind, DefTable};
use super::ty::{primitive_ty, InferCtxt, Ty, TyVarKind};
use super::{DefMeta, Resolution};

/// Infer types for every function body in `file`, annotating each expression
/// node with its resolved [`Ty`]. `asts` is the whole parsed program (read-only)
/// so a field access can reach a struct declared in another file.
pub fn infer_file(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    diags: &mut Vec<Diagnostic>,
    file: FileId,
) {
    let ast = &asts[&file];
    // Every `func` with a body is its own inference problem.
    let fns: Vec<NodeId> = ast
        .ids()
        .filter(|&id| {
            matches!(
                &ast.node(id).kind,
                NodeKind::FuncExpr { body: Some(_), .. }
            )
        })
        .collect();
    for func in fns {
        let mut cx = Inferer {
            defs,
            asts,
            ast,
            diags,
            file,
            cx: InferCtxt::new(),
            env: HashMap::new(),
            types: HashMap::new(),
            ret: Ty::Void,
            breaks: Vec::new(),
        };
        cx.infer_func(func);
        cx.finish();
    }
}

struct Inferer<'a> {
    defs: &'a DefTable,
    asts: &'a HashMap<FileId, Ast>,
    ast: &'a Ast,
    diags: &'a mut Vec<Diagnostic>,
    file: FileId,
    cx: InferCtxt,
    /// Type of each in-scope value def (params, locals) by [`DefId`].
    env: HashMap<super::def::DefId, Ty>,
    /// Per-node type, filled while inferring and finalized in [`Inferer::finish`].
    types: HashMap<NodeId, Ty>,
    /// Return type of the function currently being inferred.
    ret: Ty,
    /// The break-value type of each enclosing `loop`, innermost last.
    breaks: Vec<Ty>,
}

impl Inferer<'_> {
    fn infer_func(&mut self, func: NodeId) {
        let NodeKind::FuncExpr {
            params, ret, body, ..
        } = self.ast.node(func).kind.clone()
        else {
            return;
        };
        for p in &params {
            if let NodeKind::Param { ty, .. } = self.ast.node(*p).kind.clone() {
                let pty = match ty {
                    Some(t) => self.ty_from_node(t),
                    // A bare `self` (or an inferred closure param) gets a var.
                    None => self.cx.fresh(),
                };
                if let Some(def) = self.def_of(*p) {
                    self.env.insert(def, pty.clone());
                }
                // Record the parameter's type on its node too, so lowering can
                // read it back (params are not value expressions).
                self.types.insert(*p, pty);
            }
        }
        self.ret = ret.map(|t| self.ty_from_node(t)).unwrap_or(Ty::Void);
        let ret = self.ret.clone();
        // Stash the function's return type on the `FuncExpr` node for lowering.
        self.types.insert(func, ret.clone());
        if let Some(b) = body {
            let bty = self.infer_expr(b);
            // The body's tail value is the function's result.
            self.expect(b, &bty, &ret);
        }
    }

    /// Finalize every recorded node type (defaulting numeric literals, flagging
    /// ambiguities) and stamp it onto the arena.
    fn finish(&mut self) {
        let entries: Vec<(NodeId, Ty)> = self.types.drain().collect();
        for (node, ty) in entries {
            // Inference is intentionally partial in the bootstrap: trait
            // associated-type and method-return resolution do not exist yet, so a
            // leftover unsolved variable means "not yet knowable", not "user must
            // annotate". Resolve it silently to `Error` (which absorbs on further
            // unification) rather than emitting a false "annotations needed". The
            // hard error is reintroduced once inference is complete enough to
            // trust — see `InferCtxt::finalize`'s `on_ambiguous` hook.
            let resolved = self.cx.finalize(&ty, &mut || {});
            self.ast.set_meta(node, resolved);
        }
    }

    // ===< expressions >===

    /// Infer the type of `node`, record it, and return it.
    fn infer_expr(&mut self, node: NodeId) -> Ty {
        let ty = self.infer_expr_uncached(node);
        self.types.insert(node, ty.clone());
        ty
    }

    fn infer_expr_uncached(&mut self, node: NodeId) -> Ty {
        match self.ast.node(node).kind.clone() {
            NodeKind::Block { stmts, tail } => {
                for s in &stmts {
                    self.infer_stmt(*s);
                }
                match tail {
                    Some(t) => self.infer_expr(t),
                    // A tail-less block whose last statement diverges
                    // (`return`/`break`/`continue`) is itself divergent, so it
                    // does not force the enclosing context to `void`.
                    None if stmts.last().is_some_and(|&s| self.diverges(s)) => Ty::Never,
                    None => Ty::Void,
                }
            }
            NodeKind::Lit(lit) => self.lit_ty(&lit),
            NodeKind::InterpolatedStr { parts } => {
                for p in parts {
                    self.infer_expr(p);
                }
                Ty::Str
            }
            NodeKind::Path { .. } => self.path_ty(node),
            NodeKind::Unary { op, operand } => self.infer_unary(op, operand),
            NodeKind::Binary { op, lhs, rhs } => self.infer_binary(op, lhs, rhs),
            NodeKind::Tuple { elems } => {
                if elems.is_empty() {
                    Ty::Void
                } else {
                    Ty::Tuple(elems.iter().map(|e| self.infer_expr(*e)).collect())
                }
            }
            NodeKind::FieldAccess { base, name } => {
                // A `namespace.member` access the resolver already linked to a def
                // (a function, type, or const) is typed from that def; a value
                // `place.field` is typed from the base's struct type.
                if let Some(def) = self.resolved_def(node) {
                    return self.def_ty(def);
                }
                let bty = self.infer_expr(base);
                self.field_ty(&bty, name.as_str()).unwrap_or_else(|| self.cx.fresh())
            }
            NodeKind::TupleIndex { base, index } => {
                let bty = self.infer_expr(base);
                match self.autoderef(&bty) {
                    Ty::Tuple(elems) => elems.get(index as usize).cloned().unwrap_or(Ty::Error),
                    _ => self.cx.fresh(),
                }
            }
            NodeKind::Call { callee, args } => self.infer_call(callee, &args),
            NodeKind::GenericApply { base, args } => {
                // Record any `<Assoc = T>` bindings and infer holes; the base's
                // type carries through (turbofish is not yet a full substitution).
                for a in args {
                    self.record_generic_arg(a);
                }
                self.infer_expr(base)
            }
            NodeKind::Index { base, index } => {
                let bty = self.infer_expr(base);
                self.infer_expr(index);
                match self.autoderef(&bty) {
                    Ty::Slice { inner, .. } | Ty::Array { inner, .. } => *inner,
                    _ => self.cx.fresh(),
                }
            }
            NodeKind::Slice { base, range } => {
                let bty = self.infer_expr(base);
                self.infer_expr(range);
                // A sub-slice of anything sliceable is a read-only slice of its
                // element type.
                match self.autoderef(&bty) {
                    Ty::Slice { inner, .. } | Ty::Array { inner, .. } => {
                        Ty::Slice { mutable: false, inner }
                    }
                    _ => self.cx.fresh(),
                }
            }
            NodeKind::Deref { base } => {
                let bty = self.infer_expr(base);
                let inner = self.cx.fresh();
                let ptr = Ty::Ptr { mutable: false, inner: Box::new(inner.clone()) };
                self.expect(base, &bty, &ptr);
                inner
            }
            NodeKind::MatchExpr { scrutinee, arms } => {
                let sty = self.infer_expr(scrutinee);
                let result = self.cx.fresh();
                for arm in arms {
                    if let NodeKind::MatchArm { pattern, guard, body } =
                        self.ast.node(arm).kind.clone()
                    {
                        self.bind_pattern(pattern, &sty);
                        if let Some(g) = guard {
                            let gty = self.infer_expr(g);
                            self.expect(g, &gty, &Ty::Bool);
                        }
                        let bty = self.infer_expr(body);
                        self.expect(body, &bty, &result);
                    }
                }
                result
            }
            NodeKind::If { cond, then, els } => {
                let cty = self.infer_expr(cond);
                self.expect(cond, &cty, &Ty::Bool);
                let then_ty = self.infer_expr(then);
                match els {
                    Some(e) => {
                        let else_ty = self.infer_expr(e);
                        self.expect(e, &else_ty, &then_ty);
                        then_ty
                    }
                    // An `if` without `else` yields `void`; the `then` block must too.
                    None => {
                        self.expect(then, &then_ty, &Ty::Void);
                        Ty::Void
                    }
                }
            }
            NodeKind::IfMatch { pattern, value, then, els } => {
                let vty = self.infer_expr(value);
                self.bind_pattern(pattern, &vty);
                let then_ty = self.infer_expr(then);
                match els {
                    Some(e) => {
                        let else_ty = self.infer_expr(e);
                        self.expect(e, &else_ty, &then_ty);
                        then_ty
                    }
                    None => {
                        self.expect(then, &then_ty, &Ty::Void);
                        Ty::Void
                    }
                }
            }
            NodeKind::Loop { body } => {
                self.breaks.push(self.cx.fresh());
                self.infer_expr(body);
                self.breaks.pop().unwrap_or(Ty::Void)
            }
            NodeKind::While { cond, body } => {
                let cty = self.infer_expr(cond);
                self.expect(cond, &cty, &Ty::Bool);
                self.infer_expr(body);
                Ty::Void
            }
            NodeKind::IntrinsicCall { generic_args, args, .. } => {
                for a in &args {
                    self.infer_expr(*a);
                }
                // `$cast.<T>(x)` / `$make.<T>()` etc.: the first type argument, if
                // any, is the result; otherwise it is context-inferred.
                generic_args
                    .first()
                    .filter(|&&g| !matches!(self.ast.node(g).kind, NodeKind::TypeHole))
                    .map(|&g| self.ty_from_node(g))
                    .unwrap_or_else(|| self.cx.fresh())
            }
            NodeKind::CompositeLit { ty, body } => {
                let cty = match ty {
                    Some(t) => self.ty_from_node(t),
                    None => self.cx.fresh(),
                };
                self.infer_composite_body(&cty, &body);
                cty
            }
            NodeKind::VariantLit { args, .. } => {
                for c in variant_arg_values(&self.ast.node(node).kind) {
                    self.infer_expr(c);
                }
                let _ = args;
                // The enum type is inferred from context; leave a variable to be
                // unified against the expected type.
                self.cx.fresh()
            }
            NodeKind::Arg { value, .. } | NodeKind::FieldInit { value, .. } => {
                self.infer_expr(value)
            }
            NodeKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.infer_expr(s);
                }
                if let Some(e) = end {
                    self.infer_expr(e);
                }
                self.cx.fresh()
            }
            // A closure / nested function used as a value: its type is its
            // signature; its body is inferred independently by the file walker.
            NodeKind::FuncExpr { .. } => self.func_sig_ty(node),
            // Type-forming and declaration nodes are not value expressions.
            _ => Ty::Error,
        }
    }

    /// Infer a composite literal's body, unifying each named field value with the
    /// struct's declared field type when the composite's type is a known nominal.
    fn infer_composite_body(&mut self, cty: &Ty, body: &crate::parser::ast::CompositeBody) {
        use crate::parser::ast::CompositeBody;
        match body {
            CompositeBody::Named(fields) => {
                for &f in fields {
                    if let NodeKind::FieldInit { name, value } = self.ast.node(f).kind.clone() {
                        let vty = self.infer_expr(value);
                        if let Some(ft) = self.field_ty(cty, name.as_str()) {
                            self.expect(value, &vty, &ft);
                        }
                    }
                }
            }
            CompositeBody::Positional(elems) => {
                // Array/slice element type, when known.
                let elem = match self.autoderef(cty) {
                    Ty::Slice { inner, .. } | Ty::Array { inner, .. } => Some(*inner),
                    _ => None,
                };
                for &e in elems {
                    let ety = self.infer_expr(e);
                    if let Some(el) = &elem {
                        self.expect(e, &ety, el);
                    }
                }
            }
            CompositeBody::Repeat { value, count } => {
                self.infer_expr(*value);
                self.infer_expr(*count);
            }
        }
    }

    // ===< statements >===

    fn infer_stmt(&mut self, node: NodeId) {
        match self.ast.node(node).kind.clone() {
            NodeKind::LocalDecl { pattern, ty, value, .. } => {
                let vty = self.infer_expr(value);
                let bound = match ty {
                    Some(t) => {
                        let ann = self.ty_from_node(t);
                        self.expect(value, &vty, &ann);
                        ann
                    }
                    None => vty,
                };
                self.bind_pattern(pattern, &bound);
            }
            NodeKind::Assign { place, value, .. } => {
                let pty = self.infer_expr(place);
                let vty = self.infer_expr(value);
                self.expect(value, &vty, &pty);
            }
            NodeKind::Return { value } => {
                let vty = match value {
                    Some(v) => self.infer_expr(v),
                    None => Ty::Void,
                };
                let ret = self.ret.clone();
                let anchor = value.unwrap_or(node);
                self.expect(anchor, &vty, &ret);
            }
            NodeKind::Break { value } => {
                let vty = match value {
                    Some(v) => self.infer_expr(v),
                    None => Ty::Void,
                };
                if let Some(expected) = self.breaks.last().cloned() {
                    let anchor = value.unwrap_or(node);
                    self.expect(anchor, &vty, &expected);
                }
            }
            NodeKind::Defer { body } => {
                self.infer_expr(body);
            }
            NodeKind::Continue => {}
            // Any other statement position holds an expression.
            _ => {
                self.infer_expr(node);
            }
        }
    }

    // ===< operators >===

    fn infer_unary(&mut self, op: UnOp, operand: NodeId) -> Ty {
        let oty = self.infer_expr(operand);
        match op {
            UnOp::Ref => Ty::Ptr { mutable: false, inner: Box::new(oty) },
            UnOp::RefMut => Ty::Ptr { mutable: true, inner: Box::new(oty) },
            UnOp::Neg | UnOp::BitNot => oty,
            UnOp::Not => {
                self.expect(operand, &oty, &Ty::Bool);
                Ty::Bool
            }
        }
    }

    fn infer_binary(&mut self, op: BinOp, lhs: NodeId, rhs: NodeId) -> Ty {
        let lty = self.infer_expr(lhs);
        let rty = self.infer_expr(rhs);
        match op {
            BinOp::And | BinOp::Or => {
                self.expect(lhs, &lty, &Ty::Bool);
                self.expect(rhs, &rty, &Ty::Bool);
                Ty::Bool
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.expect(rhs, &rty, &lty);
                Ty::Bool
            }
            // Arithmetic / bitwise / shift: operands share a type; result is it.
            // (Operator-trait overloading for user types is a later pass.)
            _ => {
                self.expect(rhs, &rty, &lty);
                lty
            }
        }
    }

    // ===< calls >===

    fn infer_call(&mut self, callee: NodeId, args: &[NodeId]) -> Ty {
        // A call whose callee names a type is a construction, not a function call.
        if let Some(def) = self.callee_type_def(callee) {
            for a in args {
                self.infer_expr(*a);
            }
            let nominal = self.nominal_of(def);
            self.types.insert(callee, nominal.clone());
            return nominal;
        }
        // A method call `recv.method(args)` on a value receiver (one the resolver
        // did not link to a namespace member): resolve `method` against the
        // receiver's nominal type and instantiate its generics.
        if let NodeKind::FieldAccess { base, name } = self.ast.node(callee).kind.clone() {
            if self.resolved_def(callee).is_none() {
                let recv = self.infer_expr(base);
                if let Some(m) = self.method_def(&recv, name.as_str()) {
                    return self.infer_method_call(callee, &recv, m, args);
                }
            }
        }
        // A direct function call: build the signature and **instantiate** its
        // generic type parameters with fresh variables so each call site infers
        // its own type arguments (Rust-style).
        if let Some(def) = self.resolved_def(callee) {
            if self.defs.get(def).kind == DefKind::Func {
                let sig = self.func_def_ty(def);
                let inst = self.instantiate(&sig);
                self.types.insert(callee, inst.clone());
                return self.apply_call(callee, &inst, args);
            }
        }
        let cty = self.infer_expr(callee);
        self.apply_call(callee, &cty, args)
    }

    /// Infer the arguments and unify them against a (already-instantiated) callee
    /// function type, returning its result type.
    fn apply_call(&mut self, callee: NodeId, callee_ty: &Ty, args: &[NodeId]) -> Ty {
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.infer_expr(*a)).collect();
        match self.cx.shallow(callee_ty) {
            Ty::Func { params, ret } => {
                if params.len() == arg_tys.len() {
                    for (a, (arg_node, aty)) in params.iter().zip(args.iter().zip(&arg_tys)) {
                        self.expect(*arg_node, aty, a);
                    }
                } else {
                    self.report(
                        callee,
                        format!(
                            "this function takes {} argument(s) but {} were supplied",
                            params.len(),
                            arg_tys.len()
                        ),
                    );
                }
                *ret
            }
            // Unknown callee type: don't cascade.
            _ => self.cx.fresh(),
        }
    }

    /// Resolve a method `name` on a receiver's nominal type to its `Func` def.
    fn method_def(&self, recv: &Ty, name: &str) -> Option<super::def::DefId> {
        let Ty::Nominal { def, .. } = self.autoderef(recv) else {
            return None;
        };
        let m = *self.defs.get(def).ns.members.get(&crate::common::symbol::Symbol::new(name))?;
        (self.defs.get(m).kind == DefKind::Func).then_some(m)
    }

    /// Type a `recv.method(args)` call: instantiate the method signature, unify
    /// its `self` parameter with the receiver (linking the receiver's type
    /// arguments to the method's), then unify the rest against the arguments.
    fn infer_method_call(
        &mut self,
        callee: NodeId,
        recv: &Ty,
        method: super::def::DefId,
        args: &[NodeId],
    ) -> Ty {
        let sig = self.func_def_ty(method);
        let inst = self.instantiate(&sig);
        self.types.insert(callee, inst.clone());
        let Ty::Func { params, ret } = self.cx.shallow(&inst) else {
            return self.cx.fresh();
        };
        // Unify the `self` parameter with the receiver (through a pointer if the
        // method takes `*Self` / `*mut Self`).
        if let Some(self_param) = params.first() {
            match self.cx.shallow(self_param) {
                Ty::Ptr { inner, .. } => {
                    let _ = self.cx.unify(&inner, recv);
                }
                other => {
                    let _ = self.cx.unify(&other, recv);
                }
            }
        }
        // Unify the remaining parameters with the call arguments.
        let value_params = &params[params.len().min(1)..];
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.infer_expr(*a)).collect();
        if value_params.len() == arg_tys.len() {
            for (p, (arg_node, aty)) in value_params.iter().zip(args.iter().zip(&arg_tys)) {
                self.expect(*arg_node, aty, p);
            }
        }
        *ret
    }

    // ===< generic instantiation >===

    /// Replace every generic type-parameter occurrence in `ty` with a fresh
    /// inference variable, consistently (the same parameter maps to the same
    /// variable). This is what makes a generic function/method infer fresh type
    /// arguments at each call site — e.g. `Vec.new()` yields `Vec.<?>` whose `?`
    /// is later solved by a `push`.
    fn instantiate(&mut self, ty: &Ty) -> Ty {
        let mut params = Vec::new();
        self.collect_type_params(ty, &mut params);
        let mut map = HashMap::new();
        for def in params {
            let v = self.cx.fresh();
            map.insert(def, v);
        }
        if map.is_empty() {
            ty.clone()
        } else {
            self.subst_type_params(ty, &map)
        }
    }

    fn collect_type_params(&self, ty: &Ty, out: &mut Vec<super::def::DefId>) {
        match ty {
            Ty::Nominal { def, args } => {
                if args.is_empty() && self.defs.get(*def).kind == DefKind::TypeParam {
                    if !out.contains(def) {
                        out.push(*def);
                    }
                }
                for a in args {
                    self.collect_type_params(a, out);
                }
            }
            Ty::Ptr { inner, .. } | Ty::Slice { inner, .. } | Ty::Array { inner, .. } => {
                self.collect_type_params(inner, out)
            }
            Ty::Tuple(elems) => {
                for e in elems {
                    self.collect_type_params(e, out);
                }
            }
            Ty::Func { params, ret } => {
                for p in params {
                    self.collect_type_params(p, out);
                }
                self.collect_type_params(ret, out);
            }
            _ => {}
        }
    }

    fn subst_type_params(&self, ty: &Ty, map: &HashMap<super::def::DefId, Ty>) -> Ty {
        match ty {
            Ty::Nominal { def, args } if args.is_empty() => {
                map.get(def).cloned().unwrap_or_else(|| ty.clone())
            }
            Ty::Nominal { def, args } => Ty::Nominal {
                def: *def,
                args: args.iter().map(|a| self.subst_type_params(a, map)).collect(),
            },
            Ty::Ptr { mutable, inner } => Ty::Ptr {
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            Ty::Slice { mutable, inner } => Ty::Slice {
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            Ty::Array { len, mutable, inner } => Ty::Array {
                len: *len,
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            Ty::Tuple(elems) => {
                Ty::Tuple(elems.iter().map(|e| self.subst_type_params(e, map)).collect())
            }
            Ty::Func { params, ret } => Ty::Func {
                params: params.iter().map(|p| self.subst_type_params(p, map)).collect(),
                ret: Box::new(self.subst_type_params(ret, map)),
            },
            other => other.clone(),
        }
    }

    /// If `callee` is a bare path resolving to a `struct`/`enum` type, return its
    /// def (a tuple-struct / variant construction), else `None`.
    fn callee_type_def(&self, callee: NodeId) -> Option<super::def::DefId> {
        if !matches!(self.ast.node(callee).kind, NodeKind::Path { .. }) {
            return None;
        }
        let def = self.resolved_def(callee)?;
        matches!(self.defs.get(def).kind, DefKind::Struct | DefKind::Enum).then_some(def)
    }

    // ===< names and defs >===

    /// The type of a path expression from the def it resolved to.
    fn path_ty(&mut self, node: NodeId) -> Ty {
        match self.resolved_def(node) {
            Some(def) => self.def_ty(def),
            None => Ty::Error,
        }
    }

    /// The type a value-position reference to `def` has: a local/param from the
    /// environment, a function from its signature, a type used as a constructor
    /// value as its nominal type.
    fn def_ty(&mut self, def: super::def::DefId) -> Ty {
        if let Some(ty) = self.env.get(&def) {
            return ty.clone();
        }
        match self.defs.get(def).kind {
            DefKind::Func => self.func_def_ty(def),
            DefKind::Struct | DefKind::Enum => self.nominal_of(def),
            // A top-level const's type is not inferred in the bootstrap.
            _ => self.cx.fresh(),
        }
    }

    /// Build the [`Ty::Func`] of a function def from its signature.
    fn func_def_ty(&mut self, def: super::def::DefId) -> Ty {
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return self.cx.fresh();
        };
        // The def's node is the `ConstBind`; its RHS is the `FuncExpr`.
        let ast = &self.asts[&file];
        let func = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            NodeKind::FuncExpr { .. } => node,
            _ => return self.cx.fresh(),
        };
        self.func_sig_ty_in(file, func)
    }

    /// The signature type of a `FuncExpr` in the current file.
    fn func_sig_ty(&mut self, func: NodeId) -> Ty {
        self.func_sig_ty_in(self.file, func)
    }

    /// The signature type of a `FuncExpr` living in `file`.
    fn func_sig_ty_in(&mut self, file: FileId, func: NodeId) -> Ty {
        let ast = &self.asts[&file];
        let NodeKind::FuncExpr { params, ret, .. } = ast.node(func).kind.clone() else {
            return self.cx.fresh();
        };
        let params = params
            .iter()
            .map(|&p| match &ast.node(p).kind {
                NodeKind::Param { ty: Some(t), .. } => self.ty_from_node_in(file, *t),
                _ => self.cx.fresh(),
            })
            .collect();
        let ret = ret
            .map(|t| self.ty_from_node_in(file, t))
            .unwrap_or(Ty::Void);
        Ty::Func { params, ret: Box::new(ret) }
    }

    /// A nominal type for `def`, its type arguments left as fresh variables to be
    /// solved by context (arity from the def's declared generics).
    fn nominal_of(&mut self, def: super::def::DefId) -> Ty {
        let arity = self.generic_arity(def);
        let args = (0..arity).map(|_| self.cx.fresh()).collect();
        Ty::Nominal { def, args }
    }

    /// Number of generic parameters a type def declares.
    fn generic_arity(&self, def: super::def::DefId) -> usize {
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return 0;
        };
        let ast = &self.asts[&file];
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        match &ast.node(rhs).kind {
            NodeKind::StructType { generics, .. }
            | NodeKind::EnumType { generics, .. }
            | NodeKind::TraitType { generics, .. } => generics.len(),
            _ => 0,
        }
    }

    // ===< field access >===

    /// The declared type of field `name` on a nominal struct type, if reachable.
    /// Auto-derefs through a pointer first (§3.2).
    fn field_ty(&mut self, base: &Ty, name: &str) -> Option<Ty> {
        let base = self.autoderef(base);
        let Ty::Nominal { def, .. } = base else {
            return None;
        };
        let field = *self.defs.get(def).ns.members.get(&crate::common::symbol::Symbol::new(name))?;
        if self.defs.get(field).kind != DefKind::Field {
            return None;
        }
        let d = self.defs.get(field);
        let (file, node) = (d.file?, d.node?);
        let ast = &self.asts[&file];
        match ast.node(node).kind.clone() {
            NodeKind::Field { ty, .. } => Some(self.ty_from_node_in(file, ty)),
            _ => None,
        }
    }

    /// Peel pointers off a (shallow-resolved) type for member/field access.
    fn autoderef(&self, ty: &Ty) -> Ty {
        let mut cur = self.cx.shallow(ty);
        while let Ty::Ptr { inner, .. } = cur {
            cur = self.cx.shallow(&inner);
        }
        cur
    }

    // ===< patterns >===

    /// Bind the locals a pattern introduces, unifying against the scrutinee type.
    fn bind_pattern(&mut self, pat: NodeId, ty: &Ty) {
        match self.ast.node(pat).kind.clone() {
            NodeKind::BindingPat { .. } => {
                if let Some(def) = self.def_of(pat) {
                    self.env.insert(def, ty.clone());
                }
            }
            NodeKind::AtPat { pattern, .. } => {
                if let Some(def) = self.def_of(pat) {
                    self.env.insert(def, ty.clone());
                }
                self.bind_pattern(pattern, ty);
            }
            NodeKind::TuplePat { elems } => {
                let parts: Vec<Ty> = (0..elems.len()).map(|_| self.cx.fresh()).collect();
                let tup = Ty::Tuple(parts.clone());
                let _ = self.cx.unify(ty, &tup);
                for (e, pty) in elems.iter().zip(parts) {
                    self.bind_pattern(*e, &pty);
                }
            }
            NodeKind::RefPat { pattern } => {
                let inner = self.cx.fresh();
                let ptr = Ty::Ptr { mutable: false, inner: Box::new(inner.clone()) };
                let _ = self.cx.unify(ty, &ptr);
                self.bind_pattern(pattern, &inner);
            }
            NodeKind::OrPat { alternatives } => {
                for a in alternatives {
                    self.bind_pattern(a, ty);
                }
            }
            // Variant / struct / slice patterns bind their sub-patterns to fresh
            // types in the bootstrap (payload typing arrives with enum generics).
            NodeKind::VariantPat { .. } | NodeKind::FieldPat { .. } => {
                // Payload element types are not yet threaded from the enum's
                // generics, so each sub-binding gets a fresh variable.
                for c in self.ast.node(pat).kind.children() {
                    let v = self.cx.fresh();
                    self.bind_pattern(c, &v);
                }
                // A shorthand record field (`{ radius }`) binds the field name
                // itself rather than a sub-pattern.
                if let NodeKind::FieldPat { pattern: None, .. } = self.ast.node(pat).kind {
                    if let Some(def) = self.def_of(pat) {
                        let v = self.cx.fresh();
                        self.env.insert(def, v);
                    }
                }
            }
            NodeKind::StructPat { fields, .. } => {
                for f in fields {
                    if let NodeKind::FieldPat { pattern: Some(p), .. } =
                        self.ast.node(f).kind.clone()
                    {
                        let v = self.cx.fresh();
                        self.bind_pattern(p, &v);
                    } else if let Some(def) = self.def_of(f) {
                        self.env.insert(def, self.cx.fresh());
                    }
                }
            }
            NodeKind::TupleStructPat { elems, .. } => {
                for e in elems {
                    let v = self.cx.fresh();
                    self.bind_pattern(e, &v);
                }
            }
            NodeKind::SlicePat { elems, rest } => {
                let elem = self.cx.fresh();
                for e in elems {
                    self.bind_pattern(e, &elem);
                }
                if let Some(Some(_)) = rest {
                    if let Some(def) = self.def_of(pat) {
                        self.env
                            .insert(def, Ty::Slice { mutable: false, inner: Box::new(elem) });
                    }
                }
            }
            // Wildcards, literals, ranges bind nothing.
            _ => {}
        }
    }

    // ===< type expressions >===

    /// Convert a type-expression node into a [`Ty`], resolving named heads
    /// through the [`Resolution`] the name-resolver attached.
    fn ty_from_node(&mut self, node: NodeId) -> Ty {
        self.ty_from_node_in(self.file, node)
    }

    fn ty_from_node_in(&mut self, file: FileId, node: NodeId) -> Ty {
        let ast = &self.asts[&file];
        match ast.node(node).kind.clone() {
            NodeKind::TypeHole => self.cx.fresh(),
            NodeKind::PtrType { mutable, inner } => Ty::Ptr {
                mutable,
                inner: Box::new(self.ty_from_node_in(file, inner)),
            },
            NodeKind::SliceType { mutable, inner, .. } => Ty::Slice {
                mutable,
                inner: Box::new(self.ty_from_node_in(file, inner)),
            },
            NodeKind::ArrayType { len, mutable, inner, .. } => {
                let len = self.const_len_in(file, len);
                Ty::Array {
                    len,
                    mutable,
                    inner: Box::new(self.ty_from_node_in(file, inner)),
                }
            }
            NodeKind::TupleType { elems } => {
                if elems.is_empty() {
                    Ty::Void
                } else {
                    Ty::Tuple(elems.iter().map(|e| self.ty_from_node_in(file, *e)).collect())
                }
            }
            NodeKind::FuncType { params, ret, .. } => Ty::Func {
                params: params.iter().map(|p| self.ty_from_node_in(file, *p)).collect(),
                ret: Box::new(ret.map(|t| self.ty_from_node_in(file, t)).unwrap_or(Ty::Void)),
            },
            NodeKind::DynType { inner } => match self.type_head_def_in(file, inner) {
                Some(def) => Ty::Dyn(def),
                None => Ty::Error,
            },
            NodeKind::DistinctType { inner } => self.ty_from_node_in(file, inner),
            NodeKind::TypePath { generic_args, .. } => {
                self.typepath_ty(file, node, &generic_args)
            }
            // `Type.<args>` in expression position (e.g. a composite-literal head)
            // parses as a postfix generic application; resolve it like a typepath
            // whose head is the base.
            NodeKind::GenericApply { base, args } => self.typepath_ty(file, base, &args),
            NodeKind::Path { .. } => self.typepath_ty(file, node, &[]),
            _ => Ty::Error,
        }
    }

    /// Resolve a `TypePath` (or bare `Path` in type position) to a [`Ty`] from
    /// the def its head names.
    fn typepath_ty(&mut self, file: FileId, node: NodeId, generic_args: &[NodeId]) -> Ty {
        let Some(def) = self.resolved_def_in(file, node) else {
            return Ty::Error;
        };
        let kind = self.defs.get(def).kind;
        match kind {
            DefKind::Primitive => {
                primitive_ty(self.defs.get(def).name.as_str()).unwrap_or(Ty::Error)
            }
            DefKind::Struct | DefKind::Enum | DefKind::Trait | DefKind::TypeAlias => {
                let args = generic_args
                    .iter()
                    .filter(|&&a| {
                        !matches!(self.asts[&file].node(a).kind, NodeKind::AssocBinding { .. })
                    })
                    .map(|&a| self.ty_from_node_in(file, a))
                    .collect();
                // Record `<Assoc = T>` constraints alongside.
                for &a in generic_args {
                    self.record_generic_arg_in(file, a);
                }
                Ty::Nominal { def, args }
            }
            // A generic type parameter is a rigid opaque type of its own def.
            DefKind::TypeParam => Ty::Nominal { def, args: vec![] },
            _ => Ty::Error,
        }
    }

    /// Read a literal array length if the length expression is an int literal.
    fn const_len_in(&self, file: FileId, node: NodeId) -> Option<u64> {
        match &self.asts[&file].node(node).kind {
            NodeKind::Lit(Lit::Int(n)) if *n >= 0 => Some(*n as u64),
            _ => None,
        }
    }

    /// Record a `<Assoc = T>` binding (ignoring plain type args / holes).
    fn record_generic_arg(&mut self, node: NodeId) {
        self.record_generic_arg_in(self.file, node);
    }

    fn record_generic_arg_in(&mut self, file: FileId, node: NodeId) {
        if let NodeKind::AssocBinding { name, ty } = self.asts[&file].node(node).kind.clone() {
            let t = self.ty_from_node_in(file, ty);
            self.cx.assoc.insert(name, t);
        }
    }

    // ===< literals / helpers >===

    fn lit_ty(&mut self, lit: &Lit) -> Ty {
        match lit {
            Lit::Int(_) => self.cx.fresh_of(TyVarKind::Int),
            Lit::Float(_) => self.cx.fresh_of(TyVarKind::Float),
            Lit::Str(_) => Ty::Str,
            Lit::Char(_) => Ty::Char,
            Lit::Bool(_) => Ty::Bool,
        }
    }

    /// Unify `actual` with `expected`, reporting a mismatch anchored at `node`.
    fn expect(&mut self, node: NodeId, actual: &Ty, expected: &Ty) {
        if let Err((a, b)) = self.cx.unify(actual, expected) {
            let msg = format!(
                "type mismatch: expected `{}`, found `{}`",
                b.display(self.defs),
                a.display(self.defs)
            );
            self.report(node, msg);
        }
    }

    /// Whether a statement unconditionally transfers control out of its block.
    fn diverges(&self, node: NodeId) -> bool {
        matches!(
            self.ast.node(node).kind,
            NodeKind::Return { .. } | NodeKind::Break { .. } | NodeKind::Continue
        )
    }

    fn def_of(&self, node: NodeId) -> Option<super::def::DefId> {
        self.ast.meta::<DefMeta>(node).map(|m| m.0)
    }

    fn resolved_def(&self, node: NodeId) -> Option<super::def::DefId> {
        self.resolved_def_in(self.file, node)
    }

    fn resolved_def_in(&self, file: FileId, node: NodeId) -> Option<super::def::DefId> {
        match self.asts[&file].meta::<Resolution>(node)? {
            Resolution::Def(d) => Some(self.defs.resolve_alias(d)),
            _ => None,
        }
    }

    fn type_head_def_in(&self, file: FileId, node: NodeId) -> Option<super::def::DefId> {
        self.resolved_def_in(file, node)
    }

    fn report(&mut self, node: NodeId, message: impl Into<String>) {
        let span = self.ast.node(node).span;
        self.diags.push(
            Diagnostic::error(message).with_primary(FileSpan::new(self.file, span), ""),
        );
    }
}

/// The value nodes carried by a `VariantLit`'s payload.
fn variant_arg_values(kind: &NodeKind) -> Vec<NodeId> {
    kind.children()
}
