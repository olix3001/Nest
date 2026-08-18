//! Tree traversal for the [`Ast`] arena.
//!
//! Two traits, one shape. Both walk the tree depth-first in source order and
//! recurse through [`Ast::children`], which copies child ids out without
//! holding a borrow — so a visitor is free to read ([`Ast::node`]) or, for
//! [`MutVisitor`], rewrite ([`Ast::node_mut`]) any node during the walk without
//! tripping `RefCell`'s borrow rules.
//!
//! Each traversal produces a value: the associated [`Visitor::Output`] /
//! [`MutVisitor::Output`], returned by [`Visitor::visit_node`] and the `walk_*`
//! helpers. The default walk folds the children's outputs together with
//! [`Visitor::combine`], seeded by [`Visitor::default_output`] — so a summary
//! (a count, a collected `Vec`, a "found it" flag) flows up the tree. A pure
//! side-effecting visitor sets `Output = ()`; its `combine`/`default_output`
//! are trivial.
//!
//! Override [`Visitor::visit_node`], do your work, and return `walk_node(self,
//! ast, id)` to keep descending (or a value of your own to prune):
//!
//! ```ignore
//! struct CountCalls;
//! impl Visitor for CountCalls {
//!     type Output = usize;
//!     fn default_output(&mut self) -> usize { 0 }
//!     fn combine(&mut self, a: usize, b: usize) -> usize { a + b }
//!     fn visit_node(&mut self, ast: &Ast, id: NodeId) -> usize {
//!         let here = matches!(ast.node(id).kind, NodeKind::Call { .. }) as usize;
//!         here + walk_node(self, ast, id)
//!     }
//! }
//! ```

use super::ast::{Ast, NodeId};

/// Read-only depth-first traversal. Takes the arena shared; never mutates.
pub trait Visitor: Sized {
    /// Value produced by visiting a node (and by the tree walk overall).
    type Output;

    /// The seed [`walk_node`] folds children into (e.g. `0`, `Vec::new()`,
    /// `false`). Called once per node before its children are visited.
    fn default_output(&mut self) -> Self::Output;

    /// Fold one child's output into the running accumulator, left to right.
    fn combine(&mut self, acc: Self::Output, next: Self::Output) -> Self::Output;

    /// Visit one node. The default recurses into every child and returns their
    /// combined output; override to do work, then call [`walk_node`] to descend
    /// (or return your own value to prune).
    fn visit_node(&mut self, ast: &Ast, id: NodeId) -> Self::Output {
        walk_node(self, ast, id)
    }

    /// Traverse the whole tree from its root, returning its output. Yields
    /// [`Visitor::default_output`] if the arena has no root.
    fn visit_ast(&mut self, ast: &Ast) -> Self::Output {
        match ast.root() {
            Some(root) => self.visit_node(ast, root),
            None => self.default_output(),
        }
    }
}

/// Recurse into every child of `id`, folding their outputs with
/// [`Visitor::combine`].
pub fn walk_node<V: Visitor>(visitor: &mut V, ast: &Ast, id: NodeId) -> V::Output {
    let mut acc = visitor.default_output();
    for child in ast.children(id) {
        let out = visitor.visit_node(ast, child);
        acc = visitor.combine(acc, out);
    }
    acc
}

/// Depth-first traversal that may rewrite nodes in place.
///
/// The arena is still borrowed **shared** (`&Ast`): mutation goes through the
/// per-node [`RefCell`](std::cell::RefCell) via [`Ast::node_mut`], so recursion
/// needs no unique borrow of the whole tree. A visitor must not hold a
/// [`node_mut`](Ast::node_mut) borrow across a recursive `walk_node_mut` call
/// on the same node.
///
/// Like [`Visitor`], each visit yields an [`Output`](MutVisitor::Output) folded
/// up the tree by [`combine`](MutVisitor::combine); use `Output = ()` for a
/// pure rewrite pass.
pub trait MutVisitor: Sized {
    /// Value produced by visiting a node (and by the tree walk overall).
    type Output;

