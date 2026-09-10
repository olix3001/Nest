//! Declaration-level rules: recursive layouts, directive legality, and the
//! entry point.
//!
//! Three small checks that share a walk over the program's *declarations* rather
//! than its code. Each is the kind of rule that has no natural home in inference
//! — none of them is a typing question — and each needs the whole program: a
//! type's cycle may run through a type declared elsewhere, and there is exactly
//! one `main` in a compilation however many files there are.

use std::collections::HashSet;

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::{DefId, DefTable, DirectiveArg};
use crate::sema::ty::Ty;

use crate::ir::{IrId, Linked, Meta, TypeDef, TypeDefKind};

/// Run the declaration-level checks.
pub fn check(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    recursive_layouts(defs, meta, linked, out);
    directive_legality(defs, meta, linked, out);
    entry_point(defs, meta, linked, out);
}

// ===< Recursive layouts >===

/// A type may not contain itself **by value**.
///
/// `Node :: struct { next: Node }` has no size: laying it out means laying out a
/// `Node`, forever. Through a pointer it is fine and is the whole point of a
/// linked structure — `next: *Node` is one word whatever `Node` turns out to be,
/// and `Option.<*Node>` is how a list ends.
///
/// This has to run **before** layout, not as part of it: layout on a cyclic type
/// does not produce a wrong answer, it does not terminate.
fn recursive_layouts(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    // One diagnostic per cycle, not per type in it: `A` containing `B`
    // containing `A` is one mistake, and reporting it from both ends says the
    // same thing twice.
    let mut reported: HashSet<DefId> = HashSet::new();
    for t in linked.types() {
        if reported.contains(&t.def) {
            continue;
        }
        let mut path = Vec::new();
        let mut visiting = HashSet::new();
        if let Some(cycle) = find_cycle(meta, linked, t.def, &mut visiting, &mut path) {
            reported.extend(cycle.iter().copied());
            let names: Vec<String> = cycle
                .iter()
                .map(|&d| defs.get(d).name.to_string())
                .collect();
            let through = if names.len() == 1 {
                format!("`{}` contains itself", names[0])
            } else {
                format!(
                    "`{}` contains itself through {}",
                    names[0],
                    names[1..]
                        .iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let mut d =
                Diagnostic::error(format!("recursive type has no size: {through} by value"));
            if let Some(span) = meta.span(t.id) {
                d = d.with_primary(span, "this type cannot be laid out");
            }
            out.push(d.with_note(
                "store it behind a pointer instead — `*T` is one word whatever it points at, \
                 and `Option.<*T>` is how such a structure ends",
            ));
        }
    }
}

/// Depth-first search for a by-value cycle reachable from `def`.
fn find_cycle(
    meta: &Meta,
    linked: &Linked,
    def: DefId,
    visiting: &mut HashSet<DefId>,
    path: &mut Vec<DefId>,
) -> Option<Vec<DefId>> {
    if !visiting.insert(def) {
        // Closed a loop: the cycle is the tail of the path from here on.
        let start = path.iter().position(|&d| d == def).unwrap_or(0);
        return Some(path[start..].to_vec());
    }
    path.push(def);
    let found = by_value_members(meta, linked, def)
        .into_iter()
        .find_map(|next| find_cycle(meta, linked, next, visiting, path));
    path.pop();
    visiting.remove(&def);
    found
}

/// The nominal types `def` embeds **by value**, directly.
///
/// A pointer or a slice stops the walk: both are one word, whatever they refer
/// to. An array does not — `[4]Node` is four `Node`s laid end to end, so a type
/// containing an array of itself is just as unsized.
fn by_value_members(meta: &Meta, linked: &Linked, def: DefId) -> Vec<DefId> {
    let Some(t) = linked.ty(def) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut add = |ty: Ty| collect_nominals(&ty, &mut out);
    match &t.kind {
        TypeDefKind::Struct { members } => {
            for m in members {
                add(meta.ty_or_error(m.id));
            }
        }
        TypeDefKind::Enum { variants } => {
            for v in variants {
                for m in &v.members {
                    add(meta.ty_or_error(m.id));
                }
            }
        }
        TypeDefKind::Distinct { repr } => add(meta.ty_or_error(repr.id)),
        // A trait is not laid out; it has no size of its own to be infinite.
        TypeDefKind::Trait { .. } => {}
    }
    out
}

fn collect_nominals(ty: &Ty, out: &mut Vec<DefId>) {
    match ty {
        Ty::Nominal { def, args } => {
            out.push(*def);
            // A type argument is part of the layout: `Pair.<Node>` embeds a
            // `Node`. Walking it is what catches a cycle that only closes
            // through a generic.
            for a in args {
                collect_nominals(a, out);
            }
        }
        Ty::Tuple(elems) => elems.iter().for_each(|t| collect_nominals(t, out)),
        Ty::Array { inner, .. } => collect_nominals(inner, out),
        // A pointer or slice is one word: the walk stops.
        _ => {}
    }
}

// ===< Directive legality >===

/// Directives are *carried* through the front end without interpretation (§9),
/// which is right — `#inline` is a codegen decision and `#packed` is layout's —
/// but it means nothing has been checking that a directive is written somewhere
/// it could possibly mean something. A `#soa` on a function is not a subtle
/// mistake; it is a directive that will be silently ignored forever.
fn directive_legality(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    for t in linked.types() {
        for d in meta.directives(t.id) {
            let name = d.name.as_str();
            let ok = match name {
                // Layout directives apply to a type with fields to lay out.
                "packed" | "soa" => matches!(t.kind, TypeDefKind::Struct { .. }),
                "align" => matches!(
                    t.kind,
                    TypeDefKind::Struct { .. } | TypeDefKind::Enum { .. }
                ),
                _ => true,
            };
            if !ok {
                report_type(
                    meta,
                    t,
                    format!("`#{name}` does not apply to {}", what(t)),
                    out,
                );
                continue;
            }
            // `#align(N)`: N must be a power of two. Alignment is an address
            // constraint — "the address is a multiple of N" — and only a power
            // of two is expressible as one in any machine's addressing.
            if name == "align" {
                match d.args.first() {
                    Some(DirectiveArg::Int(n)) if *n > 0 && n.count_ones() == 1 => {}
                    Some(DirectiveArg::Int(n)) => {
                        report_type(meta, t, format!("`#align({n})` is not a power of two"), out)
                    }
                    _ => report_type(
                        meta,
                        t,
                        "`#align` needs an integer argument".to_string(),
                        out,
                    ),
                }
            }
        }
    }

    // `#section` and `#offset` name a place in the object file, so they apply to
    // the things that *become* symbols: functions and constants. On anything
    // else there is no symbol for them to describe.
    for f in linked.funcs() {
        for d in meta.directives(f.id) {
            let name = d.name.as_str();
            if matches!(name, "packed" | "soa") {
                let mut diag = Diagnostic::error(format!("`#{name}` does not apply to a function"));
                if let Some(span) = meta.span(f.id) {
                    diag = diag.with_primary(span, "");
                }
                out.push(diag.with_note("it is a layout directive, and applies to a type (§9)"));
                continue;
            }
            if name == "section" && !matches!(d.args.first(), Some(DirectiveArg::Str(_))) {
                let mut diag =
                    Diagnostic::error("`#section` needs a string naming an object-file section");
                if let Some(span) = meta.span(f.id) {
                    diag = diag.with_primary(span, "");
                }
                out.push(diag);
            }
            if name == "offset" && !matches!(d.args.first(), Some(DirectiveArg::Int(_))) {
                let mut diag = Diagnostic::error("`#offset` needs an integer position");
                if let Some(span) = meta.span(f.id) {
                    diag = diag.with_primary(span, "");
                }
                out.push(diag);
            }
        }
    }
    let _ = defs;
}

fn what(t: &TypeDef) -> &'static str {
    match t.kind {
        TypeDefKind::Struct { .. } => "a struct",
        TypeDefKind::Enum { .. } => "an enum",
        TypeDefKind::Distinct { .. } => "a `distinct` type",
        TypeDefKind::Trait { .. } => "a trait",
    }
}

fn report_type(meta: &Meta, t: &TypeDef, message: String, out: &mut Vec<Diagnostic>) {
    let mut d = Diagnostic::error(message);
    if let Some(span) = meta.span(t.id) {
        d = d.with_primary(span, "");
    }
    out.push(d.with_note("see §9 for where each directive may be written"));
}

// ===< The entry point >===

/// `main` is where a program starts, so its shape is not the author's to choose.
///
/// It takes no parameters and returns `void` or an integer status. Anything else
/// would leave the runtime with an argument it cannot supply or a result it
/// cannot use — and the mistake, being in a signature, is otherwise invisible
/// until link time.
///
/// A compilation with no `main` at all is **not** reported here: that is a
/// property of the output being built, and a library has no entry point. The
/// driver decides.
fn entry_point(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    // Only a `main` at file scope, and only outside `core` — a function called
    // `main` inside some namespace is an ordinary function.
    let mains: Vec<_> = linked
        .funcs()
        .filter(|f| f.name.as_str() == "main")
        .filter(|f| {
            let d = defs.get(f.def);
            d.parent
                .map(|p| defs.get(p).kind == crate::sema::def::DefKind::Namespace)
                .unwrap_or(false)
        })
        .collect();

    for f in mains {
        if !f.params.is_empty() {
            report_main(
                meta,
                f.id,
                "`main` takes no parameters",
                "the runtime has nothing to pass; read the command line through `core` instead",
                out,
            );
            continue;
        }
        let ret = match meta.ty(f.id) {
            Some(Ty::Func { ret, .. }) => *ret,
            _ => continue,
        };
        let ok = matches!(ret, Ty::Void | Ty::Int { .. } | Ty::Never | Ty::Error);
        if !ok {
            report_main(
                meta,
                f.id,
                &format!(
                    "`main` must return `void`, an integer status, or `never`, not `{}`",
                    ret.display(defs)
                ),
                "the runtime turns the result into a process exit status",
                out,
            );
        }
    }
}

fn report_main(meta: &Meta, at: IrId, message: &str, note: &str, out: &mut Vec<Diagnostic>) {
    let mut d = Diagnostic::error(message.to_string());
    if let Some(span) = meta.span(at) {
        d = d.with_primary(span, "");
    }
    out.push(d.with_note(note.to_string()));
}
