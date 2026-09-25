//! Semantic analysis: everything between parsing and (a future) type checking.
//!
//! The parser hands each file to this layer as a standalone
//! [`Ast`](crate::parser::ast::Ast). `sema` turns that pile of syntax trees into
//! a resolved program by running a fixed pipeline of **stages**, each in its own
//! file, over a shared [`Session`]:
//!
//! 1. **collect** ([`collect`]) — build the global [`DefTable`](def::DefTable):
//!    one [`Def`](def::Def) per namespace-level definition, with visibility and
//!    `#lang` tags. Runs on every file transitively reached by `import`.
//! 2. **imports** ([`imports`]) — load each `import` target once (sibling files
//!    and packages), then bind its names into the importing scope per the binding
//!    pattern (§4.5).
//! 3. **resolve** ([`resolve`]) — walk each file with a scope stack and attach a
//!    [`Resolution`] to every name, linking uses to the unique def they refer to
//!    (§4.6).
//! 4. **desugar** ([`desugar`]) — lower `for` / `.?` / `.!` to their core
//!    `#lang` forms (§6.13).
//! 5. **impls** ([`impls`]) — index every `impl` as a unit (trait, target,
//!    generics, members) for the solver to select over, and check each one's
//!    **coherence**: an inherent impl only where its type is defined, a trait
//!    impl only where the trait or the type is (§4.9).
//! 6. **infer** ([`infer`]) — Hindley–Milner type inference plus trait selection
//!    per function body, annotating every expression node with its resolved
//!    [`Ty`](ty::Ty) (§3.7) and every method call with how it dispatches.
//! 7. **fields** ([`fields`]) — bind each field *use* to its definition, now
//!    that inference has typed the bases those uses hang off.
//! 8. **lower** ([`lower`]) — build the typed [`ir`] tree from the resolved,
//!    desugared, typed AST (structured control flow kept, sugar and auto-deref
//!    made explicit).
//! 9. **link** ([`crate::ir::link`]) — merge the per-file programs into one
//!    whole-program [`Linked`](crate::ir::Linked). Every pass after lowering is
//!    whole-program, and this is the value they read.
//! 10. **check** ([`crate::ir::check`]) — the validation passes deferred out of
//!    inference, run over the linked IR.
//!
//! Results live in the [`Session`], not in the AST nodes: definitions in the
//! [`DefTable`], per-node facts in the arena's type-indexed metadata side table
//! (see [`Ast::set_meta`](crate::parser::ast::Ast::set_meta)). This is a library
//! — the `nestc` binary, a language server, and the tests all drive the same
//! [`Session`].

pub mod builtins;
pub mod collect;
pub mod decl;
pub mod def;
pub mod desugar;
pub mod fields;
pub mod impls;
pub mod imports;
pub mod infer;
pub mod intrinsics;
pub mod lower;
pub mod pretty;
pub mod resolve;
pub mod session;
pub mod ty;
pub mod when;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;

use crate::common::source::{FileId, FileSpan};
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, NodeId, NodeKind};

use def::{DefId, DefKind, DefTable, Visibility};
use imports::{ImportDecl, ImportTarget, RawImport, RawTarget};
use session::{FileMeta, Session};
use ty::Ty;

// ===< Per-node metadata attached by the stages >===

/// Which [`Def`](def::Def) a name refers to. Attached to name nodes (single- and
/// multi-segment [`Path`](crate::parser::ast::NodeKind::Path)s, the namespace
/// hops of a [`FieldAccess`](crate::parser::ast::NodeKind::FieldAccess), a
/// [`TypePath`](crate::parser::ast::NodeKind::TypePath)) by the resolver.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Resolution {
    /// Resolved to a unique definition.
    Def(DefId),
    /// Could not be resolved (a diagnostic was reported).
    Error,
}

/// Marks the temporary a `..` spread was bound to, so desugaring can tell its
/// own output from a spread the program wrote (see `desugar::lower_spread`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpreadBase;

/// Marks a node that *introduces* a definition, linking it to its [`DefId`] (and
/// thus its canonical name). Attached by collection and by the resolver's local
/// binding introduction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DefMeta(pub DefId);

/// What the resolver made for a closure (§5.5), stamped on its node: the
/// closure's type, its body as a function, and that function's first parameter
/// — the closure itself, which no source names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClosureDefs {
    pub ty: DefId,
    pub call: DefId,
    pub this: DefId,
}

/// The type parameters an `impl` return type is generic over (§5.4): the
/// function's own, in the order it declares them. Stamped on the return slot's
/// node, and recorded with what the body returned
/// ([`decl::ParamDecl::revealed`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OpaqueArgs(pub Vec<DefId>);

/// The locals a closure shares with the code around it, in the order it first
/// names them (§5.5): every local or parameter from outside that its body — or
/// a closure inside it — names, and that its capture list does not copy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Captures(pub Vec<DefId>);

/// A **declared** function's whole signature, stamped on its `FuncExpr` by
/// inference.
///
/// Deliberately not the node's [`Ty`](ty::Ty): `infer_func` uses that slot for
/// the function's *return* type, which is what lowering wants nearly everywhere.
/// A trait method needs the whole signature — a vtable slot's shape, and the
/// object-safety rules, are questions about the parameters as much as the result
/// — and a method with a default body would otherwise have its signature
/// overwritten by the per-function pass that runs afterwards.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Signature(pub ty::Ty);

/// What a type alias expands to, stamped on the alias's own binding node by the
/// declaration-level pass that checks it (`infer::check_type_aliases`).
///
/// An alias is expanded **on use**, and a use in another package has no tree to
/// expand: [`decl::record_types`] reads this back and files it under the alias's
/// definition, which is what travels. Derived rather than persisted for exactly
/// that reason — the answer travels once, on the def, not once per node.
#[derive(Debug, Clone)]
pub struct Expansion(pub ty::Ty);

/// The per-segment resolution of a multi-segment [`Path`], so every name in a
/// dotted path (e.g. `Self.Output`) is linked, not just the final one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PathRes(pub Vec<Resolution>);

