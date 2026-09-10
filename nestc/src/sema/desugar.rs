//! Desugaring — the stage that lowers surface control-flow sugar to the core
//! forms the rest of the compiler handles uniformly, wiring each to its `#lang`
//! item (§6.13, §8.3).
//!
//! Implemented lowerings:
//!
//! - `for pat in it { body }` → a `loop` over `IntoIterator.into_iter` /
//!   `Iterator.next`, using an `if match` to bind each element and `break` when
//!   the iterator is exhausted.
//! - `base.?` (propagate) → a `match` on `Try.branch(base)` that yields the
//!   success value and, on failure, `return`s the residual rebuilt as the
//!   enclosing function's type via a static `FromResidual.from_residual` call.
//! - `base.!` (abort) → `Try.unwrap(base)`.
//!
//! Operators (`+`, `<`, `a[i]`, `a += b`, …) are **not** touched here: they stay
//! as their parsed [`Binary`](NodeKind::Binary) / [`Index`](NodeKind::Index) /
//! [`Assign`](NodeKind::Assign) nodes and are lowered to trait calls in a later
//! pass, once type resolution can pick the right `impl`.
//!
//! Rewrites happen **in place** (a node keeps its [`NodeId`], so its parent needs
//! no fix-up); helper nodes are appended to the arena. Synthetic bindings
//! (`__it`, `__try`, …) get fresh [`DefKind::Local`] defs and their uses are
//! resolved on the spot; the lang-item method names (`next`, `branch`) are left
//! for the type checker, exactly like any hand-written method call.

use crate::common::diagnostic::Diagnostic;
use crate::common::source::{FileId, FileSpan};
use crate::common::span::Span;
use crate::common::symbol::Symbol;
use crate::parser::ast::{
    AssignOp, Ast, BinOp, NodeId, NodeKind, TryKind, VariantArgs, VariantPatArgs,
};

use super::def::{DefId, DefKind, DefTable, LangItems, Visibility};
use super::{DefMeta, Resolution};

/// Desugar every `for` / `.?` / `.!` in `file`.
pub fn desugar_file(
    ast: &mut Ast,
    defs: &mut DefTable,
    lang: &LangItems,
    diags: &mut Vec<Diagnostic>,
    file: FileId,
    file_ns: DefId,
) {
    let mut d = Desugar {
        ast,
        defs,
        lang,
        diags,
        file,
        file_ns,
        counter: 0,
    };
    if let Some(root) = d.ast.root() {
        d.walk(root);
    }
}

struct Desugar<'a> {
    ast: &'a mut Ast,
    defs: &'a mut DefTable,
    lang: &'a LangItems,
    diags: &'a mut Vec<Diagnostic>,
    file: FileId,
    file_ns: DefId,
    counter: usize,
}

