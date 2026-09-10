//! Field resolution — the pass that links every field *use* to the field it
//! names.
//!
//! [`super::resolve`] cannot do this on its own. `P { x: 1 }` and `p.x` name
//! their struct through a path it can follow, but `.{ x: 1 }` and a field access
//! on a generic receiver only learn their type from [`super::infer`], which runs
//! after. So field names are bound here instead: once inference has stamped a
//! [`Ty`] on every node, each `FieldInit` / `FieldAccess` has a known base type
//! and its name resolves to a [`DefKind::Field`] def.
//!
//! The link is recorded as a [`Resolution`] on the use, which is what lets
//! [`super::lower`] and every later stage talk about a field by definition
//! rather than by string. Mismatches were already reported by inference; this
//! pass only records what it can and stays silent otherwise.
//!
//! It also enforces the one field-level rule that needs types: an `@using` field
//! must be a struct or a pointer to one (§3.10).

use std::collections::HashMap;

use crate::common::diagnostic::Diagnostic;
use crate::common::source::{FileId, FileSpan};
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, NodeId, NodeKind};

use super::def::{DefId, DefKind, DefTable};
use super::ty::Ty;
use super::{DefMeta, Resolution};

/// Bind every field use in `file` to its definition.
pub fn resolve_fields(
    defs: &DefTable,
    asts: &HashMap<FileId, Ast>,
    diags: &mut Vec<Diagnostic>,
    file: FileId,
) {
    let ast = &asts[&file];
    let pass = Fields { defs, ast, file };
    for id in ast.ids() {
        match ast.node(id).kind.clone() {
            // `base.name` on a value — the namespace form already carries a
            // resolution from name resolution.
            NodeKind::FieldAccess { base, name } => {
                if ast.meta::<Resolution>(id).is_none() {
                    if let Some(f) = pass.field_of(pass.ty(base), &name) {
                        ast.set_meta(id, Resolution::Def(f));
                    }
                }
            }
            // `base.0` on a **tuple struct** names a field like any other: its
            // members are defs named by their positions (see
            // `collect::collect_struct`). On an anonymous tuple there is no def
            // to bind — the projection is structural — so nothing is recorded
            // and `lower` keeps it an `Expr::TupleIndex`.
            NodeKind::TupleIndex { base, index } => {
                if let Some(f) = pass.field_of(pass.ty(base), &Symbol::new(&index.to_string())) {
                    ast.set_meta(id, Resolution::Def(f));
                }
            }
            // A composite literal's field names belong to the literal's *type*,
            // so they are bound from here rather than from the `FieldInit`,
            // which has no type of its own.
            NodeKind::CompositeLit { body, .. } => {
                if let crate::parser::ast::CompositeBody::Named(fields) = body {
                    let target = pass.ty(id);
                    for f in fields {
                        let NodeKind::FieldInit { name, .. } = ast.node(f).kind.clone() else {
                            continue;
                        };
                        if let Some(d) = pass.field_of(target.clone(), &name) {
                            ast.set_meta(f, Resolution::Def(d));
                        }
                    }
                }
            }
            NodeKind::Field { attrs, ty, .. } => {
                pass.check_using_field(id, &attrs, ty, diags);
            }
            _ => {}
        }
    }
}

struct Fields<'a> {
    defs: &'a DefTable,
    ast: &'a Ast,
    file: FileId,
}

impl Fields<'_> {
    /// The `DefKind::Field` named `name` on `base`, looking through pointers the
    /// way a field access does.
    fn field_of(&self, base: Ty, name: &Symbol) -> Option<DefId> {
        let def = match self.peel(base) {
            Ty::Nominal { def, .. } => def,
            _ => return None,
        };
        let m = *self.defs.get(def).ns.members.get(name)?;
        matches!(self.defs.get(m).kind, DefKind::Field | DefKind::Variant).then_some(m)
    }

    /// Strip pointers so `p.x` through a `*P` finds `P`'s field.
    fn peel(&self, ty: Ty) -> Ty {
        match ty {
            Ty::Ptr { inner, .. } => self.peel(*inner),
            other => other,
        }
    }

    fn ty(&self, node: NodeId) -> Ty {
        self.ast.meta::<Ty>(node).unwrap_or(Ty::Error)
    }

    /// `@using` marks a field for the implicit upcast, which only makes sense
    /// when the field is itself a struct (or a pointer to one) — §3.10.
    fn check_using_field(
        &self,
        node: NodeId,
        attrs: &[NodeId],
        ty: NodeId,
        diags: &mut Vec<Diagnostic>,
    ) {
        let is_using = self
            .ast
            .meta::<DefMeta>(node)
            .is_some_and(|m| self.defs.get(m.0).using);
        if !is_using || !attrs.iter().any(|&a| self.is_using_attr(a)) {
            return;
        }
        // The field's declared type node, read through any pointer.
        let target = match self.ast.node(ty).kind.clone() {
            NodeKind::PtrType { inner, .. } => inner,
            _ => ty,
        };
        let ok = self
            .ast
            .meta::<Resolution>(target)
            .and_then(|r| match r {
                Resolution::Def(d) => Some(self.defs.resolve_alias(d)),
                _ => None,
            })
            .is_some_and(|d| self.defs.get(d).kind == DefKind::Struct);
        if !ok {
            let span = FileSpan::new(self.file, self.ast.node(node).span);
            diags.push(
                Diagnostic::error("an `@using` field must be a struct, or a pointer to one")
                    .with_primary(span, ""),
            );
        }
    }

    fn is_using_attr(&self, attr: NodeId) -> bool {
        matches!(&self.ast.node(attr).kind, NodeKind::Attribute { name, .. } if name.as_str() == "using")
    }
}