// ===< Pipeline entry points >===

/// Convenience: create a session, register `packages`, load `src` as the entry
/// file named `name`, run the whole pipeline, and return the session.
/// Whether the right-hand side of a `::` binding defines a **value** rather than
/// a type.
///
/// A `::` RHS holds either one, and only its shape says which — which is why
/// [`DefKind::Const`] alone is not the answer. It is wrong in both directions:
///
/// - An associated type written with generic arguments (`Iter :: SliceIter.<T>`)
///   is a [`NodeKind::GenericApply`], which collection cannot tell from a call,
///   so it files under `Const`.
/// - A type alias to a *named* type (`A :: u8`) has a bare path for a RHS, which
///   collection cannot tell from a reference to another constant, so it files
///   under `Const` too — and then `B :: A` inherits the confusion.
///
/// So a bare path is answered by following it: `A :: B` is a type alias when `B`
/// names a type, a constant when it names one, and a chain of such bindings is
/// whatever it bottoms out at. Resolution has already done the hard part.
pub(crate) fn is_value_rhs(
    defs: &def::DefTable,
    asts: &std::collections::HashMap<FileId, Ast>,
    ast: &Ast,
    rhs: NodeId,
) -> bool {
    is_value_rhs_depth(defs, asts, ast, rhs, 0)
}

fn is_value_rhs_depth(
    defs: &def::DefTable,
    asts: &std::collections::HashMap<FileId, Ast>,
    ast: &Ast,
    rhs: NodeId,
    depth: u32,
) -> bool {
    // A binding that names itself would otherwise loop forever. The cycle is a
    // separate error; this pass must not hang on one.
    if depth > 16 {
        return false;
    }
    match &ast.node(rhs).kind {
        // `T [ ':=' init ]` — a declared type with a value under it: a typed
        // constant (§2.5), a `#static` region (§2.6), or an associated constant
        // (§3.4).
        NodeKind::AssocConst { .. } => true,
        // Written type syntax, and the declaration forms that define something
        // other than a value.
        NodeKind::PtrType { .. }
        | NodeKind::SliceType { .. }
        | NodeKind::ArrayType { .. }
        | NodeKind::TupleType { .. }
        | NodeKind::FuncType { .. }
        | NodeKind::DynType { .. }
        | NodeKind::DistinctType { .. }
        | NodeKind::AssocType { .. }
        | NodeKind::GenericApply { .. }
        | NodeKind::TypePath { .. }
        | NodeKind::FuncExpr { .. }
        | NodeKind::NamespaceExpr { .. }
        | NodeKind::StructType { .. }
        | NodeKind::EnumType { .. }
        | NodeKind::TraitType { .. }
        | NodeKind::Import { .. } => false,
        // A tuple of types is a type — `Item :: (usize, I.Item)` — and a tuple
        // with any value in it is a value.
        NodeKind::Tuple { elems } if !elems.is_empty() => elems
            .iter()
            .any(|&e| is_value_rhs_depth(defs, asts, ast, e, depth + 1)),
        // `I.Item`: a member reached through a name, which the resolver linked
        // exactly as it links a path's last segment.
        NodeKind::Path { .. } | NodeKind::FieldAccess { .. } => match ast.meta::<Resolution>(rhs) {
            Some(Resolution::Def(d)) => {
                let d = defs.resolve_alias(d);
                let target = defs.get(d);
                match target.kind {
                    DefKind::Struct
                    | DefKind::Enum
                    | DefKind::Trait
                    | DefKind::TypeAlias
                    | DefKind::TypeParam
                    | DefKind::Primitive
                    | DefKind::Namespace => false,
                    // Another `::` binding, which may itself be either. Follow
                    // it, in the file it was written in.
                    DefKind::Const => match (target.file, target.node) {
                        (Some(f), Some(n)) => match asts.get(&f) {
                            Some(a) => match &a.node(n).kind {
                                NodeKind::ConstBind { rhs, .. } => {
                                    is_value_rhs_depth(defs, asts, a, *rhs, depth + 1)
                                }
                                _ => false,
                            },
                            None => false,
                        },
                        _ => false,
                    },
                    // A function or a local used as a value.
                    _ => true,
                }
            }
            // Unresolved: something else already reported it, and treating it as
            // a value would type-check a name that does not exist.
            _ => false,
        },
        // Everything else is an expression: a literal, a call, an operator.
        _ => true,
    }
}

pub fn analyze_source(name: &str, src: &str, packages: &[(&str, &str)]) -> Session {
    let mut session = Session::new();
    for (pkg, root) in packages {
        session.register_package(pkg, root);
    }
    let file = session.sources.add(name.to_string(), src.to_string());
    // Mirror the parse-once cache and ast store the loader path would populate.
    let (ast, errors) = crate::parser::parse::Parser::parse_file(src, file);
    for err in errors {
        session
            .diagnostics
            .push(crate::common::diagnostic::simple_error(
                file,
                err.span,
                err.message,
            ));
    }
    session.asts.insert(file, ast);
    analyze(&mut session, file);
    session
}

