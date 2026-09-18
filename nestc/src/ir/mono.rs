//! Monomorphization: turning a generic program into a concrete one.
//!
//! A generic function is not code. `func <T> (x: T) -> T` describes a *family*
//! of functions, one per `T` any call site chooses, and a machine can be handed
//! only a member of that family — the parameter has to have a size before
//! anything can be pushed onto a stack for it. This pass is where the family
//! becomes its members: it walks the call graph from the program's entry points,
//! records every distinct set of generic arguments a function is reached with,
//! and emits one concrete [`Function`] per set.
//!
//! # What it decides, and why it is the one that decides it
//!
//! Three things happen here that nothing else can do:
//!
//! 1. **Identity.** "The `push` for `Vec.<i32>`" is a different function from
//!    "the `push` for `Vec.<f64>`", and this is where they become two
//!    [`DefId`]s. Every later stage can then talk about a function without also
//!    carrying the arguments it was instantiated with.
//! 2. **Names.** Because identity is decided here, the *symbol* is too
//!    (`design/lir.md` §7). A mangled name has one job — be injective — and it
//!    is the encoding of the instantiation key, so computing it anywhere else
//!    would mean deriving the key a second time, from a second implementation
//!    free to disagree. A disagreement between the two is a duplicate symbol or
//!    a missing one.
//! 3. **Dispatch through a bound.** `t.total()` on a `<T: Summing>` is an
//!    [`Dispatch::Generic`] call: the trait is known, the impl is not, because
//!    which impl applies is a question about `T`, and `T` is not a type yet.
//!    Once it is, the question has an answer, and answering it is the last thing
//!    standing between the IR and a program whose every call has a callee.
//!
//! # Where the arguments come from
//!
//! Not from here. Inference already worked out what each call site instantiated
//! its callee with — that *is* what inferring a call is — and it recorded the
//! answer as an [`Instantiation`] on the call, against the callee's
//! [`Generics`]. This pass reads the pair back.
//!
//! The alternative would be to unify the callee's declared signature against the
//! type the call settled on and recover the arguments from the result. That is a
//! second implementation of something already computed once, and it is wrong
//! wherever the signature does not mention a parameter — `func <T> () -> usize {
//! return $size_of.<T>() }` has no `T` anywhere in `func() -> usize`.
//!
//! # What the walk is for
//!
//! Not dead-code elimination. Whether to drop a function nothing calls is a
//! decision about the artifact being built — a library keeps its public surface
//! where an executable does not — and it is not this pass's to make. Every
//! concrete function is emitted and every one gets a symbol.
//!
//! What the walk decides is narrower, and is the thing that genuinely cannot be
//! decided any other way: **which instantiations exist**. There is no list of
//! them to enumerate; `Vec.<i32>` exists because something asked for it.
//!
//! That is also why the roots are every concrete function rather than `main`
//! (see [`roots`]): since nothing is dropped, everything emitted must have the
//! callees it names, and a generic declaration does not survive this pass.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::common::diagnostic::Diagnostic;
use crate::common::source::FileId;
use crate::common::symbol::Symbol;
use crate::parser::ast::Lit;
use crate::sema::def::{DefId, DefKind, DefTable};
use crate::sema::impls::ImplTable;
use crate::sema::infer::{GenericArg, Generics, ImplTarget, Instantiation, RangeReported};
use crate::sema::ty::{Const, ConstArg, FloatWidth, Ty};

use super::const_eval::ConstValue;
use super::{
    Arm, Binding, Block, Dispatch, Expr, ExprKind, Function, ImplicitCast, IrId, Linked, Meta,
    Pattern, PatternKind, VisitorMut, walk_block_mut, walk_expr_mut, walk_function_mut,
    walk_pattern_mut, walk_stmt_mut,
};

/// What one [`Function`] in the monomorphized program *is*: an instantiation of
/// a declaration, and the two names it will be known by.
///
/// Stamped on the function's [`IrId`] rather than held as a field for the reason
/// every other cross-shape fact is metadata (see [`super::meta`]): it is
/// computed by this pass and read by the ones after it, and putting it in
/// [`Function`] would mean every earlier stage constructing a value it has no
/// answer for.
#[derive(Debug, Clone)]
pub struct Instance {
    /// The declaration this instantiates. Equal to the function's own `def` when
    /// nothing was instantiated — a concrete function is its own only instance,
    /// and saying so uniformly is what lets a consumer read this on *every*
    /// function instead of asking first whether there is one.
    pub origin: DefId,
    /// What each of the declaration's [`Generics`] was bound to, in that order.
    /// Empty for a concrete function.
    pub args: Vec<GenericArg>,
    /// The unmangled name, with the arguments written out
    /// (`core.Vec.push.<i32>`). For dumps, diagnostics and profiles; dropped at
    /// codegen.
    pub name: String,
    /// The name the linker sees. **This** is what the program is keyed by from
    /// here on (`design/lir.md` §7).
    pub symbol: Symbol,
}

/// Which function fills each slot of one vtable, stamped on the
/// `*T` → `*dyn Trait` coercion that asked for it.
///
/// One coercion, one vtable: the pair `(trait, concrete)` is what a vtable *is*,
/// and the coercion node is the only place in the program where both are written
/// down together. Two coercions to the same pair record the same answer, and the
/// LIR lowering keys its table on the pair so only one constant is emitted
/// (`design/lir.md` §7b).
///
/// A `None` slot is one no impl and no default body fills, which object safety
/// should have made impossible. It is recorded as a hole rather than filled with
/// a guess, so that a defect shows up as a hole rather than as a call to the
/// wrong function.
/// One vtable per member of the type a `member_dyn` intrinsic was
/// instantiated at, in declaration order — the table `Member.index` selects
/// from. A `None` entry is a member whose type has no impl of the trait,
/// already reported.
#[derive(Debug, Clone)]
pub struct MemberVtables(pub Vec<Option<VtableSlots>>);

#[derive(Debug, Clone)]
pub struct VtableSlots {
    pub trait_def: DefId,
    pub concrete: Ty,
    /// One entry per trait method, in the trait's declaration order.
    pub slots: Vec<Option<DefId>>,
}

/// Monomorphize `linked` in place: instantiate every generic function reached
/// from the program's entry points, resolve every call through a bound, and give
/// every function a symbol.
///
/// `defs` is taken mutably because an instantiation is a **new definition**: it
/// has a name no source wrote and needs a [`DefId`] of its own, since that is
/// what [`Linked`] is keyed by.
pub fn run(
    defs: &mut DefTable,
    meta: &Meta,
    linked: &mut Linked,
    impls: &ImplTable,
    targets: &[ImplTarget],
    compiled_elsewhere: &dyn Fn(DefId) -> bool,
) -> Vec<Diagnostic> {
    let mut mono = Mono {
        defs,
        meta,
        impls,
        targets,
        out: Vec::new(),
        externs: linked
            .funcs()
            .filter(|f| f.extern_abi.is_some())
            .map(|f| f.def)
            .collect(),
        queue: VecDeque::new(),
        emitted: HashMap::new(),
        done: HashSet::new(),
        too_deep: HashSet::new(),
        member_impl: impls
            .impls
            .iter()
            .enumerate()
            .filter(|(_, imp)| imp.trait_def.is_some())
            .flat_map(|(i, imp)| imp.members.values().map(move |&m| (m, i)))
            .collect(),
        impl_ns: HashMap::new(),
    };
    for (i, imp) in impls.impls.iter().enumerate() {
        for &m in imp.members.values() {
            if let Some(ns) = mono.defs.get(m).parent
                && mono.defs.get(ns).name.as_str().starts_with('<')
            {
                mono.impl_ns.insert(ns, i);
            }
        }
    }

    // A function a library defines was compiled with the library, and so was
    // everything it instantiates: it is declared here, not emitted, so it asks
    // for nothing. It is still named — the declaration needs its symbol — and
    // marked done, so a call to it from here does not walk its body either.
    for root in roots(mono.defs, mono.meta, linked) {
        if compiled_elsewhere(root) {
            if mono.done.insert(root)
                && let Some(f) = linked.get(root)
            {
                let id = f.id;
                let qual = mono.trait_qualifier(linked, root, &[]);
                mono.stamp(id, root, Vec::new(), 0, qual.as_ref());
            }
            continue;
        }
        mono.reach(linked, root, Vec::new(), 0);
    }
    while let Some(job) = mono.queue.pop_front() {
        let Some(original) = linked.get(job.origin).cloned() else {
            continue;
        };
        let mut func = if job.args.is_empty() {
            original
        } else {
            mono.instantiate(linked, &original, job.def, &job.args)
        };
        mono.rewrite(linked, &mut func, job.depth);
        let file = linked.file_of(job.origin).unwrap_or(FileId(0));
        linked.insert(file, func);
    }

    // A generic declaration is not code, and now that its instantiations exist
    // there is nothing it could be emitted as. Dropping it is also what makes
    // the promise checkable: **no `Dispatch::Generic` survives**, and the only
    // place one could still be hiding is the body of a function that was never
    // going to be compiled.
    // A trait's default body is the same: generic over `Self`, and emitted
    // only as the instantiations that bind it.
    let instances: HashSet<DefId> = mono.emitted.values().copied().collect();
    let generic: Vec<DefId> = linked
        .defs()
        .filter(|&d| {
            !mono.generics_of(linked, d).is_empty()
                || (mono.default_body_of(d).is_some() && !instances.contains(&d))
        })
        .collect();
    for def in generic {
        linked.remove(def);
    }

    mono.out
}

