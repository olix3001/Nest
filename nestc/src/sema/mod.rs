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

#[cfg(test)]
pub(crate) mod tests;

use crate::common::source::{FileId, FileSpan};
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, NodeId, NodeKind};

use def::{DefId, DefKind, Visibility};
use imports::{ImportDecl, ImportTarget, RawImport, RawTarget};
use session::{FileMeta, Session};

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
        NodeKind::Path { .. } => match ast.meta::<Resolution>(rhs) {
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
    if let Some(core_root) = session.load_package("core") {
        collect_reachable(session, vec![core_root]);
    }

    // Collect the entry file and its transitive imports.
    collect_reachable(session, vec![entry]);

    // The remaining stages run over every collected file (core included) —
    // except a library's, which were analyzed where the library was compiled,
    // and arrived with everything these stages would have worked out.
    let all_files: Vec<FileId> = session.files.keys().copied().collect();
    let files: Vec<FileId> = all_files
        .iter()
        .copied()
        .filter(|&f| !session.is_foreign_file(f))
        .collect();
    for file in wire_order(session, &files) {
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
    // Index every impl once the whole program is resolved; inference selects
    // over this table (operators, trait methods) per function body. This is also
    // where each impl's coherence is checked — it is the one pass that sees the
    // trait, the self type, and the package all three at once.
    let impls = {
        let Session {
            defs,
            asts,
            pkg_of,
            diagnostics,
            ..
        } = &mut *session;
        impls::build(defs, asts, pkg_of, diagnostics, &all_files)
    };
    for &file in &files {
        infer_one(session, &impls, file);
    }
    // Field uses can only be bound once their bases are typed, so this runs
    // after inference and before lowering reads the links.
    for &file in &files {
        resolve_fields_one(session, file);
    }
    for &file in &files {
        lower_one(session, file);
    }
    // Resolve each impl's target into types and keep the table. Inference is
    // done with it; monomorphization is not — choosing the impl for a
    // `Dispatch::Generic` call is a search over exactly this (see
    // [`infer::ImplTarget`]).
    let impl_targets = {
        let Session {
            defs,
            asts,
            diagnostics,
            lang_items,
            ..
        } = &mut *session;
        infer::resolve_impl_targets(defs, asts, diagnostics, lang_items, &impls)
    };
    session.impls = impls;
    session.impl_targets = impl_targets;
    // Everything past this point is whole-program: reachability starts at
    // `main`, exhaustiveness needs every variant of an enum declared elsewhere,
    // monomorphization collects instantiations across files. Merge the per-file
    // programs into the one view those passes read (see [`crate::ir::link`]).
    session.linked = crate::ir::link(&session.ir);

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
        crate::ir::check::run(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            &layouts,
        )
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
    let Session {
        defs,
        ir_meta,
        linked,
        impls,
        impl_targets,
        libraries,
        ..
    } = &mut *session;
    let foreign = |def: DefId| libraries.iter().any(|l| l.owns_def(def));
    let (mut diags, instances) =
        crate::ir::mono::run(defs, ir_meta, linked, impls, impl_targets, &foreign, &asked);

    // The `#const` check defers every call in a generic body: which function it
    // reaches is a question about the instantiation, and there were none. Now
    // there are, so the calls it skipped are ordinary static ones — and only the
    // instantiations are re-checked, because everything else was already judged
    // once above.
    let fresh: Vec<DefId> = linked.defs().filter(|d| !before.contains(d)).collect();
    crate::ir::check::constness::check_only(defs, ir_meta, linked, &fresh, &mut diags);
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
                // `impl []T` in `core/slice.nest` as living in the package that
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
fn wire_order(session: &Session, files: &[FileId]) -> Vec<FileId> {
    fn visit(
        session: &Session,
        file: FileId,
        seen: &mut std::collections::HashSet<FileId>,
        out: &mut Vec<FileId>,
    ) {
        if !seen.insert(file) {
            return;
        }
        let Some(meta) = session.files.get(&file) else {
            return;
        };
        for imp in &meta.imports {
            match imp.target {
                ImportTarget::File(f) | ImportTarget::PackageMember(f, _) => {
                    visit(session, f, seen, out)
                }
                ImportTarget::Broken => {}
            }
        }
        out.push(file);
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(files.len());
    for &file in files {
        visit(session, file, &mut seen, &mut out);
    }
    out
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
    let Session {
        asts,
        defs,
        diagnostics,
        ..
    } = &mut *session;
    let ast = &asts[&file];
    resolve::resolve_file(defs, diagnostics, ast, file, ns, &globs, builtins);
}

fn infer_one(session: &mut Session, impls: &impls::ImplTable, file: FileId) {
    let file_ns = session.files[&file].ns;
    let globs = session.prelude_globs.clone();
    let Session {
        asts,
        defs,
        diagnostics,
        lang_items,
        ..
    } = &mut *session;
    infer::infer_file(
        defs,
        asts,
        diagnostics,
        lang_items,
        impls,
        &globs,
        file_ns,
        file,
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
    let program = lower::lower_file(
        &session.defs,
        &session.lang_items,
        &session.asts,
        &session.ir_meta,
        &session.sources,
        file,
    );
    session.ir.insert(file, program);
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