/// Run the full pipeline over `entry` (already parsed into `session.asts`) and
/// every file it transitively imports.
pub fn analyze(session: &mut Session, entry: FileId) {
    session.claim_entry(entry);
    // Everything lowered from here on is this compilation's; below it are the
    // libraries' (see `crate::library`).
    session.own_ir_base = session.ir_meta.allocated();
    // The prelude is globbed into every scope, so `core` must be collected
    // before anything resolves against it.
    let mut core_files: Vec<FileId> = Vec::new();
    if let Some(core_root) = session.load_package("core") {
        collect_reachable(session, vec![core_root]);
        core_files = session.files.keys().copied().collect();
        core_files.sort_unstable();
    }

    // Collect the entry file and its transitive imports.
    collect_reachable(session, vec![entry]);

    // The remaining stages run over every collected file (core included) —
    // except a library's, which were analyzed where the library was compiled,
    // and arrived with everything these stages would have worked out.
    // **`core` first, then everything else in load order, then dependency
    // order within that.** Every pass below runs over this list, and several of
    // them write answers a later file reads — so the order is part of what they
    // mean, not a detail.
    //
    // `session.files` is a hash map, whose iteration order differs between two
    // runs of this compiler on the same program. `decl::record_types` is where
    // that showed: a constant in `core` whose right-hand side is not a literal
    // has its type recorded when `core` is inferred, and a program inferred
    // *before* `core` found nothing there — so `u8.MAX` needed a type
    // annotation on one run and not on the next.
    //
    // `core` is listed first explicitly rather than by id, because the entry
    // file is given its id before `core` is loaded, and because the prelude is
    // **not** an import: a file that writes `cast` names nothing `core` owns,
    // so no edge in `wire_order` would put `core` ahead of it.
    let mut rest: Vec<FileId> = session
        .files
        .keys()
        .copied()
        .filter(|f| !core_files.contains(f))
        .collect();
    rest.sort_unstable();
    let roots: Vec<FileId> = core_files
        .into_iter()
        .chain(rest)
        .filter(|&f| !session.is_foreign_file(f))
        .collect();
    let ordered = dependency_order(session, &roots);
    // A **library's** files are reached by the walk — they are what a
    // dependency edge points at — and are dropped again here: they were
    // analyzed where the library was compiled, and this compilation holds no
    // tree for them. They still have to be walked, because the order of what is
    // left depends on where they sit.
    let files: Vec<FileId> = ordered
        .into_iter()
        .filter(|&f| !session.is_foreign_file(f))
        .collect();
    // Two packages that depend on each other have no build order at all, and
    // the passes below would be analyzing one against a half-formed other.
    report_package_cycles(session);
    for &file in &files {
        imports::wire(session, file);
    }

    // Only `core.prelude` is globbed into every file — the rest of `core` needs
    // an explicit `import` (§4.6). It is found by its `#lang("prelude")` tag,
    // never by name or path, so a `core` laid out differently still works; and
    // the lookup has to happen *after* wiring, because the tag sits on an
    // `import` binding whose target namespace is only known once wired.
    if let Some(prelude) = session.lang_items.get("prelude") {
        let prelude = session.defs.resolve_alias(prelude);
        session.prelude_globs.push(prelude);
    }
    for &file in &files {
        resolve_one(session, file);
    }
    for &file in &files {
        desugar_one(session, file);
    }
    // What each of this package's definitions declares, written down once.
    //
    // Before anything asks: impl conformance is the first pass that looks a
    // definition up rather than walking it, and every pass after it does the
    // same. A definition in a **library** was recorded where that library was
    // compiled and arrived with its metadata, which is the whole reason this is
    // a table and not a walk (see [`decl::record`]).
    for &file in &files {
        let Session {
            defs, asts, decls, ..
        } = &mut *session;
        decl::record(defs, asts, decls, file);
    }
    // Index every impl once the whole program is resolved; inference selects
    // over this table (operators, trait methods) per function body. This is also
    // where each impl's coherence is checked — it is the one pass that sees the
    // trait, the self type, and the package all three at once.
    // Seeded with what the libraries brought, so that selection sees every impl
    // in the program and only this compilation's are read out of syntax.
    let mut impls = std::mem::take(&mut session.impls);
    {
        let Session {
            defs,
            asts,
            decls,
            pkg_of,
            diagnostics,
            ..
        } = &mut *session;
        impls::build(&mut impls, defs, asts, decls, pkg_of, diagnostics, &files);
    }
    // Resolve each impl's target into types, **before** inference rather than
    // after it: selection unifies against these on every trial of every
    // obligation, and resolving the same syntax each time was the bulk of what
    // a trial cost. Monomorphization wants the same answers long afterwards —
    // a `Dispatch::Generic` call is a bound with no impl chosen, and choosing
    // one is a search over exactly this (see [`infer::ImplTarget`]).
    let impl_targets = {
        let Session {
            defs,
            asts,
            decls,
            diagnostics,
            lang_items,
            ..
        } = &mut *session;
        infer::resolve_impl_targets(defs, asts, decls, diagnostics, lang_items, &mut impls)
    };
    // The types every declaration declares — a field's, a variant's payload,
    // a `distinct`'s representation — which only inference could work out, and
    // now has. The second half of the table `decl::record` started.
    //
    // Recorded **per file, as soon as that file is inferred**, rather than in a
    // sweep afterwards. `files` is in dependency order, so this is what puts a
    // package's answers in the table before anything that imports it is
    // inferred — and a use site asks during inference, not after it. A constant
    // whose type is not a literal is the case that needs it: `u8.MAX` reads
    // `core`'s `MAX`, whose type only `core`'s own inference worked out, and a
    // table still empty there would leave every read of it needing an
    // annotation.
    // A binding that names itself, directly or around a cycle, has nothing to
    // stand for; saying so here is what keeps the type it would have given its
    // uses from being an error nobody reported.
    report_alias_cycles(session, &files);
    for &file in &files {
        infer_one(session, &impls, file);
        {
            let Session {
                defs, asts, decls, ..
            } = &mut *session;
            decl::record_types(defs, asts, decls, file);
        }
        // And what each generic parameter's bounds carry — the types in them,
        // which only inference could resolve and which no tree here answers for
        // a parameter that arrived with a library (see [`decl::ParamDecl`]).
        let params = {
            let Session {
                defs,
                asts,
                decls,
                lang_items,
                ..
            } = &*session;
            infer::resolve_param_decls(defs, asts, decls, lang_items, &impls, file)
        };
        decl::record_param_decls(&mut session.decls, params);
    }
    // And each constant's value, which an array length in another package needs
    // and which is the same fold a length in this one does. After the types,
    // because it fills in entries `record_types` put there.
    for &file in &files {
        let values = {
            let Session {
                defs,
                asts,
                decls,
                lang_items,
                ..
            } = &*session;
            infer::fold_const_values(defs, asts, decls, lang_items, &impls, file)
        };
        decl::record_const_values(&mut session.decls, values);
    }
    // Two overloads that take the same arguments, which only the signatures
    // say — so it waits for them, where the rest of §4.3's duplicate rule is
    // checked as the names are collected.
    report_overload_conflicts(session, &files);
    // Field uses can only be bound once their bases are typed, so this runs
    // after inference and before lowering reads the links.
    for &file in &files {
        resolve_fields_one(session, file);
    }
    for &file in &files {
        lower_one(session, file);
    }
    session.impls = impls;
    session.impl_targets = impl_targets;
    // Everything past this point is whole-program: reachability starts at
    // `main`, exhaustiveness needs every variant of an enum declared elsewhere,
    // monomorphization collects instantiations across files. Merge the per-file
    // programs into the one view those passes read (see [`crate::ir::link`]).
    session.linked = crate::ir::link(&session.ir);
    // What each `impl` return type is, in its place, before anything reads a
    // type off the program (see [`crate::ir::reveal`]).
    crate::ir::reveal::run(
        &session.defs,
        &session.decls,
        &session.ir_meta,
        &mut session.linked,
    );

    // The validation passes deliberately deferred out of inference. They run on
    // the linked IR, where all surface sugar is already resolved, and they only
    // report — see [`crate::ir::check`].
    let diags = {
        // Layout is a **query**, built once and shared: "every type" is not a
        // set anyone can enumerate, so each one arrives when something asks
        // (see [`crate::ir::layout`]). This is also where the target comes back
        // after phase 5 took it out of the type layer — a pointer's width is a
        // layout question and nothing above this needs it.
        let layouts = crate::ir::layout::Layouts::new(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            session.options.target,
        );
        crate::ir::check::run(&session.defs, &session.ir_meta, &session.linked, &layouts)
    };
    session.diagnostics.extend(diags);

    // The backstop, and it runs **only when nothing else spoke**: an error type
    // that reached here with no diagnostic behind it is a defect in this
    // compiler, not in the program, and saying so with a span beats the backend
    // refusing a `void` slot three functions away (see
    // [`crate::ir::check::residue`]). After a real diagnostic the IR is full of
    // error types by design, so the condition is what keeps this quiet.
    if !session.has_errors() {
        let mut diags = Vec::new();
        crate::ir::check::residue::check(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            &mut diags,
        );
        session.diagnostics.extend(diags);
    }

    // Monomorphization. It runs **after** the checks and not before, because
    // every one of them wants to report against the program as it was written:
    // a mistake inside `func <T>` is one mistake, and an instantiation of that
    // function per call site would make it one per call site. The two
    // exceptions are the checks that could not be made yet at all — see below.
    //
    // A compilation that has already reported an error is left alone. Its IR
    // describes a program that does not type-check, so walking it would at best
    // find nothing new and at worst report a defect in this pass for a defect in
    // the program.
    session.ir_before_mono = session.ir_meta.allocated();
    session.defs_before_mono = session.defs.len() as u32;
    if !session.has_errors() {
        monomorphize(session);
    }
}

