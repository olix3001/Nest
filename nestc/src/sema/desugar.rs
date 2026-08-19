//! Desugaring — the stage that lowers surface control-flow sugar to the core
//! forms the rest of the compiler handles uniformly, wiring each to its `#lang`
//! item (§6.13, §8.3).
//!
//! Implemented lowerings:
//!
//! - `for pat in it { body }` → a `loop` over `IntoIterator.into_iter` /
//!   `Iterator.next`, using an `if match` to bind each element and `break` when
//!   the iterator is exhausted.
//! - `base.?` (propagate) / `base.!` (abort) → a `match` on `Try.branch(base)`
//!   that yields the success value and, on failure, either `return`s the residual
//!   (`.?`) or `$abort`s (`.!`).
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
use crate::parser::ast::{Ast, NodeId, NodeKind, TryKind, VariantArgs, VariantPatArgs};

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
            _ => {}
        }
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
        let it_bind = self.alloc(span, NodeKind::ConstBind { pattern: it_pat, rhs: into });

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
        let span = self.ast.node(id).span;
        let tmp = self.fresh("try");
        let v = self.fresh("v");
        let r = self.fresh("r");

        // __try :: Try.branch(base)
        let branch = self.method_call(span, base, "branch", vec![]);
        let (tmp_pat, tmp_local) = self.binding_pat(span, &tmp);
        let tmp_bind = self.alloc(span, NodeKind::ConstBind { pattern: tmp_pat, rhs: branch });

        // .ok(v) => v
        let (v_pat, v_local) = self.binding_pat(span, &v);
        let ok_pat = self.variant_pat(span, "ok", vec![v_pat]);
        let v_ref = self.local_ref(span, &v, v_local);
        let ok_arm = self.match_arm(span, ok_pat, v_ref);

        // .err(r) => <return .err(r)> | <$abort(r)>
        let (r_pat, r_local) = self.binding_pat(span, &r);
        let err_pat = self.variant_pat(span, "err", vec![r_pat]);
        let r_ref = self.local_ref(span, &r, r_local);
        let fail = match kind {
            TryKind::Propagate => {
                let err_val = self.variant_lit(span, "err", r_ref);
                self.alloc(span, NodeKind::Return { value: Some(err_val) })
            }
            TryKind::Abort => self.alloc(
                span,
                NodeKind::IntrinsicCall {
                    name: Symbol::new("abort"),
                    generic_args: vec![],
                    args: vec![r_ref],
                },
            ),
        };
        let fail_block = self.block(span, vec![fail], None);
        let err_arm = self.match_arm(span, err_pat, fail_block);

        let tmp_ref = self.local_ref(span, &tmp, tmp_local);
        let match_expr = self.alloc(
            span,
            NodeKind::MatchExpr {
                scrutinee: tmp_ref,
                arms: vec![ok_arm, err_arm],
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
        self.diags.push(
            Diagnostic::error(message).with_primary(FileSpan::new(self.file, span), ""),
        );
    }
}
