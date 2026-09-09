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

use std::collections::{HashMap, HashSet};

use crate::common::diagnostic::Diagnostic;
use crate::common::source::{FileId, FileSpan};
use crate::common::symbol::Symbol;
use crate::parser::ast::{
    Ast, BinOp, Lit, NodeId, NodeKind, SliceRest, UnOp, VariantArgs, VariantPatArgs, WideFloat,
};

use super::builtins::{self, Applies, BuiltinOp, BuiltinRow};
use super::def::{DefId, DefKind, DefTable, LangItems};
use super::impls::{ImplInfo, ImplTable};
use super::ty::{FloatWidth, InferCtxt, Obligation, Ty, TyVarKind, primitive_ty};
use super::{DefMeta, Resolution};

/// Intrinsics that never return, so a call to one types as [`Ty::Never`] rather
/// than a value: it absorbs into whatever position it appears in instead of
/// leaving an unsolvable variable behind.
const DIVERGING_INTRINSICS: &[&str] = &["abort"];

/// How an operator (or other trait-dispatched) node resolved, stamped onto the
/// operator's AST node by the trait solver so [`super::lower`] can emit a
/// **uniform** [`crate::ir::Expr::Call`] whether the operand was a primitive or
/// a user type.
///
/// [`builtin`](OpResolution::builtin) is `Some` iff the resolved impl was a
/// builtin primitive op (see [`super::builtins`]); codegen keys on it to emit
/// the machine instruction in O(1) rather than a real call.
#[derive(Debug, Clone, Copy)]
pub struct OpResolution {
    /// The trait method the operator dispatches to (the `#lang` trait's method
    /// for a builtin, the impl's method for a user type).
    pub method: DefId,
    /// The builtin-op tag, or `None` for a user impl.
    pub builtin: Option<BuiltinOp>,
}

/// Records that a `*T` was unsized to a `*dyn Trait` at this node (§3.2): the
/// value becomes a fat pointer pairing the data pointer with `T`'s vtable for
/// the trait.
///
/// The concrete pointee is kept so a later stage can pick the right vtable —
/// that choice is exactly what the coercion erases from the type.
#[derive(Debug, Clone)]
pub struct DynCoerce {
    /// The trait the object is typed as.
    pub trait_def: DefId,
    /// The pointee type being erased.
    pub concrete: Ty,
}

/// Records that an expression reaches its expected type through an `@using`
/// field's implicit upcast (§3.10), attached to the coerced node so
/// [`super::lower`] can make the "take `e.field`" explicit.
///
/// A value upcast copies the sub-object; a pointer upcast takes its address, so
/// [`through_ptr`](Upcast::through_ptr) picks which of the two lowering emits.
#[derive(Debug, Clone)]
pub struct Upcast {
    /// The `@using` field the coercion goes through.
    pub field: DefId,
    /// The receiver is a pointer, so the result is `&base.field`, not `base.field`.
    pub through_ptr: bool,
    /// The type the coercion produces — the field's type, or a pointer to it.
    /// The node's own recorded type stays the *source* type.
    pub target: Ty,
}

/// Infer types for every function body in `file`, annotating each expression
/// node with its resolved [`Ty`]. `asts` is the whole parsed program (read-only)
/// so a field access can reach a struct declared in another file.
#[allow(clippy::too_many_arguments)]
pub fn infer_file(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    diags: &mut Vec<Diagnostic>,
    lang: &LangItems,
    impls: &ImplTable,
    prelude_globs: &[DefId],
    file_ns: DefId,
    file: FileId,
) {
    let ast = &asts[&file];
    // The set of trait defs a use site in this file may select impls of: only
    // in-scope traits are candidates (§ trait selection, Rust-style).
    let in_scope_traits = in_scope_traits(defs, prelude_globs, file_ns);
    // Every `func` with a body is its own inference problem.
    let fns: Vec<NodeId> = ast
        .ids()
        .filter(|&id| matches!(&ast.node(id).kind, NodeKind::FuncExpr { body: Some(_), .. }))
        .collect();
    for func in fns {
        let mut cx = Inferer {
            defs,
            asts,
            ast,
            diags,
            lang,
            impls,
            in_scope_traits: &in_scope_traits,
            file,
            cx: InferCtxt::new(),
            env: HashMap::new(),
            types: HashMap::new(),
            ret: Ty::Void,
            breaks: Vec::new(),
            alias_stack: Vec::new(),
        };
        cx.infer_func(func);
        cx.finish();
    }
}

/// Gather every trait [`DefId`] nameable from `file_ns` — its own members and
/// imports, each enclosing namespace's, the globs pulled into any of them, and
/// the prelude (builtins + `core`). Mirrors the resolver's unqualified lookup,
/// restricted to traits: this is the candidate filter for impl selection.
pub(crate) fn in_scope_traits(
    defs: &DefTable,
    prelude_globs: &[DefId],
    file_ns: DefId,
) -> HashSet<DefId> {
    let mut set = HashSet::new();
    let add_public = |set: &mut HashSet<DefId>, ns: DefId| {
        for &m in defs.get(ns).ns.members.values() {
            let m = defs.resolve_alias(m);
            if defs.get(m).kind == DefKind::Trait && defs.get(m).vis.is_public() {
                set.insert(m);
            }
        }
    };
    for &g in prelude_globs {
        add_public(&mut set, g);
    }
    let mut cur = Some(file_ns);
    while let Some(n) = cur {
        for &m in defs
            .get(n)
            .ns
            .members
            .values()
            .chain(defs.get(n).ns.imported.values())
        {
            let m = defs.resolve_alias(m);
            if defs.get(m).kind == DefKind::Trait {
                set.insert(m);
            }
        }
        let globs = defs.get(n).ns.globs.clone();
        for g in globs {
            add_public(&mut set, g);
        }
        cur = defs.get(n).parent;
    }
    set
}