/// Instantiate every generic function, and re-run the checks that were deferred
/// waiting for exactly that.
fn monomorphize(session: &mut Session) {
    let before: std::collections::HashSet<DefId> = session.linked.defs().collect();
    // What a **test build** needs and no source names. Asked for here because
    // this is the last moment a generic can be instantiated at all.
    let wanted = test_wrappers(session);
    let asked: Vec<(DefId, Vec<infer::GenericArg>)> = wanted
        .iter()
        .map(|(_, item, args)| (*item, args.clone()))
        .collect();
    let target = session.options.target;
    let Session {
        defs,
        decls,
        ir_meta,
        linked,
        impls,
        impl_targets,
        libraries,
        ..
    } = &mut *session;
    let foreign = |def: DefId| libraries.iter().any(|l| l.owns_def(def));
    let (mut diags, instances) = crate::ir::mono::run(
        defs,
        decls,
        ir_meta,
        linked,
        impls,
        impl_targets,
        &foreign,
        &asked,
    );

    // The `#const` check defers every call in a generic body: which function it
    // reaches is a question about the instantiation, and there were none. Now
    // there are, so the calls it skipped are ordinary static ones — and only the
    // instantiations are re-checked, because everything else was already judged
    // once above.
    let fresh: Vec<DefId> = linked.defs().filter(|d| !before.contains(d)).collect();
    crate::ir::check::constness::check_only(defs, ir_meta, linked, &fresh, &mut diags);

    // A constant a generic `impl` declares is evaluated **where it is read**,
    // and a read inside a generic body only has concrete arguments once the
    // body has been instantiated — so this is the first moment it can be asked
    // at all (see [`crate::ir::check::constants::use_sites`]).
    {
        let layouts = crate::ir::layout::Layouts::new(defs, ir_meta, linked, target);
        crate::ir::check::constants::use_sites(defs, ir_meta, linked, &layouts, &mut diags);
    }
    session.diagnostics.extend(diags);
    for ((test, _, _), instance) in wanted.iter().zip(instances) {
        session.test_wrappers.insert(*test, instance);
    }
}

/// The instantiations a **test build** needs that nothing in the program names.
///
/// A `Result`-returning `@test` is run through `#lang("test_result")` at its own
/// error type (`lir::entry`): the wrapper is what turns an `.err` into a failure
/// that says what the error *was*. It is ordinary Nest in `core`, generic over
/// that error type, and no source calls it — so unless it is asked for here it
/// is dropped with every other generic declaration and there is nothing left to
/// call.
///
/// Each answer is the test, the declaration to instantiate, and what to
/// instantiate it with.
fn test_wrappers(session: &Session) -> Vec<(DefId, DefId, Vec<infer::GenericArg>)> {
    if !session.options.test {
        return Vec::new();
    }
    let Some(item) = session.lang_items.get("test_result") else {
        return Vec::new();
    };
    let item = session.defs.resolve_alias(item);
    session
        .entry_package_tests()
        .into_iter()
        .filter_map(|test| {
            let id = session.linked.get(test.def)?.id;
            let err = test_error_ty(&session.defs, &session.ir_meta.ty(id)?)?;
            Some((test.def, item, vec![infer::GenericArg::Ty(err)]))
        })
        .collect()
}