/// The functions the walk starts from: **every concrete function the
/// compilation declares**.
///
/// That is wider than "the program's entry points", and deliberately so. This
/// pass does not drop what nothing calls — whether to is a decision about the
/// artifact being built, not about types, and a library keeps its public surface
/// where an executable does not. But a function that *is* emitted must have
/// everything it calls, and a generic declaration does not survive this pass: if
/// an unreached `f` calls `len.<[]u8>` and the walk never visited `f`, `f` would
/// be left naming a function that no longer exists.
///
/// So the rule is the one that follows from not eliminating anything: every
/// function that will be emitted is a root. When dead-code elimination arrives
/// the set narrows by itself, to `main` and the `extern` symbols the outside
/// world can call — the walk here does not change, only what is kept.
///
/// A **generic** declaration is not a root. There is no instantiation of it to
/// emit, and which ones exist is a question about its callers — for a `@public`
/// generic in a library, about a consumer this compilation cannot see. Cross
/// compilation-unit generics are a separate problem and this is the shape of it.
fn roots(defs: &DefTable, meta: &Meta, linked: &Linked) -> Vec<DefId> {
    linked
        .funcs()
        .filter(|f| {
            meta.get::<Generics>(f.id)
                .is_none_or(|g| g.params.is_empty())
        })
        // A trait's default body is generic over `Self` without saying so, and
        // is instantiated once per type that takes it.
        .filter(|f| {
            defs.get(f.def)
                .parent
                .is_none_or(|p| defs.get(p).kind != DefKind::Trait)
        })
        .map(|f| f.def)
        .collect()
}

// ===< The driver >===

/// One instantiation waiting to be emitted.
struct Job {
    /// The declaration being instantiated.
    origin: DefId,
    /// The def the instantiation will be known by — a fresh one when there are
    /// arguments, `origin` itself when there are none.
    def: DefId,
    args: Vec<GenericArg>,
    /// How many instantiations deep this one is: a root is 0, and a callee is
    /// one more than the body it was found in. See [`INSTANTIATION_DEPTH`].
    depth: u32,
}

/// How many instantiations deep the walk may go before giving up.
///
/// A generic function that calls itself at a *bigger* type has no fixed point:
/// `grow.<T>` calling `grow.<Box.<T>>` asks for `Box.<Box.<T>>` next, and there
/// is no argument set at which it stops. The program is legal to write and there
/// is no finite program to emit for it, so the compiler has to say so rather
/// than run until it dies — which, without this, it does: the types grow with
/// the queue and the first thing to break is the stack, under the recursion in
/// `subst_ty` or in mangling.
///
/// Plain recursion at the *same* arguments is not affected and needs no budget:
/// the instantiation is already queued when its own body is walked, so it is
/// found in `emitted` and the walk stops there.
///
/// 64 is deep enough that no honest program reaches it — a type nested 64 deep
/// is not something anyone writes — and shallow enough to fail in well under a
/// second.
const INSTANTIATION_DEPTH: u32 = 64;

struct Mono<'a> {
    defs: &'a mut DefTable,
    meta: &'a Meta,
    impls: &'a ImplTable,
    targets: &'a [ImplTarget],
    out: Vec<Diagnostic>,
    /// Every function declared `extern("c")`. Its symbol is its bare name —
    /// that is what C expects — so mangling has to know, and the [`Function`] is
    /// not always in hand where a symbol is computed.
    externs: HashSet<DefId>,
    queue: VecDeque<Job>,
    /// Instantiation key → the def emitted for it. The key is the **symbol**,
    /// which is exactly the right thing to deduplicate on: a mangled name's one
    /// job is to be injective, so two argument sets share a symbol precisely
    /// when they are the same instantiation (`design/lir.md` §7). Keying on the
    /// arguments themselves would need a `Hash` on [`Ty`], which it does not
    /// have — a `const` argument may hold a float.
    emitted: HashMap<Symbol, DefId>,
    /// Every def already queued or emitted, so a recursive generic function
    /// (`fact.<T>` calling itself) terminates.
    done: HashSet<DefId>,
    /// Declarations already reported as having no finite set of instantiations,
    /// so one runaway generic produces one diagnostic.
    too_deep: HashSet<DefId>,
    /// Which `impl` each member was declared in.
    ///
    /// Only a **trait** impl's members are here, and only because of naming: two
    /// impls of one trait-with-arguments for one type — `impl Conv.<i32> for
    /// Vec3` beside `impl Conv.<bool> for Vec3` — are coherent (§4.9), declare
    /// the same member name, and share a canonical path. Without the trait in
    /// the symbol they would share a symbol too, which is the one thing a
    /// mangled name may not do.
    member_impl: HashMap<DefId, usize>,
    /// The anonymous namespace each structural `impl` parks its members in
    /// (`<impl []T>`), mapped to that impl. The namespace's name is a label for
    /// a dump, and not something a symbol may contain, so a member's symbol
    /// encodes the impl's self type in its place.
    impl_ns: HashMap<DefId, usize>,
}