struct Inferer<'a> {
    defs: &'a DefTable,
    asts: &'a HashMap<FileId, Ast>,
    ast: &'a Ast,
    diags: &'a mut Vec<Diagnostic>,
    /// The `#lang` registry, for mapping an operator to its trait.
    lang: &'a LangItems,
    /// The whole-program impl index the solver selects over.
    impls: &'a ImplTable,
    /// Traits selectable at this file's use sites (see [`in_scope_traits`]).
    in_scope_traits: &'a HashSet<DefId>,
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
    /// Type-alias / associated-type defs currently being expanded, to break
    /// cycles in [`Inferer::expand_alias`].
    alias_stack: Vec<DefId>,
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

    /// Discharge the queued trait/projection obligations to a fixpoint, then
    /// finalize every recorded node type (defaulting numeric literals, flagging
    /// genuine ambiguities) and stamp it onto the arena.
    fn finish(&mut self) {
        // Selection is a *search* interleaved with unification: solving one
        // obligation can concretize a variable that lets the next one commit, so
        // run to a fixpoint before finalizing.
        self.solve_to_fixpoint();
        // Any obligation still queued is stuck; report the genuinely
        // unsatisfiable ones (a concrete self with no matching impl).
        let leftover = self.cx.take_obligations();
        for ob in leftover {
            self.report_unsolved(&ob);
        }
        let entries: Vec<(NodeId, Ty)> = self.types.drain().collect();
        for (node, ty) in entries {
            // Inference is now complete enough to trust: a leftover **general**
            // variable is a real "type annotations needed" error (numeric ones
            // still default). One diagnostic per ambiguous node.
            let mut ambiguous = false;
            let resolved = self.cx.finalize(&ty, &mut || ambiguous = true);
            if ambiguous {
                self.report(node, "type annotations needed");
            }
            self.check_float_width(node, &resolved);
            self.ast.set_meta(node, resolved);
        }
        self.finalize_upcasts();
    }

    /// Resolve the target type recorded on each `@using` coercion, which was
    /// captured mid-inference and may still hold unsolved variables.
    fn finalize_upcasts(&mut self) {
        for node in self.ast.ids() {
            let Some(up) = self.ast.meta::<Upcast>(node) else {
                continue;
            };
            let target = self.cx.finalize(&up.target, &mut || {});
            self.ast.set_meta(node, Upcast { target, ..up });
        }
    }

    /// Reject a float literal whose text the parser flagged as outrunning `f64`
    /// ([`WideFloat`]) but whose settled type is `f64` or narrower.
    ///
    /// A float literal is a `comptime_float` — an `f128` — and collapses to
    /// `f64` when nothing pins its width. That collapse is silent and lossless
    /// for ordinary literals; for these it is neither, so the use site has to
    /// ask for an `f80` / `f128` explicitly.
    fn check_float_width(&mut self, node: NodeId, resolved: &Ty) {
        if self.ast.meta::<WideFloat>(node).is_none() {
            return;
        }
        let Ty::Float(w) = resolved else { return };
        if matches!(w, FloatWidth::F80 | FloatWidth::F128) {
            return;
        }
        let msg = format!(
            "float literal is too large or too precise for `{}`; \
             annotate it as `f80` or `f128`",
            resolved.display(self.defs)
        );
        self.report(node, msg);
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
            NodeKind::Binary { op, lhs, rhs } => self.infer_binary(node, op, lhs, rhs),
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
                // A field we cannot type (an unresolved base) is `Error`, not a
                // fresh variable — a dangling variable would now be a false
                // "type annotations needed" (see `finish`).
                self.field_ty(&bty, name.as_str()).unwrap_or(Ty::Error)
            }
            NodeKind::TupleIndex { base, index } => {
                let bty = self.infer_expr(base);
                match self.autoderef(&bty) {
                    Ty::Tuple(elems) => elems.get(index as usize).cloned().unwrap_or(Ty::Error),
                    _ => Ty::Error,
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
                    _ => Ty::Error,
                }
            }
            NodeKind::Slice { base, range } => {
                let bty = self.infer_expr(base);
                let rty = self.infer_expr(range);
                // Slice bounds are indices: pin the range's element type to
                // `usize` so an unbounded `a[..]` still has a solved type.
                if let Ty::Nominal { args, .. } = self.cx.shallow(&rty) {
                    if let Some(elem) = args.first() {
                        let elem = elem.clone();
                        self.expect(range, &elem, &Ty::usize());
                    }
                }
                // A sub-slice of anything sliceable is a read-only slice of its
                // element type.
                match self.autoderef(&bty) {
                    Ty::Slice { inner, .. } | Ty::Array { inner, .. } => Ty::Slice {
                        mutable: false,
                        inner,
                    },
                    _ => Ty::Error,
                }
            }
            NodeKind::Deref { base } => {
                let bty = self.infer_expr(base);
                let inner = self.cx.fresh();
                let ptr = Ty::Ptr {
                    mutable: false,
                    inner: Box::new(inner.clone()),
                };
                self.expect(base, &bty, &ptr);
                inner
            }
            NodeKind::MatchExpr { scrutinee, arms } => {
                let sty = self.infer_expr(scrutinee);
                let result = self.cx.fresh();
                for arm in arms {
                    if let NodeKind::MatchArm {
                        pattern,
                        guard,
                        body,
                    } = self.ast.node(arm).kind.clone()
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
            NodeKind::IfMatch {
                pattern,
                value,
                then,
                els,
            } => {
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
            NodeKind::IntrinsicCall {
                name,
                generic_args,
                args,
            } => {
                for a in &args {
                    self.infer_expr(*a);
                }
                // A diverging intrinsic never yields a value, so it types as
                // `never` and unifies with whatever position it appears in.
                if DIVERGING_INTRINSICS.contains(&name.as_str()) {
                    return Ty::Never;
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
            NodeKind::VariantLit { name, args } => {
                // The enum is only known from context (the expected type), so the
                // result is a fresh variable and each payload argument is tied to
                // the variant's declared payload type by a deferred obligation,
                // discharged once that variable is solved to a `Nominal` enum.
                let arg_tys = self.variant_lit_arg_tys(&args);
                let recv = self.cx.fresh();
                if !arg_tys.is_empty() {
                    self.cx.register(Obligation::VariantPayload {
                        recv: recv.clone(),
                        variant: name,
                        args: arg_tys,
                        origin: node,
                    });
                }
                recv
            }
            NodeKind::Arg { value, .. } | NodeKind::FieldInit { value, .. } => {
                self.infer_expr(value)
            }
            NodeKind::Range { start, end, .. } => {
                // A range is a `Range.<T>` over its (unified) endpoint type — a
                // real nominal, so `for x in a..<b` resolves `IntoIterator` on it.
                let elem = self.cx.fresh();
                if let Some(s) = start {
                    let t = self.infer_expr(s);
                    self.expect(s, &t, &elem);
                }
                if let Some(e) = end {
                    let t = self.infer_expr(e);
                    self.expect(e, &t, &elem);
                }
                match self.lang.get("range") {
                    Some(def) => Ty::Nominal {
                        def: self.defs.resolve_alias(def),
                        args: vec![elem],
                    },
                    None => Ty::Error,
                }
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
            NodeKind::LocalDecl {
                pattern, ty, value, ..
            } => {
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
            // A `::` binding in statement position (a block-local const, or a
            // synthetic `__it` / `__try` the desugarer introduced): type its RHS
            // and bind the pattern, exactly like an un-annotated `let`.
            NodeKind::ConstBind { pattern, rhs } => {
                let vty = self.infer_expr(rhs);
                self.bind_pattern(pattern, &vty);
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
            UnOp::Ref => Ty::Ptr {
                mutable: false,
                inner: Box::new(oty),
            },
            UnOp::RefMut => Ty::Ptr {
                mutable: true,
                inner: Box::new(oty),
            },
            UnOp::Neg | UnOp::BitNot => oty,
            UnOp::Not => {
                self.expect(operand, &oty, &Ty::Bool);
                Ty::Bool
            }
        }
    }

    fn infer_binary(&mut self, node: NodeId, op: BinOp, lhs: NodeId, rhs: NodeId) -> Ty {
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
                // Comparing a *user* type requires it to implement `Eq` (`==`
                // `!=`) or `Ord` (`<` `<=` `>` `>=`): register a trait bound the
                // solver must witness. Primitives compare directly (no builtin
                // `Eq`/`Ord` impl exists — they are the language's own).
                self.check_cmp_bound(node, op, &lty);
                Ty::Bool
            }
            // Arithmetic `+ - * / %` dispatch through the operator trait: the
            // result is the projected `Output` of the selected impl, chosen
            // uniformly for primitives (builtin) and user types (§6). Bitwise /
            // shift stay a primitive `Binary` in the bootstrap.
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                self.infer_arith_op(node, op, lty, rty)
            }
            _ => {
                self.expect(rhs, &rty, &lty);
                lty
            }
        }
    }

    /// For a comparison whose operands are a concrete nominal type, require the
    /// corresponding equality/ordering trait via a [`Obligation::Trait`] bound.
    /// A primitive or still-unknown operand is left alone (primitives are the
    /// language's own comparison).
    fn check_cmp_bound(&mut self, node: NodeId, op: BinOp, lty: &Ty) {
        if !matches!(self.cx.shallow(lty), Ty::Nominal { .. }) {
            return;
        }
        let lang = match op {
            BinOp::Eq | BinOp::Ne => "eq",
            _ => "ord",
        };
        let Some(trait_def) = self.lang.get(lang) else {
            return;
        };
        self.cx.register(Obligation::Trait {
            self_ty: lty.clone(),
            trait_def: self.defs.resolve_alias(trait_def),
            args: Vec::new(),
            origin: node,
        });
    }

    /// Type an arithmetic operator via its `#lang` operator trait: register a
    /// projection obligation for `Self.Output` and return the (fresh) result
    /// variable, solved once the impl is selected. Operands are assumed
    /// homogeneous (the numeric core and bootstrap operator overloading both
    /// have `Rhs = Self`), matching the pre-trait numeric behavior.
    fn infer_arith_op(&mut self, node: NodeId, op: BinOp, lty: Ty, rty: Ty) -> Ty {
        self.expect(node, &rty, &lty);
        let Some(trait_def) = self.lang.get(binop_lang(op)) else {
            // No operator trait registered: fall back to primitive typing.
            return lty;
        };
        let trait_def = self.defs.resolve_alias(trait_def);
        let out = self.cx.fresh();
        // Numeric-core threading: for a primitive or still-unknown operand the
        // result *is* the operand type (`Output = Self`), so link them eagerly.
        // This keeps the pre-trait behavior — a literal's type flows through a
        // chain of `+`s and back from the return — even while the projection is
        // still deferred. A concrete *nominal* operand is left to the impl,
        // whose `Output` may legitimately differ from `Self`.
        if !matches!(self.cx.shallow(&lty), Ty::Nominal { .. }) {
            let _ = self.cx.unify(&out, &lty);
        }
        self.cx.register(Obligation::Projection {
            self_ty: lty,
            trait_def,
            args: vec![rty],
            assoc: Symbol::new("Output"),
            out: out.clone(),
            origin: node,
            op: Some(op),
        });
        out
    }

    // ===< trait solver: selection, projection, fulfillment >===

    /// Discharge queued obligations, retrying until a full sweep makes no
    /// progress. Solving one obligation can solve a variable that unblocks
    /// another, so a single pass is not enough; a pass that decides nothing new
    /// means the rest are stuck (reported by [`Inferer::report_unsolved`]).
    fn solve_to_fixpoint(&mut self) {
        while self.cx.has_obligations() {
            let obligations = self.cx.take_obligations();
            let mut progressed = false;
            let mut deferred = Vec::new();
            for ob in obligations {
                match self.try_solve(&ob) {
                    Outcome::Solved | Outcome::Failed => progressed = true,
                    Outcome::Deferred => deferred.push(ob),
                }
            }
            for ob in deferred {
                self.cx.register(ob);
            }
            if !progressed {
                break;
            }
        }
    }

    /// Attempt to discharge one obligation: select its impl, and for a
    /// projection also compute and unify the associated type. Returns whether it
    /// was solved, is still blocked on an unsolved variable, or failed (a
    /// diagnostic was reported).
    fn try_solve(&mut self, ob: &Obligation) -> Outcome {
        match ob {
            Obligation::Trait {
                self_ty,
                trait_def,
                args,
                origin,
            } => match self.select(self_ty, *trait_def, args) {
                Select::Ok(Choice::User(i)) => {
                    self.commit_impl(i, self_ty, args);
                    Outcome::Solved
                }
                Select::Ok(Choice::Builtin(_)) | Select::Error => Outcome::Solved,
                Select::Defer => Outcome::Deferred,
                Select::NoImpl => {
                    self.report_no_impl(*origin, self_ty, *trait_def);
                    Outcome::Failed
                }
                Select::Ambiguous => {
                    self.report_ambiguous(*origin, self_ty, *trait_def);
                    Outcome::Failed
                }
            },
            Obligation::Projection {
                self_ty,
                trait_def,
                args,
                assoc,
                out,
                origin,
                op,
            } => match self.select(self_ty, *trait_def, args) {
                Select::Ok(choice) => {
                    let assoc_ty = match choice {
                        Choice::Builtin(row) => self.builtin_output(row, self_ty),
                        Choice::User(i) => {
                            let map = self.commit_impl(i, self_ty, args);
                            self.user_assoc(i, *origin, assoc, &map)
                        }
                    };
                    self.expect(*origin, &assoc_ty, out);
                    if let Some(binop) = op {
                        self.stamp_op(*origin, choice, *trait_def, *binop);
                    }
                    Outcome::Solved
                }
                Select::Error => {
                    let _ = self.cx.unify(out, &Ty::Error);
                    Outcome::Solved
                }
                Select::Defer => Outcome::Deferred,
                Select::NoImpl => {
                    self.report_no_impl(*origin, self_ty, *trait_def);
                    let _ = self.cx.unify(out, &Ty::Error);
                    Outcome::Failed
                }
                Select::Ambiguous => {
                    self.report_ambiguous(*origin, self_ty, *trait_def);
                    let _ = self.cx.unify(out, &Ty::Error);
                    Outcome::Failed
                }
            },
            Obligation::VariantPayload { recv, variant, args, origin } => {
                match self.cx.shallow(recv) {
                    // Enum still unknown: retry once it is solved.
                    Ty::Var(_) => Outcome::Deferred,
                    // Not an enum (or an error): nothing to constrain.
                    base if !matches!(base, Ty::Nominal { .. }) => Outcome::Solved,
                    base => {
                        if let Some(payload) = self.variant_payload(&base, variant.as_str()) {
                            for (i, (arg_name, arg_ty)) in args.iter().enumerate() {
                                let target = match arg_name {
                                    Some(n) => payload
                                        .iter()
                                        .find(|(pn, _)| pn.as_ref() == Some(n))
                                        .map(|(_, t)| t.clone()),
                                    None => payload.get(i).map(|(_, t)| t.clone()),
                                };
                                if let Some(t) = target {
                                    self.expect(*origin, arg_ty, &t);
                                }
                            }
                        }
                        Outcome::Solved
                    }
                }
            }
        }
    }

    /// Pick the impl of `trait_def` that applies to `self_ty` (with trait
    /// arguments `args`). Builtins and user impls are considered uniformly. A
    /// concrete impl beats a generic (blanket) one; two equally specific matches
    /// are an ambiguity error. An unknown self type defers; a known one with no
    /// candidate is a "does not implement" error. Only [`in_scope`] traits are
    /// candidates.
    ///
    /// [`in_scope`]: Inferer::in_scope_traits
    fn select(&mut self, self_ty: &Ty, trait_def: DefId, args: &[Ty]) -> Select {
        let s = self.cx.shallow(self_ty);
        if matches!(s, Ty::Error) {
            return Select::Error;
        }
        if !self.in_scope_traits.contains(&trait_def) {
            return if is_var(&s) {
                Select::Defer
            } else {
                Select::NoImpl
            };
        }

        // Track the best (highest specificity) match, flagging a tie as
        // ambiguous. Concrete impls (and builtins) score 2; a blanket impl for
        // one of its own generics scores 1.
        let mut best: Option<(u8, Choice)> = None;
        let mut ambiguous = false;
        let consider =
            |score: u8, choice: Choice, best: &mut Option<(u8, Choice)>, ambiguous: &mut bool| {
                match best {
                    Some((bs, _)) if *bs > score => {}
                    Some((bs, _)) if *bs == score => *ambiguous = true,
                    _ => {
                        *best = Some((score, choice));
                        *ambiguous = false;
                    }
                }
            };

        if let Some(row) = self
            .builtin_row_for_trait(trait_def)
            .filter(|r| self.builtin_matches(r, &s))
        {
            consider(2, Choice::Builtin(row), &mut best, &mut ambiguous);
        }
        let candidates: Vec<usize> = (0..self.impls.impls.len())
            .filter(|&i| self.impls.impls[i].trait_def == Some(trait_def))
            .collect();
        for i in candidates {
            let generic = self.impls.impls[i].self_is_generic();
            if self.trial_impl(i, &s, args) {
                let score = if generic { 1 } else { 2 };
                consider(score, Choice::User(i), &mut best, &mut ambiguous);
            }
        }

        match best {
            Some(_) if ambiguous => Select::Ambiguous,
            Some((_, choice)) => Select::Ok(choice),
            None if is_var(&s) => Select::Defer,
            None => Select::NoImpl,
        }
    }

    /// Speculatively unify a candidate impl's self type (and trait args) with
    /// the obligation, rolling back afterwards; returns whether it fit.
    fn trial_impl(&mut self, i: usize, s: &Ty, args: &[Ty]) -> bool {
        let imp = self.impls.impls[i].clone();
        let snap = self.cx.snapshot();
        let map = self.fresh_impl_map(&imp.generics);
        let impl_self = self.impl_self_ty(&imp, &map);
        let mut ok = !matches!(impl_self, Ty::Error) && self.cx.unify(s, &impl_self).is_ok();
        if ok && !imp.trait_args.is_empty() && imp.trait_args.len() == args.len() {
            for (&node, a) in imp.trait_args.iter().zip(args) {
                let t = self.ty_from_node_in(imp.file, node);
                let t = self.subst_type_params(&t, &map);
                if self.cx.unify(&t, a).is_err() {
                    ok = false;
                    break;
                }
            }
        }
        self.cx.rollback(snap);
        ok
    }

    /// Commit the chosen impl for real (no rollback), binding its generics; the
    /// returned map (impl generic → solved type) drives associated-type
    /// projection.
    fn commit_impl(&mut self, i: usize, self_ty: &Ty, args: &[Ty]) -> HashMap<DefId, Ty> {
        let imp = self.impls.impls[i].clone();
        let map = self.fresh_impl_map(&imp.generics);
        let impl_self = self.impl_self_ty(&imp, &map);
        let _ = self.cx.unify(self_ty, &impl_self);
        if !imp.trait_args.is_empty() && imp.trait_args.len() == args.len() {
            for (&node, a) in imp.trait_args.iter().zip(args) {
                let t = self.ty_from_node_in(imp.file, node);
                let t = self.subst_type_params(&t, &map);
                let _ = self.cx.unify(&t, a);
            }
        }
        map
    }

    /// Build the impl's self [`Ty`] with its generics substituted by `map`.
    fn impl_self_ty(&mut self, imp: &ImplInfo, map: &HashMap<DefId, Ty>) -> Ty {
        let raw = self.ty_from_node_in(imp.file, imp.self_node);
        self.subst_type_params(&raw, map)
    }

    /// A fresh inference variable per impl generic parameter.
    fn fresh_impl_map(&mut self, generics: &[DefId]) -> HashMap<DefId, Ty> {
        generics.iter().map(|&g| (g, self.cx.fresh())).collect()
    }

    /// The associated type `assoc` a user impl binds, with the impl's generics
    /// substituted. Reports if the impl fails to bind it.
    fn user_assoc(
        &mut self,
        i: usize,
        origin: NodeId,
        assoc: &Symbol,
        map: &HashMap<DefId, Ty>,
    ) -> Ty {
        let imp = self.impls.impls[i].clone();
        match imp.assoc.get(assoc) {
            Some(&node) => {
                let t = self.ty_from_node_in(imp.file, node);
                self.subst_type_params(&t, map)
            }
            None => {
                self.report(
                    origin,
                    format!("impl does not define associated type `{assoc}`"),
                );
                Ty::Error
            }
        }
    }

    /// Stamp how an operator resolved onto its node, so lowering emits a uniform
    /// call (builtin-tagged for primitives).
    fn stamp_op(&mut self, origin: NodeId, choice: Choice, trait_def: DefId, op: BinOp) {
        let (method, builtin) = match choice {
            Choice::Builtin(row) => (
                self.defs
                    .get(trait_def)
                    .ns
                    .members
                    .get(&Symbol::new(row.method))
                    .copied(),
                Some(row.op),
            ),
            Choice::User(i) => (
                self.impls.impls[i]
                    .members
                    .get(&Symbol::new(binop_method(op)))
                    .copied(),
                None,
            ),
        };
        if let Some(method) = method {
            self.ast.set_meta(origin, OpResolution { method, builtin });
        }
    }

    /// The associated `Output` a builtin row produces for `self_ty`.
    fn builtin_output(&self, row: &BuiltinRow, self_ty: &Ty) -> Ty {
        match row.output {
            builtins::OutputRule::SameAsSelf => self.cx.shallow(self_ty),
        }
    }

    /// The builtin operator row a trait carries via its `#lang` tag, if any.
    fn builtin_row_for_trait(&self, trait_def: DefId) -> Option<&'static BuiltinRow> {
        let lang = self.defs.get(trait_def).lang.as_ref()?;
        builtins::row_for_lang(lang.as_str())
    }

    /// Whether a builtin row applies to a (shallow) self type — a concrete
    /// primitive of the right family, or a numeric literal variable of that
    /// family (still un-defaulted, but already known to become one).
    fn builtin_matches(&self, row: &BuiltinRow, self_shallow: &Ty) -> bool {
        if row.applies.matches(self_shallow) {
            return true;
        }
        match self.cx.var_kind(self_shallow) {
            Some(TyVarKind::Int) => matches!(row.applies, Applies::Int | Applies::Numeric),
            Some(TyVarKind::Float) => matches!(row.applies, Applies::Float | Applies::Numeric),
            _ => false,
        }
    }

    /// Report the obligations still stuck after the fixpoint. A concrete self
    /// with no impl is a real error; a self still unknown is suppressed here (its
    /// operand surfaces as "type annotations needed"), and the projection result
    /// is pinned to `Error` so it does not cascade.
    fn report_unsolved(&mut self, ob: &Obligation) {
        let (self_ty, trait_def, origin, out) = match ob {
            Obligation::Trait {
                self_ty,
                trait_def,
                origin,
                ..
            } => (self_ty.clone(), *trait_def, *origin, None),
            Obligation::Projection {
                self_ty,
                trait_def,
                origin,
                out,
                ..
            } => (self_ty.clone(), *trait_def, *origin, Some(out.clone())),
            // A variant literal whose enum was never determined: the result
            // variable itself surfaces as "type annotations needed" in finalize.
            Obligation::VariantPayload { .. } => return,
        };
        let s = self.cx.shallow(&self_ty);
        if !is_var(&s) {
            self.report_no_impl(origin, &self_ty, trait_def);
        }
        if let Some(o) = out {
            let _ = self.cx.unify(&o, &Ty::Error);
        }
    }

    fn report_no_impl(&mut self, node: NodeId, self_ty: &Ty, trait_def: DefId) {
        let s = self.cx.resolve(self_ty);
        let msg = format!(
            "`{}` does not implement `{}`",
            s.display(self.defs),
            self.defs.canonical_string(trait_def)
        );
        self.report(node, msg);
    }

    fn report_ambiguous(&mut self, node: NodeId, self_ty: &Ty, trait_def: DefId) {
        let s = self.cx.resolve(self_ty);
        let msg = format!(
            "multiple applicable impls of `{}` for `{}`",
            self.defs.canonical_string(trait_def),
            s.display(self.defs)
        );
        self.report(node, msg);
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
                // Inherent (or trait-impl) method already collected into the
                // receiver type's namespace: the fast path.
                if let Some(m) = self.method_def(&recv, name.as_str()) {
                    return self.infer_method_call(callee, &recv, m, args);
                }
                // Otherwise search in-scope trait impls whose self type unifies
                // with the receiver — the only way to reach a method on a
                // structural receiver (`[]T`, a range), whose impl parks its
                // members outside any nominal namespace.
                if let Some(m) = self.trait_method_def(&recv, name.as_str()) {
                    return self.infer_method_call(callee, &recv, m, args);
                }
                // A method on a bounded type parameter resolves in the bound:
                // `<I: Summing>` makes `it.total()` mean `Summing.total`, with
                // the concrete impl picked once `I` is instantiated.
                if let Some(m) = self.bound_method_def(&recv, name.as_str()) {
                    return self.infer_method_call(callee, &recv, m, args);
                }
                // A method on a trait object resolves in the trait itself; which
                // impl runs is a vtable lookup a later stage performs.
                if let Some(m) = self.dyn_method_def(&recv, name.as_str()) {
                    return self.infer_method_call(callee, &recv, m, args);
                }
                // Last, the one ergonomic exception `@using` grants (§3.10): a
                // method the outer struct does not have resolves on the upcast
                // target, with the receiver bound to the embedded sub-object.
                if let Some((m, up)) = self.using_method_def(&recv, name.as_str()) {
                    self.ast.set_meta(base, up.clone());
                    return self.infer_method_call(callee, &up.target, m, args);
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
            // Unknown callee type: don't cascade (and don't dangle a variable).
            _ => Ty::Error,
        }
    }

    /// Resolve a method `name` on a receiver's nominal type to its `Func` def.
    fn method_def(&self, recv: &Ty, name: &str) -> Option<super::def::DefId> {
        let Ty::Nominal { def, .. } = self.autoderef(recv) else {
            return None;
        };
        let m = *self
            .defs
            .get(def)
            .ns
            .members
            .get(&crate::common::symbol::Symbol::new(name))?;
        (self.defs.get(m).kind == DefKind::Func).then_some(m)
    }

    /// Resolve a method `name` by searching in-scope trait impls whose self type
    /// unifies with the receiver. Used when the method is not an inherent /
    /// namespace member — notably for structural receivers (`[]T`, a range),
    /// whose impls have no host namespace. Concrete impls beat generic; a tie is
    /// treated as unresolved (no dispatch).
    fn trait_method_def(&mut self, recv: &Ty, name: &str) -> Option<DefId> {
        let s = self.cx.shallow(recv);
        let s = self.autoderef(&s);
        if matches!(s, Ty::Error) || is_var(&s) {
            return None;
        }
        let sym = crate::common::symbol::Symbol::new(name);
        let mut best: Option<(u8, DefId)> = None;
        let mut ambiguous = false;
        for i in 0..self.impls.impls.len() {
            let imp = self.impls.impls[i].clone();
            let Some(td) = imp.trait_def else { continue };
            if !self.in_scope_traits.contains(&td) {
                continue;
            }
            let Some(&method) = imp.members.get(&sym) else { continue };
            if self.defs.get(method).kind != DefKind::Func {
                continue;
            }
            if self.trial_impl(i, &s, &[]) {
                let score = if imp.self_is_generic() { 1 } else { 2 };
                match best {
                    Some((bs, _)) if bs > score => {}
                    Some((bs, _)) if bs == score => ambiguous = true,
                    _ => {
                        best = Some((score, method));
                        ambiguous = false;
                    }
                }
            }
        }
        if ambiguous {
            None
        } else {
            best.map(|(_, m)| m)
        }
    }

    /// Resolve `name` through the trait bounds of a generic type parameter
    /// receiver (`<I: Summing>` → `it.total()` is `Summing.total`).
    fn bound_method_def(&mut self, recv: &Ty, name: &str) -> Option<DefId> {
        let s = self.autoderef(&self.cx.shallow(recv));
        let Ty::Nominal { def, .. } = s else {
            return None;
        };
        let d = self.defs.get(def);
        if d.kind != DefKind::TypeParam {
            return None;
        }
        let (file, node) = (d.file?, d.node?);
        let NodeKind::GenericTypeParam { constraint, .. } = self.asts[&file].node(node).kind.clone()
        else {
            return None;
        };
        let sym = crate::common::symbol::Symbol::new(name);
        for bound in self.bound_nodes(file, constraint?) {
            let Some(t) = self.type_head_def_in(file, bound) else {
                continue;
            };
            if self.defs.get(t).kind != DefKind::Trait || !self.in_scope_traits.contains(&t) {
                continue;
            }
            if let Some(&m) = self.defs.get(t).ns.members.get(&sym) {
                if self.defs.get(m).kind == DefKind::Func {
                    return Some(m);
                }
            }
        }
        None
    }

    /// The individual trait nodes of a generic parameter's constraint, which is
    /// either a `+`-separated [`NodeKind::Bounds`] list or a single trait.
    fn bound_nodes(&self, file: FileId, constraint: NodeId) -> Vec<NodeId> {
        match self.asts[&file].node(constraint).kind.clone() {
            NodeKind::Bounds { bounds } => bounds,
            _ => vec![constraint],
        }
    }

    /// Rewrite a trait *declaration*'s `Self` to what the receiver actually is.
    ///
    /// Only calls that land on a trait's own declaration need this — dispatch
    /// through a trait object (`*dyn Summing`) or through a type parameter's
    /// bound (`<I: Summing>`). A call that selected a concrete impl already has
    /// the impl's signature and is left alone.
    fn subst_trait_self(&mut self, sig: &Ty, method: DefId, recv: &Ty) -> Ty {
        let Some(parent) = self.defs.get(method).parent else {
            return sig.clone();
        };
        if self.defs.get(parent).kind != DefKind::Trait {
            return sig.clone();
        }
        // Look through the receiver's pointer: `*dyn T` and `*I` both stand for
        // a `Self` of `dyn T` / `I`.
        let head = match self.cx.shallow(recv) {
            Ty::Ptr { inner, .. } => self.cx.shallow(&inner),
            other => other,
        };
        if matches!(head, Ty::Error) || is_var(&head) {
            return sig.clone();
        }
        let map = HashMap::from([(parent, head)]);
        self.subst_type_params(sig, &map)
    }

    /// Resolve `name` on a trait-object receiver (`dyn Trait` or `*dyn Trait`) to
    /// the trait's own method declaration.
    fn dyn_method_def(&mut self, recv: &Ty, name: &str) -> Option<DefId> {
        let s = self.cx.shallow(recv);
        let trait_def = match &s {
            Ty::Dyn(d) => *d,
            Ty::Ptr { inner, .. } => match self.cx.shallow(inner) {
                Ty::Dyn(d) => d,
                _ => return None,
            },
            _ => return None,
        };
        let m = *self
            .defs
            .get(trait_def)
            .ns
            .members
            .get(&crate::common::symbol::Symbol::new(name))?;
        (self.defs.get(m).kind == DefKind::Func).then_some(m)
    }

    /// Resolve `name` on the `@using` field's type when the receiver's own type
    /// does not have it, returning the method and the coercion that reaches it.
    ///
    /// Only one hop, and only when the outer struct has no such member itself —
    /// `@using` promotes nothing else onto the outer type (§3.10).
    fn using_method_def(&mut self, recv: &Ty, name: &str) -> Option<(DefId, Upcast)> {
        let s = self.cx.shallow(recv);
        let head = match &s {
            Ty::Nominal { def, .. } => *def,
            Ty::Ptr { inner, .. } => match self.cx.shallow(inner) {
                Ty::Nominal { def, .. } => def,
                _ => return None,
            },
            _ => return None,
        };
        let field = self.defs.using_field(head)?;
        // The outer type keeps priority: `@using` only fills in what it lacks.
        if self.method_def(&s, name).is_some() {
            return None;
        }
        let target = self.field_ty(&s, self.defs.get(field).name.as_str())?;
        let method = self
            .method_def(&target, name)
            .or_else(|| self.trait_method_def(&target, name))?;
        // The receiver lowers to the sub-object itself (`e.t` / `p.*.t`); the
        // method's own `*Self` parameter re-addresses it as usual, so this is
        // never the pointer form of the coercion.
        Some((
            method,
            Upcast {
                field,
                through_ptr: false,
                target,
            },
        ))
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
        // Dispatching through a trait object or a bound reaches the trait's
        // *declaration*, whose `Self` is the trait's own nominal. For this call
        // `Self` is the receiver, so say so rather than leaving the signature
        // claiming a bare `Trait`.
        let inst = self.subst_trait_self(&inst, method, recv);
        self.types.insert(callee, inst.clone());
        let Ty::Func { params, ret } = self.cx.shallow(&inst) else {
            return Ty::Error;
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
                args: args
                    .iter()
                    .map(|a| self.subst_type_params(a, map))
                    .collect(),
            },
            Ty::Ptr { mutable, inner } => Ty::Ptr {
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            Ty::Slice { mutable, inner } => Ty::Slice {
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            Ty::Array {
                len,
                mutable,
                inner,
            } => Ty::Array {
                len: *len,
                mutable: *mutable,
                inner: Box::new(self.subst_type_params(inner, map)),
            },
            Ty::Tuple(elems) => Ty::Tuple(
                elems
                    .iter()
                    .map(|e| self.subst_type_params(e, map))
                    .collect(),
            ),
            Ty::Func { params, ret } => Ty::Func {
                params: params
                    .iter()
                    .map(|p| self.subst_type_params(p, map))
                    .collect(),
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
        Ty::Func {
            params,
            ret: Box::new(ret),
        }
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

    /// Infer each variant-literal payload argument, tagging record entries with
    /// their field name (tuple entries get `None`).
    fn variant_lit_arg_tys(
        &mut self,
        args: &VariantArgs,
    ) -> Vec<(Option<crate::common::symbol::Symbol>, Ty)> {
        match args {
            VariantArgs::None => Vec::new(),
            VariantArgs::Tuple(ids) => ids.iter().map(|&a| (None, self.infer_expr(a))).collect(),
            VariantArgs::Record(ids) => ids
                .iter()
                .map(|&f| match self.ast.node(f).kind.clone() {
                    NodeKind::FieldInit { name, value } => (Some(name), self.infer_expr(value)),
                    _ => (None, self.infer_expr(f)),
                })
                .collect(),
        }
    }

    // ===< generic substitution over members >===

    /// The generic type-parameter [`DefId`]s a type def declares, in order.
    fn type_param_defs(&self, def: DefId) -> Vec<DefId> {
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Vec::new();
        };
        let ast = &self.asts[&file];
        let rhs = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let generics = match &ast.node(rhs).kind {
            NodeKind::StructType { generics, .. }
            | NodeKind::EnumType { generics, .. }
            | NodeKind::TraitType { generics, .. } => generics.clone(),
            _ => return Vec::new(),
        };
        generics
            .iter()
            .filter_map(|&g| self.def_meta_in(file, g))
            .collect()
    }

    /// The substitution `{ generic-param → type-arg }` for a nominal use
    /// `Type.<args>` — how a `T`-typed field / variant payload becomes concrete.
    fn nominal_subst(&self, def: DefId, args: &[Ty]) -> HashMap<DefId, Ty> {
        self.type_param_defs(def)
            .into_iter()
            .zip(args.iter().cloned())
            .collect()
    }

    fn def_meta_in(&self, file: FileId, node: NodeId) -> Option<DefId> {
        self.asts[&file].meta::<DefMeta>(node).map(|m| m.0)
    }

    // ===< field access >===

    /// The declared type of field `name` on a nominal struct type, if reachable,
    /// with the struct's generics substituted by the use-site's type arguments
    /// (so `Wrap.<i32>`'s `T` field reads back as `i32`). Auto-derefs through a
    /// pointer first (§3.2).
    fn field_ty(&mut self, base: &Ty, name: &str) -> Option<Ty> {
        let base = self.autoderef(base);
        let Ty::Nominal { def, args } = base else {
            return None;
        };
        let field = *self
            .defs
            .get(def)
            .ns
            .members
            .get(&crate::common::symbol::Symbol::new(name))?;
        if self.defs.get(field).kind != DefKind::Field {
            return None;
        }
        let d = self.defs.get(field);
        let (file, node) = (d.file?, d.node?);
        match self.asts[&file].node(node).kind.clone() {
            NodeKind::Field { ty, .. } => {
                let map = self.nominal_subst(def, &args);
                let t = self.ty_from_node_in(file, ty);
                Some(self.subst_type_params(&t, &map))
            }
            _ => None,
        }
    }

    /// The declared payload types of enum variant `name` on `base`, in order,
    /// each paired with its field name (for record variants) and with the enum's
    /// generics substituted. `None` if `base` is not an enum with that variant.
    fn variant_payload(&mut self, base: &Ty, name: &str) -> Option<Vec<(Option<crate::common::symbol::Symbol>, Ty)>> {
        use crate::parser::ast::VariantPayload;
        let base = self.autoderef(base);
        let Ty::Nominal { def, args } = base else {
            return None;
        };
        let variant = *self
            .defs
            .get(def)
            .ns
            .members
            .get(&crate::common::symbol::Symbol::new(name))?;
        if self.defs.get(variant).kind != DefKind::Variant {
            return None;
        }
        let vd = self.defs.get(variant);
        let (file, node) = (vd.file?, vd.node?);
        let payload = match &self.asts[&file].node(node).kind {
            NodeKind::Variant { payload, .. } => payload.clone(),
            _ => return None,
        };
        let map = self.nominal_subst(def, &args);
        let mut out = Vec::new();
        match payload {
            VariantPayload::None => {}
            VariantPayload::Tuple(tys) => {
                for t in tys {
                    let ty = self.ty_from_node_in(file, t);
                    out.push((None, self.subst_type_params(&ty, &map)));
                }
            }
            VariantPayload::Record(fields) => {
                for f in fields {
                    if let NodeKind::Field { name, ty, .. } = self.asts[&file].node(f).kind.clone() {
                        let ty = self.ty_from_node_in(file, ty);
                        out.push((Some(name), self.subst_type_params(&ty, &map)));
                    }
                }
            }
        }
        Some(out)
    }

    /// The positional field types of a tuple struct `Type(A, B, …)`, with
    /// generics substituted. `None` if `base` is not a tuple struct.
    fn tuple_struct_tys(&mut self, base: &Ty) -> Option<Vec<Ty>> {
        use crate::parser::ast::StructKind;
        let base = self.autoderef(base);
        let Ty::Nominal { def, args } = base else {
            return None;
        };
        let d = self.defs.get(def);
        let (file, node) = (d.file?, d.node?);
        let rhs = match &self.asts[&file].node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let tys = match &self.asts[&file].node(rhs).kind {
            NodeKind::StructType { kind: StructKind::Tuple(tys), .. } => tys.clone(),
            _ => return None,
        };
        let map = self.nominal_subst(def, &args);
        Some(
            tys.iter()
                .map(|&t| {
                    let ty = self.ty_from_node_in(file, t);
                    self.subst_type_params(&ty, &map)
                })
                .collect(),
        )
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
                let ptr = Ty::Ptr {
                    mutable: false,
                    inner: Box::new(inner.clone()),
                };
                let _ = self.cx.unify(ty, &ptr);
                self.bind_pattern(pattern, &inner);
            }
            NodeKind::OrPat { alternatives } => {
                for a in alternatives {
                    self.bind_pattern(a, ty);
                }
            }
            // A variant pattern binds each payload sub-pattern to the variant's
            // declared payload type (generics substituted from the scrutinee).
            NodeKind::VariantPat { name, args } => match args {
                VariantPatArgs::None => {}
                VariantPatArgs::Tuple(elems) => {
                    let payload = self.variant_payload(ty, name.as_str());
                    for (i, e) in elems.iter().enumerate() {
                        let pty = payload
                            .as_ref()
                            .and_then(|p| p.get(i))
                            .map(|(_, t)| t.clone())
                            .unwrap_or_else(|| self.cx.fresh());
                        self.bind_pattern(*e, &pty);
                    }
                }
                VariantPatArgs::Record { fields, .. } => {
                    let payload = self.variant_payload(ty, name.as_str());
                    for f in fields {
                        self.bind_record_field(f, payload.as_deref());
                    }
                }
            },
            // A `FieldPat` reached on its own (defensive: normally handled by its
            // enclosing struct/variant record).
            NodeKind::FieldPat { .. } => self.bind_record_field(pat, None),
            NodeKind::StructPat { fields, .. } => {
                for f in fields {
                    let NodeKind::FieldPat { name, pattern, .. } = self.ast.node(f).kind.clone()
                    else {
                        continue;
                    };
                    let fty = self.field_ty(ty, name.as_str()).unwrap_or_else(|| self.cx.fresh());
                    match pattern {
                        Some(p) => self.bind_pattern(p, &fty),
                        None => {
                            if let Some(def) = self.def_of(f) {
                                self.env.insert(def, fty);
                            }
                        }
                    }
                }
            }
            NodeKind::TupleStructPat { elems, .. } => {
                let tys = self.tuple_struct_tys(ty);
                for (i, e) in elems.iter().enumerate() {
                    let pty = tys
                        .as_ref()
                        .and_then(|t| t.get(i))
                        .cloned()
                        .unwrap_or_else(|| self.cx.fresh());
                    self.bind_pattern(*e, &pty);
                }
            }
            NodeKind::SlicePat { elems, rest } => {
                // Every element pattern matches the scrutinee's element type.
                let elem = match self.autoderef(ty) {
                    Ty::Slice { inner, .. } | Ty::Array { inner, .. } => *inner,
                    _ => self.cx.fresh(),
                };
                for e in elems {
                    self.bind_pattern(e, &elem);
                }
                if let Some(SliceRest { name: Some(_), .. }) = rest {
                    if let Some(def) = self.def_of(pat) {
                        self.env.insert(
                            def,
                            Ty::Slice {
                                mutable: false,
                                inner: Box::new(elem),
                            },
                        );
                    }
                }
            }
            // Wildcards, literals, ranges bind nothing.
            _ => {}
        }
    }

    /// Bind one record `FieldPat` (`{ radius }` shorthand or `{ radius: p }`),
    /// typed from `payload` (the enclosing variant's field types) by name.
    fn bind_record_field(
        &mut self,
        f: NodeId,
        payload: Option<&[(Option<crate::common::symbol::Symbol>, Ty)]>,
    ) {
        let NodeKind::FieldPat { name, pattern, .. } = self.ast.node(f).kind.clone() else {
            return;
        };
        let fty = payload
            .and_then(|p| p.iter().find(|(n, _)| n.as_ref() == Some(&name)).map(|(_, t)| t.clone()))
            .unwrap_or_else(|| self.cx.fresh());
        match pattern {
            Some(p) => self.bind_pattern(p, &fty),
            None => {
                if let Some(def) = self.def_of(f) {
                    self.env.insert(def, fty);
                }
            }
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
            NodeKind::ArrayType {
                len,
                mutable,
                inner,
                ..
            } => {
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
                    Ty::Tuple(
                        elems
                            .iter()
                            .map(|e| self.ty_from_node_in(file, *e))
                            .collect(),
                    )
                }
            }
            NodeKind::FuncType { params, ret, .. } => Ty::Func {
                params: params
                    .iter()
                    .map(|p| self.ty_from_node_in(file, *p))
                    .collect(),
                ret: Box::new(
                    ret.map(|t| self.ty_from_node_in(file, t))
                        .unwrap_or(Ty::Void),
                ),
            },
            NodeKind::DynType { inner } => match self.type_head_def_in(file, inner) {
                Some(def) => Ty::Dyn(def),
                None => Ty::Error,
            },
            NodeKind::DistinctType { inner } => self.ty_from_node_in(file, inner),
            NodeKind::TypePath { generic_args, .. } => self.typepath_ty(file, node, &generic_args),
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
            DefKind::Struct | DefKind::Enum | DefKind::Trait => {
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
            // A type alias (a `distinct`/plain alias, or an impl's associated-type
            // binding `Output :: Vec3`) expands to its right-hand side. This is
            // what turns `Self.Output` on a concrete type into the impl's chosen
            // type (§ associated-type projection).
            DefKind::TypeAlias => self.expand_alias(def),
            // A generic type parameter is a rigid opaque type of its own def.
            DefKind::TypeParam => Ty::Nominal { def, args: vec![] },
            _ => Ty::Error,
        }
    }

    /// Expand a type-alias / associated-type binding to the type it names.
    ///
    /// For an impl's `Output :: Vec3` this is `Vec3`; for a `distinct`/plain
    /// alias it is the aliased type. An **abstract** associated type (a trait's
    /// `Output :: type`, reached when the self type is still generic) has no
    /// concrete value, so it becomes a fresh variable to be pinned by context
    /// (e.g. the enclosing return type). Cycles fall back to an opaque nominal.
    fn expand_alias(&mut self, def: DefId) -> Ty {
        if self.alias_stack.contains(&def) {
            return Ty::Nominal { def, args: Vec::new() };
        }
        let d = self.defs.get(def);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Ty::Nominal { def, args: Vec::new() };
        };
        let rhs = match &self.asts[&file].node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        if matches!(self.asts[&file].node(rhs).kind, NodeKind::AssocType { .. }) {
            return self.cx.fresh();
        }
        self.alias_stack.push(def);
        let ty = self.ty_from_node_in(file, rhs);
        self.alias_stack.pop();
        ty
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
    ///
    /// A plain mismatch gets one more chance: a struct with an `@using` field
    /// implicitly upcasts to that field's type (§3.10), so try the coercion
    /// before reporting.
    fn expect(&mut self, node: NodeId, actual: &Ty, expected: &Ty) {
        let snapshot = self.cx.snapshot();
        if let Err((a, b)) = self.cx.unify(actual, expected) {
            self.cx.rollback(snapshot);
            if self.try_dyn_coerce(node, actual, expected) || self.try_upcast(node, actual, expected)
            {
                return;
            }
            let msg = format!(
                "type mismatch: expected `{}`, found `{}`",
                b.display(self.defs),
                a.display(self.defs)
            );
            self.report(node, msg);
        }
    }

    /// Try to reach `expected` from `actual` by unsizing a concrete pointer to a
    /// trait object: `*T` coerces to `*dyn Trait` when `T: Trait` (§3.2),
    /// recording a [`DynCoerce`] on `node` so lowering builds the fat pointer.
    fn try_dyn_coerce(&mut self, node: NodeId, actual: &Ty, expected: &Ty) -> bool {
        // Only pointers unsize; the pointee must be a real type on the left and
        // the trait object on the right, with mutability the usual `*mut` → `*`.
        let (Ty::Ptr { mutable, inner }, Ty::Ptr { mutable: em, inner: ei }) =
            (self.cx.shallow(actual), self.cx.shallow(expected))
        else {
            return false;
        };
        if em && !mutable {
            return false;
        }
        let Ty::Dyn(trait_def) = self.cx.shallow(&ei) else {
            return false;
        };
        let concrete = self.cx.shallow(&inner);
        if is_var(&concrete) || matches!(concrete, Ty::Error) {
            return false;
        }
        // The coercion is only sound when the concrete type really implements
        // the trait; an unsatisfied bound stays a plain type mismatch.
        if !matches!(self.select(&concrete, trait_def, &[]), Select::Ok(_)) {
            return false;
        }
        self.ast.set_meta(
            node,
            DynCoerce {
                trait_def,
                concrete,
            },
        );
        true
    }

    /// Try to reach `expected` from `actual` through an `@using` field's implicit
    /// upcast, recording an [`Upcast`] on `node` when it works.
    ///
    /// `Entity` coerces to `Transform` by copying the field; `*Entity` coerces to
    /// `*Transform` by taking the sub-object's address. Only one hop is tried: a
    /// chain of upcasts is deliberately not implicit.
    fn try_upcast(&mut self, node: NodeId, actual: &Ty, expected: &Ty) -> bool {
        let (head, through_ptr) = match self.cx.shallow(actual) {
            Ty::Nominal { def, .. } => (def, false),
            Ty::Ptr { inner, .. } => match self.cx.shallow(&inner) {
                Ty::Nominal { def, .. } => (def, true),
                _ => return false,
            },
            _ => return false,
        };
        let Some(field) = self.defs.using_field(head) else {
            return false;
        };
        let Some(fty) = self.field_ty(actual, self.defs.get(field).name.as_str()) else {
            return false;
        };
        // The upcast of a pointer yields a pointer to the sub-object, keeping the
        // receiver's mutability.
        let target = match (through_ptr, self.cx.shallow(actual)) {
            (true, Ty::Ptr { mutable, .. }) => Ty::Ptr {
                mutable,
                inner: Box::new(fty),
            },
            _ => fty,
        };
        let snapshot = self.cx.snapshot();
        if self.cx.unify(&target, expected).is_err() {
            self.cx.rollback(snapshot);
            return false;
        }
        self.ast.set_meta(
            node,
            Upcast {
                field,
                through_ptr,
                target,
            },
        );
        true
    }

    /// Whether a statement unconditionally transfers control out of its block.
    fn diverges(&self, node: NodeId) -> bool {
        match &self.ast.node(node).kind {
            NodeKind::Return { .. } | NodeKind::Break { .. } | NodeKind::Continue => true,
            // A diverging intrinsic in statement position ends the block just as
            // a `return` does — `.!` leans on this to type its abort arm.
            NodeKind::IntrinsicCall { name, .. } => {
                DIVERGING_INTRINSICS.contains(&name.as_str())
            }
            _ => false,
        }
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
        self.diags
            .push(Diagnostic::error(message).with_primary(FileSpan::new(self.file, span), ""));
    }
}

/// The outcome of attempting one [`Obligation`].
enum Outcome {
    /// Discharged.
    Solved,
    /// Blocked on an unsolved variable; retry after more inference.
    Deferred,
    /// Unsatisfiable; a diagnostic was reported.
    Failed,
}

/// The result of impl selection for an obligation.
enum Select {
    /// A unique best impl was found.
    Ok(Choice),
    /// The self type is not yet known; try again later.
    Defer,
    /// No candidate impl applies to a known self type.
    NoImpl,
    /// Two or more equally specific impls apply.
    Ambiguous,
    /// The self type is already `Error`; absorb without further diagnostics.
    Error,
}

/// The selected impl: a builtin primitive op, or a user impl (by table index).
#[derive(Clone, Copy)]
enum Choice {
    Builtin(&'static BuiltinRow),
    User(usize),
}

fn is_var(ty: &Ty) -> bool {
    matches!(ty, Ty::Var(_))
}

/// The `#lang` tag of the operator trait an arithmetic [`BinOp`] dispatches to.
fn binop_lang(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Rem => "rem",
        _ => "",
    }
}

/// The trait method name an arithmetic [`BinOp`] calls.
fn binop_method(op: BinOp) -> &'static str {
    // For the arithmetic operators the method name equals the `#lang` tag.
    binop_lang(op)
}