/// The `E` of a function returning `Result.<void, E>`, and nothing for any other
/// return type — a `void` test needs no wrapper.
fn test_error_ty(defs: &def::DefTable, ty: &ty::Ty) -> Option<ty::Ty> {
    let ty::Ty::Func { ret, .. } = ty else {
        return None;
    };
    let ty::Ty::Nominal { def, args } = &**ret else {
        return None;
    };
    if defs
        .get(*def)
        .lang
        .as_ref()
        .is_none_or(|l| l.as_str() != "result")
    {
        return None;
    }
    args.get(1).cloned()
}

/// Drain a worklist of files, parsing (already done by the loader), creating each
/// file's namespace def, collecting its definitions, and enqueuing the targets of
/// its imports. Files are processed at most once.
fn collect_reachable(session: &mut Session, mut queue: Vec<FileId>) {
    while let Some(file) = queue.pop() {
        if session.is_collected(file) {
            continue;
        }
        let name = session
            .sources
            .file(file)
            .map(|f| f.name.clone())
            .unwrap_or_default();
        // A file is a namespace of its own, named by where it sits in its
        // package — so two files declaring `Error` declare two types.
        let canonical = session
            .pkg_of
            .get(&file)
            .map(|n| {
                let mut path = vec![Symbol::new(n)];
                path.extend(session.module_path(n, &name));
                path
            })
            .unwrap_or_else(|| session.program_module_path(file, &name));
        if let Some(pkg) = session.pkg_of.get(&file).cloned()
            && let Some(twin) = session.module_twin(&pkg, &name)
            && session.twins_reported.insert(canonical.clone())
        {
            let path = canonical
                .iter()
                .map(Symbol::as_str)
                .collect::<Vec<_>>()
                .join(".");
            session.diagnostics.push(
                crate::common::diagnostic::Diagnostic::error(format!(
                    "`{name}` and `{twin}` are both the module `{path}`; a package may have one or the other"
                ))
                .with_primary(
                    crate::common::source::FileSpan::new(file, crate::common::span::Span::new(0, 0)),
                    "",
                ),
            );
        }
        let ns_name = canonical
            .last()
            .cloned()
            .unwrap_or_else(|| Symbol::new("<file>"));
        let ns = session.defs.alloc(
            ns_name,
            DefKind::Namespace,
            Visibility::Public,
            None,
            Some(file),
            None,
            None,
            canonical,
        );
        session.files.insert(
            file,
            FileMeta {
                ns,
                imports: Vec::new(),
                name: name.clone(),
            },
        );

        // Conditional compilation, before anything reads a name: a declaration
        // `#when` excludes is gone from the tree by the time collection walks
        // it, so it defines nothing and resolves nothing (`when`).
        let conds = when::Conditions::new(&session.options, session.in_entry_package(file));
        {
            let Session {
                asts, diagnostics, ..
            } = &mut *session;
            when::strip(&asts[&file], file, &conds, diagnostics);
        }

        // Collect definitions (disjoint field borrows while reading the ast).
        let in_core = session.pkg_of.get(&file).map(String::as_str) == Some("core");
        let raw_imports = {
            let Session {
                asts,
                defs,
                lang_items,
                diagnostics,
                ..
            } = &mut *session;
            let ast = &asts[&file];
            collect::collect_file(defs, lang_items, diagnostics, ast, file, ns, in_core)
        };

        // Load each import target and enqueue it for collection.
        let mut decls = Vec::with_capacity(raw_imports.len());
        let in_package = session.pkg_of.get(&file).cloned();
        for raw in raw_imports {
            let (target, enqueue) = load_target(session, file, &name, &raw);
            if let Some(f) = enqueue {
                // Package membership is transitive along *file* imports: a
                // package is a directory, so every file it reaches relatively is
                // its own. This is what gives `core`'s topic files the canonical
                // path `core` — and what lets the coherence rules recognize
                // `impl []T` in `core/iter/slice.nest` as living in the package that
                // owns the structural types (§4.9).
                //
                // A *package* import is not transitive: importing `<std>` from
                // inside `core` does not make `std` part of `core`.
                if let (Some(pkg), ImportTarget::File(_)) = (&in_package, &target) {
                    session.pkg_of.entry(f).or_insert_with(|| pkg.clone());
                }
                queue.push(f);
            }
            decls.push(ImportDecl {
                pattern: raw.pattern,
                scope: raw.scope,
                reexport: raw.reexport,
                target,
                lang: raw.lang,
                span: raw.span,
            });
        }
        session.files.get_mut(&file).unwrap().imports = decls;
    }
}