impl Mono<'_> {
    /// What `def` is generic over, or an empty list when it is concrete or
    /// inference never reached it.
    fn generics_of(&self, linked: &Linked, def: DefId) -> Vec<DefId> {
        linked
            .get(def)
            .and_then(|f| self.meta.get::<Generics>(f.id))
            .map(|g| g.params)
            .unwrap_or_default()
    }

    /// The trait a method implements, with its arguments, or `None` for a free
    /// function and for an inherent impl's method.
    ///
    /// This is what keeps two impls apart in a name. `impl Conv.<i32> for Vec3`
    /// and `impl Conv.<bool> for Vec3` both declare `to`, both park it under
    /// `Vec3`, and neither takes a generic argument of its own — so the
    /// canonical path is the same for both and the symbol would be too.
    ///
    /// The impl's *own* generics are substituted out of the arguments first: for
    /// `impl <T> Conv.<T> for Wrap.<T>` instantiated at `i32`, the qualifier is
    /// `Conv.<i32>` and not `Conv.<T>`, because two instantiations of one impl
    /// are two functions and have to be named as such.
    fn trait_qualifier(&self, linked: &Linked, origin: DefId, args: &[GenericArg]) -> Option<Ty> {
        let i = *self.member_impl.get(&origin)?;
        let trait_def = self.impls.impls[i].trait_def?;
        let trait_args = self.targets.get(i).map(|t| t.trait_args.clone())?;

        // The impl's generics are the tail of the instantiation's arguments (see
        // [`Generics::own`]), and they are what the impl's trait arguments are
        // written in terms of.
        let params = self.generics_of(linked, origin);
        let own = self.own_count(linked, origin).min(params.len());
        let mut subst = Subst::default();
        for (p, a) in params[own..].iter().zip(args.iter().skip(own)) {
            match a {
                GenericArg::Ty(t) => {
                    subst.tys.insert(*p, t.clone());
                }
                GenericArg::Const(k) => {
                    subst.consts.insert(*p, k.clone());
                }
            }
        }
        Some(Ty::Nominal {
            def: trait_def,
            args: trait_args.iter().map(|t| subst_ty(&subst, t)).collect(),
        })
    }

    /// The self type of the anonymous `impl` namespace `def` is declared in, if
    /// it is declared in one.
    fn impl_self(&self, def: DefId) -> Option<&Ty> {
        let ns = self.defs.get(def).parent?;
        let &i = self.impl_ns.get(&ns)?;
        self.targets.get(i).map(|t| &t.self_ty)
    }

    /// How many of `def`'s generic parameters it declared itself (the rest being
    /// the enclosing impl's) — see [`Generics::own`].
    fn own_count(&self, linked: &Linked, def: DefId) -> usize {
        linked
            .get(def)
            .and_then(|f| self.meta.get::<Generics>(f.id))
            .map(|g| g.own)
            .unwrap_or(0)
    }

    /// Note that `origin` is reached with `args`, queueing it if this is the
    /// first time, and return the def its instantiation will have.
    fn reach(
        &mut self,
        linked: &Linked,
        origin: DefId,
        args: Vec<GenericArg>,
        depth: u32,
    ) -> DefId {
        // A bodyless declaration — an `extern("c") func` — is a symbol and a
        // signature and nothing else. It is reached, not instantiated.
        if args.is_empty() {
            if !self.done.insert(origin) {
                return origin;
            }
            if let Some(f) = linked.get(origin) {
                let id = f.id;
                let qual = self.trait_qualifier(linked, origin, &[]);
                self.stamp(id, origin, Vec::new(), 0, qual.as_ref());
            }
            self.queue.push_back(Job {
                origin,
                def: origin,
                args,
                depth,
            });
            return origin;
        }

        if depth >= INSTANTIATION_DEPTH {
            self.report_too_deep(linked, origin);
            return origin;
        }

        let own = self.own_count(linked, origin);
        let qual = self.trait_qualifier(linked, origin, &args);
        let symbol = mangle(
            self.defs,
            &self.externs,
            origin,
            &args,
            own,
            qual.as_ref(),
            self.impl_self(origin),
        );
        if let Some(&d) = self.emitted.get(&symbol) {
            return d;
        }
        // An instantiation is a definition the source never wrote. It keeps the
        // declaration's canonical path — a diagnostic about it should name the
        // function the programmer can see — and carries what makes it *this* one
        // in its [`Instance`].
        let d = self.defs.get(origin);
        let (name, vis, parent, file, span, node, canonical) = (
            d.name.clone(),
            d.vis,
            d.parent,
            d.file,
            d.span,
            d.node,
            d.canonical.clone(),
        );
        let def = self.defs.alloc(
            name,
            DefKind::Func,
            vis,
            parent,
            file,
            span,
            node,
            canonical,
        );
        self.defs.get_mut(def).directives = self.defs.get(origin).directives.clone();
        self.emitted.insert(symbol, def);
        self.done.insert(def);
        self.queue.push_back(Job {
            origin,
            def,
            args,
            depth,
        });
        def
    }

    /// Report a generic that has no finite set of instantiations, once per
    /// declaration.
    ///
    /// Once, because the walk is a breadth-first queue: every sibling of the
    /// call that ran out of depth is about to run out too, and one mistake
    /// should not print sixty times.
    fn report_too_deep(&mut self, linked: &Linked, origin: DefId) {
        if !self.too_deep.insert(origin) {
            return;
        }
        let name = self.defs.canonical_string(origin);
        let mut d = Diagnostic::error(format!(
            "`{name}` has no finite set of instantiations: it was still being instantiated \
             {INSTANTIATION_DEPTH} levels deep"
        ));
        if let Some(span) = linked.get(origin).and_then(|f| self.meta.span(f.id)) {
            d = d.with_primary(span, "this function instantiates itself at a larger type");
        }
        self.out.push(
            d.with_note(
                "a generic that calls itself at a *different* argument — `f.<T>` calling \
             `f.<Box.<T>>` — asks for a new function every time, so there is no finite \
             program to emit"
                    .to_string(),
            ),
        );
    }

    /// Record what a function is, and the two names it will be known by.
    fn stamp(
        &mut self,
        id: IrId,
        origin: DefId,
        args: Vec<GenericArg>,
        own: usize,
        qual: Option<&Ty>,
    ) {
        let symbol = mangle(
            self.defs,
            &self.externs,
            origin,
            &args,
            own,
            qual,
            self.impl_self(origin),
        );
        let name = display_name(self.defs, origin, &args, own, qual);
        self.meta.set(
            id,
            Instance {
                origin,
                args,
                name,
                symbol,
            },
        );
    }

    // ===< Emitting one instantiation >===

    /// Clone `original`'s body into a concrete function, with every occurrence of
    /// its generic parameters replaced by `args`.
    ///
    /// The clone is a **renumbering**: every node gets a fresh [`IrId`] and the
    /// facts hung off the old one are copied across with their types
    /// substituted. It cannot share ids with the declaration, because the one
    /// fact that differs between two instantiations is precisely the per-node
    /// type, and that is keyed by id.
    ///
    /// What is deliberately *not* renumbered is a **local's [`DefId`]**. Two
    /// instantiations of a function share one declaration for each of its
    /// locals, which is what a local's def is: the place it was written. Their
    /// *types* differ and live on the fresh ids above; nothing else about a
    /// local does.
    fn instantiate(
        &mut self,
        linked: &Linked,
        original: &Function,
        def: DefId,
        args: &[GenericArg],
    ) -> Function {
        let params = self
            .meta
            .get::<Generics>(original.id)
            .map(|g| g.params)
            .unwrap_or_default();
        let mut subst = Subst::default();
        for (p, a) in params.iter().zip(args) {
            match a {
                GenericArg::Ty(t) => {
                    subst.tys.insert(*p, t.clone());
                }
                GenericArg::Const(k) => {
                    subst.consts.insert(*p, k.clone());
                }
            }
        }
        // A default body's `Self` is the argument past its declared ones.
        if let (Some(trait_def), Some(GenericArg::Ty(t))) =
            (self.default_body_of(original.def), args.get(params.len()))
        {
            subst.tys.insert(trait_def, t.clone());
        }

        let mut func = original.clone();
        func.def = def;
        let own = self
            .meta
            .get::<Generics>(original.id)
            .map(|g| g.own)
            .unwrap_or(0);
        let mut cloner = Cloner {
            meta: self.meta,
            subst: &subst,
        };
        cloner.visit_function(&mut func);
        let qual = self.trait_qualifier(linked, original.def, args);
        self.stamp(func.id, original.def, args.to_vec(), own, qual.as_ref());
        func
    }

    // ===< Walking a concrete function >===

    /// Walk an already-concrete function: resolve each call through a bound into
    /// a direct one, and queue every callee it reaches.
    fn rewrite(&mut self, linked: &Linked, func: &mut Function, depth: u32) {
        let mut r = Rewriter {
            mono: self,
            linked,
            depth,
        };
        r.visit_function(func);
    }

    /// Resolve one call and queue what it reaches.
    fn rewrite_call(&mut self, linked: &Linked, e: &mut Expr, depth: u32) {
        let recorded: Option<Vec<GenericArg>> = self.meta.get::<Instantiation>(e.id).map(|i| i.0);
        let callee_ty = self.meta.ty(call_callee_id(e).unwrap_or(e.id));
        let ExprKind::Call {
            callee,
            builtin,
            dispatch,
            ..
        } = &mut e.kind
        else {
            return;
        };
        // A builtin operator *is* a machine instruction. There is no function on
        // the other end of it, so there is nothing to instantiate and nothing to
        // name — the same O(1) escape every other pass over calls takes.
        if builtin.is_some() {
            return;
        }
        match dispatch {
            // Which function a vtable slot holds is a property of the vtable,
            // not of the call: it stays as it is, and the impls that fill the
            // slots are reached through the `*dyn` coercion that built them.
            Dispatch::Virtual { .. } => {}
            Dispatch::Generic {
                trait_def,
                method,
                self_ty,
                trait_args,
            } => {
                let (trait_def, method) = (*trait_def, *method);
                let (self_ty, trait_args) = (self_ty.clone(), trait_args.clone());
                let call_args = recorded.unwrap_or_default();
                match self.select(linked, trait_def, method, &self_ty, &trait_args, &call_args) {
                    Some((target, targs)) => {
                        let def = self.reach(linked, target, targs, depth);
                        *dispatch = Dispatch::Static;
                        callee.kind = ExprKind::Global(def);
                    }
                    // Inference proved an impl exists — that is what satisfying
                    // the bound *was* — so failing to find one here is a defect
                    // in this pass, not in the program. Saying so is better than
                    // leaving a call the next stage will trip over silently.
                    None => self.report_unresolved(e.id, trait_def, &self_ty),
                }
            }
            Dispatch::Static => {
                let ExprKind::Global(target) = callee.kind else {
                    return;
                };
                // A method of a trait called directly on a receiver whose type
                // is now concrete: the call a default body makes on `self`,
                // written against the trait's own `Self` and meaningful only
                // once an instantiation said what that is.
                if let Some(trait_def) = self.default_body_of(target) {
                    let has_receiver = match linked.ty(trait_def).map(|t| &t.kind) {
                        Some(super::TypeDefKind::Trait { methods, .. }) => methods
                            .iter()
                            .any(|m| m.def == target && m.recv != super::Recv::None),
                        _ => false,
                    };
                    let receiver = has_receiver
                        .then_some(callee_ty.as_ref())
                        .flatten()
                        .and_then(|t| match t {
                            Ty::Func { params, .. } => params.first().map(strip_ptr),
                            _ => None,
                        })
                        .filter(|t| {
                            !matches!(t, Ty::Dyn(_))
                                && !matches!(t, Ty::Nominal { def, .. } if *def == trait_def)
                        });
                    if let Some(self_ty) = receiver {
                        let call_args = recorded.clone().unwrap_or_default();
                        if let Some((to, targs)) =
                            self.select(linked, trait_def, target, &self_ty, &[], &call_args)
                        {
                            let def = self.reach(linked, to, targs, depth);
                            callee.kind = ExprKind::Global(def);
                            return;
                        }
                    }
                }
                if !linked.contains(target) {
                    return;
                }
                let args = match recorded {
                    Some(a) => a,
                    None => self.args_from_signature(linked, target, callee_ty.as_ref()),
                };
                let def = self.reach(linked, target, args, depth);
                callee.kind = ExprKind::Global(def);
            }
        }
    }

    /// Recover a call's generic arguments from the shape of its callee.
    ///
    /// The ordinary route is the [`Instantiation`] inference recorded, and it is
    /// the one to trust. An **operator** call has none: `a + b` picks its impl
    /// through trait selection rather than through the generic-instantiation
    /// path, so nothing along the way had an argument list to record. What it
    /// does have is the callee's type, reconstructed by lowering from the
    /// already-concrete operands — so matching the declaration's signature
    /// against it says what each parameter must be.
    ///
    /// This is exactly the derivation the recorded form exists to avoid, kept as
    /// the narrow fallback it is sound for: here both signatures are in hand and
    /// the arguments all appear in them, which is the case the recorded form
    /// covers and this one cannot.
    fn args_from_signature(
        &self,
        linked: &Linked,
        target: DefId,
        concrete: Option<&Ty>,
    ) -> Vec<GenericArg> {
        let params = self.generics_of(linked, target);
        if params.is_empty() {
            return Vec::new();
        }
        let (Some(f), Some(concrete)) = (linked.get(target), concrete) else {
            return Vec::new();
        };
        let Some(declared) = self.meta.ty(f.id) else {
            return Vec::new();
        };
        let mut bindings = Subst::default();
        match_ty(&params, &declared, concrete, &mut bindings);
        params
            .iter()
            .map(|p| match bindings.consts.get(p) {
                Some(k) => GenericArg::Const(k.clone()),
                None => GenericArg::Ty(bindings.tys.get(p).cloned().unwrap_or(Ty::Error)),
            })
            .collect()
    }

    /// Queue every method the vtable of `concrete` for `trait_def` will hold,
    /// and record **which** of them fills each slot.
    ///
    /// The slot order is the trait's declaration order, which
    /// [`TypeDefKind::Trait`](super::TypeDefKind::Trait) fixes for exactly this
    /// purpose, and walking it is also what makes two builds of one program
    /// identical — the impl's own member map is a `HashMap`, whose order varies
    /// between runs.
    ///
    /// Recording the answer is this pass's job for the same reason naming is:
    /// which function a slot holds is a question about *identity*, and the
    /// instantiated function that fills it does not exist until this pass makes
    /// it. The LIR lowering builds the vtable constant out of this (§7b) rather
    /// than re-selecting the impl, which would be a second implementation of the
    /// selection free to disagree with the first.
    fn reach_vtable(
        &mut self,
        linked: &Linked,
        trait_def: DefId,
        concrete: &Ty,
        depth: u32,
        at: IrId,
    ) {
        if let Some(slots) = self.vtable_slots(linked, trait_def, concrete, depth) {
            self.meta.set(at, slots);
        }
    }

    /// [`Mono::reach_vtable`] without recording the answer anywhere.
    fn vtable_slots(
        &mut self,
        linked: &Linked,
        trait_def: DefId,
        concrete: &Ty,
        depth: u32,
    ) -> Option<VtableSlots> {
        // A vtable is built for a trait as a *type* (`*dyn Trait`), which has no
        // arguments to give — `dyn Add.<f64>` would carry them in the type
        // itself, and object safety is a separate question. Nothing to match.
        let (i, bindings) = self.match_impl(trait_def, concrete, &[])?;
        let methods: Vec<(Symbol, DefId)> = match linked.ty(trait_def).map(|t| &t.kind) {
            Some(super::TypeDefKind::Trait { methods, .. }) => {
                methods.iter().map(|m| (m.name.clone(), m.def)).collect()
            }
            _ => Vec::new(),
        };
        let mut slots = Vec::with_capacity(methods.len());
        for (name, decl) in methods {
            // An impl that does not override a method still supplies it when the
            // trait gave it a default body; that body belongs to the trait, so
            // the trait's own declaration is the function to put in the slot.
            let target = self.impls.impls[i]
                .members
                .get(&name)
                .copied()
                .filter(|&m| linked.contains(m))
                .unwrap_or(decl);
            if self.defs.get(target).kind != DefKind::Func || !linked.contains(target) {
                slots.push(None);
                continue;
            }
            // A vtable slot takes no generic arguments of its own — that is what
            // object safety guarantees — so the impl's are the whole list.
            let mut args = self.inherited_args(linked, target, &bindings);
            if target == decl {
                args.push(GenericArg::Ty(concrete.clone()));
            }
            slots.push(Some(self.reach(linked, target, args, depth)));
        }
        Some(VtableSlots {
            trait_def,
            concrete: concrete.clone(),
            slots,
        })
    }

    /// Every vtable a `member_dyn.<T>` can hand out: one per member of `T`, for
    /// the trait its result type names.
    ///
    /// This is the call-graph edge the intrinsic is. Nothing else in the
    /// program names the members' impls, so without it they would never be
    /// instantiated — the same reason a `DynCast` reaches its vtable.
    fn reach_member_vtables(
        &mut self,
        linked: &Linked,
        trait_def: DefId,
        owner: &Ty,
        depth: u32,
        at: IrId,
    ) {
        let mut tables = Vec::new();
        for (name, member) in member_types(linked, self.meta, owner) {
            let slots = self.vtable_slots(linked, trait_def, &member, depth);
            if slots.is_none() {
                let mut d = Diagnostic::error(format!(
                    "member `{name}` of `{}` is a `{}`, which does not implement `{}`",
                    owner.display(self.defs),
                    member.display(self.defs),
                    self.defs.canonical_string(trait_def),
                ));
                if let Some(span) = self.meta.span(at) {
                    d = d.with_primary(span, "a trait object is made for every member here");
                }
                self.out.push(d);
            }
            tables.push(slots);
        }
        self.meta.set(at, MemberVtables(tables));
    }

    // ===< Selecting the impl a bound stood for >===

    /// Turn a call on a bound into the function it really calls.
    ///
    /// `self_ty` is concrete by now — that is what instantiating the enclosing
    /// function did to it — so the search is a *match* rather than a unification:
    /// each candidate impl's target is a pattern whose holes are its own
    /// generics, and matching binds them.
    ///
    /// The returned arguments are the two halves of [`Generics`] joined back
    /// together: the method's own come from the call site (the trait's
    /// declaration and the impl's list the same ones, so they line up by
    /// position), the impl's from the match.
    fn select(
        &mut self,
        linked: &Linked,
        trait_def: DefId,
        method: DefId,
        self_ty: &Ty,
        trait_args: &[Ty],
        call_args: &[GenericArg],
    ) -> Option<(DefId, Vec<GenericArg>)> {
        let name = self.defs.get(method).name.clone();
        // A receiver by pointer arrives as the `self` parameter's type, `*X`, and
        // the impl is for `X`. Matching `*X` first let a blanket `impl <T> Trait
        // for T` claim it at `T = *X` before `impl Trait for X` was tried, so the
        // pointer comes off exactly when the declaration says it is there.
        let by_ptr = match linked.ty(trait_def).map(|t| &t.kind) {
            Some(super::TypeDefKind::Trait { methods, .. }) => methods
                .iter()
                .any(|m| m.def == method && matches!(m.recv, super::Recv::Ptr | super::Recv::MutPtr)),
            _ => false,
        };
        let matched = match self_ty {
            Ty::Ptr { inner, .. } if by_ptr => self.match_impl_exact(trait_def, inner, trait_args),
            _ => self.match_impl(trait_def, self_ty, trait_args),
        };
        let (i, bindings) = matched?;
        let target = self.impls.impls[i]
            .members
            .get(&name)
            .copied()
            // An impl that does not override the method still supplies it when
            // the trait declared a default body. The default's code belongs to
            // the trait, so that is the function to call.
            .filter(|&m| linked.contains(m))
            .unwrap_or(method);
        if !linked.contains(target) {
            return None;
        }
        let own = self.own_count(linked, target);
        let mut args: Vec<GenericArg> = call_args.iter().take(own).cloned().collect();
        args.extend(self.inherited_args(linked, target, &bindings));
        if target == method && self.default_body_of(target).is_some() {
            args.push(GenericArg::Ty(strip_ptr(self_ty)));
        }
        Some((target, args))
    }

    /// The trait `def` is a default method body of, if it is one.
    ///
    /// A default body is written once and means something different for every
    /// implementing type: its `Self` is the trait's own nominal type until an
    /// instantiation binds it. So it is instantiated per `Self`, with that type
    /// as one argument past the ones it declares.
    fn default_body_of(&self, def: DefId) -> Option<DefId> {
        let parent = self.defs.get(def).parent?;
        (self.defs.get(parent).kind == DefKind::Trait).then_some(parent)
    }

    /// The arguments `target` inherits from the impl it belongs to, read out of
    /// the match that selected that impl.
    fn inherited_args(&self, linked: &Linked, target: DefId, bindings: &Subst) -> Vec<GenericArg> {
        let params = self.generics_of(linked, target);
        let own = self.own_count(linked, target);
        params[own.min(params.len())..]
            .iter()
            .map(|p| match bindings.consts.get(p) {
                Some(k) => GenericArg::Const(k.clone()),
                None => GenericArg::Ty(bindings.tys.get(p).cloned().unwrap_or(Ty::Error)),
            })
            .collect()
    }

    /// The impl of `trait_def` whose target matches `self_ty`, and what matching
    /// it bound the impl's generics to.
    ///
    /// A concrete target beats a blanket one (`impl <T> Trait for T`), exactly
    /// as it does during inference: the specific answer is the one the program
    /// meant. Nothing else needs ranking here, because inference already proved
    /// the choice is unambiguous — this is re-deriving a settled answer, not
    /// making it again.
    fn match_impl(
        &self,
        trait_def: DefId,
        self_ty: &Ty,
        trait_args: &[Ty],
    ) -> Option<(usize, Subst)> {
        // A method's `self` may be declared by pointer (`func (self: *Self)`),
        // and a `Dispatch::Generic` records the **parameter's** type — so the
        // receiver of `d.weight()` on a `*D` arrives here as `*Entity` while the
        // impl is written `impl Describe for Entity`. Try the type as written
        // first, so an impl really written for a pointer still wins, then
        // through it.
        self.match_impl_exact(trait_def, self_ty, trait_args)
            .or_else(|| {
                let inner = strip_ptr(self_ty);
                (inner != *self_ty).then(|| self.match_impl_exact(trait_def, &inner, trait_args))?
            })
    }

    fn match_impl_exact(
        &self,
        trait_def: DefId,
        self_ty: &Ty,
        trait_args: &[Ty],
    ) -> Option<(usize, Subst)> {
        let mut best: Option<(u8, usize, Subst)> = None;
        for (i, imp) in self.impls.impls.iter().enumerate() {
            if imp.trait_def != Some(trait_def) {
                continue;
            }
            let Some(target) = self.targets.get(i) else {
                continue;
            };
            let mut bindings = Subst::default();
            if !match_ty(&imp.generics, &target.self_ty, self_ty, &mut bindings) {
                continue;
            }
            // The trait's own arguments are the other half of the question, and
            // the only half when two impls agree about the self type: `impl
            // Add.<f64> for Vec3` beside `impl Add.<i32> for Vec3` is coherent
            // (§4.9) and both match `Vec3`. A bound that wrote no arguments does
            // not constrain them — a trait that takes none is the usual reason —
            // so an empty list matches anything rather than only an impl that
            // also wrote none.
            if !trait_args.is_empty() {
                if target.trait_args.len() != trait_args.len() {
                    continue;
                }
                let ok = target
                    .trait_args
                    .iter()
                    .zip(trait_args)
                    .all(|(p, a)| match_ty(&imp.generics, p, a, &mut bindings));
                if !ok {
                    continue;
                }
            }
            let score = if imp.self_is_generic() { 1 } else { 2 };
            if best.as_ref().is_none_or(|(b, _, _)| score > *b) {
                best = Some((score, i, bindings));
            }
        }
        best.map(|(_, i, b)| (i, b))
    }

    fn report_unresolved(&mut self, at: IrId, trait_def: DefId, self_ty: &Ty) {
        let mut d = Diagnostic::error(format!(
            "internal: no impl of `{}` for `{}` at monomorphization",
            self.defs.canonical_string(trait_def),
            self_ty.display(self.defs)
        ));
        if let Some(span) = self.meta.span(at) {
            d = d.with_primary(span, "this call has no callee");
        }
        self.out.push(d.with_note(
            "the bound was satisfied during inference, so this is a compiler defect".to_string(),
        ));
    }
}