impl Desugar<'_> {
    /// Post-order walk: lower children before the node, so a `for` whose body
    /// contains a `.?` is fully lowered.
    fn walk(&mut self, id: NodeId) {
        let kind = self.ast.node(id).kind.clone();
        for child in kind.children() {
            self.walk(child);
        }
        match kind {
            NodeKind::For {
                pattern,
                iter,
                body,
            } => self.lower_for(id, pattern, iter, body),
            NodeKind::Try { base, kind } => self.lower_try(id, base, kind),
            // `a op= b` → `a = a op b`, so the resulting `op` lowers through the
            // operator trait like any other binary. `a` is shared between the
            // place and the operator's left operand (both already resolved).
            NodeKind::Assign { op, place, value } if op != AssignOp::Assign => {
                self.lower_compound_assign(id, op, place, value)
            }
            _ => {}
        }
    }

    // ===< compound assignment >===

    fn lower_compound_assign(&mut self, id: NodeId, op: AssignOp, place: NodeId, value: NodeId) {
        let Some(bin_op) = compound_binop(op) else {
            return;
        };
        let span = self.ast.node(id).span;
        let binary = self.alloc(
            span,
            NodeKind::Binary {
                op: bin_op,
                lhs: place,
                rhs: value,
            },
        );
        self.replace(
            id,
            NodeKind::Assign {
                op: AssignOp::Assign,
                place,
                value: binary,
            },
        );
    }

    // ===< for >===

    fn lower_for(&mut self, id: NodeId, pattern: NodeId, iter: NodeId, body: NodeId) {
        if self.lang.get("iterator").is_none() {
            self.report(id, "`for` requires the `#lang(\"iterator\")` item");
            return;
        }
        let span = self.ast.node(id).span;
        let it_name = self.fresh("it");

        // __it :: (iter).into_iter()
        let into = self.method_call(span, iter, "into_iter", vec![]);
        let (it_pat, it_local) = self.binding_pat(span, &it_name);
        let it_bind = self.alloc(
            span,
            NodeKind::ConstBind {
                pattern: it_pat,
                rhs: into,
            },
        );

        // if match .some(pat) := __it.next() then body else { break }
        let it_ref = self.local_ref(span, &it_name, it_local);
        let next = self.method_call(span, it_ref, "next", vec![]);
        let some_pat = self.alloc(
            span,
            NodeKind::VariantPat {
                name: Symbol::new("some"),
                args: VariantPatArgs::Tuple(vec![pattern]),
            },
        );
        let brk = self.alloc(span, NodeKind::Break { value: None });
        let els = self.block(span, vec![brk], None);
        let if_match = self.alloc(
            span,
            NodeKind::IfMatch {
                pattern: some_pat,
                value: next,
                then: body,
                els: Some(els),
            },
        );
        let loop_body = self.block(span, vec![], Some(if_match));
        let loop_node = self.alloc(span, NodeKind::Loop { body: loop_body });
        self.replace(
            id,
            NodeKind::Block {
                stmts: vec![it_bind],
                tail: Some(loop_node),
            },
        );
    }

    // ===< .? / .! >===

    fn lower_try(&mut self, id: NodeId, base: NodeId, kind: TryKind) {
        if self.lang.get("try").is_none() {
            self.report(id, "`.?` / `.!` require the `#lang(\"try\")` item");
            return;
        }
        match kind {
            TryKind::Abort => self.lower_try_abort(id, base),
            TryKind::Propagate => self.lower_try_propagate(id, base),
        }
    }

    /// `base.!` → `Try.unwrap(base)` (§8.3). The abort on failure lives in the
    /// `unwrap` body the selected impl provides, so this is a plain method call.
    fn lower_try_abort(&mut self, id: NodeId, base: NodeId) {
        let span = self.ast.node(id).span;
        let call = self.method_call(span, base, "unwrap", vec![]);
        let kind = self.ast.node(call).kind.clone();
        self.replace(id, kind);
    }

    /// `base.?` → branch on the value and either continue with its output or
    /// return the residual, rebuilt as the *enclosing function's* type:
    ///
    /// ```text
    /// {
    ///   __try :: base.branch()
    ///   __try.match {
    ///     .proceed(__v) => __v,
    ///     .stop(__r)    => { return FromResidual.from_residual(__r) },
    ///   }
    /// }
    /// ```
    ///
    /// `FromResidual.from_residual` is an ordinary **static trait call**: it has
    /// no receiver, so `Self` is whatever the context wants — here the enclosing
    /// function's return type, which the `return` supplies. That is what makes
    /// this work for *any* `Try` type, and what lets a residual cross error
    /// types when a conversion impl exists (§8.3). Nothing here names `Result`
    /// or `Option`.
    fn lower_try_propagate(&mut self, id: NodeId, base: NodeId) {
        let Some(rebuild) = self.trait_member("from_residual", "from_residual") else {
            self.report(
                id,
                "`.?` requires the `#lang(\"from_residual\")` item, with a `from_residual` member",
            );
            return;
        };
        let span = self.ast.node(id).span;
        let tmp = self.fresh("try");
        let v = self.fresh("v");
        let r = self.fresh("r");

        // __try :: Try.branch(base)
        let branch = self.method_call(span, base, "branch", vec![]);
        let (tmp_pat, tmp_local) = self.binding_pat(span, &tmp);
        let tmp_bind = self.alloc(
            span,
            NodeKind::ConstBind {
                pattern: tmp_pat,
                rhs: branch,
            },
        );

        // .proceed(__v) => __v
        let (v_pat, v_local) = self.binding_pat(span, &v);
        let ok_pat = self.variant_pat(span, "proceed", vec![v_pat]);
        let v_ref = self.local_ref(span, &v, v_local);
        let ok_arm = self.match_arm(span, ok_pat, v_ref);

        // .stop(__r) => { return $from_residual(__r) }
        let (r_pat, r_local) = self.binding_pat(span, &r);
        let stop_pat = self.variant_pat(span, "stop", vec![r_pat]);
        let r_ref = self.local_ref(span, &r, r_local);
        let rebuilt = self.static_call(span, rebuild, vec![r_ref]);
        let ret = self.alloc(
            span,
            NodeKind::Return {
                value: Some(rebuilt),
            },
        );
        let fail_block = self.block(span, vec![ret], None);
        let stop_arm = self.match_arm(span, stop_pat, fail_block);

        let tmp_ref = self.local_ref(span, &tmp, tmp_local);
        let match_expr = self.alloc(
            span,
            NodeKind::MatchExpr {
                scrutinee: tmp_ref,
                arms: vec![ok_arm, stop_arm],
            },
        );
        self.replace(
            id,
            NodeKind::Block {
                stmts: vec![tmp_bind],
                tail: Some(match_expr),
            },
        );
    }

    // ===< node builders >===

    fn alloc(&mut self, span: Span, kind: NodeKind) -> NodeId {
        self.ast.alloc(span, self.file, kind)
    }

    /// Overwrite a node's kind in place, keeping its id and span.
    fn replace(&mut self, id: NodeId, kind: NodeKind) {
        self.ast.node_mut(id).kind = kind;
    }

    fn block(&mut self, span: Span, stmts: Vec<NodeId>, tail: Option<NodeId>) -> NodeId {
        self.alloc(span, NodeKind::Block { stmts, tail })
    }

    fn method_call(&mut self, span: Span, recv: NodeId, name: &str, args: Vec<NodeId>) -> NodeId {
        let callee = self.alloc(
            span,
            NodeKind::FieldAccess {
                base: recv,
                name: Symbol::new(name),
            },
        );
        self.alloc(span, NodeKind::Call { callee, args })
    }

    fn match_arm(&mut self, span: Span, pattern: NodeId, body: NodeId) -> NodeId {
        self.alloc(
            span,
            NodeKind::MatchArm {
                pattern,
                guard: None,
                body,
            },
        )
    }

    fn variant_pat(&mut self, span: Span, name: &str, elems: Vec<NodeId>) -> NodeId {
        self.alloc(
            span,
            NodeKind::VariantPat {
                name: Symbol::new(name),
                args: VariantPatArgs::Tuple(elems),
            },
        )
    }

    fn variant_lit(&mut self, span: Span, name: &str, value: NodeId) -> NodeId {
        let arg = self.alloc(span, NodeKind::Arg { name: None, value });
        self.alloc(
            span,
            NodeKind::VariantLit {
                name: Symbol::new(name),
                args: VariantArgs::Tuple(vec![arg]),
            },
        )
    }

    /// A fresh `BindingPat` plus its `Local` def; returns `(pattern, def)`.
    fn binding_pat(&mut self, span: Span, name: &Symbol) -> (NodeId, DefId) {
        let pat = self.alloc(
            span,
            NodeKind::BindingPat {
                mutable: false,
                name: name.clone(),
            },
        );
        let def = self.defs.alloc(
            name.clone(),
            DefKind::Local,
            Visibility::Private,
            Some(self.file_ns),
            Some(self.file),
            Some(span),
            Some(pat),
            vec![name.clone()],
        );
        self.ast.set_meta(pat, DefMeta(def));
        (pat, def)
    }

    /// The member `name` of the trait carrying `#lang(tag)`.
    fn trait_member(&self, tag: &str, name: &str) -> Option<DefId> {
        let trait_def = self.defs.resolve_alias(self.lang.get(tag)?);
        let d = self.defs.get(trait_def);
        (d.kind == DefKind::Trait)
            .then(|| d.ns.members.get(&Symbol::new(name)).copied())
            .flatten()
    }

    /// A call to a trait member with **no receiver** — `Trait.member(args)`.
    ///
    /// `Self` is not any argument here; the type checker solves it from the
    /// context the call sits in and then selects the impl (see
    /// `infer::open_trait_self`). That is what lets `.?` name
    /// `FromResidual.from_residual` without knowing which type the enclosing
    /// function returns.
    fn static_call(&mut self, span: Span, member: DefId, args: Vec<NodeId>) -> NodeId {
        let name = self.defs.get(member).name.clone();
        let callee = self.alloc(span, NodeKind::Path { segments: vec![name] });
        self.ast.set_meta(callee, Resolution::Def(member));
        self.alloc(span, NodeKind::Call { callee, args })
    }

    /// A `Path` referencing a synthetic local, pre-resolved to `def`.
    fn local_ref(&mut self, span: Span, name: &Symbol, def: DefId) -> NodeId {
        let node = self.alloc(
            span,
            NodeKind::Path {
                segments: vec![name.clone()],
            },
        );
        self.ast.set_meta(node, Resolution::Def(def));
        node
    }

    fn fresh(&mut self, tag: &str) -> Symbol {
        self.counter += 1;
        Symbol::new(&format!("__{tag}{}", self.counter))
    }

    fn report(&mut self, node: NodeId, message: impl Into<String>) {
        let span = self.ast.node(node).span;
        self.diags
            .push(Diagnostic::error(message).with_primary(FileSpan::new(self.file, span), ""));
    }
}

/// The binary operator a compound assignment (`+=`, `*=`, …) expands to.
fn compound_binop(op: AssignOp) -> Option<BinOp> {
    match op {
        AssignOp::Add => Some(BinOp::Add),
        AssignOp::Sub => Some(BinOp::Sub),
        AssignOp::Mul => Some(BinOp::Mul),
        AssignOp::Div => Some(BinOp::Div),
        AssignOp::Rem => Some(BinOp::Rem),
        AssignOp::Assign => None,
    }
}