/// The order to wire imports in: every file before the files that import it.
///
/// Wiring is order-sensitive in one case, and it is the case `<core/ops>` is:
/// walking a package path (§4.5) looks up a *member* of the target namespace,
/// and a member that the target itself produces by re-export (`@public ops ::
/// import "ops.nest"`) does not exist until that file is wired. A glob or a
/// whole-namespace bind has no such dependency — it names the namespace, not
/// something inside it — which is why an arbitrary order worked until `core`
/// stopped globbing.
///
/// A post-order DFS over the import graph gives the order. Import cycles are
/// legal (`option.nest` and `control.nest` are one), so a file already being
/// visited is left where it is: something in a cycle has to be wired first, and
/// which one is arbitrary by construction.
/// The files of this compilation in **dependency order**, and every import
/// cycle found on the way.
///
/// Every pass in `analyze` runs over one list of files, and several of them
/// write answers a later file reads — what a declaration declares
/// ([`decl::record`], [`decl::record_types`]), what a constant is worth
/// ([`decl::record_const_values`]). A file inferred before the file it imports
/// finds nothing recorded there, so the order is part of what those passes
/// mean.
///
/// The import graph is a DAG, and this is its topological sort: a
/// depth-first walk that emits a file only once everything it imports has been
/// emitted. Roots are visited in the order given, which is what makes the
/// result the same on every run — the callers hand over a list they ordered
/// themselves rather than a hash map's iteration.
///
/// A **cycle** is the one thing a DAG cannot have, and a file graph can. Files
/// within one package may import each other freely — a package root that
/// re-exports a member the member reaches back through is an ordinary shape,
/// not a mistake — so a back edge is *broken* here and nothing is said about
/// it: the walk emits the file anyway, the order stays total, and every later
/// pass still runs. What is refused is a cycle between **packages**, which is a
/// different question with a different answer (see
/// [`report_package_cycles`]).
///
/// Breaking the edge is what the grey/black colouring below is for: grey is "on
/// the walk's own stack", and an edge to a grey file is the one edge not
/// followed.
fn dependency_order(session: &Session, files: &[FileId]) -> Vec<FileId> {
    #[derive(Clone, Copy, PartialEq)]
    enum Colour {
        /// On the walk's own stack: an edge back to one of these is a cycle.
        Grey,
        /// Emitted, with everything it depends on already emitted.
        Black,
    }

    fn visit(
        session: &Session,
        file: FileId,
        colour: &mut std::collections::HashMap<FileId, Colour>,
        out: &mut Vec<FileId>,
    ) {
        match colour.get(&file) {
            Some(Colour::Black) => return,
            // A back edge: this file is already on the walk's own stack, so
            // following it again would not terminate. Break it and let the
            // file be emitted by the frame that is already working on it.
            Some(Colour::Grey) => return,
            None => {}
        }
        colour.insert(file, Colour::Grey);
        if let Some(meta) = session.files.get(&file) {
            for imp in &meta.imports {
                match imp.target {
                    ImportTarget::File(f) | ImportTarget::PackageMember(f, _) => {
                        visit(session, f, colour, out)
                    }
                    ImportTarget::Broken => {}
                }
            }
        }
        colour.insert(file, Colour::Black);
        out.push(file);
    }

    let mut colour = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(files.len());
    for &file in files {
        visit(session, file, &mut colour, &mut out);
    }
    out
}

/// Refuse a cycle between **packages**, and say which import closes it.
///
/// Files inside one package may import each other however they like: a package
/// is compiled as a unit, and a root that re-exports a member the member
/// reaches back through is an ordinary shape. Two *packages* that depend on
/// each other are not the same thing. Each is built, published and read as a
/// whole — a library carries the answers a later compilation reads back — so
/// neither can be built first, and there is no build order that produces them
/// at all. Saying so here beats a link that fails, or a `.nlib` compiled
/// against a half-finished version of the package it depends on.
///
/// The graph is packages, one node each, with an edge wherever a file of one
/// imports a file of another. It is walked in name order so the same program
/// reports the same cycle every time, and each cycle is reported once, anchored
/// at the import that closes it.
/// What an overload set names has to be callable, and has to be **tellable
/// apart** (§4.3).
///
/// Checked once, where the set is written, rather than at each call through it:
/// a member that is not a function, a set that reaches itself, and two members
/// a call could never choose between are all mistakes in the declaration, and
/// reporting them at call sites would report one mistake as many.
/// Report every `::` binding whose right-hand side is a name that leads back
/// to it — `A :: B` with `B :: A`, or `void :: void` inside a namespace, where
/// the name on the right is the binding itself.
///
/// Such a binding stands for nothing, and before this every use of it was an
/// error type with no diagnostic behind it. Each binding in a cycle is reported
/// once, where it is written.
fn report_alias_cycles(session: &mut Session, files: &[FileId]) {
    // What each binding's right-hand side names, when it is only a name.
    let mut next: HashMap<DefId, DefId> = HashMap::new();
    for d in session.defs.iter() {
        if !matches!(d.kind, DefKind::Const | DefKind::TypeAlias)
            || !d.file.is_some_and(|f| files.contains(&f))
        {
            continue;
        }
        let (Some(file), Some(node)) = (d.file, d.node) else {
            continue;
        };
        let Some(ast) = session.asts.get(&file) else {
            continue;
        };
        let NodeKind::ConstBind { rhs, .. } = &ast.node(node).kind else {
            continue;
        };
        let head = match &ast.node(*rhs).kind {
            NodeKind::Path { .. } => *rhs,
            NodeKind::GenericApply { base, .. } => *base,
            NodeKind::TypePath { path, .. } => *path,
            _ => continue,
        };
        if let Some(Resolution::Def(to)) = ast.meta::<Resolution>(head) {
            next.insert(d.id, session.defs.resolve_alias(to));
        }
    }
    let mut reported: std::collections::HashSet<DefId> = std::collections::HashSet::new();
    let mut starts: Vec<DefId> = next.keys().copied().collect();
    starts.sort();
    for start in starts {
        let mut path = vec![start];
        let mut at = start;
        while let Some(&to) = next.get(&at) {
            if let Some(i) = path.iter().position(|&p| p == to) {
                let cycle = path[i..].to_vec();
                for &d in &cycle {
                    if !reported.insert(d) {
                        continue;
                    }
                    let def = session.defs.get(d);
                    let (Some(file), Some(span)) = (def.file, def.span) else {
                        continue;
                    };
                    let name = def.name.clone();
                    let message = if cycle.len() == 1 {
                        format!("`{name}` names itself, so it stands for nothing")
                    } else {
                        let around: Vec<String> = cycle
                            .iter()
                            .map(|&c| format!("`{}`", session.defs.get(c).name))
                            .collect();
                        format!(
                            "`{name}` names itself through {}, so it stands for nothing",
                            around.join(" → ")
                        )
                    };
                    session.diagnostics.push(
                        crate::common::diagnostic::Diagnostic::error(message)
                            .with_primary(FileSpan::new(file, span), ""),
                    );
                }
                break;
            }
            path.push(to);
            at = to;
        }
    }
}