// ===< The walk over one concrete function >===

/// Walks a concrete function, handing every call to [`Mono::rewrite_call`].
///
/// It is a [`VisitorMut`] rather than a hand-written recursion so that a node
/// shape added to the IR later reaches this pass by default instead of silently
/// not being walked.
struct Rewriter<'a, 'b> {
    mono: &'b mut Mono<'a>,
    linked: &'b Linked,
    /// How deep the function being walked is. Everything it reaches is one
    /// deeper — see [`INSTANTIATION_DEPTH`].
    depth: u32,
}

impl VisitorMut for Rewriter<'_, '_> {
    fn visit_expr(&mut self, expr: &mut Expr) {
        match &expr.kind {
            ExprKind::Call { .. } => self.mono.rewrite_call(self.linked, expr, self.depth + 1),
            // `member_dyn.<T>` makes a trait object for whichever member it is
            // handed, so every member's vtable is an edge from here.
            ExprKind::Intrinsic { name, .. } if name.as_str() == "member_dyn" => {
                let owner = match self.mono.meta.get::<Instantiation>(expr.id) {
                    Some(Instantiation(args)) => match args.first() {
                        Some(GenericArg::Ty(t)) => Some(t.clone()),
                        _ => None,
                    },
                    None => None,
                };
                if let (Some(owner), Some(t)) = (owner, dyn_trait(self.mono.meta, expr.id)) {
                    self.mono
                        .reach_member_vtables(self.linked, t, &owner, self.depth + 1, expr.id);
                }
            }
            // A `*T` unsized to `*dyn Trait` is a call-graph edge with no call
            // in it: the vtable built for `concrete` holds that impl's methods,
            // and something will later jump through one of them. Nothing else in
            // the program mentions those methods, so without this they would
            // never be instantiated and the vtable would have holes.
            ExprKind::DynCast { concrete, .. } => {
                let concrete = concrete.clone();
                if let Some(t) = dyn_trait(self.mono.meta, expr.id) {
                    self.mono
                        .reach_vtable(self.linked, t, &concrete, self.depth + 1, expr.id);
                }
            }
            _ => {}
        }
        walk_expr_mut(self, expr);
    }
}