    /// The seed [`walk_node_mut`] folds children into. Called once per node
    /// before its children are visited.
    fn default_output(&mut self) -> Self::Output;

    /// Fold one child's output into the running accumulator, left to right.
    fn combine(&mut self, acc: Self::Output, next: Self::Output) -> Self::Output;

    /// Visit one node, optionally rewriting it via [`Ast::node_mut`]. The
    /// default recurses into every child and returns their combined output;
    /// override and call [`walk_node_mut`] to descend.
    fn visit_node(&mut self, ast: &Ast, id: NodeId) -> Self::Output {
        walk_node_mut(self, ast, id)
    }

    /// Traverse the whole tree from its root, returning its output. Yields
    /// [`MutVisitor::default_output`] if the arena has no root.
    fn visit_ast(&mut self, ast: &Ast) -> Self::Output {
        match ast.root() {
            Some(root) => self.visit_node(ast, root),
            None => self.default_output(),
        }
    }
}

/// Recurse into every child of `id`, folding their outputs with
/// [`MutVisitor::combine`].
///
/// Children are snapshotted before recursing, so a visitor that replaces a
/// node's `kind` sees the pre-mutation child set for that node — rewrite the
/// children explicitly if a structural change must be traversed.
pub fn walk_node_mut<V: MutVisitor>(visitor: &mut V, ast: &Ast, id: NodeId) -> V::Output {
    let mut acc = visitor.default_output();
    for child in ast.children(id) {
        let out = visitor.visit_node(ast, child);
        acc = visitor.combine(acc, out);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::span::Span;
    use crate::parser::ast::{BinOp, FileId, Lit, NodeKind};

    fn build() -> (Ast, NodeId, NodeId, NodeId) {
        // (1 + 2) as the root.
        let mut ast = Ast::new();
        let sp = Span::new(0, 0);
        let file = FileId(0);
        let l = ast.alloc(sp, file, NodeKind::Lit(Lit::Int(1)));
        let r = ast.alloc(sp, file, NodeKind::Lit(Lit::Int(2)));
        let add = ast.alloc(
            sp,
            file,
            NodeKind::Binary {
                op: BinOp::Add,
                lhs: l,
                rhs: r,
            },
        );
        ast.set_root(add);
        (ast, add, l, r)
    }

    #[test]
    fn visitor_output_counts_every_node() {
        struct Counter;
        impl Visitor for Counter {
            type Output = usize;
            fn default_output(&mut self) -> usize {
                0
            }
            fn combine(&mut self, a: usize, b: usize) -> usize {
                a + b
            }
            fn visit_node(&mut self, ast: &Ast, id: NodeId) -> usize {
                1 + walk_node(self, ast, id)
            }
        }
        let (ast, ..) = build();
        assert_eq!(Counter.visit_ast(&ast), 3); // add, lhs, rhs
    }

    #[test]
    fn mut_visitor_rewrites_and_reports() {
        // Double every integer literal; Output counts how many were rewritten.
        struct Doubler;
        impl MutVisitor for Doubler {
            type Output = usize;
            fn default_output(&mut self) -> usize {
                0
            }
            fn combine(&mut self, a: usize, b: usize) -> usize {
                a + b
            }
            fn visit_node(&mut self, ast: &Ast, id: NodeId) -> usize {
                let hit = if let NodeKind::Lit(Lit::Int(n)) = &mut ast.node_mut(id).kind {
                    *n *= 2;
                    1
                } else {
                    0
                };
                hit + walk_node_mut(self, ast, id)
            }
        }
        let (ast, _add, l, r) = build();
        assert_eq!(Doubler.visit_ast(&ast), 2);
        assert!(matches!(ast.node(l).kind, NodeKind::Lit(Lit::Int(2))));
        assert!(matches!(ast.node(r).kind, NodeKind::Lit(Lit::Int(4))));
    }
}
