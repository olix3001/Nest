//! A compact, readable rendering of an [`Ast`] as an indented tree — the form
//! the parser's snapshot tests capture. Each line is a node: its kind plus the
//! scalar data it carries inline (names, literals, operators, flags); children
//! are printed underneath in source order via [`NodeKind::children`], so the
//! renderer needs no per-field maintenance as the tree evolves.

use super::ast::{
    Ast, CompositeBody, Lit, NodeId, NodeKind, RangeKind, StructKind, VariantArgs, VariantPatArgs,
    VariantPayload,
};

/// Render the whole arena from its root as an indented tree. Returns
/// `"<empty ast>"` when there is no root.
pub fn tree_to_string(ast: &Ast) -> String {
    let mut out = String::new();
    match ast.root() {
        Some(root) => render(ast, root, 0, &mut out),
        None => out.push_str("<empty ast>\n"),
    }
    out
}

fn render(ast: &Ast, id: NodeId, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(&summary(ast, id));
    out.push('\n');
    for child in ast.node(id).kind.children() {
        render(ast, child, depth + 1, out);
    }
}

/// The one-line label for a node: its variant name plus any inline scalars.
/// Public so later passes (e.g. `sema::pretty`) can render the same base line
/// and append their own metadata annotations.
pub fn summary(ast: &Ast, id: NodeId) -> String {
    use NodeKind::*;
    let node = ast.node(id);
    match &node.kind {
        File { .. } => "File".into(),
        Attribute { name, .. } => format!("Attribute @{name}"),
        Directive { name, .. } => format!("Directive #{name}"),
        Decl { .. } => "Decl".into(),
        ConstBind { .. } => "ConstBind ::".into(),
        LocalDecl { is_const, .. } => {
            format!("LocalDecl {}", if *is_const { "const" } else { "let" })
        }
        Assign { op, .. } => format!("Assign {op:?}"),
        Defer { .. } => "Defer".into(),
        Return { .. } => "Return".into(),
        Break { .. } => "Break".into(),
        Continue => "Continue".into(),
        Loop { .. } => "Loop".into(),
        While { .. } => "While".into(),
        For { .. } => "For".into(),
        Block { tail, .. } => {
            if tail.is_some() {
                "Block (has tail)".into()
            } else {
                "Block".into()
            }
        }
        Lit(lit) => format!("Lit {}", lit_str(lit)),
        InterpolatedStr { .. } => "InterpolatedStr".into(),
        Path { segments } => {
            let path = segments
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(".");
            format!("Path `{path}`")
        }
        Unary { op, .. } => format!("Unary {op:?}"),
        Binary { op, .. } => format!("Binary {op:?}"),
        Tuple { .. } => "Tuple".into(),
        FieldAccess { name, .. } => format!("FieldAccess .{name}"),
        TupleIndex { index, .. } => format!("TupleIndex .{index}"),
        Call { .. } => "Call".into(),
        GenericApply { .. } => "GenericApply .<>".into(),
        Index { .. } => "Index".into(),
        Slice { .. } => "Slice".into(),
        Deref { .. } => "Deref .*".into(),
        Try { kind, .. } => format!("Try {kind:?}"),
        MatchExpr { .. } => "MatchExpr".into(),
        Arg { name, .. } => match name {
            Some(n) => format!("Arg {n}:"),
            None => "Arg".into(),
        },
        CompositeLit { ty, body } => {
            let shape = match body {
                CompositeBody::Named(_) => "named",
                CompositeBody::Positional(_) => "positional",
                CompositeBody::Repeat { .. } => "repeat",
            };
            let typed = if ty.is_some() { "typed" } else { "inferred" };
            format!("CompositeLit ({typed}, {shape})")
        }
        VariantLit { name, args } => format!("VariantLit .{name}{}", variant_args_tag(args)),
        FieldInit { name, .. } => format!("FieldInit {name}:"),
        If { els, .. } => {
            if els.is_some() {
                "If (has else)".into()
            } else {
                "If".into()
            }
        }
        IfMatch { els, .. } => {
            if els.is_some() {
                "IfMatch (has else)".into()
            } else {
                "IfMatch".into()
            }
        }
        MatchArm { guard, .. } => {
            if guard.is_some() {
                "MatchArm (guarded)".into()
            } else {
                "MatchArm".into()
            }
        }
        Range { kind, start, end } => {
            format!(
                "Range {}{}",
                range_kind_str(*kind),
                bounds_tag(start.is_some(), end.is_some())
            )
        }
        TypePath { .. } => "TypePath".into(),
        TypeHole => "TypeHole _".into(),
        CallerLocation => "CallerLocation #caller_location".into(),
        AssocBinding { name, .. } => format!("AssocBinding {name} ="),
        PtrType { mutable, .. } => format!("PtrType *{}", mut_tag(*mutable)),
        SliceType { mutable, .. } => format!("SliceType []{}", mut_tag(*mutable)),
        ArrayType { mutable, .. } => format!("ArrayType [N]{}", mut_tag(*mutable)),
        TupleType { .. } => "TupleType".into(),
        DynType { .. } => "DynType dyn".into(),
        DistinctType { .. } => "DistinctType distinct".into(),
        FuncType { .. } => "FuncType".into(),
        StructType { kind, .. } => {
            let shape = match kind {
                StructKind::Record(_) => "record",
                StructKind::Tuple(_) => "tuple",
                StructKind::Unit => "unit",
            };
            format!("StructType ({shape})")
        }
        Field { name, .. } => format!("Field {name}:"),
        EnumType { .. } => "EnumType".into(),
        Variant { name, payload, .. } => {
            let tag = match payload {
                VariantPayload::None => "",
                VariantPayload::Tuple(_) => " (tuple)",
                VariantPayload::Record(_) => " (record)",
            };
            format!("Variant {name}{tag}")
        }
        TraitType { .. } => "TraitType".into(),
        AssocType { .. } => "AssocType type".into(),
        AssocConst { default, .. } => {
            if default.is_some() {
                "AssocConst (with default)".into()
            } else {
                "AssocConst".into()
            }
        }
        FuncExpr {
            extern_abi, body, ..
        } => {
            let mut tag = String::from("FuncExpr");
            if let Some(abi) = extern_abi {
                tag.push_str(&format!(" extern(\"{abi}\")"));
            }
            if body.is_none() {
                tag.push_str(" (bodyless)");
            }
            tag
        }
        GenericTypeParam { name, .. } => format!("GenericTypeParam {name}"),
        GenericConstParam { name, .. } => format!("GenericConstParam const {name}"),
        Bounds { .. } => "Bounds +".into(),
        Param { name, ty, default } => {
            // Children print in `ty, default` order; the tag says which are there
            // so a lone child is never ambiguous.
            format!(
                "Param {name}{}{}",
                if ty.is_some() { ":" } else { " (inferred)" },
                if default.is_some() {
                    " (default :=)"
                } else {
                    ""
                }
            )
        }
        NamespaceExpr { .. } => "NamespaceExpr".into(),
        ImplBlock { for_ty, .. } => {
            if for_ty.is_some() {
                "ImplBlock (trait impl)".into()
            } else {
                "ImplBlock".into()
            }
        }
        Import { path } => format!("Import {}", import_str(path)),
        WildcardPat => "WildcardPat _".into(),
        GlobPat => "GlobPat *".into(),
        BindingPat { mutable, name } => {
            format!("BindingPat {}{name}", if *mutable { "mut " } else { "" })
        }
        AtPat { name, .. } => format!("AtPat {name} @"),
        LitPat(lit) => format!("LitPat {}", lit_str(lit)),
        RangePat { kind, start, end } => format!(
            "RangePat {}{}",
            range_kind_str(*kind),
            bounds_tag(start.is_some(), end.is_some())
        ),
        VariantPat { name, args } => format!("VariantPat .{name}{}", variant_pat_args_tag(args)),
        StructPat { rest, .. } => {
            if *rest {
                "StructPat (has ..)".into()
            } else {
                "StructPat".into()
            }
        }
        TupleStructPat { rest, .. } => {
            if *rest {
                "TupleStructPat (has ..)".into()
            } else {
                "TupleStructPat".into()
            }
        }
        TuplePat { .. } => "TuplePat".into(),
        SlicePat { rest, .. } => match rest {
            Some(r) => match &r.name {
                Some(name) => format!("SlicePat (@{} .. {name})", r.at),
                None => format!("SlicePat (@{} ..)", r.at),
            },
            None => "SlicePat".into(),
        },
        RefPat { .. } => "RefPat &".into(),
        OrPat { .. } => "OrPat |".into(),
        FieldPat { mutable, name, .. } => {
            format!("FieldPat {}{name}", if *mutable { "mut " } else { "" })
        }
        Error => "Error".into(),
    }
}