/// The members of a struct or a tuple, as the concrete types an instantiation
/// gives them, in declaration order — the order `Member.index` counts in.
fn member_types(linked: &Linked, meta: &Meta, ty: &Ty) -> Vec<(String, Ty)> {
    match ty {
        Ty::Tuple(elems) => elems
            .iter()
            .enumerate()
            .map(|(i, t)| (i.to_string(), t.clone()))
            .collect(),
        Ty::Nominal { def, args } => {
            let Some(t) = linked.ty(*def) else {
                return Vec::new();
            };
            // A `distinct` has one member at index 0, its representation — the
            // one a `member_dyn` over it reaches, since the description lists
            // no members to select it by.
            let members = match &t.kind {
                super::TypeDefKind::Struct { members } => members.as_slice(),
                super::TypeDefKind::Distinct { repr } => std::slice::from_ref(repr),
                _ => return Vec::new(),
            };
            let mut subst = Subst::default();
            if let Some(Ty::Nominal { args: params, .. }) = meta.ty(t.id) {
                for (p, a) in params.iter().zip(args) {
                    if let Ty::Nominal { def: pd, args } = p
                        && args.is_empty()
                    {
                        subst.tys.insert(*pd, a.clone());
                    }
                }
            }
            members
                .iter()
                .map(|m| (m.name.to_string(), subst_ty(&subst, &meta.ty_or_error(m.id))))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The id of a call's callee expression, which is where its instantiated
/// signature was stamped.
fn call_callee_id(e: &Expr) -> Option<IrId> {
    match &e.kind {
        ExprKind::Call { callee, .. } => Some(callee.id),
        _ => None,
    }
}

/// The value type behind any number of pointers.
fn strip_ptr(ty: &Ty) -> Ty {
    match ty {
        Ty::Ptr { inner, .. } => strip_ptr(inner),
        other => other.clone(),
    }
}

/// The trait a `*dyn Trait` coercion erased to, read off the node's own type.
fn dyn_trait(meta: &Meta, id: IrId) -> Option<DefId> {
    match meta.ty(id)? {
        Ty::Ptr { inner, .. } => match *inner {
            Ty::Dyn(d) => Some(d),
            _ => None,
        },
        Ty::Dyn(d) => Some(d),
        _ => None,
    }
}

/// The name a dump, a diagnostic or a profile shows (`core.Vec.<i32>.push`).
///
/// The arguments are placed where they were written: the enclosing impl's on
/// the type the impl is for, the function's own on the function. That is the
/// same split the symbol is built from, and the reason is the same — a reader
/// looking at `core.Vec.<i32>.push` should see the thing they wrote.
fn display_name(
    defs: &DefTable,
    origin: DefId,
    args: &[GenericArg],
    own: usize,
    qual: Option<&Ty>,
) -> String {
    let render = |args: &[GenericArg]| {
        let inner: Vec<String> = args
            .iter()
            .map(|a| match a {
                GenericArg::Ty(t) => t.display(defs),
                GenericArg::Const(k) => k.display(defs),
            })
            .collect();
        format!(".<{}>", inner.join(", "))
    };
    let d = defs.get(origin);
    let path: Vec<String> = if d.canonical.is_empty() {
        vec![d.name.to_string()]
    } else {
        d.canonical.iter().map(|s| s.to_string()).collect()
    };
    let own = own.min(args.len());
    let (own_args, inherited) = args.split_at(own);

    let mut out = String::new();
    for (i, seg) in path.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        // `Vec3.<as Conv.<i32>>.to` — the pseudo-segment says which impl this
        // member came from, in the same angle-bracketed style the IR dump
        // already uses for `core.<impl []T>.len`. Two frames both labelled
        // `Vec3.to` would be a worse dump than a longer one.
        if i + 1 == path.len()
            && let Some(t) = qual.filter(|_| path.len() >= 2)
        {
            out.push_str(&format!("<as {}>.", t.display(defs)));
        }
        out.push_str(seg);
        if i + 2 == path.len() && !inherited.is_empty() {
            out.push_str(&render(inherited));
        }
    }
    if !own_args.is_empty() {
        out.push_str(&render(own_args));
    }
    // Nowhere to hang the impl's arguments — a one-component path — so they
    // join the function's rather than vanish.
    if !inherited.is_empty() && path.len() < 2 {
        out.push_str(&render(inherited));
    }
    out
}

// ===< Substitution, and matching >===

/// A binding of generic parameters to what they stand for.
#[derive(Debug, Clone, Default)]
struct Subst {
    tys: HashMap<DefId, Ty>,
    consts: HashMap<DefId, Const>,
}

/// Replace every generic parameter in `ty` by what `subst` binds it to.
///
/// A parameter this substitution says nothing about is left alone rather than
/// erased: a nested generic's parameters travel through here untouched on their
/// way to their own instantiation.
fn subst_ty(subst: &Subst, ty: &Ty) -> Ty {
    match ty {
        Ty::Nominal { def, args } if args.is_empty() => {
            subst.tys.get(def).cloned().unwrap_or_else(|| ty.clone())
        }
        Ty::Nominal { def, args } => Ty::Nominal {
            def: *def,
            args: args.iter().map(|a| subst_ty(subst, a)).collect(),
        },
        Ty::Int { signed, width } => Ty::Int {
            signed: *signed,
            width: subst_const(subst, width),
        },
        Ty::Ptr { mutable, inner } => Ty::Ptr {
            mutable: *mutable,
            inner: Box::new(subst_ty(subst, inner)),
        },
        Ty::Slice { mutable, inner } => Ty::Slice {
            mutable: *mutable,
            inner: Box::new(subst_ty(subst, inner)),
        },
        Ty::Array {
            len,
            mutable,
            inner,
        } => Ty::Array {
            len: subst_const(subst, len),
            mutable: *mutable,
            inner: Box::new(subst_ty(subst, inner)),
        },
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|e| subst_ty(subst, e)).collect()),
        Ty::Func { params, ret } => Ty::Func {
            params: params.iter().map(|p| subst_ty(subst, p)).collect(),
            ret: Box::new(subst_ty(subst, ret)),
        },
        other => other.clone(),
    }
}

