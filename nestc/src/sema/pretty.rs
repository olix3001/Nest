//! Metadata-aware tree rendering.
//!
//! [`tree_to_string`] prints a file's AST exactly like
//! [`crate::parser::pretty`], but appends each node's analysis metadata: the
//! [`DefMeta`] a defining node carries (its `DefId`, canonical name, kind, and
//! visibility) and the [`Resolution`] a use-site was linked to. [`defs_to_string`]
//! dumps the whole [`DefTable`] and the `#lang` registry for debugging and
//! snapshot tests.

use crate::common::source::FileId;
use crate::parser::ast::{Ast, NodeId};
use crate::parser::pretty::summary;

use super::def::DefTable;
use super::session::Session;
use super::{DefMeta, PathRes, Resolution};

/// Render `file`'s tree with per-node metadata annotations.
pub fn tree_to_string(session: &Session, file: FileId) -> String {
    let Some(ast) = session.asts.get(&file) else {
        return "<no such file>\n".to_string();
    };
    let mut out = String::new();
    match ast.root() {
        Some(root) => render(session, ast, root, 0, &mut out),
        None => out.push_str("<empty ast>\n"),
    }
    out
}

fn render(session: &Session, ast: &Ast, id: NodeId, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(&summary(ast, id));
    out.push_str(&annotate(session, ast, id));
    out.push('\n');
    for child in ast.node(id).kind.children() {
        render(session, ast, child, depth + 1, out);
    }
}

/// The trailing ` [ ... ]` annotation for a node, or empty if it carries none.
fn annotate(session: &Session, ast: &Ast, id: NodeId) -> String {
    let defs = &session.defs;
    if let Some(DefMeta(def)) = ast.meta::<DefMeta>(id) {
        let d = defs.get(def);
        let vis = if d.vis.is_public() { "pub" } else { "priv" };
        let lang = d
            .lang
            .as_ref()
            .map(|t| format!(" lang=\"{t}\""))
            .unwrap_or_default();
        return format!(
            "  [def d{} `{}` {} {}{}]",
            def.0,
            defs.canonical_string(def),
            d.kind.label(),
            vis,
            lang
        );
    }
    if let Some(PathRes(segs)) = ast.meta::<PathRes>(id) {
        let parts = segs
            .iter()
            .map(|r| res_str(defs, r))
            .collect::<Vec<_>>()
            .join(" . ");
        return format!("  [path {parts}]");
    }
    if let Some(res) = ast.meta::<Resolution>(id) {
        return format!("  [{}]", res_str(defs, &res));
    }
    String::new()
}

fn res_str(defs: &DefTable, res: &Resolution) -> String {
    match res {
        Resolution::Def(d) => format!("-> d{} `{}`", d.0, defs.canonical_string(*d)),
        Resolution::Intrinsic(name) => format!("-> ${name}"),
        Resolution::Error => "unresolved".to_string(),
    }
}

/// Dump every definition and the `#lang` registry — a stable, whole-program view
/// for snapshot tests.
pub fn defs_to_string(session: &Session) -> String {
    let defs = &session.defs;
    let mut out = String::new();
    out.push_str("=== defs ===\n");
    for def in defs.iter() {
        let vis = if def.vis.is_public() { "pub" } else { "priv" };
        let parent = def
            .parent
            .map(|p| format!(" parent=d{}", p.0))
            .unwrap_or_default();
        let alias = def
            .alias
            .map(|a| format!(" alias=d{}", a.0))
            .unwrap_or_default();
        let lang = def
            .lang
            .as_ref()
            .map(|t| format!(" lang=\"{t}\""))
            .unwrap_or_default();
        // `#lang` prints on its own above; the rest of the directives print
        // here, because a `#soa` on a struct or a `#packed` is carried on the
        // def and is the only record of it once the AST is behind us (§9).
        let directives: String = def
            .directives
            .iter()
            .filter(|d| !d.is("lang"))
            .map(|d| format!(" {}", crate::ir::pretty::directive_str(d)))
            .collect();
        out.push_str(&format!(
            "d{:<3} {:<10} {:<5} `{}`{}{}{}{}\n",
            def.id.0,
            def.kind.label(),
            vis,
            defs.canonical_string(def.id),
            parent,
            alias,
            lang,
            directives
        ));
    }
    let mut langs: Vec<_> = session.lang_items.iter().collect();
    langs.sort_by_key(|(k, _)| k.as_str().to_string());
    if !langs.is_empty() {
        out.push_str("=== lang items ===\n");
        for (tag, def) in langs {
            out.push_str(&format!("\"{tag}\" -> d{}\n", def.0));
        }
    }
    out
}