fn lit_str(lit: &Lit) -> String {
    match lit {
        Lit::Int(n) => format!("Int({n})"),
        Lit::Float(x) => format!("Float({x})"),
        Lit::Str(s) => format!("Str({s:?})"),
        Lit::Bytes(b) => format!("Bytes({})", crate::parser::ast::bytes_repr(b)),
        Lit::Char(c) => format!("Char({c:?})"),
        Lit::Bool(b) => format!("Bool({b})"),
    }
}

fn mut_tag(mutable: bool) -> &'static str {
    if mutable { "mut" } else { "" }
}

fn range_kind_str(kind: RangeKind) -> &'static str {
    match kind {
        RangeKind::HalfOpen => "..<",
        RangeKind::Closed => "..=",
        RangeKind::Open => "..",
    }
}

fn bounds_tag(has_start: bool, has_end: bool) -> &'static str {
    match (has_start, has_end) {
        (true, true) => " [start, end]",
        (true, false) => " [start]",
        (false, true) => " [end]",
        (false, false) => " [unbounded]",
    }
}

fn variant_args_tag(args: &VariantArgs) -> &'static str {
    match args {
        VariantArgs::None => "",
        VariantArgs::Tuple(_) => " (tuple)",
        VariantArgs::Record(_) => " (record)",
    }
}

fn variant_pat_args_tag(args: &VariantPatArgs) -> &'static str {
    match args {
        VariantPatArgs::None => "",
        VariantPatArgs::Tuple(_) => " (tuple)",
        VariantPatArgs::Record { .. } => " (record)",
    }
}

fn import_str(path: &super::ast::ImportPath) -> String {
    match path {
        super::ast::ImportPath::Package(segs) => {
            let joined = segs
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("/");
            format!("package <{joined}>")
        }
        super::ast::ImportPath::File(name) => format!("file {name:?}"),
    }
}