fn subst_const(subst: &Subst, k: &Const) -> Const {
    match k {
        Const::Param(d) => subst.consts.get(d).cloned().unwrap_or_else(|| k.clone()),
        _ => k.clone(),
    }
}

/// Match `pattern` — an impl's target, whose holes are the defs in `holes` —
/// against the concrete `ty`, recording what each hole must be.
///
/// This is one-way on purpose. Inference unifies, because there both sides may
/// have unknowns; here the right-hand side is a type the program settled on and
/// the only question is what the impl's generics would have to be for it to
/// apply. A hole that is asked to be two different things makes the match fail,
/// which is what keeps `impl <T> Pair.<T, T>` from matching `Pair.<i32, f64>`.
fn match_ty(holes: &[DefId], pattern: &Ty, ty: &Ty, out: &mut Subst) -> bool {
    if let Ty::Nominal { def, args } = pattern
        && args.is_empty()
        && holes.contains(def)
    {
        return match out.tys.get(def) {
            Some(prev) => prev == ty,
            None => {
                out.tys.insert(*def, ty.clone());
                true
            }
        };
    }
    match (pattern, ty) {
        (Ty::Nominal { def: a, args: xs }, Ty::Nominal { def: b, args: ys }) => {
            a == b
                && xs.len() == ys.len()
                && xs.iter().zip(ys).all(|(x, y)| match_ty(holes, x, y, out))
        }
        (
            Ty::Int {
                signed: a,
                width: x,
            },
            Ty::Int {
                signed: b,
                width: y,
            },
        ) => a == b && match_const(holes, x, y, out),
        (
            Ty::Ptr {
                mutable: a,
                inner: x,
            },
            Ty::Ptr {
                mutable: b,
                inner: y,
            },
        )
        | (
            Ty::Slice {
                mutable: a,
                inner: x,
            },
            Ty::Slice {
                mutable: b,
                inner: y,
            },
        ) => a == b && match_ty(holes, x, y, out),
        (
            Ty::Array {
                len: n,
                mutable: a,
                inner: x,
            },
            Ty::Array {
                len: m,
                mutable: b,
                inner: y,
            },
        ) => a == b && match_const(holes, n, m, out) && match_ty(holes, x, y, out),
        (Ty::Tuple(xs), Ty::Tuple(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| match_ty(holes, x, y, out))
        }
        (
            Ty::Func {
                params: xs,
                ret: rx,
            },
            Ty::Func {
                params: ys,
                ret: ry,
            },
        ) => {
            xs.len() == ys.len()
                && xs.iter().zip(ys).all(|(x, y)| match_ty(holes, x, y, out))
                && match_ty(holes, rx, ry, out)
        }
        (a, b) => a == b,
    }
}

fn match_const(holes: &[DefId], pattern: &Const, k: &Const, out: &mut Subst) -> bool {
    if let Const::Param(d) = pattern
        && holes.contains(d)
    {
        return match out.consts.get(d) {
            Some(prev) => prev == k,
            None => {
                out.consts.insert(*d, k.clone());
                true
            }
        };
    }
    // A width written as `int.<32>` and one written `i32` are the same type
    // (§3.1), and both normalize to a bare width — so comparing the numbers is
    // comparing the types, with no target consulted.
    match (pattern.value(), k.value()) {
        (Some(a), Some(b)) => a == b,
        _ => pattern == k,
    }
}

// ===< Cloning one body into one instantiation >===

/// Renumbers a cloned body and substitutes its types (see
/// [`Mono::instantiate`]).
struct Cloner<'a> {
    meta: &'a Meta,
    subst: &'a Subst,
}

impl Cloner<'_> {
    /// A fresh id carrying `old`'s facts, with every type substituted.
    fn renumber(&self, old: IrId) -> IrId {
        let new = self.meta.fresh();
        if let Some(span) = self.meta.span(old) {
            self.meta.set_span(new, span);
        }
        if let Some(ty) = self.meta.ty(old) {
            self.meta.set_ty(new, subst_ty(self.subst, &ty));
        }
        let directives = self.meta.directives(old);
        if !directives.is_empty() {
            self.meta.set_directives(new, directives);
        }
        if self.meta.has::<ImplicitCast>(old) {
            self.meta.set(new, ImplicitCast);
        }
        if self.meta.has::<RangeReported>(old) {
            self.meta.set(new, RangeReported);
        }
        // A nested call's own generic arguments travel with it, substituted: a
        // `push` inside `Vec.<T>.extend` is instantiated at `T`, and this is
        // where `T` becomes the argument the enclosing instantiation chose.
        if let Some(Instantiation(args)) = self.meta.get::<Instantiation>(old) {
            let args = args
                .iter()
                .map(|a| match a {
                    GenericArg::Ty(t) => GenericArg::Ty(subst_ty(self.subst, t)),
                    GenericArg::Const(k) => GenericArg::Const(subst_const(self.subst, k)),
                })
                .collect();
            self.meta.set(new, Instantiation(args));
        }
        new
    }

    fn binding(&self, b: &mut Binding) {
        b.id = self.renumber(b.id);
    }
}