fn report_overload_conflicts(session: &mut Session, files: &[FileId]) {
    let sets: Vec<DefId> = session
        .defs
        .iter()
        .filter(|d| d.kind == DefKind::Overload)
        .filter(|d| d.file.is_some_and(|f| files.contains(&f)))
        .map(|d| d.id)
        .collect();
    for set in sets {
        let decls = decl::Decls::new(&session.defs, &session.asts, &session.decls);
        let members = decls.overload_members(set);
        let candidates = decls.overload_candidates(set);
        let at = |session: &Session| match (session.defs.get(set).file, session.defs.get(set).span)
        {
            (Some(file), Some(span)) => Some(FileSpan::new(file, span)),
            _ => None,
        };
        // A member that is neither a function nor another set is not something
        // a call can reach.
        let wrong: Vec<Symbol> = members
            .iter()
            .filter(|&&m| !matches!(session.defs.get(m).kind, DefKind::Func | DefKind::Overload))
            .map(|&m| session.defs.get(m).name.clone())
            .collect();
        for name in wrong {
            if let Some(span) = at(session) {
                session.diagnostics.push(
                    crate::common::diagnostic::Diagnostic::error(format!(
                        "`{name}` is not a function, so an overload set cannot name it"
                    ))
                    .with_primary(span, ""),
                );
            }
        }
        // A set that reaches itself has no members to speak of: the walk stops
        // at the cycle, and saying so is better than a call with nothing to
        // choose from.
        if decls.overload_is_cyclic(set) {
            if let Some(span) = at(session) {
                let name = session.defs.get(set).name.clone();
                session.diagnostics.push(
                    crate::common::diagnostic::Diagnostic::error(format!(
                        "the overload set `{name}` names itself"
                    ))
                    .with_primary(span, ""),
                );
            }
            continue;
        }
        for (i, &later) in candidates.iter().enumerate() {
            let Some(mine) = params_of(session, later) else {
                continue;
            };
            for &earlier in &candidates[..i] {
                let Some(theirs) = params_of(session, earlier) else {
                    continue;
                };
                if !same_params(&session.defs, &mine, &theirs) {
                    continue;
                }
                let (a, b) = (
                    session.defs.get(earlier).name.clone(),
                    session.defs.get(later).name.clone(),
                );
                if let Some(span) = at(session) {
                    session.diagnostics.push(
                        crate::common::diagnostic::Diagnostic::error(format!(
                            "`{a}` and `{b}` take the same parameters, so a call through this \
                             overload set could not choose between them"
                        ))
                        .with_primary(span, ""),
                    );
                }
                break;
            }
        }
    }
}

/// The parameter types of a function, as its declaration recorded them.
fn params_of(session: &Session, def: DefId) -> Option<Vec<Ty>> {
    let decls = decl::Decls::new(&session.defs, &session.asts, &session.decls);
    match decls.signature(def)? {
        Ty::Func { params, .. } => Some(params),
        _ => None,
    }
}

/// Whether two parameter lists are the same signature — with each function's
/// **generic parameters matched up** rather than compared by identity, since
/// two declarations never share one: `func <T: Eq> (a: T)` twice is a conflict,
/// and `func <T: Eq> (a: T)` against `func <T: Ord> (a: T)` is not, because a
/// call with a type that is only `Eq` can tell them apart.
pub(crate) fn same_params(defs: &DefTable, a: &[Ty], b: &[Ty]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut map: HashMap<DefId, DefId> = HashMap::new();
    a.iter().zip(b).all(|(x, y)| same_ty(defs, x, y, &mut map))
}

fn same_ty(defs: &DefTable, a: &Ty, b: &Ty, map: &mut HashMap<DefId, DefId>) -> bool {
    let all = |x: &[Ty], y: &[Ty], map: &mut HashMap<DefId, DefId>| {
        x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same_ty(defs, p, q, map))
    };
    match (a, b) {
        (Ty::Nominal { def: p, args: x }, Ty::Nominal { def: q, args: y }) => {
            let generic = |d: DefId| defs.get(d).kind == DefKind::TypeParam;
            if generic(*p) || generic(*q) {
                return generic(*p)
                    && generic(*q)
                    && bounds_of(defs, *p) == bounds_of(defs, *q)
                    && *map.entry(*p).or_insert(*q) == *q;
            }
            p == q && all(x, y, map)
        }
        (
            Ty::Ptr {
                mutable: m,
                inner: x,
            },
            Ty::Ptr {
                mutable: n,
                inner: y,
            },
        )
        | (
            Ty::Slice {
                mutable: m,
                inner: x,
            },
            Ty::Slice {
                mutable: n,
                inner: y,
            },
        ) => m == n && same_ty(defs, x, y, map),
        (
            Ty::Array {
                len: l,
                mutable: m,
                inner: x,
            },
            Ty::Array {
                len: k,
                mutable: n,
                inner: y,
            },
        ) => l == k && m == n && same_ty(defs, x, y, map),
        (Ty::Tuple(x), Ty::Tuple(y)) => all(x, y, map),
        (Ty::Struct(x), Ty::Struct(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((n, p), (m, q))| n == m && same_ty(defs, p, q, map))
        }
        (
            Ty::Func {
                params: x,
                ret: p,
                c: a,
            },
            Ty::Func {
                params: y,
                ret: q,
                c: b,
            },
        ) => a == b && all(x, y, map) && same_ty(defs, p, q, map),
        _ => a == b,
    }
}

