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

/// Marks a type that contains itself by value, so the layout pass knows the
/// failure it is about to hit has already been explained.
///
/// It is a marker rather than a second computation for the reason every "already
/// reported" marker in this compiler is one: the two passes ask different
/// questions — "is there a cycle" and "how big is this" — and a cycle is the
/// answer to the first, so the second should not re-derive it and phrase it
/// worse.
#[derive(Debug, Clone, Copy)]
pub struct RecursiveLayout;

/// Run the declaration-level checks.
pub fn check(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    recursive_layouts(defs, meta, linked, out);
    directive_legality(defs, meta, linked, out);
    entry_point(defs, meta, linked, out);
    tests(defs, meta, linked, out);
    tests_are_not_named(defs, meta, linked, out);
}

/// Whether `def` was written `@test`.
///
/// The attribute is carried as a directive (`crate::sema::collect`), so this is
/// the one place that spelling is turned back into a question anyone asks.
pub fn is_test(defs: &DefTable, def: DefId) -> bool {
    defs.get(def).directives.iter().any(|d| d.is("test"))
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
            // Mark every type in the cycle, so the layout pass does not say the
            // same thing again in its own words: a type that contains itself has
            // no size *because* of the cycle, and one mistake gets one
            // diagnostic (the same rule [`RangeReported`] follows).
            for &d in &cycle {
                if let Some(ty) = linked.ty(d) {
                    meta.set(ty.id, RecursiveLayout);
                }
            }
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

    // A **member** carries directives too, and the same argument applies: one
    // written where it cannot mean anything will be ignored forever. `#align(N)`
    // over-aligns a field (§9) and `#raw` suppresses its zero-initialization;
    // `#packed` and `#soa` describe how an aggregate stores *its* members, which
    // is not a thing a single field can do.
    for t in linked.types() {
        for m in members_of(t) {
            for d in meta.directives(m.id) {
                let name = d.name.as_str();
                if matches!(name, "packed" | "soa") {
                    let mut diag =
                        Diagnostic::error(format!("`#{name}` does not apply to a field"));
                    if let Some(span) = meta.span(m.id) {
                        diag = diag.with_primary(span, "");
                    }
                    out.push(diag.with_note(
                        "it describes how an aggregate stores its members; write it on the \
                         type (§9)",
                    ));
                    continue;
                }
                if name == "align"
                    && !matches!(d.args.first(), Some(DirectiveArg::Int(n)) if *n > 0 && n.count_ones() == 1)
                {
                    let mut diag = Diagnostic::error(
                        "`#align` needs an integer argument that is a power of two",
                    );
                    if let Some(span) = meta.span(m.id) {
                        diag = diag.with_primary(span, "");
                    }
                    out.push(diag.with_note(
                        "alignment is an address constraint — \"a multiple of N\" — and only a \
                         power of two is one",
                    ));
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

/// Every member a type declares, a variant's payload included.
fn members_of(t: &TypeDef) -> Vec<&crate::ir::Member> {
    match &t.kind {
        TypeDefKind::Struct { members } => members.iter().collect(),
        TypeDefKind::Distinct { repr } => vec![repr],
        TypeDefKind::Enum { variants } => variants.iter().flat_map(|v| &v.members).collect(),
        TypeDefKind::Trait { .. } => Vec::new(),
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
    // `main` inside some namespace is an ordinary function. The rule is
    // `Linked::mains`, shared with the pass that synthesizes the entry point,
    // so the function checked here is the function that is called there.
    let mains: Vec<_> = linked.mains(defs).collect();

    // Two of them is two entry points, and picking one is not this compiler's
    // decision to make silently: whichever is reported second is a `main` the
    // program will never start at.
    for f in mains.iter().skip(1) {
        report_main(
            meta,
            f.id,
            "a program has one `main`",
            "another file in this compilation already defines one at file scope",
            out,
        );
    }

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

// ===< `@test` >===

/// A test's shape is the runner's to decide, the way `main`'s is the runtime's.
///
/// It takes no parameters — there is nothing to pass one — and it is not
/// generic, because there would be no instantiation of it to run. What it
/// *returns* is either nothing, or a `Result` whose success carries nothing:
/// a test says it failed by failing (a trap, a failed `assert`) or by returning
/// `.err`, and a value it returned successfully has nobody to read it.
fn tests(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    for f in linked.funcs() {
        if !is_test(defs, f.def) {
            continue;
        }
        if !f.params.is_empty() {
            report_test(meta, f.id, "a `@test` function takes no parameters", out);
            continue;
        }
        if meta
            .get::<crate::sema::infer::Generics>(f.id)
            .is_some_and(|g| !g.params.is_empty())
        {
            report_test(
                meta,
                f.id,
                "a `@test` function is not generic: there would be no instantiation of it to run",
                out,
            );
            continue;
        }
        let Some(Ty::Func { ret, .. }) = meta.ty(f.id) else {
            continue;
        };
        if !test_return_ok(defs, &ret) {
            report_test(
                meta,
                f.id,
                &format!(
                    "a `@test` function returns `void` or `Result.<void, E>`, not `{}`",
                    ret.display(defs)
                ),
                out,
            );
        }
    }
}

/// `void`, or a `Result` whose success type is `void`.
fn test_return_ok(defs: &DefTable, ret: &Ty) -> bool {
    match ret {
        Ty::Void | Ty::Never | Ty::Error => true,
        Ty::Nominal { def, args } => {
            defs.get(*def)
                .lang
                .as_ref()
                .is_some_and(|l| l.as_str() == "result")
                && matches!(args.first(), Some(Ty::Void))
        }
        _ => false,
    }
}

/// **Nothing in a program may name a `@test` function.**
///
/// A test is run by `nestc --test` and by `twig test`, and by nothing else. The
/// rule is not about what is in the binary — a `@test` function is compiled like
/// any other — but about what a test *is*: something the runner calls, once,
/// with a guard around it. A program that called one would be running a test
/// outside the only place a failure means anything.
///
/// It is **naming** rather than calling, because the two are the same thing: a
/// direct call's callee is the function's name (`Dispatch::Static`), and taking
/// its address is the same expression with nothing after it.
fn tests_are_not_named(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    for f in linked.funcs() {
        let Some(body) = &f.body else {
            continue;
        };
        super::block_children(body, &mut |e| {
            visit_named(defs, meta, e, out);
        });
    }
}

fn visit_named(defs: &DefTable, meta: &Meta, e: &crate::ir::Expr, out: &mut Vec<Diagnostic>) {
    if let crate::ir::ExprKind::Global(def) = &e.kind
        && is_test(defs, *def)
    {
        let mut d = Diagnostic::error(format!(
            "`{}` is a `@test` function, and a program cannot name one",
            defs.canonical_string(*def)
        ));
        if let Some(span) = meta.span(e.id) {
            d = d.with_primary(span, "");
        }
        out.push(
            d.with_note(
                "tests are run by `twig test`, which is the only place a failing one is reported"
                    .to_string(),
            ),
        );
    }
    super::children_of(e, &mut |c| visit_named(defs, meta, c, out));
}

fn report_test(meta: &Meta, at: IrId, message: &str, out: &mut Vec<Diagnostic>) {
    let mut d = Diagnostic::error(message.to_string());
    if let Some(span) = meta.span(at) {
        d = d.with_primary(span, "");
    }
    out.push(d.with_note("a test is run by `twig test`, which has nothing to pass it and nowhere to put a result".to_string()));
}

fn report_main(meta: &Meta, at: IrId, message: &str, note: &str, out: &mut Vec<Diagnostic>) {
    let mut d = Diagnostic::error(message.to_string());
    if let Some(span) = meta.span(at) {
        d = d.with_primary(span, "");
    }
    out.push(d.with_note(note.to_string()));
}