impl VisitorMut for Cloner<'_> {
    fn visit_function(&mut self, func: &mut Function) {
        func.id = self.renumber(func.id);
        for p in &mut func.params {
            p.id = self.renumber(p.id);
        }
        walk_function_mut(self, func);
    }

    fn visit_block(&mut self, block: &mut Block) {
        block.id = self.renumber(block.id);
        walk_block_mut(self, block);
    }

    fn visit_stmt(&mut self, stmt: &mut super::Stmt) {
        stmt.id = self.renumber(stmt.id);
        walk_stmt_mut(self, stmt);
    }

    fn visit_arm(&mut self, arm: &mut Arm) {
        arm.id = self.renumber(arm.id);
        crate::ir::walk_arm_mut(self, arm);
    }

    fn visit_pattern(&mut self, pattern: &mut Pattern) {
        pattern.id = self.renumber(pattern.id);
        // The two forms that carry a `Binding` of their own: the walk below
        // reaches sub-*patterns*, and a binding is not one.
        match &mut pattern.kind {
            PatternKind::At { binding, .. } => self.binding(binding),
            PatternKind::Slice {
                rest: Some(Some(b)),
                ..
            } => self.binding(b),
            _ => {}
        }
        walk_pattern_mut(self, pattern);
    }

    fn visit_expr(&mut self, expr: &mut Expr) {
        expr.id = self.renumber(expr.id);
        match &mut expr.kind {
            // A `const` generic parameter has no storage: it *is* the value the
            // instantiation chose, and this is the point at which it becomes
            // one. Leaving it as a name would leave the IR referring to a
            // parameter that no longer exists.
            ExprKind::ConstParam(def) => {
                if let Some(lit) = self.subst.consts.get(def).and_then(const_lit) {
                    expr.kind = ExprKind::Lit(lit);
                }
            }
            // The two expression forms that carry a type of their own. Both are
            // written in the declaration's terms and both have to arrive in the
            // instantiation's.
            ExprKind::DynCast { concrete, .. } => {
                *concrete = subst_ty(self.subst, concrete);
            }
            ExprKind::Call {
                dispatch:
                    Dispatch::Generic {
                        self_ty,
                        trait_args,
                        ..
                    },
                ..
            } => {
                *self_ty = subst_ty(self.subst, self_ty);
                for a in trait_args.iter_mut() {
                    *a = subst_ty(self.subst, a);
                }
            }
            _ => {}
        }
        walk_expr_mut(self, expr);
    }
}

/// The literal a `const` argument becomes where its parameter was used as a
/// value.
fn const_lit(k: &Const) -> Option<Lit> {
    match k {
        Const::Width(n) => Some(Lit::Int((*n).into())),
        Const::Value(a) => match &a.value {
            ConstValue::Int(n) => Some(Lit::Int(n.clone())),
            ConstValue::Float(f) => Some(Lit::Float(*f)),
            ConstValue::Bool(b) => Some(Lit::Bool(*b)),
            ConstValue::Char(c) => Some(Lit::Char(*c)),
            ConstValue::Str(s) => Some(Lit::Str(s.clone())),
            ConstValue::Bytes(b) => Some(Lit::Bytes(b.clone())),
            _ => None,
        },
        _ => None,
    }
}

// ===< Mangling (`design/lir.md` §7) >===

/// The symbol `origin` instantiated at `args` will have in the object file.
///
/// The scheme is Itanium-flavoured because the length-prefixed form is easy to
/// demangle and uses only characters every object format accepts. Its one
/// requirement is **injectivity**: two instantiations a linker could confuse must
/// encode differently, and the same instantiation reached twice must encode the
/// same way. Everything else is secondary to that.
///
/// `@link_name("...")` wins outright when it is present — the program named the
/// symbol, and a mangled version of a name someone chose for a C library to find
/// is of no use to anyone. `@no_mangle` is the same statement with the name left
/// out: emit it under the one the program wrote. An `extern` function with no
/// `@link_name` mangles to its bare name, because that is what C expects.
///
/// Both give up injectivity, and deliberately: a name a linker outside this
/// program has to write is a name this compiler does not get to choose. Two
/// declarations claiming one symbol is a collision the linker reports, which is
/// the same place C reports it.
///
/// The arguments are split where [`Generics::own`] says: the enclosing impl's go
/// on the type the impl is for, the function's own on the function, so
/// `core.Vec.<i32>.push` mangles the way `design/lir.md` §7 writes it.
///
/// `impl_self` is the self type of the anonymous impl namespace `origin` is
/// declared in, when it is declared in one: that component is written `M` + the
/// type rather than as its `<impl []T>` label.
fn mangle(
    defs: &DefTable,
    externs: &HashSet<DefId>,
    origin: DefId,
    args: &[GenericArg],
    own: usize,
    qual: Option<&Ty>,
    impl_self: Option<&Ty>,
) -> Symbol {
    let d = defs.get(origin);
    if let Some(link) = d.directives.iter().find(|x| x.is("link_name"))
        && let Some(crate::sema::def::DirectiveArg::Str(name)) = link.args.first()
    {
        return name.clone();
    }
    if d.directives.iter().any(|x| x.is("no_mangle")) {
        return d.name.clone();
    }
    let path = if d.canonical.is_empty() {
        vec![d.name.clone()]
    } else {
        d.canonical.clone()
    };
    if args.is_empty() && externs.contains(&origin) {
        return d.name.clone();
    }

    let own = own.min(args.len());
    let (own_args, inherited) = args.split_at(own);
    let mut s = String::from("_NC");
    // Every component but the last is the path to the member; the last is the
    // member itself, and the trait qualifier goes between them.
    let (head, member) = path.split_at(path.len() - 1);
    for (i, seg) in head.iter().enumerate() {
        // `M` + the self type for a structural impl's namespace. It stands where
        // a component stands, and a component starts with a digit, so the letter
        // is enough to tell them apart; a type's encoding is prefix-free, so
        // nothing is needed to end it.
        match impl_self {
            Some(t) if i + 1 == head.len() => {
                s.push('M');
                push_ty(&mut s, defs, t);
            }
            _ => push_len(&mut s, seg.as_str()),
        }
        // The impl's arguments belong to the type the impl is for, which is the
        // component before the member.
        if i + 1 == head.len() && !inherited.is_empty() {
            push_args(&mut s, defs, inherited);
        }
    }
    // `X` + the trait: what this member implements, for the impls that a path
    // alone cannot tell apart (see [`Mono::trait_qualifier`]). It is a letter, so
    // the length-prefixed path before it ends unambiguously.
    if let Some(t) = qual.filter(|_| !head.is_empty()) {
        s.push('X');
        push_ty(&mut s, defs, t);
    }
    push_len(&mut s, member[0].as_str());
    // Nowhere to hang the impl's arguments (a one-component path — a free
    // function reached through a blanket impl): they still have to be in the
    // symbol, so they join the function's own.
    let mut trailing = own_args.to_vec();
    if head.is_empty() {
        trailing.splice(0..0, inherited.iter().cloned());
    }
    if !trailing.is_empty() {
        push_args(&mut s, defs, &trailing);
    }
    Symbol::new(&s)
}

/// The symbol a **global** is emitted under.
///
/// A `#static` is not instantiated, so it has no [`Instance`] to carry a name —
/// but it still needs one the linker can see, so it mangles the way a function
/// does: the same length-prefixed path, under its own tag. `@link_name` and
/// `@no_mangle` win outright, for the reason they win on a function.
///
/// It is **not** injective on its own, and cannot be: a `#static` written inside
/// a function body has no canonical path — its name is whatever the source wrote
/// in that body, and two functions may each write `n`. Uniqueness is therefore
/// the caller's, and `lir::lower` is where it happens, because that is the one
/// place every global in the program passes through.
pub fn global_symbol(defs: &DefTable, def: DefId) -> Symbol {
    let d = defs.get(def);
    if let Some(link) = d.directives.iter().find(|x| x.is("link_name"))
        && let Some(crate::sema::def::DirectiveArg::Str(name)) = link.args.first()
    {
        return name.clone();
    }
    if d.directives.iter().any(|x| x.is("no_mangle")) {
        return d.name.clone();
    }
    let path = if d.canonical.is_empty() {
        vec![d.name.clone()]
    } else {
        d.canonical.clone()
    };
    let mut s = String::from("_NG");
    for seg in &path {
        push_len(&mut s, seg.as_str());
    }
    Symbol::new(&s)
}

/// The mangled encoding of one type, on its own.
///
/// Exposed because it is the right **cache key** for anything keyed by a type:
/// its one job is injectivity, so two types share it precisely when they are the
/// same type. [`Ty`] itself cannot be one — a `const` argument may hold a float,
/// so there is no `Hash` and no `Eq` — and a display string is not injective
/// either, because two types in different namespaces can print alike.
pub fn type_key(defs: &DefTable, ty: &Ty) -> String {
    let mut s = String::new();
    push_ty(&mut s, defs, ty);
    s
}