/// A generic parameter's bounds, as a set that can be compared.
fn bounds_of(defs: &DefTable, def: DefId) -> std::collections::BTreeSet<DefId> {
    defs.get(def)
        .param_bounds
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn report_package_cycles(session: &mut Session) {
    // package -> (package it depends on, the file and import span that says so)
    let mut edges: std::collections::BTreeMap<
        String,
        Vec<(String, FileId, crate::common::span::Span)>,
    > = std::collections::BTreeMap::new();
    for (&file, meta) in &session.files {
        let Some(from) = session.pkg_of.get(&file) else {
            continue;
        };
        for imp in &meta.imports {
            let target = match imp.target {
                ImportTarget::File(f) | ImportTarget::PackageMember(f, _) => f,
                ImportTarget::Broken => continue,
            };
            let Some(to) = session.pkg_of.get(&target) else {
                continue;
            };
            if to == from {
                continue;
            }
            let row = edges.entry(from.clone()).or_default();
            if !row.iter().any(|(p, _, _)| p == to) {
                row.push((to.clone(), file, imp.span));
            }
        }
    }
    for row in edges.values_mut() {
        row.sort_by(|a, b| a.0.cmp(&b.0));
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Colour {
        Grey,
        Black,
    }
    fn visit(
        pkg: &str,
        edges: &std::collections::BTreeMap<
            String,
            Vec<(String, FileId, crate::common::span::Span)>,
        >,
        colour: &mut std::collections::HashMap<String, Colour>,
        path: &mut Vec<String>,
        found: &mut Vec<(Vec<String>, FileId, crate::common::span::Span)>,
    ) {
        match colour.get(pkg) {
            Some(Colour::Black) => return,
            Some(Colour::Grey) => return,
            None => {}
        }
        colour.insert(pkg.to_string(), Colour::Grey);
        path.push(pkg.to_string());
        for (next, file, span) in edges.get(pkg).map(Vec::as_slice).unwrap_or(&[]) {
            if let Some(at) = path.iter().position(|p| p == next) {
                let mut cycle: Vec<String> = path[at..].to_vec();
                cycle.push(next.clone());
                found.push((cycle, *file, *span));
                continue;
            }
            visit(next, edges, colour, path, found);
        }
        path.pop();
        colour.insert(pkg.to_string(), Colour::Black);
    }

    let mut colour = std::collections::HashMap::new();
    let mut path = Vec::new();
    let mut found: Vec<(Vec<String>, FileId, crate::common::span::Span)> = Vec::new();
    for pkg in edges.keys() {
        visit(pkg, &edges, &mut colour, &mut path, &mut found);
    }
    // One diagnostic per distinct loop, however many walks reached it.
    let mut said: Vec<Vec<String>> = Vec::new();
    for (cycle, file, span) in found {
        let mut key = cycle.clone();
        key.pop();
        key.sort();
        key.dedup();
        if said.contains(&key) {
            continue;
        }
        said.push(key);
        let loop_text = cycle.join(" -> ");
        session.error(
            file,
            span,
            format!(
                "this import closes a cycle between packages, and neither can be built \
                 before the other: {loop_text}"
            ),
        );
    }
}

/// Load a [`RawImport`]'s target once, returning the resolved [`ImportTarget`]
/// and the file (if any) to enqueue for collection.
fn load_target(
    session: &mut Session,
    from: FileId,
    from_name: &str,
    raw: &RawImport,
) -> (ImportTarget, Option<FileId>) {
    match &raw.target {
        RawTarget::File(spec) => {
            match session.load_file(from_name, spec, FileSpan::new(from, raw.span)) {
                Some(f) => (ImportTarget::File(f), Some(f)),
                None => (ImportTarget::Broken, None),
            }
        }
        RawTarget::Package(segs) => {
            let pkg = segs[0].as_str();
            // A library that is here only because a dependency was compiled
            // against it: the package being compiled did not say it depends on
            // it, and a package imports what it depends on.
            if session
                .libraries
                .iter()
                .any(|l| l.name == pkg && !l.importable)
            {
                session.error(
                    from,
                    raw.span,
                    format!(
                        "`{pkg}` is not a dependency of this package, only of one it depends on; depend on it directly to import it"
                    ),
                );
                return (ImportTarget::Broken, None);
            }
            match session.load_package(pkg) {
                Some(root) => (
                    ImportTarget::PackageMember(root, segs[1..].to_vec()),
                    Some(root),
                ),
                None => {
                    session.error(from, raw.span, format!("unknown package `{pkg}`"));
                    (ImportTarget::Broken, None)
                }
            }
        }
    }
}

fn resolve_one(session: &mut Session, file: FileId) {
    let ns = session.files[&file].ns;
    let globs = session.prelude_globs.clone();
    let builtins = session.builtins;
    let doc = session.lang_items.get("doc");
    let Session {
        asts,
        defs,
        diagnostics,
        pkg_of,
        ..
    } = &mut *session;
    let ast = &asts[&file];
    resolve::resolve_file(defs, diagnostics, ast, file, ns, &globs, builtins, pkg_of, doc);
}

fn infer_one(session: &mut Session, impls: &impls::ImplTable, file: FileId) {
    let file_ns = session.files[&file].ns;
    let globs = session.prelude_globs.clone();
    let Session {
        asts,
        defs,
        decls,
        diagnostics,
        lang_items,
        pkg_of,
        ..
    } = &mut *session;
    infer::infer_file(
        defs,
        asts,
        decls,
        diagnostics,
        lang_items,
        impls,
        &globs,
        file_ns,
        file,
        pkg_of,
    );
}

/// Bind every field use to its definition, now that inference has typed the
/// bases those uses hang off.
fn resolve_fields_one(session: &mut Session, file: FileId) {
    let Session {
        asts,
        defs,
        diagnostics,
        ..
    } = &mut *session;
    fields::resolve_fields(defs, asts, diagnostics, file);
}

fn lower_one(session: &mut Session, file: FileId) {
    let lowered = lower::lower_file(
        &session.defs,
        &session.lang_items,
        &session.asts,
        &session.decls,
        &session.ir_meta,
        &session.sources,
        file,
    );
    decl::record_defaults(&mut session.decls, lowered.defaults);
    session.ir.insert(file, lowered.program);
}

fn desugar_one(session: &mut Session, file: FileId) {
    let ns = session.files[&file].ns;
    let Session {
        asts,
        defs,
        lang_items,
        diagnostics,
        ..
    } = &mut *session;
    let ast = asts.get_mut(&file).unwrap();
    desugar::desugar_file(ast, defs, lang_items, diagnostics, file, ns);
}