/// One length-prefixed path component.
///
/// A component that is not an identifier — an `<impl []T>` label reached some
/// way [`mangle`] could not replace with its type — is written `L` + the
/// length-prefixed text with every byte outside `[A-Za-z0-9]` escaped as `_` and
/// two hex digits. The escape is injective and uses only characters every
/// object format accepts, and the `L` keeps `a_5f` written as an identifier
/// apart from `a_` escaped.
fn push_len(s: &mut String, text: &str) {
    let plain = text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80);
    if plain {
        s.push_str(&text.len().to_string());
        s.push_str(text);
        return;
    }
    let mut escaped = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() {
            escaped.push(b as char);
        } else {
            escaped.push_str(&format!("_{b:02x}"));
        }
    }
    s.push('L');
    s.push_str(&escaped.len().to_string());
    s.push_str(&escaped);
}

fn push_args(s: &mut String, defs: &DefTable, args: &[GenericArg]) {
    s.push('I');
    for a in args {
        match a {
            GenericArg::Ty(t) => push_ty(s, defs, t),
            GenericArg::Const(k) => push_const(s, defs, k),
        }
    }
    s.push('E');
}

/// Encode one type argument.
///
/// A primitive mangles **as a primitive** even though it is sugar: `i32` is
/// `int.<32>` in the type system, and encoding it that way would make every
/// symbol in every program longer to record something no two types disagree
/// about. The sugar is the canonical spelling here.
fn push_ty(s: &mut String, defs: &DefTable, ty: &Ty) {
    match ty {
        Ty::Int { signed, width } => match width.bits() {
            Some(n) => {
                s.push(if *signed { 'i' } else { 'u' });
                s.push_str(&n.to_string());
            }
            // A width still symbolic at this point is a `const` parameter of an
            // impl's self type (see [`mangle`]) or a defect. Either way it keeps
            // its signedness, so `impl <const N> T for int.<N>` and its `uint`
            // twin stay apart.
            None => {
                s.push(if *signed { 'i' } else { 'u' });
                push_const(s, defs, width);
            }
        },
        Ty::Float(w) => {
            s.push('f');
            s.push_str(match w {
                FloatWidth::F16 => "16",
                FloatWidth::F32 => "32",
                FloatWidth::F64 => "64",
                FloatWidth::F80 => "80",
                FloatWidth::F128 => "128",
            });
        }
        Ty::Bool => s.push('b'),
        Ty::Char => s.push('c'),
        Ty::Void => s.push('v'),
        Ty::Never => s.push('N'),
        // `opaque` mangles even though no value of it exists: it is the pointee
        // of a `*opaque`, and `*opaque` and `*u8` are different types that must
        // not share a symbol.
        Ty::Opaque => s.push('O'),
        Ty::Ptr { mutable, inner } => {
            s.push('P');
            if *mutable {
                s.push('m');
            }
            push_ty(s, defs, inner);
        }
        Ty::Slice { mutable, inner } => {
            s.push('S');
            if *mutable {
                s.push('m');
            }
            push_ty(s, defs, inner);
        }
        Ty::Array { len, inner, .. } => {
            s.push('A');
            s.push_str(&len.value().map(|n| n.to_string()).unwrap_or_default());
            push_ty(s, defs, inner);
        }
        Ty::Tuple(elems) => {
            s.push('T');
            for e in elems {
                push_ty(s, defs, e);
            }
            s.push('E');
        }
        // An anonymous struct: `X`, then each field as its length-prefixed name
        // followed by its type, then `E`. The names are part of the encoding
        // because they are part of the type — `struct { a: i32 }` and
        // `struct { b: i32 }` are two types and must not mangle alike — and the
        // fields arrive sorted, so one type has one encoding.
        Ty::Struct(fields) => {
            s.push('X');
            for (name, t) in fields {
                push_len(s, name.as_str());
                push_ty(s, defs, t);
            }
            s.push('E');
        }
        Ty::Func { params, ret } => {
            s.push('F');
            for p in params {
                push_ty(s, defs, p);
            }
            s.push('E');
            push_ty(s, defs, ret);
        }
        Ty::Dyn(d) => {
            s.push('D');
            for seg in &defs.get(*d).canonical {
                push_len(s, seg.as_str());
            }
            s.push('E');
        }
        // The pointer-sized integers have an encoding of their own, `is` / `us`,
        // rather than the path of the `core` declaration they are (§3.1). They
        // stand exactly where a primitive stands, they are in every other
        // program, and `4core5isizeIE` in every symbol that touches one would be
        // noise. Keyed on the `#lang` tag, never the name, so `core` may still
        // spell them however it likes.
        Ty::Nominal { def, args }
            if args.is_empty()
                && defs
                    .get(*def)
                    .lang
                    .as_ref()
                    .is_some_and(|l| matches!(l.as_str(), "usize" | "isize")) =>
        {
            s.push_str(
                if defs.get(*def).lang.as_ref().unwrap().as_str() == "isize" {
                    "is"
                } else {
                    "us"
                },
            );
        }
        // `N` + the length-prefixed path + the arguments in `I ... E`.
        //
        // Two details here are repairs to `design/lir.md` §7's table, both in
        // service of the one thing a mangled name has to be — injective — and
        // both recorded there:
        //
        // - The **`N`** is what keeps a path from being read as the tail of the
        //   encoding before it. An integer is a letter followed by digits, and a
        //   path component starts with digits, so `i324core3Foo` would be either
        //   `i32` then `core.Foo` or `i324` then something. With `N`, every type
        //   encoding starts with a letter and the ambiguity cannot arise.
        // - The arguments are written `I ... E` **even when there are none**,
        //   because the list is what tells a reader where the path stops.
        //   Without it `N4core6OptionN4core6Option` could be one four-component
        //   path or two two-component ones.
        // A type parameter is only ever reached through an impl's self type (see
        // [`mangle`]); everything instantiated is concrete by then. `G` + its
        // name keeps it from reading as the declaration's canonical path, which
        // passes through the impl's namespace again.
        Ty::Nominal { def, args }
            if args.is_empty() && defs.get(*def).kind == DefKind::TypeParam =>
        {
            s.push('G');
            push_len(s, defs.get(*def).name.as_str());
        }
        Ty::Nominal { def, args } => {
            s.push('N');
            let d = defs.get(*def);
            let path = if d.canonical.is_empty() {
                vec![d.name.clone()]
            } else {
                d.canonical.clone()
            };
            for seg in &path {
                push_len(s, seg.as_str());
            }
            s.push('I');
            for a in args {
                push_ty(s, defs, a);
            }
            s.push('E');
        }
        Ty::ComptimeInt => s.push_str("Ci"),
        Ty::ComptimeFloat => s.push_str("Cf"),
        Ty::ComptimeStr => s.push_str("Cs"),
        Ty::Var(_) | Ty::Error => s.push('Z'),
    }
}

/// Encode one `const` argument.
///
/// It carries **its type**. A `const` parameter may be any primitive (§5), so
/// `K3` would be ambiguous between `3usize` and `3u8`, and those are different
/// instantiations.
///
/// A numeric value is prefixed `p` or `n` for its sign. The `n` is
/// `design/lir.md` §7's, and for its reason: `-` is not safe in every object
/// format. The `p` is the repair that makes the encoding injective — a width is
/// digits and a value is digits, so `Ku167` would be `u16` at `7` or `u167` at
/// nothing. One letter between them settles it, and the sign has to be written
/// anyway.
fn push_const(s: &mut String, defs: &DefTable, k: &Const) {
    s.push('K');
    match k {
        // A width is a bare `u16` and carries no type of its own (§3.1); it is
        // spelled as one here because that is the type a program writes it at.
        Const::Width(n) => {
            s.push_str("u16p");
            s.push_str(&n.to_string());
        }
        Const::Value(a) => {
            let ConstArg { ty, value } = &**a;
            push_ty(s, defs, ty);
            match value {
                ConstValue::Int(n) => {
                    if n.sign() == num_bigint::Sign::Minus {
                        s.push('n');
                        s.push_str(&(-n).to_string());
                    } else {
                        s.push('p');
                        s.push_str(&n.to_string());
                    }
                }
                ConstValue::Bool(b) => s.push(if *b { '1' } else { '0' }),
                ConstValue::Char(c) => {
                    s.push('p');
                    s.push_str(&(*c as u32).to_string());
                }
                ConstValue::Float(f) => {
                    s.push('p');
                    s.push_str(&f.to_bits().to_string());
                }
                // An aggregate is not an admissible `const` argument (§5), so
                // reaching here is a defect. `Z` keeps the symbol injective
                // among the values that do arrive rather than inventing one.
                _ => s.push('Z'),
            }
        }
        Const::Param(d) => {
            s.push('G');
            push_len(s, defs.get(*d).name.as_str());
        }
        _ => s.push('Z'),
    }
}
