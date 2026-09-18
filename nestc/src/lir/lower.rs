//! IR → LIR: structured control flow becomes a graph.
//!
//! The IR keeps the source's shape — `if`, `match`, a `loop` with `break`,
//! blocks that are expressions — because that is what the checks running on it
//! read. This is where that shape is spent. What comes out is `design/lir.md`
//! §1: locals declared up front, basic blocks connected by explicit jumps, one
//! terminator each.
//!
//! # The three things that make the walk simple
//!
//! - **A call is an instruction.** A panic does not unwind (§2), so a call has
//!   one successor and a block is cut only by a *branch*, never by a call. That
//!   is the single largest simplification here: an expression full of calls
//!   still lowers to a straight run of statements.
//! - **Every expression produces an operand.** [`Lowerer::eval`] hands back a
//!   value for every form; the four that branch — `if`, `match`, `loop`, a
//!   block — write their result into a slot and hand back a read of it. Control
//!   flow is therefore confined to four constructs.
//! - **Monomorphization ran first.** Every type is concrete, every call has a
//!   callee, every function has a symbol. Nothing here asks what a generic
//!   parameter stands for, and nothing here *constructs* a symbol — it reads the
//!   one monomorphization decided (§7), because a second implementation of a
//!   mangling is free to disagree with the first.
//!
//! # The cleanup ladder (§3)
//!
//! A scope's `defer` bodies run on **every** path that leaves it, in reverse of
//! registration, and outer scopes run after inner ones. The lowering is a ladder
//! of ordinary blocks: one rung per scope, chained outward, and every exit jumps
//! in at the depth it is leaving from.
//!
//! The rungs are built **once per kind of exit** — falling off the end, a
//! `return`, a `break`, a `continue` — and not once per exit *site*, which is
//! the requirement §3 states: three `return`s inside one scope share the rung
//! that runs its defers. They cannot share across kinds, because what follows
//! the rung differs: a `return` continues outward to the function's exit and a
//! `break` continues outward only to the loop's.
//!
//! They are also built once per **registration count**, because a `defer` runs
//! only on the exits below it (spec §8.4): a `return` written above a `defer`
//! and one written below it are leaving scopes with different contents, and
//! sharing a rung between them would run a body control never registered.
//!
//! That is also why a `return` writes its value into a **slot** rather than
//! returning directly (§3's `ret`): the real `return` happens after the ladder,
//! and a rung shared by three of them cannot tell which value it is carrying. A
//! `break` with a value does the same thing for the same reason.
//!
//! # What it decides that nothing earlier could
//!
//! **The overflow setting** (§7d). `overflow=trap` is not a flag on an
//! instruction: it is a checked operation, an extra edge, and a block that does
//! not come back. Every pass after this one — drops, safepoints, liveness — has
//! to see that edge to be correct, so it must be in the graph rather than appear
//! underneath it at codegen.

use std::collections::HashMap;

use crate::common::options::{Options, OverflowMode};
use crate::common::source::{FileSpan, SourceMap};
use crate::common::symbol::Symbol;
use crate::ir::layout::{Layout, Layouts};
use crate::ir::{
    self, ConstValue, Dispatch, Expr, ExprKind, IrId, Linked, Meta, Pattern, PatternKind, StmtKind,
    TypeDefKind,
};
use crate::parser::ast::{BinOp, Lit, UnOp};
use crate::sema::builtins::BuiltinOp;
use crate::sema::def::{DefId, DefTable, Directive, DirectiveArg, LangItems};
use crate::sema::ty::Ty;

use super::{
    Aggregate, Base, Block, BlockId, Callee, CastKind, Constant, FuncId, Function, FunctionAttrs,
    Global,
    GlobalId, Inline, Intrinsic, Local, LocalId, Op, Operand, Origin, Place, Program, Projection,
    Rvalue, Stmt, StmtKind as LirStmtKind, TermKind, Terminator, Ty as LirTy, TypeDef, TypeId,
    TypeMember, VariantDef,
};

/// How long a `value ; count` array may be before it is filled by a loop
/// instead of written out element by element.
///
/// Both forms are correct; this is where one stops being cheaper than the
/// other. Below it the elements are operands the constant evaluator can fold
/// and the backend can emit as one constant. Above it they are `n` operands
/// carried through every later stage for no gain, which is a cost the *compiler*
/// pays rather than the program — and it grows with `n` without bound.
const REPEAT_UNROLL: u64 = 32;

/// Lower the whole monomorphized program.
#[allow(clippy::too_many_arguments)]
pub fn lower(
    defs: &DefTable,
    meta: &Meta,
    linked: &Linked,
    layouts: &Layouts,
    options: &Options,
    lang: &LangItems,
    sources: &SourceMap,
) -> Program {
    lower_against_libraries(defs, meta, linked, layouts, options, lang, sources, &[], &|_| false)
}

/// [`lower`], for a program some of whose definitions came from libraries.
///
/// `foreign` says which: a function a library defines was compiled there, so
/// here it is **declared** and not lowered, and a library's `#static` is
/// imported rather than defined. What monomorphization instantiated here is this
/// compilation's own, whoever wrote the generic — and the library may have
/// instantiated the same one at the same arguments, so an instantiation is
/// marked [`FunctionAttrs::shared`] and the linker keeps one of the copies.
#[allow(clippy::too_many_arguments)]
pub fn lower_against_libraries(
    defs: &DefTable,
    meta: &Meta,
    linked: &Linked,
    layouts: &Layouts,
    options: &Options,
    lang: &LangItems,
    sources: &SourceMap,
    tests: &[(String, DefId)],
    foreign: &dyn Fn(DefId) -> bool,
) -> Program {
    let mut cx = Cx {
        defs,
        meta,
        linked,
        layouts,
        options,
        lang,
        sources,
        drops: super::escape::analyze(linked.funcs()),
        types: Vec::new(),
        type_index: HashMap::new(),
        globals: Vec::new(),
        global_of: HashMap::new(),
        data_index: HashMap::new(),
        vtable_index: HashMap::new(),
        vtable_types: HashMap::new(),
        funcs: Vec::new(),
        func_of: HashMap::new(),
    };

    // Every function gets its slot **before** any body is lowered, because a
    // call names its callee by index and a program is full of calls that run
    // ahead of the definition they name.
    let irfuncs: Vec<&ir::Function> = linked.funcs().collect();
    for f in &irfuncs {
        cx.reserve_func(f);
    }

    // A `#static` region is program-lifetime storage, so it belongs to the
    // program rather than to any function. A `::` constant is **not** here: it
    // *is* its value (§2.5), and every use of one carries that value — unless
    // the value is a blob, which is storage again and comes back as a global of
    // its own (see [`Cx::data_global`]).
    let mut taken: std::collections::HashSet<Symbol> = std::collections::HashSet::new();
    for g in linked.globals() {
        if !g.mutable {
            continue;
        }
        let ty = meta.ty_or_error(g.id);
        let lty = cx.lir(&ty);
        let imported = foreign(g.def);
        let init = if imported {
            None
        } else {
            meta.get::<ConstValue>(g.id).map(|v| cx.const_data(&v, &ty))
        };
        let id = GlobalId(cx.globals.len() as u32);
        cx.globals.push(Global {
            name: defs.canonical_string(g.def),
            symbol: unique(&mut taken, crate::ir::mono::global_symbol(defs, g.def)),
            ty: lty,
            init,
            mutable: true,
            // A `#static` is one region for the whole program, wherever it is
            // read from, so the linker has to see the name (§11).
            linkage: if imported {
                super::Linkage::Imported
            } else {
                super::Linkage::External
            },
            span: meta.span(g.id),
        });
        cx.global_of.insert(g.def, id);
    }

    for f in irfuncs {
        // Compiled where it was defined; the declaration reserved above is all
        // a call from here needs.
        if foreign(f.def) {
            continue;
        }
        let func = Lowerer::new(&mut cx, f).run();
        let id = cx.func_of[&f.def];
        cx.funcs[id.0 as usize] = func;
    }

    // Which function the entry point calls. The rule for what counts as one
    // lives in `Linked::mains`, because `ir::check::declarations` asks the same
    // question and two copies of it could answer differently. More than one is
    // that pass's diagnostic; here the first is taken, so a program that is
    // already an error still lowers.
    let main = linked
        .mains(defs)
        .next()
        .and_then(|f| cx.func_of.get(&f.def).copied());
    // The program's own start sequence, if a library claimed the tag. A program
    // built without one — without `std` — has nowhere to hand its arguments and
    // the entry calls `main` directly (`super::entry`).
    let start = lang
        .get("start")
        .map(|d| defs.resolve_alias(d))
        .and_then(|d| cx.func_of.get(&d).copied());

    // Safepoints after everything, and they have to be: what is live at a call
    // is a property of the finished graph, and which types hold references is a
    // question for the table that was only just built (§6).
    let mut unit = super::Unit {
        name: String::new(),
        types: cx.types,
        globals: cx.globals,
        funcs: cx.funcs,
    };
    // The entry point, if this build is producing a program (§5.6). It is added
    // before the safepoints and before the split, because it is an ordinary
    // function from here on: its calls are safepoints like any others, and the
    // unit it lands in is decided by the same rule as every other function's.
    // A test binary runs the package's tests instead of its `main` — the same
    // entry point, handed a different function to call. With no `#lang`
    // test runner there is nothing to run them with, and the build falls back to
    // the ordinary entry: `ir::check` is where a missing one is reported, not
    // here.
    let runner = lang
        .get("test_runner")
        .map(|d| defs.resolve_alias(d))
        .and_then(|d| cx.func_of.get(&d).copied());
    let failed = lang
        .get("test_failed")
        .map(|d| defs.resolve_alias(d))
        .and_then(|d| cx.func_of.get(&d).copied());
    if options.entry == crate::common::options::EntryMode::Auto {
        match (options.test, runner) {
            (true, Some(runner)) => {
                let cases: Vec<(String, super::FuncId)> = tests
                    .iter()
                    .filter_map(|(name, def)| {
                        cx.func_of.get(def).map(|&f| (name.clone(), f))
                    })
                    .collect();
                super::entry::synthesize_tests(
                    &mut unit,
                    &cases,
                    runner,
                    failed,
                    start,
                    options.target,
                );
            }
            _ => {
                if let Some(id) = main {
                    super::entry::synthesize(&mut unit, id, start, options.target);
                }
            }
        }
    }
    super::safepoint::annotate(&mut unit);
    // Which functions can reach themselves, and so need a stack check in their
    // prologue. After the entry point is in, because it is a function like any
    // other and its calls are edges like any others.
    super::recursion::mark(&mut unit);
    super::unit::split(unit, options.codegen_units, sources)
}

/// State shared by every function's lowering: the tables that answer questions
/// about the *program* rather than about one body.
struct Cx<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    linked: &'a Linked,
    layouts: &'a Layouts<'a>,
    options: &'a Options,
    /// The `#lang` registry. LIR reaches into it for exactly one thing: the
    /// `panic` a trapped overflow or an out-of-bounds index calls (§2). Finding
    /// it by tag is what keeps the compiler's own failures and the program's on
    /// the same code path — and what lets a program replace the handler.
    lang: &'a LangItems,
    /// The source text, for turning a span into the `file`/`line`/`column` a
    /// compiler-raised panic reports. A source-written `panic(...)` gets the
    /// same three numbers from `#caller_location` (§5.2); this is that, for a
    /// call site the source did not write.
    sources: &'a SourceMap,
    /// Which allocations each IR block may free on the way out (§5), from
    /// [`super::escape`].
    drops: super::escape::Drops,
    /// The flattened type table being built, indexed by [`TypeId`].
    types: Vec<TypeDef>,
    /// Where each type is, by [`crate::ir::mono::type_key`].
    type_index: HashMap<String, TypeId>,
    globals: Vec<Global>,
    /// Where each `#static` is.
    global_of: HashMap<DefId, GlobalId>,
    /// Where the storage for a constant blob is, by its contents (§2.5). Two
    /// occurrences of `"hello"` are one global: the bytes are immutable, so
    /// sharing them is invisible to the program.
    data_index: HashMap<String, GlobalId>,
    /// The vtable global for one `(trait, concrete type)` pair (§7b).
    vtable_index: HashMap<(DefId, String), GlobalId>,
    /// The struct type one trait's vtable has — one per trait, whatever the
    /// impl, which is what makes a dispatch an ordinary member read.
    vtable_types: HashMap<DefId, TypeId>,
    funcs: Vec<Function>,
    func_of: HashMap<DefId, FuncId>,
}

impl Cx<'_> {
    fn ty_of(&self, id: IrId) -> Ty {
        self.strip(&self.meta.ty_or_error(id))
    }

    /// `ty` with every `distinct` replaced by what it is distinct *from*, and
    /// every mutability dropped.
    ///
    /// A `distinct T` has exactly `T`'s representation (§2.4) — the difference
    /// between the two is a rule about which values may be assigned to which
    /// names, and that rule was enforced before this pass ran. Keeping it here
    /// would mean emitting `usize` as a one-member struct wrapping a `u64`,
    /// which is not what it is: a struct of one scalar is passed differently
    /// from the scalar under every C ABI there is, so a backend would have to
    /// know to unwrap it and LIR would have made a distinction that costs
    /// something and means nothing.
    ///
    /// The peeling goes through pointers, slices, arrays and tuples, because
    /// `*usize` is `*u64` for the same reason. It does **not** go into a
    /// nominal type's generic arguments: `Vec.<usize>` is the instantiation
    /// monomorphization made and named, and renaming it here would name a
    /// function that does not exist.
    ///
    /// **Mutability goes the same way, and for the same reason.** No target has
    /// two kinds of address: LLVM, C and wasm each have one, and the rule that
    /// needed the distinction — who may write through this pointer — was
    /// enforced in sema, long before here (§9). Keeping it would put `*T` and
    /// `*mut T` in the type table as two entries describing one machine type,
    /// and would make a backend read a field it has no use for. The **symbol**
    /// is the exception: `mono::type_key` mangles mutability and monomorphization
    /// already decided every name, so nothing here recomputes one from a
    /// stripped type.
    fn strip(&self, ty: &Ty) -> Ty {
        self.strip_at(ty, 0)
    }

    fn strip_at(&self, ty: &Ty, depth: u32) -> Ty {
        // A `distinct` over a `distinct` is legal; a cycle among them is not,
        // and `check::declarations` has already said so.
        if depth > 64 {
            return ty.clone();
        }
        match ty {
            Ty::Nominal { def, .. } => {
                let Some(t) = self.linked.ty(*def) else {
                    return ty.clone();
                };
                if !matches!(t.kind, TypeDefKind::Distinct { .. }) {
                    return ty.clone();
                }
                match self.layouts.member_types(ty).as_deref() {
                    Some([(_, repr)]) => self.strip_at(repr, depth + 1),
                    _ => ty.clone(),
                }
            }
            Ty::Ptr { inner, .. } => Ty::Ptr {
                mutable: false,
                inner: Box::new(self.strip_at(inner, depth + 1)),
            },
            Ty::Slice { inner, .. } => Ty::Slice {
                mutable: false,
                inner: Box::new(self.strip_at(inner, depth + 1)),
            },
            Ty::Array { len, inner, .. } => Ty::Array {
                len: len.clone(),
                mutable: false,
                inner: Box::new(self.strip_at(inner, depth + 1)),
            },
            Ty::Tuple(elems) => Ty::Tuple(
                elems
                    .iter()
                    .map(|t| self.strip_at(t, depth + 1))
                    .collect(),
            ),
            _ => ty.clone(),
        }
    }

    fn key(&self, ty: &Ty) -> String {
        crate::ir::mono::type_key(self.defs, ty)
    }

    /// The two names monomorphization decided for a function (§7).
    ///
    /// A function with no [`Instance`](crate::ir::mono::Instance) is one
    /// monomorphization never reached — a trait method that only states a
    /// signature — and its own name is the honest stand-in.
    fn names(&self, id: IrId, fallback: &Symbol) -> (String, Symbol) {
        match self.meta.get::<crate::ir::mono::Instance>(id) {
            Some(i) => (i.name, i.symbol),
            None => (fallback.to_string(), fallback.clone()),
        }
    }

    /// The value of an index expression, when there is one.
    ///
    /// The same evaluator `check::bounds` asked, so the two cannot disagree
    /// about whether an index is known — and a fresh one per question, because
    /// the answer is a property of the expression rather than of the walk.
    ///
    /// `None` is the overwhelmingly common answer and means nothing is wrong: a
    /// loop counter has no compile-time value, which is exactly when a run-time
    /// check is what the language promised (§3.2).
    fn const_index(&self, e: &Expr) -> Option<u64> {
        self.const_value(e)?.as_u64()
    }

    /// The value of `e`, when the const evaluator can produce one.
    ///
    /// A fresh evaluator per question, for the reason [`Self::const_index`]
    /// gives: the answer is a property of the expression, not of the walk.
    fn const_value(&self, e: &Expr) -> Option<ConstValue> {
        let mut cx = crate::ir::const_eval::ConstEval::new(
            self.defs,
            self.meta,
            self.linked,
            self.layouts,
        );
        cx.eval(e).ok()
    }

    /// `usize`, as wide as this target's pointer.
    fn usize_ty(&self) -> Ty {
        Ty::int((self.layouts.pointer_size() * 8) as u16, false)
    }

    /// The bytes between one `ty` and the next in an array of them — its
    /// layout's size, which already includes tail padding.
    ///
    /// `0` where the layout is in error, which is a program that has already
    /// been reported and will not be emitted.
    fn stride(&self, ty: &Ty) -> u64 {
        self.layouts.of(ty).map(|l| l.size).unwrap_or(0)
    }

    // ===< Functions >===

    /// Give a function its slot, before any body is lowered.
    /// What `f`'s directives and visibility decide, and whether it is an
    /// instantiation — the one fact here monomorphization decided rather than
    /// the source.
    fn func_attrs(&self, f: &ir::Function) -> FunctionAttrs {
        FunctionAttrs {
            shared: self
                .meta
                .with::<crate::ir::mono::Instance, _>(f.id, |i| !i.args.is_empty())
                .unwrap_or(false),
            ..attrs_of(&self.meta.directives(f.id), self.defs.get(f.def).vis.is_public())
        }
    }

    fn reserve_func(&mut self, f: &ir::Function) {
        if self.func_of.contains_key(&f.def) {
            return;
        }
        let (name, symbol) = self.names(f.id, &f.name);
        let id = FuncId(self.funcs.len() as u32);
        let ret = match self.meta.ty(f.id) {
            Some(Ty::Func { ret, .. }) => self.lir(&ret),
            _ => LirTy::Void,
        };
        let mut params: Vec<Local> = Vec::with_capacity(f.params.len());
        for p in &f.params {
            let ty = self.strip(&self.meta.ty_or_error(p.id));
            if is_void(&ty) {
                continue;
            }
            let lty = self.lir(&ty);
            params.push(Local {
                id: LocalId(params.len() as u32),
                name: Some(p.name.clone()),
                ty: lty,
                span: self.meta.span(p.id),
            });
        }
        let n = params.len();
        self.funcs.push(Function {
            name,
            symbol,
            locals: params,
            params: n,
            ret,
            blocks: Vec::new(),
            extern_abi: f.extern_abi.clone(),
            span: self.meta.span(f.id),
            attrs: self.func_attrs(f),
        });
        self.func_of.insert(f.def, id);
    }

    /// The function `def` names, declared if the program never lowered a body
    /// for it — a trait method that only states a signature is still something
    /// a call can name.
    fn func_id(&mut self, def: DefId) -> FuncId {
        if let Some(id) = self.func_of.get(&def) {
            return *id;
        }
        let id = FuncId(self.funcs.len() as u32);
        let d = self.defs.get(def);
        self.funcs.push(Function {
            name: self.defs.canonical_string(def),
            symbol: d.name.clone(),
            locals: Vec::new(),
            params: 0,
            ret: LirTy::Void,
            blocks: Vec::new(),
            extern_abi: None,
            span: None,
            attrs: FunctionAttrs::default(),
        });
        self.func_of.insert(def, id);
        id
    }

    // ===< Types (§7b) >===

    /// A front-end type, as LIR holds it.
    ///
    /// This is where the type system stops being about what a program may say
    /// and starts being about what a machine holds: `distinct` is peeled (§9),
    /// mutability is dropped (no target has two kinds of address), a `char` is
    /// the 32-bit number it is, and every aggregate becomes an index into the
    /// type table.
    fn lir(&mut self, ty: &Ty) -> LirTy {
        self.lir_at(&self.strip(ty), 0)
    }

    fn lir_at(&mut self, ty: &Ty, depth: u32) -> LirTy {
        if depth > 64 {
            return LirTy::Void;
        }
        match ty {
            Ty::Int { signed, width } => LirTy::Int {
                bits: width.bits().unwrap_or(64) as u16,
                signed: *signed,
            },
            Ty::Float(w) => LirTy::Float {
                bits: float_bits(*w),
            },
            Ty::Bool => LirTy::Bool,
            // A `char` is a Unicode scalar value, which is a number in
            // `0..=0x10FFFF` — `u32` on every target, and a case a backend would
            // have had to map to one anyway.
            Ty::Char => LirTy::Int {
                bits: 32,
                signed: false,
            },
            Ty::Void => LirTy::Void,
            Ty::Never => LirTy::Never,
            // A pointer to a trait object is the **fat** pointer, which is a
            // struct and not a pointer at all (§7b). `dyn Trait` on its own is
            // unsized and is only ever reached through one (§3.4).
            Ty::Ptr { inner, .. } if matches!(**inner, Ty::Dyn(_)) => {
                LirTy::Named(self.intern(ty, depth))
            }
            Ty::Ptr { inner, .. } => LirTy::ptr(self.lir_at(&self.strip(inner), depth + 1)),
            Ty::Array { len, inner, .. } => {
                let elem = self.lir_at(&self.strip(inner), depth + 1);
                LirTy::Array {
                    len: len.value().unwrap_or(0),
                    elem: Box::new(elem),
                }
            }
            // A function *pointer*'s type is the signature the call will have,
            // which is the erased one: a `void` parameter is not passed (§9), so
            // it is not in the type either.
            Ty::Func { params, ret } => {
                let stripped: Vec<Ty> = params
                    .iter()
                    .map(|p| self.strip(p))
                    .filter(|p| !is_void(p))
                    .collect();
                let mut params = Vec::with_capacity(stripped.len());
                for p in &stripped {
                    params.push(self.lir_at(p, depth + 1));
                }
                let ret = self.lir_at(&self.strip(ret), depth + 1);
                LirTy::Func {
                    params,
                    ret: Box::new(ret),
                }
            }
            Ty::Slice { .. } | Ty::Tuple(_) | Ty::Struct(_) => LirTy::Named(self.intern(ty, depth)),
            Ty::Nominal { def, .. } => match self.linked.ty(*def).map(|t| &t.kind) {
                Some(TypeDefKind::Struct { .. } | TypeDefKind::Enum { .. }) => {
                    LirTy::Named(self.intern(ty, depth))
                }
                // A name with no shape behind it: a type parameter that reached
                // this far (a trait method's `Self`), or a definition analysis
                // rejected. An opaque pointer is the honest stand-in — it is
                // what the value is reached through in every case that gets
                // here.
                _ => LirTy::ptr(LirTy::Void),
            },
            // Unsized on its own, and never the type of a slot.
            Ty::Dyn(_) => LirTy::ptr(LirTy::Void),
            // `opaque` has no size, so there is nothing for LIR to describe and
            // nothing it would ever be asked to load. It reaches here only as
            // the pointee of a `*opaque`, which becomes `ptr(void)` — the same
            // stand-in a name with no shape behind it already gets, so LIR
            // learns no new case for a type no slot can hold.
            Ty::Opaque => LirTy::Void,
            // A literal's type survives only until inference is done with it
            // (§6.5); one here is a fold that did not happen, and the widest
            // machine type is the honest answer.
            Ty::ComptimeInt => LirTy::Int {
                bits: 64,
                signed: true,
            },
            Ty::ComptimeFloat => LirTy::Float { bits: 64 },
            Ty::ComptimeStr => LirTy::ptr(LirTy::Int {
                bits: 8,
                signed: false,
            }),
            // A program that did not type-check is not emitted.
            Ty::Var(_) | Ty::Error => LirTy::Void,
        }
    }

    /// Record `ty`'s flattened definition and hand back its index.
    ///
    /// The id is reserved **before** the members are computed, because a struct
    /// may contain a pointer to itself and the recursion has to find the entry
    /// already there.
    fn intern(&mut self, ty: &Ty, depth: u32) -> TypeId {
        let key = self.key(ty);
        if let Some(id) = self.type_index.get(&key) {
            return *id;
        }
        let id = TypeId(self.types.len() as u32);
        self.type_index.insert(key.clone(), id);
        self.types.push(TypeDef {
            id,
            key: key.clone(),
            name: ty.display(self.defs),
            members: Vec::new(),
            layout: Layout::ZERO,
            origin: Origin::Struct,
        });
        let def = self.flatten(ty, id, depth);
        self.types[id.0 as usize] = def;
        id
    }

    /// `ty` as a struct: its members, its layout and what it was.
    fn flatten(&mut self, ty: &Ty, id: TypeId, depth: u32) -> TypeDef {
        let key = self.key(ty);
        let name = ty.display(self.defs);
        let layout = self.layouts.of(ty).unwrap_or(Layout::ZERO);
        let mut def = TypeDef {
            id,
            key,
            name,
            members: Vec::new(),
            layout,
            origin: Origin::Struct,
        };
        match ty {
            // A trait object's fat pointer: the data, and the vtable. The
            // vtable's type is the **trait's**, not the impl's — a `dyn` has
            // erased the concrete type, and one struct type per trait is what
            // makes a dispatch an ordinary member read at a known offset.
            Ty::Ptr { inner, .. } => {
                let Ty::Dyn(trait_def) = &**inner else {
                    return def;
                };
                let w = self.layouts.pointer_size();
                let vt = self.vtable_type(*trait_def);
                def.members = vec![
                    TypeMember {
                        name: Symbol::new("data"),
                        ty: LirTy::ptr(LirTy::Void),
                        offset: 0,
                    },
                    TypeMember {
                        name: Symbol::new("vtable"),
                        ty: LirTy::ptr(LirTy::Named(vt)),
                        offset: w,
                    },
                ];
                def.layout = Layout {
                    size: w * 2,
                    align: w,
                };
                def.origin = Origin::Dyn {
                    trait_name: self.defs.canonical_string(*trait_def),
                };
            }
            Ty::Tuple(elems) => {
                let offsets = self
                    .layouts
                    .fields(ty)
                    .and_then(|f| f.ok())
                    .map(|f| f.offsets)
                    .unwrap_or_default();
                def.members = elems
                    .iter()
                    .enumerate()
                    .map(|(i, t)| TypeMember {
                        name: Symbol::new(&i.to_string()),
                        ty: self.lir_at(&self.strip(t), depth + 1),
                        offset: offsets.get(i).copied().unwrap_or(0),
                    })
                    .collect();
                def.origin = Origin::Tuple;
            }
            // An anonymous struct flattens to exactly what it is: its fields,
            // under their own names, at the offsets the layout gave them. It
            // stays `Origin::Struct` — there is nothing about it a backend has
            // to treat differently from a named one.
            Ty::Struct(fields) => {
                let offsets = self
                    .layouts
                    .fields(ty)
                    .and_then(|f| f.ok())
                    .map(|f| f.offsets)
                    .unwrap_or_default();
                def.members = fields
                    .iter()
                    .enumerate()
                    .map(|(i, (name, t))| TypeMember {
                        name: name.clone(),
                        ty: self.lir_at(&self.strip(t), depth + 1),
                        offset: offsets.get(i).copied().unwrap_or(0),
                    })
                    .collect();
            }
            // A slice **does** flatten, and it is the interesting near-miss: it
            // is a pointer and a length, and neither is indexed by a run-time
            // value. The indexing happens through the pointer it holds (§7b).
            Ty::Slice { inner, .. } => {
                let w = self.layouts.pointer_size();
                let elem = self.lir_at(&self.strip(inner), depth + 1);
                def.members = vec![
                    TypeMember {
                        name: Symbol::new("ptr"),
                        ty: LirTy::ptr(elem),
                        offset: 0,
                    },
                    TypeMember {
                        name: Symbol::new("len"),
                        ty: LirTy::Int {
                            bits: (w * 8) as u16,
                            signed: false,
                        },
                        offset: w,
                    },
                ];
                def.origin = Origin::Slice;
            }
            Ty::Nominal { def: tdef, .. } => {
                let kind = self.linked.ty(*tdef).map(|t| t.kind.clone());
                match kind {
                    Some(TypeDefKind::Struct { .. }) => {
                        let offsets = self
                            .layouts
                            .fields(ty)
                            .and_then(|f| f.ok())
                            .map(|f| f.offsets)
                            .unwrap_or_default();
                        let members = self.layouts.member_types(ty).unwrap_or_default();
                        def.members = members
                            .into_iter()
                            .enumerate()
                            .map(|(i, (n, t))| TypeMember {
                                name: n,
                                ty: self.lir_at(&self.strip(&t), depth + 1),
                                offset: offsets.get(i).copied().unwrap_or(0),
                            })
                            .collect();
                        def.origin = Origin::Struct;
                    }
                    // An enum becomes `{ tag, payload }`, and each variant
                    // becomes a struct type of its own saying how to read those
                    // payload bytes (§7b). The shared payload is aligned for
                    // every variant, so the reinterpretation is always legal —
                    // and the member offsets under it then come from the table
                    // like every other type's.
                    Some(TypeDefKind::Enum { variants }) => {
                        let Some(Ok(e)) = self.layouts.enum_layout(ty) else {
                            return def;
                        };
                        def.members = vec![
                            TypeMember {
                                name: Symbol::new("tag"),
                                ty: LirTy::Int {
                                    bits: (e.tag.size * 8) as u16,
                                    signed: false,
                                },
                                offset: 0,
                            },
                            TypeMember {
                                name: Symbol::new("payload"),
                                ty: LirTy::Array {
                                    len: e.payload.size,
                                    elem: Box::new(LirTy::Int {
                                        bits: 8,
                                        signed: false,
                                    }),
                                },
                                offset: e.payload_at,
                            },
                        ];
                        let vs = variants
                            .iter()
                            .enumerate()
                            .map(|(i, v)| {
                                let vty = self.variant_type(ty, i, &v.name, depth);
                                VariantDef {
                                    name: v.name.clone(),
                                    tag: i as i128,
                                    ty: vty,
                                    tuple: v.tuple,
                                }
                            })
                            .collect();
                        def.origin = Origin::Enum { variants: vs };
                    }
                    // A `distinct` never reaches here: `Cx::strip` replaced it
                    // with what it is distinct from before any type was
                    // recorded (§9). A trait is not a type with a shape.
                    _ => {}
                }
            }
            _ => {}
        }
        def
    }

    /// One enum variant's payload, as a struct type of its own.
    ///
    /// Its members sit at offsets **relative to the payload**, which is exactly
    /// what [`Projection::Cast`] reinterprets: the enum's `payload` member is
    /// the address, and this type says what is at it.
    fn variant_type(&mut self, enum_ty: &Ty, index: usize, name: &Symbol, depth: u32) -> TypeId {
        let key = format!("{}$v{index}", self.key(enum_ty));
        if let Some(id) = self.type_index.get(&key) {
            return *id;
        }
        let id = TypeId(self.types.len() as u32);
        self.type_index.insert(key.clone(), id);
        let parent = enum_ty.display(self.defs);
        self.types.push(TypeDef {
            id,
            key: key.clone(),
            name: format!("{parent}.{name}"),
            members: Vec::new(),
            layout: Layout::ZERO,
            origin: Origin::Variant {
                parent: parent.clone(),
            },
        });
        let e = match self.layouts.enum_layout(enum_ty) {
            Some(Ok(e)) => e,
            _ => return id,
        };
        let offsets = e
            .variants
            .get(index)
            .map(|v| v.offsets.clone())
            .unwrap_or_default();
        let members: Vec<TypeMember> = self
            .layouts
            .variant_member_types(enum_ty, index)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(j, (n, t))| TypeMember {
                name: n,
                ty: self.lir_at(&self.strip(&t), depth + 1),
                offset: offsets.get(j).copied().unwrap_or(0),
            })
            .collect();
        let layout = e
            .variants
            .get(index)
            .map(|v| v.layout)
            .unwrap_or(Layout::ZERO);
        self.types[id.0 as usize].members = members;
        self.types[id.0 as usize].layout = layout;
        id
    }

    // ===< Vtables (§7b) >===

    /// The struct type one trait's vtable has: one member per method, in the
    /// trait's declaration order, each a pointer to code.
    ///
    /// One type per **trait** rather than per impl, because that is what a `dyn`
    /// knows: the concrete type is erased, so every impl's table has to be a
    /// value of one type for the slot offsets to be knowable at the call.
    fn vtable_type(&mut self, trait_def: DefId) -> TypeId {
        if let Some(id) = self.vtable_types.get(&trait_def) {
            return *id;
        }
        let key = format!("$vt{}", trait_def.0);
        let id = TypeId(self.types.len() as u32);
        self.type_index.insert(key.clone(), id);
        self.vtable_types.insert(trait_def, id);
        let trait_name = self.defs.canonical_string(trait_def);
        self.types.push(TypeDef {
            id,
            key,
            name: format!("vtable.{trait_name}"),
            members: Vec::new(),
            layout: Layout::ZERO,
            origin: Origin::Vtable {
                trait_name: trait_name.clone(),
            },
        });
        let methods = match self.linked.ty(trait_def).map(|t| t.kind.clone()) {
            Some(TypeDefKind::Trait { methods, .. }) => methods,
            _ => Vec::new(),
        };
        let w = self.layouts.pointer_size();
        let members: Vec<TypeMember> = methods
            .iter()
            .enumerate()
            .map(|(i, m)| TypeMember {
                name: m.name.clone(),
                ty: self.slot_ty(m.id),
                offset: i as u64 * w,
            })
            .collect();
        let layout = Layout {
            size: members.len() as u64 * w,
            align: w,
        };
        self.types[id.0 as usize].members = members;
        self.types[id.0 as usize].layout = layout;
        id
    }

    /// The type of one vtable slot: the method's signature with the receiver
    /// erased, which is what a `dyn` call actually has in hand.
    fn slot_ty(&mut self, method: IrId) -> LirTy {
        let Some(Ty::Func { params, ret }) = self.meta.ty(method) else {
            return LirTy::ptr(LirTy::Void);
        };
        let ret = self.lir(&ret);
        let mut ps: Vec<LirTy> = Vec::with_capacity(params.len());
        for (i, p) in params.iter().enumerate() {
            // `self` is a `*dyn Trait` at the call site and the concrete type is
            // gone; every implementation takes the data pointer.
            if i == 0 {
                ps.push(LirTy::ptr(LirTy::Void));
                continue;
            }
            let p = self.strip(p);
            if is_void(&p) {
                continue;
            }
            ps.push(self.lir(&p));
        }
        LirTy::Func {
            params: ps,
            ret: Box::new(ret),
        }
    }

    /// The vtable for one `(trait, concrete type)` pair, built the first time a
    /// coercion asks for it — an ordinary immutable global of function
    /// addresses.
    ///
    /// Which function fills each slot is **not** decided here: monomorphization
    /// decided it, because the instantiated method that fills a slot does not
    /// exist until that pass makes it, and re-selecting the impl would be a
    /// second implementation of a selection free to disagree with the first.
    /// What this does is turn that answer into data.
    fn vtable(&mut self, slots: &crate::ir::mono::VtableSlots) -> GlobalId {
        let key = (slots.trait_def, self.key(&slots.concrete));
        if let Some(id) = self.vtable_index.get(&key) {
            return *id;
        }
        let ty = self.vtable_type(slots.trait_def);
        let filled: Vec<Constant> = slots
            .slots
            .iter()
            .map(|slot| match slot {
                // A slot object safety should have made impossible to leave
                // empty. `undef` is how that shows up as a defect rather than as
                // a call to the wrong function.
                Some(def) => Constant::Func(self.func_id(*def)),
                None => Constant::Undef,
            })
            .collect();
        let symbol = Symbol::new(&format!(
            "_NV{}{}",
            self.key(&Ty::Nominal {
                def: slots.trait_def,
                args: Vec::new()
            }),
            self.key(&slots.concrete)
        ));
        let id = GlobalId(self.globals.len() as u32);
        self.globals.push(Global {
            name: format!(
                "vtable.{}.for.{}",
                self.defs.canonical_string(slots.trait_def),
                slots.concrete.display(self.defs)
            ),
            symbol,
            ty: LirTy::Named(ty),
            init: Some(Constant::Aggregate(filled)),
            mutable: false,
            // Nothing can name a vtable, so each unit that builds a trait object
            // carries its own copy rather than depending on a neighbour's data.
            linkage: super::Linkage::Internal,
            span: None,
        });
        self.vtable_index.insert(key, id);
        id
    }

    // ===< Constants that need storage (§2.5) >===

    /// Where a `#static` lives. Every one of them was recorded before any body
    /// was lowered; a name that is not there is a program analysis rejected, and
    /// an empty region keeps the reference resolvable.
    fn global_id(&mut self, def: DefId) -> GlobalId {
        if let Some(id) = self.global_of.get(&def) {
            return *id;
        }
        let id = GlobalId(self.globals.len() as u32);
        self.globals.push(Global {
            name: self.defs.canonical_string(def),
            symbol: crate::ir::mono::global_symbol(self.defs, def),
            ty: LirTy::Void,
            init: None,
            mutable: true,
            linkage: super::Linkage::External,
            span: None,
        });
        self.global_of.insert(def, id);
        id
    }

    /// The stand-in type for an aggregate that is not one: a struct with no
    /// members, so that every [`TypeId`] in a unit still resolves.
    fn error_type(&mut self) -> TypeId {
        let key = "$error".to_string();
        if let Some(id) = self.type_index.get(&key) {
            return *id;
        }
        let id = TypeId(self.types.len() as u32);
        self.type_index.insert(key.clone(), id);
        self.types.push(TypeDef {
            id,
            key,
            name: "<error>".to_string(),
            members: Vec::new(),
            layout: Layout::ZERO,
            origin: Origin::Struct,
        });
        id
    }

    /// A global holding `init`, shared by every use of the same contents.
    fn data_global(&mut self, name: &str, ty: LirTy, init: Constant, key: String) -> GlobalId {
        if let Some(id) = self.data_index.get(&key) {
            return *id;
        }
        let n = self.globals.len();
        let id = GlobalId(n as u32);
        self.globals.push(Global {
            name: format!("const.{name}.{n}"),
            symbol: Symbol::new(&format!("_NK{n}")),
            ty,
            init: Some(init),
            mutable: false,
            linkage: super::Linkage::Internal,
            span: None,
        });
        self.data_index.insert(key, id);
        id
    }

    /// A constant, as a global's initializer: the whole value, blobs included.
    ///
    /// This is the one place a composite constant is written out rather than
    /// referred to, because a global's contents are exactly what a backend has
    /// to emit into a section.
    fn const_data(&mut self, v: &ConstValue, ty: &Ty) -> Constant {
        let ty = self.strip(ty);
        match v {
            ConstValue::Int(n) => Constant::Int(n.clone()),
            ConstValue::Float(f) => Constant::Float(*f),
            ConstValue::Bool(b) => Constant::Bool(*b),
            ConstValue::Char(c) => Constant::Int((*c as u32).into()),
            ConstValue::Void => Constant::Undef,
            // Text is bytes with an address. As the contents of a `[N]u8` the
            // bytes *are* the value; as a `str` or a `[]u8` the value is a view
            // of them, which is a pointer and a length.
            ConstValue::Str(s) => self.text_data(s.as_bytes(), &ty),
            ConstValue::Bytes(b) => self.text_data(b, &ty),
            // A slice is a view, like text: the elements are an array of their
            // own, and the value is its address and length.
            ConstValue::Aggregate(items) if let Ty::Slice { inner, .. } = &ty => {
                let (g, len) = self.slice_storage(v, items, inner);
                Constant::Aggregate(vec![Constant::Global(g), Constant::Int((len as i128).into())])
            }
            ConstValue::Aggregate(items) => {
                let tys = self.member_tys(&ty);
                let parts = items
                    .iter()
                    .enumerate()
                    .map(|(i, x)| {
                        let t = tys.get(i).cloned().unwrap_or(Ty::Error);
                        self.const_data(x, &t)
                    })
                    .collect();
                Constant::Aggregate(parts)
            }
            ConstValue::Variant { name, payload } => {
                let (tag, tys) = self.variant_info(&ty, name);
                let parts = payload
                    .iter()
                    .enumerate()
                    .map(|(i, x)| {
                        let t = tys.get(i).cloned().unwrap_or(Ty::Error);
                        self.const_data(x, &t)
                    })
                    .collect();
                Constant::Variant {
                    tag,
                    name: name.clone(),
                    payload: parts,
                }
            }
        }
    }

    /// The global holding a slice constant's elements, `[N]inner`, and `N`.
    fn slice_storage(&mut self, v: &ConstValue, items: &[ConstValue], inner: &Ty) -> (GlobalId, u64) {
        let elems = items.iter().map(|x| self.const_data(x, inner)).collect();
        let lty = LirTy::Array { len: items.len() as u64, elem: Box::new(self.lir(inner)) };
        let key = format!("[{}]{}:{}", items.len(), self.key(inner), v.display());
        (self.data_global("data", lty, Constant::Aggregate(elems), key), items.len() as u64)
    }

    /// A run of bytes at the type it is being used as.
    fn text_data(&mut self, bytes: &[u8], ty: &Ty) -> Constant {
        match ty {
            // The bytes themselves.
            Ty::Array { .. } => Constant::Bytes(bytes.to_vec()),
            // A view of them: the address of the storage, and the length.
            _ => {
                let g = self.bytes_global(bytes);
                Constant::Aggregate(vec![
                    Constant::Global(g),
                    Constant::Int((bytes.len() as i128).into()),
                ])
            }
        }
    }

    /// The global holding these bytes.
    fn bytes_global(&mut self, bytes: &[u8]) -> GlobalId {
        let ty = LirTy::Array {
            len: bytes.len() as u64,
            elem: Box::new(LirTy::Int {
                bits: 8,
                signed: false,
            }),
        };
        let key = format!("bytes:{}", crate::parser::ast::bytes_repr(bytes));
        self.data_global("str", ty, Constant::Bytes(bytes.to_vec()), key)
    }

    /// The types of an aggregate's members, in declaration order.
    fn member_tys(&self, ty: &Ty) -> Vec<Ty> {
        match ty {
            Ty::Ptr { inner, .. } => self.member_tys(inner),
            Ty::Tuple(elems) => elems.clone(),
            Ty::Array { inner, len, .. } => {
                let n = len.value().unwrap_or(0) as usize;
                vec![(**inner).clone(); n]
            }
            Ty::Slice { mutable, inner } => vec![
                Ty::Ptr {
                    mutable: *mutable,
                    inner: inner.clone(),
                },
                self.usize_ty(),
            ],
            _ => self
                .layouts
                .member_types(ty)
                .map(|ms| ms.into_iter().map(|(_, t)| t).collect())
                .unwrap_or_default(),
        }
    }

    /// A variant's tag and its payload element types.
    // ===< Reflection (§9) >===

    /// The nominal type `core` tagged `#lang(tag)`.
    fn lang_nominal(&self, tag: &str) -> Option<Ty> {
        let def = self.defs.resolve_alias(self.lang.get(tag)?);
        Some(Ty::Nominal {
            def,
            args: Vec::new(),
        })
    }

    /// The identity of `ty`, as `core`'s `TypeId`.
    ///
    /// A hash of [`crate::ir::mono::type_key`] — the same whole-program string
    /// every symbol in the binary is mangled from, which is what makes this
    /// stable across units: two of them agree because they agree on the
    /// mangling. The key keeps `distinct` and mutability where [`Cx::strip`]
    /// erases both, so `usize` and `u64` have different identities, which is
    /// the answer a checked read wants.
    fn type_id_const(&self, ty: &Ty) -> Constant {
        let key = crate::ir::mono::type_key(self.defs, ty);
        // One `u128`, which is what `core`'s `TypeId` is: the language has
        // arbitrary integer widths, so there is no half to split this into.
        Constant::Int(num_bigint::BigInt::from(fnv1a_128(key.as_bytes())))
    }

    /// Which variant of `core`'s `Kind` enum `ty` is, with the description of
    /// the type it is made of where it is made of one.
    ///
    /// A `distinct` is asked about before `strip` erases it: it is the one kind
    /// whose answer is about the declaration rather than the representation.
    fn kind_const(&mut self, ty: &Ty) -> Constant {
        let Some(kind_ty) = self.lang_nominal("reflect_kind") else {
            return Constant::Undef;
        };
        let repr = match ty {
            Ty::Nominal { def, .. }
                if matches!(
                    self.linked.ty(*def).map(|t| &t.kind),
                    Some(TypeDefKind::Distinct { .. })
                ) =>
            {
                self.layouts
                    .member_types(ty)
                    .and_then(|ms| ms.into_iter().next())
                    .map(|(_, t)| t)
            }
            _ => None,
        };
        let (name, payload) = match (repr, ty) {
            (Some(inner), _) => ("Distinct", self.info_ptr(&inner).into_iter().collect()),
            (None, Ty::Ptr { inner, .. }) => ("Ptr", self.info_ptr(inner).into_iter().collect()),
            (None, Ty::Slice { inner, .. }) => {
                ("Slice", self.info_ptr(inner).into_iter().collect())
            }
            (None, Ty::Array { len, inner, .. }) => {
                let n = len.value().unwrap_or(0) as i128;
                let mut parts = vec![Constant::Int(n.into())];
                parts.extend(self.info_ptr(inner));
                ("Array", parts)
            }
            (None, _) => (kind_name(self, ty), Vec::new()),
        };
        let name = Symbol::new(name);
        let (tag, _) = self.variant_info(&kind_ty, &name);
        Constant::Variant { tag, name, payload }
    }

    /// The address of `ty`'s description, as a `*TypeInfo` constant.
    fn info_ptr(&mut self, ty: &Ty) -> Option<Constant> {
        self.type_info_global(ty).map(Constant::Global)
    }

    /// The global holding the description of `ty`, built once per type.
    ///
    /// It is read-only data and the call to `type_info.<T>()` is a copy of it:
    /// `T` is concrete after monomorphization, so there is nothing left to
    /// compute at run time.
    fn type_info_global(&mut self, ty: &Ty) -> Option<GlobalId> {
        let key = crate::ir::mono::type_key(self.defs, ty);
        let info_ty = self.lang_nominal("reflect_info")?;
        let member_ty = self.lang_nominal("reflect_member")?;
        // A description can reach itself — `Node { next: *Node }` — so the
        // global is claimed before its contents are built, and a second request
        // for the same type on the way down is handed the address.
        let info_key = format!("reflect.info:{key}");
        if let Some(id) = self.data_index.get(&info_key) {
            return Some(*id);
        }
        let info_lty = self.lir(&info_ty);
        let slot = self.data_global("type_info", info_lty, Constant::Undef, info_key);
        let text = Ty::Slice {
            mutable: false,
            inner: Box::new(Ty::u8()),
        };

        // One `Member` per member, in declaration order — the order every
        // `Projection::Field` index is already counted in.
        let members = match ty {
            // A tuple's positions are members named `0`, `1`, … (§3.3), and
            // `fields` below already lays them out.
            Ty::Tuple(elems) => elems
                .iter()
                .enumerate()
                .map(|(i, e)| (Symbol::new(&i.to_string()), e.clone()))
                .collect(),
            // A `distinct` has a member in its layout and none in its
            // description: it *is* its representation, not a wrapper round it.
            Ty::Nominal { def, .. }
                if matches!(
                    self.linked.ty(*def).map(|t| &t.kind),
                    Some(TypeDefKind::Struct { .. })
                ) =>
            {
                self.layouts.member_types(ty).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        let offsets = match self.layouts.fields(ty) {
            Some(Ok(f)) => f.offsets,
            _ => Vec::new(),
        };
        // A member's own def, so the attributes a program wrote on it can be
        // found: `member_types` gives names and types, and the def table is
        // what carries everything else about a declaration.
        let defs_of: Vec<Option<DefId>> = match ty {
            Ty::Nominal { def, .. } => match self.linked.ty(*def).map(|t| &t.kind) {
                Some(TypeDefKind::Struct { members }) => members.iter().map(|m| m.def).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let mut parts: Vec<Constant> = Vec::new();
        for (i, (name, mty)) in members.iter().enumerate() {
            let size = self.layouts.of(mty).map(|l| l.size).unwrap_or(0);
            let name_const = self.text_data(name.as_str().as_bytes(), &text);
            let attrs = self.attrs_const(defs_of.get(i).copied().flatten(), &key, i);
            parts.push(Constant::Aggregate(vec![
                name_const,
                Constant::Int((*offsets.get(i).unwrap_or(&0) as i128).into()),
                Constant::Int((size as i128).into()),
                self.kind_const(mty),
                self.type_id_const(mty),
                attrs,
                Constant::Int((i as i128).into()),
            ]));
        }
        let count = parts.len() as u64;
        let member_lty = self.lir(&member_ty);
        let array = LirTy::Array {
            len: count,
            elem: Box::new(member_lty),
        };
        let table = self.data_global(
            "members",
            array,
            Constant::Aggregate(parts),
            format!("reflect.members:{key}"),
        );

        let layout = self.layouts.of(ty).unwrap_or(crate::ir::layout::Layout::ZERO);
        let name_const = self.text_data(ty.display(self.defs).as_bytes(), &text);
        let own = match ty {
            Ty::Nominal { def, .. } => Some(*def),
            _ => None,
        };
        let own_attrs = self.attrs_const(own, &key, usize::MAX);
        let info = Constant::Aggregate(vec![
            name_const,
            Constant::Int((layout.size as i128).into()),
            Constant::Int((layout.align as i128).into()),
            self.kind_const(ty),
            self.type_id_const(ty),
            // A slice is `{ ptr, len }` (§7b), and the pointer is the table.
            Constant::Aggregate(vec![
                Constant::Global(table),
                Constant::Int((count as i128).into()),
            ]),
            own_attrs,
        ]);
        self.globals[slot.0 as usize].init = Some(info);
        Some(slot)
    }

    /// The `[]Attr` slice for the attributes written on `def`, as a constant.
    ///
    /// Each value goes to a global of its own — read-only data, like every
    /// other constant here — and the descriptor holds its address beside the
    /// identity that says how to read it. `where` distinguishes one member's
    /// table from another's in the global's key.
    fn attrs_const(&mut self, def: Option<DefId>, key: &str, where_: usize) -> Constant {
        // A slice with no elements still needs a *pointer*: a backend builds a
        // global's initializer with no builder to hand, so an integer where an
        // address belongs has nowhere to be converted. The empty table is a
        // `[0]Attr` global, exactly as a zero-length string is a `[0]u8` one.
        let Some(attr_ty) = self.lang_nominal("reflect_attr") else {
            return Constant::Aggregate(vec![
                Constant::Undef,
                Constant::Int(0.into()),
            ]);
        };
        let written = match def {
            Some(d) => self.defs.get(d).attrs.clone(),
            None => Vec::new(),
        };
        let text = Ty::Slice {
            mutable: false,
            inner: Box::new(Ty::u8()),
        };
        let mut parts: Vec<Constant> = Vec::new();
        for a in &written {
            let ty = Ty::Nominal {
                def: a.def,
                args: Vec::new(),
            };
            // The arguments, put in the struct's *declaration* order — which is
            // the order the layout engine reports members in, and the one thing
            // name resolution could not know.
            let names: Vec<Symbol> = self
                .layouts
                .member_types(&ty)
                .unwrap_or_default()
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            let mut ordered: Vec<ConstValue> = Vec::new();
            for (i, n) in names.iter().enumerate() {
                let found = a
                    .args
                    .iter()
                    .find(|(an, _)| an.as_ref() == Some(n))
                    .or_else(|| a.args.get(i).filter(|(an, _)| an.is_none()));
                match found {
                    Some((_, v)) => ordered.push(v.clone()),
                    // Resolution checked the count and the names, so this is
                    // a program that did not type-check; drop the attribute
                    // rather than emit a half-written value.
                    None => continue,
                }
            }
            let value = self.const_data(&ConstValue::Aggregate(ordered), &ty);
            let lty = self.lir(&ty);
            let g = self.data_global(
                "attr",
                lty,
                value,
                format!("reflect.attr:{key}:{where_}:{}", parts.len()),
            );
            let name = self.defs.get(a.def).name.clone();
            let name_const = self.text_data(name.as_str().as_bytes(), &text);
            parts.push(Constant::Aggregate(vec![
                name_const,
                self.type_id_const(&ty),
                Constant::Global(g),
            ]));
        }
        let n = parts.len() as u64;
        let attr_lty = self.lir(&attr_ty);
        let array = LirTy::Array {
            len: n,
            elem: Box::new(attr_lty),
        };
        let table = self.data_global(
            "attrs",
            array,
            Constant::Aggregate(parts),
            format!("reflect.attrs:{key}:{where_}"),
        );
        Constant::Aggregate(vec![
            Constant::Global(table),
            Constant::Int((n as i128).into()),
        ])
    }

    fn variant_info(&self, ty: &Ty, name: &Symbol) -> (i128, Vec<Ty>) {
        let Ty::Nominal { def, .. } = ty else {
            return (0, Vec::new());
        };
        let Some(t) = self.linked.ty(*def) else {
            return (0, Vec::new());
        };
        let TypeDefKind::Enum { variants } = &t.kind else {
            return (0, Vec::new());
        };
        let Some(i) = variants.iter().position(|v| &v.name == name) else {
            return (0, Vec::new());
        };
        let tys = self
            .layouts
            .variant_member_types(ty, i)
            .map(|ms| ms.into_iter().map(|(_, t)| t).collect())
            .unwrap_or_default();
        (i as i128, tys)
    }
}

/// What a function's directives *mean*, decided once (§7).
fn attrs_of(directives: &[Directive], public: bool) -> FunctionAttrs {
    let mut attrs = FunctionAttrs {
        public,
        ..FunctionAttrs::default()
    };
    for d in directives {
        match d.name.as_str() {
            "section" => {
                if let Some(DirectiveArg::Str(s)) = d.args.first() {
                    attrs.section = Some(s.clone());
                }
            }
            "inline" => {
                attrs.inline = match d.args.first() {
                    Some(DirectiveArg::Name(n)) if n.as_str() == "never" => Inline::Never,
                    _ => Inline::Always,
                };
            }
            "offset" => {
                if let Some(DirectiveArg::Int(n)) = d.args.first() {
                    attrs.offset = Some(*n);
                }
            }
            "unsafe" => attrs.unchecked = true,
            "c_vararg" => attrs.c_variadic = true,
            _ => {}
        }
    }
    attrs
}


// ===< One function >===

/// Which kind of exit a cleanup rung continues to. See the module docs for why
/// a rung is shared across *sites* but not across *kinds*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Exit {
    /// Out of the function.
    Return,
    /// Out of the `n`th enclosing `loop`.
    Break(usize),
    /// Back to the top of the `n`th enclosing `loop`.
    Continue(usize),
}

/// One lexical scope: the `defer` bodies registered in it **so far**, and the
/// ladder rungs already built for it.
struct Scope {
    id: u32,
    /// Grows as the lowering walks past each `defer` statement, so an exit
    /// reads the ones control has actually reached — and only those.
    defers: Vec<Expr>,
    /// The allocations escape analysis proved do not outlive this scope (§5).
    /// They are freed on every exit, after the `defer` bodies — a `defer` may
    /// still read the object, and the memory has to survive until it has.
    drops: Vec<DefId>,
}

/// Where `break` and `continue` go, and where `break value` writes.
struct LoopCtx {
    break_to: BlockId,
    continue_to: BlockId,
    result: Option<LocalId>,
    /// How many scopes were open when the loop was entered — the rung a `break`
    /// stops unwinding at.
    floor: usize,
}

/// A block being built. Its terminator is filled in when control leaves it.
struct PartialBlock {
    stmts: Vec<Stmt>,
    term: Option<Terminator>,
    label: Option<String>,
}

/// One function's lowering.
struct Lowerer<'a, 'c> {
    cx: &'a mut Cx<'c>,
    /// The IR function being lowered, borrowed out of the whole-program view so
    /// that `cx` stays usable beside it.
    f: &'c ir::Function,
    locals: Vec<Local>,
    /// The front-end type of each local, beside the machine type the slot
    /// carries — see [`Lowerer::new_local`].
    local_tys: Vec<Ty>,
    /// Where each bound name lives.
    local_of: HashMap<DefId, LocalId>,
    blocks: Vec<PartialBlock>,
    /// The block statements are currently appended to.
    at: BlockId,
    loops: Vec<LoopCtx>,
    scopes: Vec<Scope>,
    next_scope: u32,
    /// Ladder rungs already built, by the scope they unwind, the kind of exit
    /// they continue to, and how many of that scope's `defer`s were registered
    /// when control reached the exit. This is what makes a rung once-per-kind
    /// rather than once-per-site (§3), and what keeps an exit above a `defer`
    /// from sharing a rung with one below it.
    rungs: HashMap<(u32, Exit, usize), BlockId>,
    /// The slot a `return` writes, and the block that finally returns it.
    ret_slot: Option<LocalId>,
    ret_block: Option<BlockId>,
    /// The declared result type.
    ret: Ty,
    /// Whether the run-time safety checks are off here — `#unsafe` (§9).
    ///
    /// A function-level one covers the whole body. A block-level one is a
    /// *scope*, so it is saved and restored around the block rather than set
    /// once.
    unguarded: bool,
}

impl<'a, 'c> Lowerer<'a, 'c> {
    fn new(cx: &'a mut Cx<'c>, f: &'c ir::Function) -> Self {
        let ret = match cx.meta.ty(f.id) {
            Some(Ty::Func { ret, .. }) => cx.strip(&ret),
            _ => Ty::Void,
        };
        let unguarded = cx.meta.has_directive(f.id, "unsafe");
        Lowerer {
            cx,
            f,
            locals: Vec::new(),
            local_tys: Vec::new(),
            local_of: HashMap::new(),
            blocks: Vec::new(),
            at: BlockId(0),
            loops: Vec::new(),
            scopes: Vec::new(),
            next_scope: 0,
            rungs: HashMap::new(),
            ret_slot: None,
            ret_block: None,
            ret,
            unguarded,
        }
    }

    fn run(mut self) -> Function {
        // Parameters occupy the first slots, in declaration order: matching a
        // call's arguments to a callee's frame should be a position, not a
        // search.
        for p in &self.f.params {
            let ty = self.cx.ty_of(p.id);
            // A `void` parameter is no parameter: it holds nothing, so there is
            // nothing to pass and nothing to hold it in. Both sides erase it by
            // the same rule — see [`Lowerer::passed`] — so a caller and a callee
            // cannot disagree about the signature.
            if is_void(&ty) {
                continue;
            }
            let span = self.cx.meta.span(p.id);
            let id = self.new_local(Some(p.name.clone()), &ty, span);
            self.local_of.insert(p.def, id);
        }
        let params = self.locals.len();

        let mut blocks = Vec::new();
        if let Some(body) = &self.f.body {
            let entry = self.new_block(Some("entry".to_string()));
            self.at = entry;
            // The body's tail value is the function's result, so a body that
            // falls off the end returns it. One that ended in a `return` has
            // already terminated its block and this does nothing.
            let span = self.cx.meta.span(body.id);
            let value = self.block_value(body);
            self.finish_return(value, span);
            blocks = self.finish_blocks();
        }

        let (name, symbol) = self.cx.names(self.f.id, &self.f.name);
        let ret = self.cx.lir(&self.ret.clone());
        Function {
            name,
            symbol,
            locals: self.locals,
            params,
            ret,
            blocks,
            extern_abi: self.f.extern_abi.clone(),
            span: self.cx.meta.span(self.f.id),
            attrs: self.cx.func_attrs(self.f),
        }
    }

    /// End the fall-off-the-end path.
    ///
    /// With no ladder anywhere this is a plain `return`. With one, the value
    /// goes into the slot and the path joins the ladder like every other exit —
    /// which is the whole point of the slot: the real `return` happens *after*
    /// the deferred bodies run.
    fn finish_return(&mut self, value: Option<Operand>, span: Option<FileSpan>) {
        if self.ended() {
            return;
        }
        match self.ret_block {
            Some(b) => {
                if let (Some(v), Some(slot)) = (value, self.ret_slot) {
                    self.assign(Place::local(slot), Rvalue::Use(v), span);
                }
                self.goto(b, span);
            }
            None => {
                let value = self.returned(value);
                self.terminate(Terminator::new(TermKind::Return(value), span))
            }
        }
    }

    /// What a `return` actually carries.
    ///
    /// A `void` function returns **nothing**, not an `undef` of a type no
    /// machine has. That is the same erasure §9 applies to every slot, every
    /// parameter and every argument, and the terminator was the one place it had
    /// not reached: `return undef` out of a `-> void` function reads harmlessly
    /// in a dump and is a type error the moment a backend looks at it.
    fn returned(&self, value: Option<Operand>) -> Option<Operand> {
        if is_void(&self.ret) { None } else { value }
    }

    // ===< Blocks and locals >===

    /// A slot, and the front-end type it holds.
    ///
    /// The slot keeps the **LIR** type — what a machine holds — and the walk
    /// keeps the front-end one beside it, because the questions still ahead of
    /// it (which member is `len`, what a variant's payload contains) are asked
    /// of a type the layout engine understands.
    fn new_local(&mut self, name: Option<Symbol>, ty: &Ty, span: Option<FileSpan>) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        let lty = self.cx.lir(ty);
        self.locals.push(Local {
            id,
            name,
            ty: lty,
            span,
        });
        self.local_tys.push(ty.clone());
        id
    }

    /// A slot no source name produced. It has none, which is honest: a debugger
    /// shows it as a slot rather than as an invented identifier (§7c).
    fn temp(&mut self, ty: Ty, span: Option<FileSpan>) -> LocalId {
        self.new_local(None, &ty, span)
    }

    /// A slot whose type is already the machine's — the few places where there
    /// is no front-end type to convert, because the value did not come from one
    /// (a vtable slot's function pointer).
    fn temp_lir(&mut self, ty: LirTy, span: Option<FileSpan>) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(Local {
            id,
            name: None,
            ty,
            span,
        });
        self.local_tys.push(Ty::Error);
        id
    }

    fn new_block(&mut self, label: Option<String>) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(PartialBlock {
            stmts: Vec::new(),
            term: None,
            label,
        });
        id
    }

    fn push(&mut self, kind: LirStmtKind, span: Option<FileSpan>) {
        let at = self.at.0 as usize;
        // The block already ended: everything after a `return` in the same
        // straight run is unreachable, and dropping it here is what keeps a
        // block a block rather than a list with a terminator in the middle.
        if self.blocks[at].term.is_some() {
            return;
        }
        self.blocks[at].stmts.push(Stmt::new(kind, span));
    }

    fn terminate(&mut self, term: Terminator) {
        let at = self.at.0 as usize;
        if self.blocks[at].term.is_none() {
            self.blocks[at].term = Some(term);
        }
    }

    fn goto(&mut self, target: BlockId, span: Option<FileSpan>) {
        self.terminate(Terminator::new(TermKind::Goto(target), span));
    }

    /// Whether the current block has already ended — how the walk knows a
    /// `return` / `break` / `continue` happened underneath it.
    fn ended(&self) -> bool {
        self.blocks[self.at.0 as usize].term.is_some()
    }

    /// Seal every block. One still without a terminator is one nothing can
    /// leave, which after this walk means nothing reaches it either.
    fn finish_blocks(&mut self) -> Vec<Block> {
        std::mem::take(&mut self.blocks)
            .into_iter()
            .enumerate()
            .map(|(i, b)| Block {
                id: BlockId(i as u32),
                stmts: b.stmts,
                term: b.term.unwrap_or(Terminator::new(TermKind::Unreachable, None)),
                label: b.label,
            })
            .collect()
    }

    fn assign(&mut self, place: Place, value: Rvalue, span: Option<FileSpan>) {
        self.push(LirStmtKind::Assign { place, value }, span);
    }

    /// Compute `value` into a fresh slot and hand back an operand reading it.
    /// The workhorse: every nested expression becomes a named slot, which is
    /// what turns a tree into a straight run.
    #[allow(clippy::wrong_self_convention)]
    fn into_temp(&mut self, value: Rvalue, ty: Ty, span: Option<FileSpan>) -> Operand {
        // Nothing to keep, and nowhere to keep it. The statement still runs: it
        // is the *value* that is empty, not the work that produced it.
        if is_void(&ty) {
            if let Rvalue::Use(Operand::Const(_)) = value {
                return Operand::Const(Constant::Undef);
            }
            let t = self.temp(Ty::Bool, span);
            self.assign(Place::local(t), value, span);
            return Operand::Const(Constant::Undef);
        }
        let t = self.temp(ty, span);
        self.assign(Place::local(t), value, span);
        Operand::local(t)
    }

    /// Branch on `cond`, continuing in a fresh block when it is true.
    fn branch_if(&mut self, cond: Operand, on_false: BlockId, span: Option<FileSpan>) {
        let next = self.new_block(None);
        self.terminate(Terminator::new(
            TermKind::Switch {
                value: cond,
                ty: LirTy::Bool,
                arms: vec![(1, next)],
                otherwise: on_false,
            },
            span,
        ));
        self.at = next;
    }

    // ===< Scopes and the cleanup ladder (§3) >===

    /// Open a scope. It starts with **no** defers: each is registered when the
    /// walk reaches the statement that writes it.
    fn push_scope(&mut self, block: IrId) {
        let id = self.next_scope;
        self.next_scope += 1;
        let drops = self.cx.drops.get(&block).cloned().unwrap_or_default();
        self.scopes.push(Scope {
            id,
            defers: Vec::new(),
            drops,
        });
    }

    /// Leave the innermost scope by falling off its end: its bodies run here,
    /// in reverse of registration, in the block control is already in.
    ///
    /// This path is not a ladder rung, because there is exactly one site that
    /// takes it — the end of the scope — so there is nothing to share it with.
    fn pop_scope(&mut self) {
        let Some(scope) = self.scopes.pop() else { return };
        if self.ended() {
            return;
        }
        for d in scope.defers.iter().rev() {
            self.eval(d);
        }
        self.emit_drops(&scope.drops);
    }

    /// Free this scope's non-escaping allocations (§5).
    ///
    /// A candidate whose local does not exist yet is skipped rather than
    /// invented: that is the ordering rule `lir::escape` states, and the skip is
    /// what makes a mistake there a leak instead of a free of a slot nothing
    /// wrote.
    fn emit_drops(&mut self, drops: &[DefId]) {
        for def in drops.iter().rev() {
            let Some(&l) = self.local_of.get(def) else {
                continue;
            };
            let local = &self.locals[l.0 as usize];
            let span = local.span;
            let ty = self.local_tys[l.0 as usize].clone();
            // `drop` frees **a pointer**, always. A `make`d slice is
            // `{ ptr, len }` by now (§7b) and the allocation is what the first
            // member names, so the projection happens here rather than becoming
            // a second shape every backend has to recognize.
            let what = match ty {
                Ty::Slice { .. } => Operand::Copy(Place::local(l).then(Projection::Field {
                    index: 0,
                    name: Symbol::new("ptr"),
                })),
                _ => Operand::local(l),
            };
            self.push(LirStmtKind::Drop(what), span);
        }
    }

    /// The block an exit of this kind jumps to from the current depth: the
    /// innermost rung of the ladder it has to climb.
    fn ladder(&mut self, exit: Exit, span: Option<FileSpan>) -> BlockId {
        let floor = match exit {
            Exit::Return => 0,
            Exit::Break(n) | Exit::Continue(n) => self.loops[n].floor,
        };
        self.rung(exit, self.scopes.len(), floor, span)
    }

    /// Rung `depth - 1` of the ladder for `exit`, built once and remembered.
    fn rung(&mut self, exit: Exit, depth: usize, floor: usize, span: Option<FileSpan>) -> BlockId {
        if depth <= floor {
            return self.landing(exit, span);
        }
        let scope = &self.scopes[depth - 1];
        let (id, defers, drops) = (scope.id, scope.defers.clone(), scope.drops.clone());
        // A scope with nothing to run on the way out is not a rung: it would be
        // a block whose only statement is a jump, which is a block a reader has
        // to follow to learn nothing.
        if defers.is_empty() && drops.is_empty() {
            return self.rung(exit, depth - 1, floor, span);
        }
        // How many of this scope's defers control has registered by now. An
        // exit reached before a `defer` was written runs fewer of them, and gets
        // a rung of its own rather than sharing one that runs a body it never
        // registered.
        let count = defers.len();
        if let Some(b) = self.rungs.get(&(id, exit, count)) {
            return *b;
        }
        let next = self.rung(exit, depth - 1, floor, span);
        let block = self.new_block(Some(format!("cleanup {} ({})", id, exit_label(exit))));
        self.rungs.insert((id, exit, count), block);
        let resume = self.at;
        self.at = block;
        for d in defers.iter().rev() {
            self.eval(d);
        }
        self.emit_drops(&drops);
        self.goto(next, span);
        self.at = resume;
        block
    }

    /// Where a ladder ends: the function's exit, or a loop's.
    fn landing(&mut self, exit: Exit, span: Option<FileSpan>) -> BlockId {
        match exit {
            Exit::Return => self.return_block(span),
            Exit::Break(n) => self.loops[n].break_to,
            Exit::Continue(n) => self.loops[n].continue_to,
        }
    }

    /// The one block that actually returns, created the first time a ladder
    /// needs somewhere to land.
    ///
    /// Its value comes from a slot rather than from an operand, because the
    /// block is shared by every `return` in the function and a shared block
    /// cannot tell which of them arrived (§3).
    fn return_block(&mut self, span: Option<FileSpan>) -> BlockId {
        if let Some(b) = self.ret_block {
            return b;
        }
        let slot = if matches!(self.ret, Ty::Void) {
            None
        } else {
            Some(self.temp(self.ret.clone(), span))
        };
        let b = self.new_block(Some("return".to_string()));
        let resume = self.at;
        self.at = b;
        self.terminate(Terminator::new(TermKind::Return(slot.map(Operand::local)), span));
        self.at = resume;
        self.ret_slot = slot;
        self.ret_block = Some(b);
        b
    }

    /// Whether any scope currently open has something to run on the way out —
    /// which is what decides whether an exit needs a ladder at all.
    fn any_defers(&self, floor: usize) -> bool {
        self.scopes[floor..]
            .iter()
            .any(|s| !s.defers.is_empty() || !s.drops.is_empty())
    }

    // ===< Blocks and statements >===

    /// Lower a block and hand back its value, if it has one.
    fn block_value(&mut self, b: &ir::Block) -> Option<Operand> {
        // `#unsafe` on a block is a scope (§9), so it is restored on the way
        // out — and it only ever turns checks *off*, never back on.
        let outer = self.unguarded;
        self.unguarded |= self.cx.meta.has_directive(b.id, "unsafe");
        self.push_scope(b.id);
        for s in &b.stmts {
            self.stmt(s);
        }
        let value = match &b.tail {
            Some(t) if !self.ended() => Some(self.eval(t)),
            _ => None,
        };
        self.pop_scope();
        self.unguarded = outer;
        value
    }

    fn stmt(&mut self, s: &ir::Stmt) {
        if self.ended() {
            return;
        }
        let span = self.cx.meta.span(s.id);
        match &s.kind {
            StmtKind::Let { pattern, init } => {
                let ty = self.cx.ty_of(init.id);
                // A `let` pattern is irrefutable, so there is nothing to test:
                // the initializer goes into a slot and the pattern's bindings
                // are projections out of it. A `void` initializer still runs and
                // still has nothing to put anywhere.
                let v = self.eval(init);
                if is_void(&ty) {
                    return;
                }
                let slot = self.temp(ty.clone(), span);
                self.assign(Place::local(slot), Rvalue::Use(v), span);
                self.bind_irrefutable(pattern, &Place::local(slot), &ty);
            }
            StmtKind::Assign { place, value } => {
                let v = self.eval(value);
                if let Some(p) = self.place_of(place) {
                    self.assign(p, Rvalue::Use(v), span);
                }
            }
            StmtKind::Expr(e) => {
                self.eval(e);
            }
            StmtKind::Return(e) => {
                let value = e.as_ref().map(|e| self.eval(e));
                if self.any_defers(0) {
                    let target = self.ladder(Exit::Return, span);
                    if let (Some(v), Some(slot)) = (value, self.ret_slot) {
                        self.assign(Place::local(slot), Rvalue::Use(v), span);
                    }
                    self.goto(target, span);
                } else {
                    let value = self.returned(value);
                    self.terminate(Terminator::new(TermKind::Return(value), span));
                }
            }
            StmtKind::Break(e) => {
                let Some(n) = self.loops.len().checked_sub(1) else {
                    return;
                };
                let result = self.loops[n].result;
                let value = e.as_ref().map(|e| self.eval(e));
                if let (Some(v), Some(slot)) = (value, result) {
                    self.assign(Place::local(slot), Rvalue::Use(v), span);
                }
                let target = self.ladder(Exit::Break(n), span);
                self.goto(target, span);
            }
            StmtKind::Continue => {
                let Some(n) = self.loops.len().checked_sub(1) else {
                    return;
                };
                let target = self.ladder(Exit::Continue(n), span);
                self.goto(target, span);
            }
            // Registering one emits no code: it is the *exits below here* that
            // grow a body to run (§3, spec §8.4).
            StmtKind::Defer(e) => {
                if let Some(scope) = self.scopes.last_mut() {
                    scope.defers.push(e.clone());
                }
            }
        }
    }

    /// Bind every name an irrefutable pattern introduces, by projecting out of
    /// `from`. No tests: a `let`'s pattern cannot fail.
    ///
    /// `ty` is the type of the value **at `from`**, threaded down rather than
    /// read off each pattern node. A pattern is matched *against* a type; its
    /// own node carries whatever inference happened to leave there, which for a
    /// variant's payload element is nothing at all. Carrying the scrutinee's
    /// type down is the only way a binding gets the type it really has.
    fn bind_irrefutable(&mut self, p: &Pattern, from: &Place, ty: &Ty) {
        let span = self.cx.meta.span(p.id);
        match &p.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding { def, name } => {
                if is_void(ty) {
                    return;
                }
                let id = self.new_local(Some(name.clone()), ty, span);
                self.local_of.insert(*def, id);
                self.assign(
                    Place::local(id),
                    Rvalue::Use(Operand::Copy(from.clone())),
                    span,
                );
            }
            PatternKind::At { binding, pattern } => {
                if is_void(ty) {
                    self.bind_irrefutable(pattern, from, ty);
                    return;
                }
                let id = self.new_local(Some(binding.name.clone()), ty, span);
                self.local_of.insert(binding.def, id);
                self.assign(
                    Place::local(id),
                    Rvalue::Use(Operand::Copy(from.clone())),
                    span,
                );
                self.bind_irrefutable(pattern, from, ty);
            }
            PatternKind::Tuple(ps) | PatternKind::TupleStruct { elems: ps, .. } => {
                let tys = self.member_tys(ty);
                for (i, sub) in ps.iter().enumerate() {
                    let f = from.clone().then(Projection::Field {
                        index: i as u32,
                        name: Symbol::new(&i.to_string()),
                    });
                    let t = tys.get(i).cloned().unwrap_or(Ty::Error);
                    self.bind_irrefutable(sub, &f, &t);
                }
            }
            PatternKind::Struct { fields, .. } => {
                let tys = self.member_tys(ty);
                for (name, sub) in fields {
                    let index = self.field_index(ty, name).unwrap_or(0);
                    let f = from.clone().then(Projection::Field {
                        index,
                        name: name.clone(),
                    });
                    let t = tys.get(index as usize).cloned().unwrap_or(Ty::Error);
                    self.bind_irrefutable(sub, &f, &t);
                }
            }
            PatternKind::Deref(inner) => {
                let f = from.clone().then(Projection::Deref);
                let t = match ty {
                    Ty::Ptr { inner, .. } => (**inner).clone(),
                    other => other.clone(),
                };
                self.bind_irrefutable(inner, &f, &t);
            }
            // Every remaining form tests something, which an irrefutable
            // position cannot contain. Inference rejected it already; there is
            // nothing to bind and nothing to say.
            _ => {}
        }
    }

    /// The types of an aggregate's members, in declaration order.
    fn member_tys(&self, ty: &Ty) -> Vec<Ty> {
        match ty {
            Ty::Ptr { inner, .. } => self.member_tys(inner),
            Ty::Tuple(elems) => elems.clone(),
            Ty::Slice { mutable, inner } => vec![
                Ty::Ptr {
                    mutable: *mutable,
                    inner: inner.clone(),
                },
                self.cx.usize_ty(),
            ],
            _ => self
                .cx
                .layouts
                .member_types(ty)
                .map(|ms| ms.into_iter().map(|(_, t)| t).collect())
                .unwrap_or_default(),
        }
    }

    /// The types of one enum variant's payload elements.
    fn variant_tys(&self, ty: &Ty, index: u32) -> Vec<Ty> {
        self.cx
            .layouts
            .variant_member_types(ty, index as usize)
            .map(|ms| ms.into_iter().map(|(_, t)| t).collect())
            .unwrap_or_default()
    }

    // ===< Expressions >===

    /// Lower `e` and hand back an operand holding its value.
    fn eval(&mut self, e: &Expr) -> Operand {
        let ty = self.cx.ty_of(e.id);
        let span = self.cx.meta.span(e.id);
        match &e.kind {
            ExprKind::Block(b) => {
                let v = self.block_value(b);
                v.unwrap_or(Operand::Const(Constant::Undef))
            }
            ExprKind::If { cond, then, els } => self.lower_if(e, cond, then, els.as_ref()),
            ExprKind::Match { scrutinee, arms } => self.lower_match(e, scrutinee, arms),
            ExprKind::Loop { body } => self.lower_loop(e, body),
            _ => match self.rvalue(e) {
                Some(Rvalue::Use(op)) => op,
                Some(v) => self.into_temp(v, ty, span),
                None => Operand::Const(Constant::Undef),
            },
        }
    }

    /// The straight-line forms: everything that is one statement in the block it
    /// is already in. `None` means the expression produced nothing at all — an
    /// error node, or a call that does not return.
    fn rvalue(&mut self, e: &Expr) -> Option<Rvalue> {
        let ty = self.cx.ty_of(e.id);
        let span = self.cx.meta.span(e.id);
        match &e.kind {
            ExprKind::Lit(l) => {
                let v = lit_value(l);
                Some(self.const_rvalue(&v, &ty, span))
            }
            ExprKind::Local(_)
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Deref { .. } => {
                let p = self.place_of(e)?;
                Some(Rvalue::Use(Operand::Copy(p)))
            }
            ExprKind::Global(def) => Some(Rvalue::Use(self.global_operand(*def))),
            // Monomorphization replaced every one of these with the literal the
            // instantiation chose. One still here is a program that did not get
            // that far.
            ExprKind::ConstParam(_) => Some(Rvalue::Use(Operand::Const(Constant::Undef))),
            // `&x` and `&mut x` are one instruction: what the second permits was
            // decided in sema, and no target has two kinds of address (§9).
            ExprKind::Ref { place, .. } => {
                let p = self.place_of(place)?;
                Some(Rvalue::Ref(p))
            }
            ExprKind::Binary { op, lhs, rhs } => Some(self.lower_binary(*op, lhs, rhs, &ty, span)),
            ExprKind::Unary { op, operand } => {
                let at = self.cx.ty_of(operand.id);
                let v = self.eval(operand);
                let at = self.cx.lir(&at);
                Some(Rvalue::Op {
                    op: op_of_un(*op),
                    ty: at,
                    args: vec![v],
                })
            }
            // `()` is `void`, not an aggregate of nothing: there is no value to
            // build and no slot to build it in.
            ExprKind::Tuple { elems } if elems.is_empty() => {
                Some(Rvalue::Use(Operand::Const(Constant::Undef)))
            }
            ExprKind::Tuple { elems } => {
                let fields = elems.iter().map(|x| self.eval(x)).collect();
                let ty = self.cx.lir(&ty);
                Some(Rvalue::Aggregate {
                    kind: self.struct_kind(&ty),
                    fields,
                })
            }
            ExprKind::Construct { fields, .. } => Some(self.lower_construct(fields, &ty)),
            ExprKind::Variant { name, args } => Some(self.lower_variant(name, args, &ty)),
            ExprKind::DynCast { value, .. } => {
                let data = self.eval(value);
                let vt = self.dyn_vtable(e.id);
                let ty = self.cx.lir(&ty);
                Some(Rvalue::Aggregate {
                    kind: self.struct_kind(&ty),
                    fields: vec![data, vt],
                })
            }
            ExprKind::Call {
                callee,
                args,
                builtin,
                dispatch,
            } => self.lower_call(e, callee, args, *builtin, dispatch),
            ExprKind::Intrinsic { name, args } => self.lower_intrinsic(e, name, args),
            ExprKind::Error => None,
            // Handled by `eval`, which owns the four branching forms.
            ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Loop { .. }
            | ExprKind::Block(_) => {
                let v = self.eval(e);
                Some(Rvalue::Use(v))
            }
        }
    }

    /// A reference to a top-level item, as a value.
    fn global_operand(&mut self, def: DefId) -> Operand {
        if let Some(g) = self.cx.linked.global(def) {
            // A `#static` is a **place**: the region exists for the whole run
            // and code may have written to it since the initializer ran.
            if g.mutable {
                return Operand::Copy(Place {
                    base: Base::Global(self.cx.global_id(def)),
                    projection: Vec::new(),
                });
            }
            // A `::` constant *is* its value (§2.5) — the evaluator produced it
            // and every use carries it, which is why constants are not in
            // `Program::globals`.
            if let Some(v) = self.cx.meta.get::<ConstValue>(g.id) {
                let ty = self.cx.ty_of(g.id);
                return self.const_operand(&v, &ty, None);
            }
        }
        if self.cx.linked.get(def).is_some() {
            return Operand::Const(Constant::Func(self.cx.func_id(def)));
        }
        Operand::Const(Constant::Undef)
    }

    /// `&&` and `||` short-circuit, so they are **control flow** rather than
    /// operations: the right operand must not run when the left already decided
    /// the answer. Everything else in the primitive core is one instruction.
    fn lower_binary(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        ty: &Ty,
        span: Option<FileSpan>,
    ) -> Rvalue {
        if matches!(op, BinOp::And | BinOp::Or) {
            let slot = self.temp(ty.clone(), span);
            let place = Place::local(slot);
            let a = self.eval(lhs);
            self.assign(place.clone(), Rvalue::Use(a.clone()), span);
            let other = self.new_block(Some(
                if op == BinOp::And { "&& rhs" } else { "|| rhs" }.to_string(),
            ));
            let join = self.new_block(Some("join".to_string()));
            // `a && b` runs `b` when `a` is true; `a || b` runs it when `a` is
            // false. One switch, with the arms the other way round.
            let (t, f) = if op == BinOp::And {
                (other, join)
            } else {
                (join, other)
            };
            self.terminate(Terminator::new(
                TermKind::Switch {
                    value: a,
                    ty: LirTy::Bool,
                    arms: vec![(1, t)],
                    otherwise: f,
                },
                span,
            ));
            self.at = other;
            let b = self.eval(rhs);
            self.assign(place.clone(), Rvalue::Use(b), span);
            self.goto(join, span);
            self.at = join;
            return Rvalue::Use(Operand::Copy(place));
        }
        let at = self.cx.ty_of(lhs.id);
        let a = self.eval(lhs);
        let b = self.eval(rhs);
        let at = self.cx.lir(&at);
        Rvalue::Op {
            op: op_of_bin(op),
            ty: at,
            args: vec![a, b],
        }
    }

    fn lower_construct(&mut self, fields: &[(Symbol, Expr)], ty: &Ty) -> Rvalue {
        let order = self.member_names(ty);
        let mut slots: Vec<Operand> = vec![Operand::Const(Constant::Undef); order.len()];
        for (name, value) in fields {
            let v = self.eval(value);
            // A field the definition does not have was reported by inference; a
            // wrong offset would be worse than a missing one.
            if let Some(i) = order.iter().position(|m| m == name) {
                slots[i] = v;
            }
        }
        let ty = self.cx.lir(ty);
        Rvalue::Aggregate {
            kind: self.struct_kind(&ty),
            fields: slots,
        }
    }

    fn lower_variant(&mut self, name: &Symbol, args: &[Expr], ty: &Ty) -> Rvalue {
        let fields: Vec<Operand> = args.iter().map(|a| self.eval(a)).collect();
        let Some((_, index)) = self.variant_index(ty, name) else {
            return Rvalue::Use(Operand::Const(Constant::Undef));
        };
        let Some(kind) = self.variant_kind(ty, index) else {
            return Rvalue::Use(Operand::Const(Constant::Undef));
        };
        Rvalue::Aggregate { kind, fields }
    }

    /// The vtable a `*T` → `*dyn Trait` coercion pairs with the data pointer.
    ///
    /// The slots were decided by monomorphization and stamped on this very node
    /// — the coercion is the only place in the program where the trait and the
    /// concrete type are written down together.
    fn dyn_vtable(&mut self, at: IrId) -> Operand {
        let Some(slots) = self.cx.meta.get::<crate::ir::mono::VtableSlots>(at) else {
            return Operand::Const(Constant::Undef);
        };
        let id = self.cx.vtable(&slots);
        Operand::Const(Constant::Global(id))
    }

    // ===< Panicking (§2) >===

    /// Emit the panic a compiler-raised failure makes: a call to `core`'s
    /// `panic`, then `unreachable`.
    ///
    /// A trapped overflow and an index past the end are **the program failing**,
    /// not a compiler-private abort, so they go through the same function a
    /// written `panic("...")` does — found by its `#lang("panic")` tag, like
    /// every other item the compiler reaches into `core` for. Two things follow
    /// from that and both are the point: a program that replaces
    /// `#lang("panic_handler")` replaces what these do too, and there is no
    /// `$panic` operation for a backend to implement, only a call it already
    /// knows how to emit.
    ///
    /// The `Location` is built here because the source did not write this call
    /// site: `#caller_location` fills the argument in for a call the *program*
    /// wrote (§5.2), and this is the same three numbers taken from the span the
    /// failing operation already carries.
    fn panic_at(&mut self, msg: &str, span: Option<FileSpan>) {
        let message = self.text_operand(msg.as_bytes(), span);
        let Some(def) = self.cx.lang.get("panic") else {
            // A `core` with no `#lang("panic")` item: inference has said so
            // already. Stop the block rather than lose the edge.
            self.terminate(Terminator::new(TermKind::Unreachable, span));
            return;
        };
        let callee = self.static_callee(def);
        let mut args = vec![message];
        if let Some(loc) = self.location(span) {
            args.push(loc);
        }
        self.emit_call(callee, args, Ty::Never, span);
    }

    /// The `Location` value for `span`, as an operand.
    fn location(&mut self, span: Option<FileSpan>) -> Option<Operand> {
        let def = self.cx.defs.resolve_alias(self.cx.lang.get("location")?);
        let (file, line, column) = match span.and_then(|s| {
            self.cx
                .sources
                .file(s.file)
                .map(|f| (f.name.clone(), f.line_col(s.span.start)))
        }) {
            Some((name, lc)) => (name, lc.line, lc.column),
            None => (String::new(), 0, 0),
        };
        let ty = Ty::Nominal {
            def,
            args: Vec::new(),
        };
        let name = self.text_operand(file.as_bytes(), span);
        let lty = self.cx.lir(&ty);
        let kind = self.struct_kind(&lty);
        let value = Rvalue::Aggregate {
            kind,
            fields: vec![
                name,
                Operand::int(line as i128),
                Operand::int(column as i128),
            ],
        };
        Some(self.into_temp(value, ty, span))
    }

    /// Read an enum's tag.
    ///
    /// It is an ordinary member read: an enum is `{ tag, payload }` by §7b, so
    /// the discriminant is a field like any other and needs no operation of its
    /// own. What a decision tree switches on is the value in that field, read
    /// **once** (§4) into a slot the groups share.
    fn tag_of(&mut self, place: &Place, ty: &Ty, span: Option<FileSpan>) -> Operand {
        let width = match self.cx.layouts.enum_layout(ty) {
            Some(Ok(e)) => (e.tag.size * 8) as u16,
            _ => 8,
        };
        let tag = place.clone().then(Projection::Field {
            index: 0,
            name: Symbol::new("tag"),
        });
        self.into_temp(
            Rvalue::Use(Operand::Copy(tag)),
            Ty::int(width, false),
            span,
        )
    }

    // ===< Calls >===

    fn lower_call(
        &mut self,
        e: &Expr,
        callee: &Expr,
        args: &[Expr],
        builtin: Option<BuiltinOp>,
        dispatch: &Dispatch,
    ) -> Option<Rvalue> {
        let ty = self.cx.ty_of(e.id);
        let span = self.cx.meta.span(e.id);

        // A builtin operator *is* a machine instruction: there is no function on
        // the other end of it (§6.13), and this is where the build's `overflow=`
        // setting becomes a second block rather than a flag (§7d).
        if let Some(op) = builtin {
            let vals: Vec<Operand> = args.iter().map(|a| self.eval(a)).collect();
            // Dividing by zero is **not** overflow, so it is not the `overflow=`
            // setting's to turn off (§7d): there is no wrapped answer for it the
            // way there is for `i32::MAX + 1`. It is the shape §3.2's bounds
            // check takes, for the same reason — a comparison, an edge, and a
            // block that does not come back.
            self.zero_check(op, &vals, &ty, span);
            self.range_check(op, &vals, &ty, span);
            if self.traps(op, &ty) {
                return Some(self.checked_op(op, vals, &ty, span));
            }
            // `-x` has no checked opcode and needs none: it *is* `0 - x`, and
            // the one value that overflows — the minimum, whose negation is not
            // in the type — is exactly the one `sub_checked` reports. Writing it
            // as a subtraction reuses the whole mechanism instead of inventing
            // a `neg_checked` that every backend would then have to implement.
            if matches!(op, BuiltinOp::Neg) && self.negation_traps(&ty) {
                let mut args = vec![Operand::int(0)];
                args.extend(vals);
                return Some(self.checked_op(BuiltinOp::Sub, args, &ty, span));
            }
            let at = self.cx.lir(&ty);
            return Some(Rvalue::Op {
                op: op_of_builtin(op, false),
                ty: at,
                args: vals,
            });
        }

        let mut vals = self.passed(args);
        let callee = match dispatch {
            // Which function a vtable slot holds is a property of the vtable,
            // not of the call. Reaching it is two ordinary projections and an
            // indirect call — the trait has disappeared by this level (§9).
            Dispatch::Virtual { trait_def, method } => {
                let callee = self.vtable_slot(*trait_def, *method, vals.first(), span)?;
                // **The receiver passed is the data pointer, not the fat
                // pointer.** A `*dyn Trait` is `{ data, vtable }` (§7b) and the
                // slot's type says `func(*void, …)` — `Cx::slot_ty` erases the
                // receiver precisely because every implementation takes the
                // address of the value, not the pair. Passing the pair here
                // would be a call whose argument is two words wide against a
                // parameter that is one, which no target can do and which only a
                // backend would ever have noticed.
                if let Some(Operand::Copy(p)) = vals.first().cloned() {
                    vals[0] = Operand::Copy(p.then(Projection::Field {
                        index: 0,
                        name: Symbol::new("data"),
                    }));
                }
                callee
            }
            // A `Generic` call that survived monomorphization is a defect there,
            // already reported. Treating the callee as a value keeps the graph
            // well formed instead of losing the call.
            Dispatch::Static | Dispatch::Generic { .. } => match &callee.kind {
                ExprKind::Global(def) => self.static_callee(*def),
                _ => {
                    let f = self.eval(callee);
                    Callee::Indirect(f)
                }
            },
        };
        self.spill_variadic_tail(&callee, &mut vals, args, span);
        self.emit_call(callee, vals, ty, span)
    }

    /// Give every argument in a `#c_vararg` tail a slot to live in.
    ///
    /// A tail argument is the one operand in this language with no type attached
    /// to it anywhere: a fixed parameter lends its type to the argument filling
    /// it, and there is no parameter here. That is fine for a [`Place`], which
    /// carries its local's type, and not for a [`Constant`] — `Constant::Int` is
    /// a [`BigInt`](num_bigint::BigInt) and nothing more, so `printf(c"%d", 5)`
    /// would reach a backend as a number with no width.
    ///
    /// So the constant is written to a slot and the slot is passed. The type is
    /// the argument's own, which the front end already promoted to what C will
    /// read (`i32`, `f64`) — and the store is a move a backend removes, which is
    /// the cheapest way to say something LIR otherwise cannot.
    fn spill_variadic_tail(
        &mut self,
        callee: &Callee,
        vals: &mut [Operand],
        args: &[Expr],
        span: Option<FileSpan>,
    ) {
        let Callee::Static(id) = callee else { return };
        let f = &self.cx.funcs[id.0 as usize];
        if !f.attrs.c_variadic {
            return;
        }
        let fixed = f.params;
        // `passed` drops every `void` argument, and §9 drops the parameters they
        // would have filled, so the two lists are still in step — but only the
        // arguments that survived are here, and their types have to be read from
        // the same surviving ones.
        let tys: Vec<Ty> = args
            .iter()
            .map(|a| self.cx.ty_of(a.id))
            .filter(|t| !is_void(t))
            .collect();
        for i in fixed..vals.len() {
            if !matches!(vals[i], Operand::Const(_)) {
                continue;
            }
            let Some(ty) = tys.get(i).cloned() else { continue };
            let slot = self.temp(ty, span);
            let value = Rvalue::Use(vals[i].clone());
            self.assign(Place::local(slot), value, span);
            vals[i] = Operand::local(slot);
        }
    }

    /// The arguments a call actually passes.
    ///
    /// Every one of them is **evaluated** — an argument is an expression and its
    /// effects happen whether or not its value goes anywhere — and the ones
    /// whose type is `void` are then dropped, because the parameter they would
    /// fill was dropped too. The rule is the same on both sides of the call, so
    /// a caller and a callee cannot come out with different arities.
    fn passed(&mut self, args: &[Expr]) -> Vec<Operand> {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            let ty = self.cx.ty_of(a.id);
            let v = self.eval(a);
            if !is_void(&ty) {
                vals.push(v);
            }
        }
        vals
    }

    /// A direct callee, by the symbol monomorphization decided.
    fn static_callee(&mut self, def: DefId) -> Callee {
        Callee::Static(self.cx.func_id(def))
    }

    /// Load a vtable slot: the receiver is a `*dyn Trait` fat pointer, so the
    /// vtable is its second member and the slot is a member of that.
    fn vtable_slot(
        &mut self,
        trait_def: DefId,
        method: DefId,
        recv: Option<&Operand>,
        span: Option<FileSpan>,
    ) -> Option<Callee> {
        let index = self.trait_slot(trait_def, method)?;
        let name = self.cx.defs.get(method).name.clone();
        let Some(Operand::Copy(p)) = recv else {
            return None;
        };
        let table = p.clone().then(Projection::Field {
            index: 1,
            name: Symbol::new("vtable"),
        });
        let slot = table.then(Projection::Deref).then(Projection::Field {
            index,
            name: name.clone(),
        });
        // The slot's type is the one the trait's vtable struct gives it, which
        // is the whole point of the vtable being a struct: the offset and the
        // signature both come from the table.
        let vt = self.cx.vtable_type(trait_def);
        let fty = self
            .cx
            .types
            .get(vt.0 as usize)
            .and_then(|t| t.members.get(index as usize))
            .map(|m| m.ty.clone())
            .unwrap_or(LirTy::ptr(LirTy::Void));
        let f = self.temp_lir(fty, span);
        self.assign(Place::local(f), Rvalue::Use(Operand::Copy(slot)), span);
        Some(Callee::Indirect(Operand::local(f)))
    }

    /// Which slot of `trait_def`'s vtable `method` is. The order is the trait's
    /// declaration order, which `ir::TypeDefKind::Trait` fixes for this reason.
    fn trait_slot(&self, trait_def: DefId, method: DefId) -> Option<u32> {
        let t = self.cx.linked.ty(trait_def)?;
        let TypeDefKind::Trait { methods, .. } = &t.kind else {
            return None;
        };
        methods
            .iter()
            .position(|m| m.def == method)
            .map(|i| i as u32)
    }

    /// Emit an intrinsic, and end the block when it does not return.
    ///
    /// The same three cases [`Self::emit_call`] has, for the same reasons — a
    /// `never` operation ends the block (§2), a `void` one keeps no
    /// destination, and only the third needs a slot. A `void` or `never` local
    /// would be a slot no machine has.
    fn emit_intrinsic(
        &mut self,
        name: Symbol,
        args: Vec<Operand>,
        ty: Ty,
        span: Option<FileSpan>,
    ) -> Option<Rvalue> {
        let callee = Callee::Intrinsic(Intrinsic::from_name(&name));
        self.emit_call(callee, args, ty, span)
    }

    /// Emit the call, and end the block when the callee does not return.
    ///
    /// §2: a function declared `-> never` is checked to genuinely never return,
    /// so a call to one is followed by `unreachable` and the block ends there.
    /// That is what lets `panic(...)` sit in any expression position without the
    /// type checker special-casing it.
    fn emit_call(
        &mut self,
        callee: Callee,
        args: Vec<Operand>,
        ty: Ty,
        span: Option<FileSpan>,
    ) -> Option<Rvalue> {
        if matches!(ty, Ty::Never) {
            self.push(
                LirStmtKind::Call {
                    dest: None,
                    callee,
                    args,
                },
                span,
            );
            self.terminate(Terminator::new(TermKind::Unreachable, span));
            return None;
        }
        if matches!(ty, Ty::Void) {
            self.push(
                LirStmtKind::Call {
                    dest: None,
                    callee,
                    args,
                },
                span,
            );
            return Some(Rvalue::Use(Operand::Const(Constant::Undef)));
        }
        let dest = self.temp(ty, span);
        self.push(
            LirStmtKind::Call {
                dest: Some(Place::local(dest)),
                callee,
                args,
            },
            span,
        );
        Some(Rvalue::Use(Operand::local(dest)))
    }

    /// Guard an index against the sequence's length.
    ///
    /// The length comes from wherever the sequence keeps it: a `[N]T` has it in
    /// its type and a `[]T` in its second member (§7b). Either way the emitted
    /// shape is the same one a checked add has — a comparison, a branch, and a
    /// block that panics — because "this program has gone wrong" has one
    /// answer in this language and it does not return.
    ///
    /// Two cases emit nothing:
    ///
    /// - **`#unsafe`** (§9). The directive's whole meaning is that the run-time
    ///   safety checks in that scope are off, and a check emitted anyway would
    ///   make it a comment.
    /// - **Both numbers already known.** `a[1]` on a `[3]i32` has a comparison
    ///   whose answer cannot change, and `check::bounds` has already reported
    ///   the case where that answer is "no". Emitting the branch would be
    ///   emitting a block nothing can reach.
    fn bounds_check(
        &mut self,
        base: &Place,
        seq: &Ty,
        index_expr: &Expr,
        index: &Operand,
        span: Option<FileSpan>,
    ) {
        if self.unguarded {
            return;
        }
        let n = match seq {
            Ty::Array { len, .. } => match len.value() {
                Some(n) => Some(n),
                // A length monomorphization did not substitute: there is no
                // number to compare against and no program to run either.
                None => return,
            },
            Ty::Slice { .. } => None,
            // Not a sequence — already a type error.
            _ => return,
        };
        // An array whose index the evaluator can work out needs no branch: the
        // comparison has two known numbers in it. The *answer* is not asked
        // here — `check::bounds` owns the diagnostic and asked the same
        // evaluator, so one that came out "no" has already been reported and
        // one that came out "yes" is what this skips.
        if let (Some(n), Some(i)) = (n, self.cx.const_index(index_expr))
            && i < n
        {
            return;
        }
        let len = match n {
            Some(n) => Operand::int(n as i128),
            None => Operand::Copy(base.clone().then(Projection::Field {
                index: 1,
                name: Symbol::new("len"),
            })),
        };
        let usize_ty = self.cx.usize_ty();
        let usize_ty = self.cx.lir(&usize_ty);
        let ok = self.into_temp(
            Rvalue::Op {
                op: Op::Lt,
                ty: usize_ty,
                args: vec![index.clone(), len],
            },
            Ty::Bool,
            span,
        );
        let trap = self.new_block(Some("out of bounds".to_string()));
        let go_on = self.new_block(None);
        self.terminate(Terminator::new(
            TermKind::Switch {
                value: ok,
                ty: LirTy::Bool,
                arms: vec![(1, go_on)],
                otherwise: trap,
            },
            span,
        ));
        self.at = trap;
        self.panic_at("index out of bounds", span);
        self.at = go_on;
    }

    // ===< The overflow setting, made real (§7d) >===

    /// Guard an integer division against a zero divisor.
    ///
    /// It is unconditional, unlike the overflow trap beside it. `overflow=wrap`
    /// says what `i32::MAX + 1` *means*; it says nothing about `x / 0`, which
    /// has no meaning to give — the machine instruction faults, and on a target
    /// where it does not the answer would be a number nobody can name. So the
    /// only thing that removes this check is `#unsafe` (§9), the directive whose
    /// meaning is that the checks in that scope are off.
    ///
    /// A divisor the evaluator proves non-zero needs no branch, for the reason
    /// the bounds check skips a known-good index: the comparison would have an
    /// answer nothing can change.
    fn zero_check(&mut self, op: BuiltinOp, args: &[Operand], ty: &Ty, span: Option<FileSpan>) {
        if self.unguarded || !matches!(op, BuiltinOp::Div | BuiltinOp::Rem) {
            return;
        }
        if !ty.is_int() {
            // A float divided by zero is an infinity, which is a value and not a
            // fault (§3.1).
            return;
        }
        let Some(divisor) = args.get(1) else { return };
        if let Operand::Const(Constant::Int(n)) = divisor
            && *n != num_bigint::BigInt::from(0)
        {
            return;
        }
        let at = self.cx.lir(ty);
        let ok = self.into_temp(
            Rvalue::Op {
                op: Op::Ne,
                ty: at,
                args: vec![divisor.clone(), Operand::int(0)],
            },
            Ty::Bool,
            span,
        );
        let trap = self.new_block(Some("division by zero".to_string()));
        let go_on = self.new_block(None);
        self.terminate(Terminator::new(
            TermKind::Switch {
                value: ok,
                ty: LirTy::Bool,
                arms: vec![(1, go_on)],
                otherwise: trap,
            },
            span,
        ));
        self.at = trap;
        self.panic_at("division by zero", span);
        self.at = go_on;
    }

    /// Whether this operation traps on overflow through a **checked opcode** in
    /// this build.
    ///
    /// Exactly the three §7d names: `add_checked`, `sub_checked`, `mul_checked`.
    /// The list is short because the shape is one instruction that yields a
    /// value *and* a flag, and only these three have one on the machines this
    /// compiles for.
    ///
    /// The other integer operations that can leave their width do **not** belong
    /// here, and listing them was a real bug: `op_of_builtin(Div, true)` is a
    /// plain `div`, so the pair slot `checked_op` allocated had its flag member
    /// left unwritten and the branch that read it read whatever the stack held.
    /// Each of them is handled where its own shape is: a signed `/` or `%` by
    /// [`Self::range_check`], a shift by the same, and `-x` by the `sub_checked`
    /// it already is.
    ///
    /// A float has no overflow to trap on — it has infinities — and a bitwise
    /// operation cannot leave its width at all.
    ///
    /// `#unsafe` turns these off with everything else (§9). The directive makes
    /// one claim — *this scope has already been reasoned about* — and there is
    /// no reading of it under which an index is covered and an addition is not.
    /// A scope that says so gets the plain opcode and no flag to branch on.
    fn traps(&self, op: BuiltinOp, ty: &Ty) -> bool {
        !self.unguarded
            && self.cx.options.overflow == OverflowMode::Trap
            && ty.is_int()
            && matches!(op, BuiltinOp::Add | BuiltinOp::Sub | BuiltinOp::Mul)
    }

    /// Whether `-x` at this type needs the checked form.
    ///
    /// Only a signed integer has a value whose negation it cannot hold. On an
    /// unsigned one every negation but `-0` is out of range, which is a thing
    /// the type checker refuses rather than something to branch on.
    fn negation_traps(&self, ty: &Ty) -> bool {
        self.cx.options.overflow == OverflowMode::Trap
            && matches!(self.cx.strip(ty).int_parts(), Some((true, _)))
    }

    /// The overflow checks that are a **comparison** rather than an opcode.
    ///
    /// Two operations leave their width without a machine flag to say so, and
    /// both are the shape §3.2's bounds check is — a comparison, an edge, and a
    /// block that does not come back:
    ///
    /// - **A signed `/` or `%`** by `-1`, applied to the minimum. There is no
    ///   `sdiv.with.overflow` on any target this emits for, and on x86 the
    ///   instruction faults, so the guard is the comparison the hardware does
    ///   not do. The unsigned families need none: no unsigned quotient leaves
    ///   the width.
    /// - **A shift** by an amount at least as wide as the type. `x << 32` on a
    ///   `u32` has no answer the machine agrees on — LLVM calls it undefined and
    ///   the two common architectures disagree about it in practice — so the
    ///   check is on the *amount*, not on the bits that fall off the end. Bits
    ///   leaving the top of a `<<` are what a shift is for.
    ///
    /// Both are removed by `#unsafe`, like every other run-time check: the
    /// directive's meaning is that the checks in that scope are off, and a
    /// program that says so about its own arithmetic is saying the same thing it
    /// says about an index. `overflow=wrap` removes them too, and differently —
    /// it changes what leaving the width *means* everywhere, where `#unsafe`
    /// says only that this scope has already been reasoned about. Both are also
    /// skipped when the operand the check is about is a constant that already
    /// answers it.
    fn range_check(&mut self, op: BuiltinOp, args: &[Operand], ty: &Ty, span: Option<FileSpan>) {
        if self.unguarded || self.cx.options.overflow != OverflowMode::Trap {
            return;
        }
        let Some((signed, bits)) = self.cx.strip(ty).int_parts() else {
            return;
        };
        let at = self.cx.lir(ty);
        match op {
            BuiltinOp::Div | BuiltinOp::Rem if signed => {
                let (Some(lhs), Some(rhs)) = (args.first(), args.get(1)) else {
                    return;
                };
                // A divisor that is not `-1` cannot produce the case, and the
                // constant says so without a branch.
                if let Operand::Const(Constant::Int(n)) = rhs
                    && *n != num_bigint::BigInt::from(-1)
                {
                    return;
                }
                let min = -(num_bigint::BigInt::from(1) << (bits - 1));
                let is_min = self.into_temp(
                    Rvalue::Op {
                        op: Op::Eq,
                        ty: at.clone(),
                        args: vec![lhs.clone(), Operand::Const(Constant::Int(min))],
                    },
                    Ty::Bool,
                    span,
                );
                let is_minus_one = self.into_temp(
                    Rvalue::Op {
                        op: Op::Eq,
                        ty: at,
                        args: vec![rhs.clone(), Operand::int(-1)],
                    },
                    Ty::Bool,
                    span,
                );
                let bad = self.into_temp(
                    Rvalue::Op {
                        op: Op::BitAnd,
                        ty: LirTy::Bool,
                        args: vec![is_min, is_minus_one],
                    },
                    Ty::Bool,
                    span,
                );
                self.trap_when(bad, "integer overflow", span);
            }
            BuiltinOp::Shl | BuiltinOp::Shr => {
                let Some(rhs) = args.get(1) else { return };
                if let Operand::Const(Constant::Int(n)) = rhs
                    && *n < num_bigint::BigInt::from(bits)
                {
                    return;
                }
                let too_far = self.into_temp(
                    Rvalue::Op {
                        op: Op::Ge,
                        ty: at,
                        args: vec![rhs.clone(), Operand::int(bits as i128)],
                    },
                    Ty::Bool,
                    span,
                );
                self.trap_when(too_far, "shift amount is wider than the type", span);
            }
            _ => {}
        }
    }

    /// Panic with `message` when `bad` is true, and carry on where it is not.
    fn trap_when(&mut self, bad: Operand, message: &str, span: Option<FileSpan>) {
        let trap = self.new_block(Some(message.to_string()));
        let go_on = self.new_block(None);
        self.terminate(Terminator::new(
            TermKind::Switch {
                value: bad,
                ty: LirTy::Bool,
                arms: vec![(1, trap)],
                otherwise: go_on,
            },
            span,
        ));
        self.at = trap;
        self.panic_at(message, span);
        self.at = go_on;
    }

    /// A checked operation: the value, a flag, and an edge to a block that does
    /// not come back.
    ///
    /// This is the whole of why `overflow=` is lowering's decision rather than
    /// codegen's (§7d). The trap form is not a flag on an instruction; it is a
    /// second basic block and an extra edge, and every pass after this one has
    /// to see that edge to be correct.
    fn checked_op(
        &mut self,
        op: BuiltinOp,
        args: Vec<Operand>,
        ty: &Ty,
        span: Option<FileSpan>,
    ) -> Rvalue {
        // The checked form yields both answers at once: the (wrapped) result and
        // whether it wrapped. One instruction, because every machine that has
        // the operation has the flag beside it.
        let pair = self.temp(Ty::Tuple(vec![ty.clone(), Ty::Bool]), span);
        let at = self.cx.lir(ty);
        self.assign(
            Place::local(pair),
            Rvalue::Op {
                op: op_of_builtin(op, true),
                ty: at,
                args,
            },
            span,
        );
        // Member `1` of a tuple is named `1`. It would read better as
        // `overflowed`, and it used to — but the type table says a `(i32,
        // bool)` has members `0` and `1`, and a projection naming a member the
        // type does not have is a dump that lies to the next reader.
        let flag = Place::local(pair).then(Projection::Field {
            index: 1,
            name: Symbol::new("1"),
        });
        let trap = self.new_block(Some("overflow".to_string()));
        let ok = self.new_block(None);
        self.terminate(Terminator::new(
            TermKind::Switch {
                value: Operand::Copy(flag),
                ty: LirTy::Bool,
                arms: vec![(1, trap)],
                otherwise: ok,
            },
            span,
        ));

        self.at = trap;
        self.panic_at("integer overflow", span);

        self.at = ok;
        Rvalue::Use(Operand::Copy(Place::local(pair).then(Projection::Field {
            index: 0,
            name: Symbol::new("0"),
        })))
    }

    // ===< Intrinsics (§9: gone as calls) >===

    fn lower_intrinsic(&mut self, e: &Expr, name: &Symbol, args: &[Expr]) -> Option<Rvalue> {
        let ty = self.cx.ty_of(e.id);
        let span = self.cx.meta.span(e.id);
        match name.as_str() {
            // The number layout computed. It is a *constant* by the time codegen
            // runs, and the type it asks about lives on the call's
            // instantiation, because `func <T> () -> usize` mentions `T`
            // nowhere.
            "size_of" | "align_of" => {
                let t = self.type_argument(e.id)?;
                let l = self.cx.layouts.of(&t).ok()?;
                let n = if name.as_str() == "size_of" {
                    l.size
                } else {
                    l.align
                };
                Some(Rvalue::Use(Operand::int(n as i128)))
            }
            // A conversion between primitives. `from` travels beside `to`
            // because what the conversion *is* — a truncation, a sign extension,
            // a rounding — depends on both, and recovering it from the operand
            // would be codegen re-deriving a type this stage already had.
            //
            // **A comptime literal is not a conversion.** `10` written where an
            // `i32` is wanted is an `i32` whose value is ten — the `$cast` the
            // IR carries is bookkeeping about where the literal's type came
            // from (§6.5), and the only stage that needed it was inference.
            // Emitting it here would leave `comptime_int` in the type of a
            // machine operand, which is a type no backend has a register for, so
            // the literal is folded into its definition and the cast disappears.
            "cast" if args.len() == 1 => {
                let from = self.cx.ty_of(args[0].id);
                // A cast the evaluator can perform is a **value**, whatever the
                // source type was. `cast.<u16>(7)` is `7`, and a `comptime_int`
                // that reached here at all has no other form: there is no
                // machine type to cast *from*, so the fold is not an
                // optimization but the only lowering there is.
                if let Some(v) = self.cx.const_value(e) {
                    return Some(self.const_rvalue(&v, &ty, span));
                }
                // A cast between two names for one type is nothing to do. It
                // is the shape the IR gives a `distinct` peel — `cast.<Point>`
                // on a `Handle` — and since a `distinct` is its representation
                // here (§9) both sides are the same type by the time this runs.
                if from == ty {
                    return Some(Rvalue::Use(self.eval(&args[0])));
                }
                let from_lir = self.cx.lir(&from);
                let to_lir = self.cx.lir(&ty);
                // Two aggregates of the same size and alignment: this is §3.8's
                // named-to-anonymous `cast`, and it moves no bits. The bytes are
                // already the value the target wants — the fields are the same
                // fields at the same offsets — so it is the one address read at
                // the other type, which is exactly `Projection::Cast`. Doing it
                // as an instruction instead would mean inventing a struct-to-
                // struct conversion no backend has.
                if let (LirTy::Named(_), LirTy::Named(id)) = (&from_lir, &to_lir) {
                    let same = self
                        .cx
                        .layouts
                        .of(&from)
                        .ok()
                        .zip(self.cx.layouts.of(&ty).ok())
                        .is_some_and(|(a, b)| a == b);
                    if same && let Some(place) = self.place_of(&args[0]) {
                        let id = *id;
                        return Some(Rvalue::Use(Operand::Copy(
                            place.then(Projection::Cast(id)),
                        )));
                    }
                }
                let v = self.eval(&args[0]);
                let (from, to) = (from_lir, to_lir);
                // Which conversion this is, decided once, here. A backend reads
                // the answer; it does not re-derive it from the pair.
                let kind = CastKind::of(&from, &to);
                Some(Rvalue::Cast {
                    value: v,
                    kind,
                    from,
                    to,
                })
            }
            // ===< Reflection (§9) >===
            //
            // All three are what §10 asks an intrinsic to be. Two are constants
            // — the description and the identity are known once `T` is
            // concrete, which it is by the time this runs — and the third is the
            // byte offset a static field access already computes.
            "type_id" => {
                let t = self.type_argument(e.id)?;
                Some(Rvalue::Use(Operand::Const(self.cx.type_id_const(&t))))
            }
            "type_info" => {
                let t = self.type_argument(e.id)?;
                let g = self.cx.type_info_global(&t)?;
                Some(Rvalue::Use(Operand::Copy(Place::global(g))))
            }
            // `base + m.offset`, in bytes. The offset is read out of the
            // descriptor the caller passed, which is the whole difference
            // between this and `p.y`: the selector is a value.
            "member_ptr" if args.len() == 2 => {
                let base = self.eval(&args[0]);
                let m = self.place_of(&args[1])?;
                let offset = Operand::Copy(m.then(Projection::Field {
                    index: 1,
                    name: Symbol::new("offset"),
                }));
                Some(Rvalue::Offset {
                    ptr: base,
                    index: offset,
                    stride: 1,
                })
            }
            // A member as a trait object: the data half is `member_ptr`, and the
            // vtable is read out of a table monomorphization filled with one
            // vtable per member, at the index the descriptor carries. The table
            // is what lets a run-time selector reach a statically chosen impl.
            "member_dyn" if args.len() == 2 => {
                let owner = self.type_argument(e.id)?;
                let tables = self.cx.meta.get::<crate::ir::mono::MemberVtables>(e.id)?;
                let base = self.eval(&args[0]);
                let m = self.place_of(&args[1])?;
                let offset = Operand::Copy(m.clone().then(Projection::Field {
                    index: 1,
                    name: Symbol::new("offset"),
                }));
                let address = Ty::Ptr {
                    mutable: true,
                    inner: Box::new(Ty::u8()),
                };
                let data = self.into_temp(
                    Rvalue::Offset {
                        ptr: base,
                        index: offset,
                        stride: 1,
                    },
                    address.clone(),
                    span,
                );
                let entries: Vec<Constant> = tables
                    .0
                    .iter()
                    .map(|slots| match slots {
                        Some(slots) => Constant::Global(self.cx.vtable(slots)),
                        // Already reported by monomorphization.
                        None => Constant::Undef,
                    })
                    .collect();
                let trait_key = tables
                    .0
                    .iter()
                    .flatten()
                    .next()
                    .map(|s| self.cx.defs.canonical_string(s.trait_def))
                    .unwrap_or_default();
                let elem = self.cx.lir(&address);
                let table = self.cx.data_global(
                    "member_vtables",
                    LirTy::Array {
                        len: entries.len() as u64,
                        elem: Box::new(elem),
                    },
                    Constant::Aggregate(entries),
                    format!(
                        "member_dyn:{trait_key}:{}",
                        crate::ir::mono::type_key(self.cx.defs, &owner)
                    ),
                );
                let index = Operand::Copy(m.then(Projection::Field {
                    index: 6,
                    name: Symbol::new("index"),
                }));
                let vt = Operand::Copy(Place::global(table).then(Projection::Index(index)));
                let ty = self.cx.lir(&ty);
                Some(Rvalue::Aggregate {
                    kind: self.struct_kind(&ty),
                    fields: vec![data, vt],
                })
            }
            // An explicit release (§6.9). It is the *same* instruction escape
            // analysis emits on its own (§5) — a pointer and a free — so there
            // is one thing for a backend to implement rather than two, and the
            // program's `drop(p)` and the compiler's are the same statement in
            // the dump.
            //
            // Nothing here has to stop the automatic drop as well: passing a
            // local to a call is what disqualifies it from being one (§5's
            // whitelist), and `drop(p)` is a call.
            "drop" if args.len() == 1 => {
                let v = self.eval(&args[0]);
                self.push(LirStmtKind::Drop(v), span);
                Some(Rvalue::Use(Operand::Const(Constant::Undef)))
            }
            // Indexing a built-in sequence. `core`'s `Index` / `IndexMut` impls
            // are these (§6.13), and both promise a **pointer** to the element,
            // which is what makes `a[i]` a place.
            //
            // The two sequences reach it differently, and that is §7b's whole
            // point about the slice being the interesting near-miss. An array
            // kept its own shape, so element `i` is an ordinary projection and
            // its address is a `&`. A slice did **not**: it is `{ ptr, len }`,
            // a struct has members rather than elements, so the address is the
            // pointer it holds moved along by `i` — pointer arithmetic, and the
            // only place in the compiler that does any.
            "index" | "index_mut" if args.len() == 2 => {
                let seq = match self.cx.ty_of(args[0].id) {
                    Ty::Ptr { inner, .. } => *inner,
                    other => other,
                };
                let base = self.place_of(&args[0])?.then(Projection::Deref);
                let i = self.eval(&args[1]);
                // Out of bounds **traps** (§3.2). The check is here for the same
                // reason `overflow=trap` is (§7d): it is not a flag on an
                // instruction, it is a comparison, an edge, and a block that
                // does not come back, and every pass after this one has to see
                // that edge to be correct.
                self.bounds_check(&base, &seq, &args[1], &i, span);
                match seq {
                    Ty::Slice { inner, .. } => {
                        let ptr = base.then(Projection::Field {
                            index: 0,
                            name: Symbol::new("ptr"),
                        });
                        Some(Rvalue::Offset {
                            ptr: Operand::Copy(ptr),
                            index: i,
                            stride: self.cx.stride(&inner),
                        })
                    }
                    _ => Some(Rvalue::Ref(base.then(Projection::Index(i)))),
                }
            }
            // A **slice** literal: storage, the elements written into it, and
            // the header over them.
            //
            // The array case below is an aggregate because an array *is* its
            // storage. A slice is not — its elements have to live somewhere —
            // and where that is is an allocation question. So this is `make`
            // and a store per element, which are instructions a backend already
            // has, rather than a fourteenth intrinsic for it to implement.
            "array" if matches!(ty, Ty::Slice { .. }) => {
                let Ty::Slice { inner, .. } = &ty else {
                    return None;
                };
                let elem = (**inner).clone();
                let stride = self.cx.stride(&elem);
                let vals: Vec<Operand> = args.iter().map(|a| self.eval(a)).collect();
                let n = vals.len() as i128;

                let slot = self.temp(ty.clone(), span);
                self.push(
                    LirStmtKind::Call {
                        dest: Some(Place::local(slot)),
                        callee: Callee::Intrinsic(Intrinsic::Make),
                        args: vec![Operand::int(n)],
                    },
                    span,
                );

                let data = Place::local(slot).then(Projection::Field {
                    index: 0,
                    name: Symbol::new("ptr"),
                });
                for (i, v) in vals.into_iter().enumerate() {
                    let at = self.temp(
                        Ty::Ptr {
                            mutable: true,
                            inner: Box::new(elem.clone()),
                        },
                        span,
                    );
                    self.assign(
                        Place::local(at),
                        Rvalue::Offset {
                            ptr: Operand::Copy(data.clone()),
                            index: Operand::int(i as i128),
                            stride,
                        },
                        span,
                    );
                    self.assign(
                        Place::local(at).then(Projection::Deref),
                        Rvalue::Use(v),
                        span,
                    );
                }
                Some(Rvalue::Use(Operand::Copy(Place::local(slot))))
            }
            // A sub-slice: an **address and a length**, and no intrinsic at
            // all by the time a backend sees it.
            //
            // Three plain numbers arrive here — `sema::lower`'s `lower_slice`
            // decomposed the range, because the syntax already knew which of
            // the six forms was written. So there is nothing left to branch on:
            // the pointer is the base advanced by `start` elements and the
            // length is `end - start`, which is one `Offset` and one
            // `Aggregate`. A `$slice` that took a `Range` would have made every
            // backend switch on a tag to rediscover that.
            "slice" if args.len() == 3 => {
                let seq = match self.cx.ty_of(args[0].id) {
                    Ty::Ptr { inner, .. } => *inner,
                    other => other,
                };
                let base = self.place_of(&args[0])?;
                let start = self.eval(&args[1]);
                let end = self.eval(&args[2]);
                let (elem, from) = match &seq {
                    // A slice of a slice starts at the data pointer it already
                    // holds.
                    Ty::Slice { inner, .. } => (
                        (**inner).clone(),
                        Operand::Copy(base.then(Projection::Field {
                            index: 0,
                            name: Symbol::new("ptr"),
                        })),
                    ),
                    // An array *is* its storage, so the address of it is where
                    // the elements begin.
                    Ty::Array { inner, .. } => {
                        let addr = self.temp(
                            Ty::Ptr {
                                mutable: false,
                                inner: inner.clone(),
                            },
                            span,
                        );
                        self.assign(Place::local(addr), Rvalue::Ref(base), span);
                        ((**inner).clone(), Operand::local(addr))
                    }
                    // Anything else is a program that did not type-check.
                    _ => return None,
                };
                let stride = self.cx.stride(&elem);
                let usize_ty = self.cx.usize_ty();

                let ptr = self.temp(
                    Ty::Ptr {
                        mutable: false,
                        inner: Box::new(elem),
                    },
                    span,
                );
                self.assign(
                    Place::local(ptr),
                    Rvalue::Offset {
                        ptr: from,
                        index: start.clone(),
                        stride,
                    },
                    span,
                );
                let len = self.temp(usize_ty.clone(), span);
                let at = self.cx.lir(&usize_ty);
                self.assign(
                    Place::local(len),
                    Rvalue::Op {
                        op: Op::Sub,
                        ty: at,
                        args: vec![end, start],
                    },
                    span,
                );
                let lty = self.cx.lir(&ty);
                let kind = self.struct_kind(&lty);
                Some(Rvalue::Aggregate {
                    kind,
                    fields: vec![Operand::local(ptr), Operand::local(len)],
                })
            }
            // A sequence's length: a fixed array's is part of its type and a
            // slice keeps it in its second member, so neither needs code.
            "len" if args.len() == 1 => {
                let arg = self.cx.ty_of(args[0].id);
                if let Ty::Array { len, .. } = arg {
                    return len.value().map(|n| Rvalue::Use(Operand::int(n as i128)));
                }
                let p = self.place_of(&args[0])?;
                Some(Rvalue::Use(Operand::Copy(p.then(Projection::Field {
                    index: 1,
                    name: Symbol::new("len"),
                }))))
            }
            // Arithmetic the program asked to wrap (§6.6). It is an
            // instruction on every target — the ordinary one, with the overflow
            // check the build would otherwise have added left off — so it is an
            // opcode rather than a name a backend has to know.
            "wrapping_add" | "wrapping_sub" | "wrapping_mul" if args.len() == 2 => {
                let vals: Vec<Operand> = args.iter().map(|a| self.eval(a)).collect();
                let at = self.cx.lir(&ty);
                // The same instruction `overflow=wrap` emits: an `add` wraps,
                // by definition (§6.6, §7d). An opcode of its own would be a
                // second spelling of one operation.
                let op = match name.as_str() {
                    "wrapping_add" => Op::Add,
                    "wrapping_sub" => Op::Sub,
                    _ => Op::Mul,
                };
                Some(Rvalue::Op {
                    op,
                    ty: at,
                    args: vals,
                })
            }
            // A composite literal. It is an intrinsic in the IR because the
            // surface form is one syntax over two types; here the array case is
            // an aggregate like any other. A **slice** literal is not: its
            // elements need storage somewhere, and where that is is an
            // allocation question rather than a shape one.
            "array" if matches!(ty, Ty::Array { .. }) => {
                let fields = args.iter().map(|a| self.eval(a)).collect();
                Some(Rvalue::Aggregate {
                    kind: Aggregate::Array,
                    fields,
                })
            }
            // `value ; count` — one element, written `count` times.
            //
            // Two shapes, for the same reason the literal above has two. An
            // **array** is its own storage and its length is part of its type,
            // so the list is known right here: this is the aggregate above with
            // one operand repeated. The value is evaluated **once** because the
            // source wrote it once — `.{ f(); 3 }` calls `f` one time and stores
            // the answer three times.
            //
            // A **slice** has no storage of its own and its count is an ordinary
            // run-time value, so it is the `make` above with a counter instead
            // of a fixed list. That loop is why this was never an intrinsic: a
            // backend would have had to build a comparison, an edge and a block
            // that comes back, and §10 says an intrinsic is one instruction or
            // one call.
            "repeat" if args.len() == 2 => self.lower_repeat(&args[0], &args[1], &ty, span),
            // Bulk memory. Both are declared over **slices**, so the byte count
            // is computed here from a length the value already carries rather
            // than taken from the caller — the two-argument mistake C's
            // versions are famous for cannot be written.
            "memcpy" if args.len() == 2 => {
                let dest = self.slice_start(&args[0])?;
                let src = self.slice_start(&args[1])?;
                let bytes = self.slice_bytes(&args[1], span)?;
                self.push(
                    LirStmtKind::Call {
                        dest: None,
                        callee: Callee::Intrinsic(Intrinsic::Memcpy),
                        args: vec![dest, src, bytes],
                    },
                    span,
                );
                Some(Rvalue::Use(Operand::Const(Constant::Undef)))
            }
            "memset" if args.len() == 2 => {
                let dest = self.slice_start(&args[0])?;
                let bytes = self.slice_bytes(&args[0], span)?;
                let byte = self.eval(&args[1]);
                self.push(
                    LirStmtKind::Call {
                        dest: None,
                        callee: Callee::Intrinsic(Intrinsic::Memset),
                        args: vec![dest, byte, bytes],
                    },
                    span,
                );
                Some(Rvalue::Use(Operand::Const(Constant::Undef)))
            }
            // The compile-time assertion produces no code. Its condition was
            // evaluated and judged by `ir::check::constants` long before this,
            // which is the whole of what it does; reaching run time at all
            // would make "costs nothing" false.
            "comptime_assert" => Some(Rvalue::Use(Operand::Const(Constant::Undef))),
            _ => {
                let vals = self.passed(args);
                self.emit_intrinsic(name.clone(), vals, ty, span)
            }
        }
    }

    /// The address a slice argument starts at — its `ptr` member (§7b).
    fn slice_start(&mut self, arg: &Expr) -> Option<Operand> {
        let place = self.place_of(arg)?;
        Some(Operand::Copy(place.then(Projection::Field {
            index: 0,
            name: Symbol::new("ptr"),
        })))
    }

    /// How many **bytes** a slice argument spans: its length times its element's
    /// stride. The stride, not the element's data size, for the reason an array
    /// uses it too — the elements sit end to end at that spacing.
    fn slice_bytes(&mut self, arg: &Expr, span: Option<FileSpan>) -> Option<Operand> {
        let ty = self.cx.ty_of(arg.id);
        let Ty::Slice { inner, .. } = &ty else {
            return None;
        };
        let stride = self.cx.stride(inner);
        let place = self.place_of(arg)?;
        let len = Operand::Copy(place.then(Projection::Field {
            index: 1,
            name: Symbol::new("len"),
        }));
        let usize_ty = self.cx.usize_ty();
        let usize_lir = self.cx.lir(&usize_ty);
        // A stride of one is the common case — every `[]u8` — and multiplying
        // by it would be an instruction that says nothing.
        if stride == 1 {
            return Some(len);
        }
        Some(self.into_temp(
            Rvalue::Op {
                op: Op::Mul,
                ty: usize_lir,
                args: vec![len, Operand::int(stride as i128)],
            },
            usize_ty,
            span,
        ))
    }

    /// The single byte a repeated element is made of, when it is made of one.
    ///
    /// Two cases, and no attempt at a third. **Zero** is uniform whatever the
    /// element's size or shape: an all-zero element of any type is `size` zero
    /// bytes. A **one-byte** element is uniform because it is one byte. Anything
    /// else — a repeated `0x0101`, say — would need the target's byte order to
    /// decide, and the loop that handles it is correct without asking.
    fn uniform_byte(&mut self, value: &Expr, elem_size: Option<u64>) -> Option<u8> {
        let v = self.cx.const_value(value)?;
        let zero = match &v {
            crate::ir::ConstValue::Int(n) => *n == num_bigint::BigInt::from(0),
            crate::ir::ConstValue::Float(f) => *f == 0.0 && f.is_sign_positive(),
            crate::ir::ConstValue::Bool(b) => !*b,
            crate::ir::ConstValue::Char(c) => *c == '\0',
            _ => false,
        };
        if zero {
            return Some(0);
        }
        if elem_size != Some(1) {
            return None;
        }
        match &v {
            crate::ir::ConstValue::Int(n) => u8::try_from(n).ok(),
            crate::ir::ConstValue::Bool(b) => Some(*b as u8),
            _ => None,
        }
    }

    /// Fill `dest` — an array place — with `v`, `n` times, as a loop.
    ///
    /// `i := 0; while i < n { dest[i] = v; i = i + 1 }`, written out as the
    /// three blocks a `while` is by this point. The counter is a length, so it
    /// cannot overflow before the storage it walks would have: the unchecked
    /// `add` is the same instruction the checked form leaves behind, without
    /// the branch that can never be taken.
    fn fill_loop(&mut self, dest: Place, v: Operand, n: Operand, span: Option<FileSpan>) {
        let usize_ty = self.cx.usize_ty();
        let usize_lir = self.cx.lir(&usize_ty);
        let i = self.temp(usize_ty, span);
        self.assign(Place::local(i), Rvalue::Use(Operand::int(0)), span);
        let head = self.new_block(Some("repeat".to_string()));
        let body = self.new_block(None);
        let done = self.new_block(None);
        self.terminate(Terminator::new(TermKind::Goto(head), span));

        self.at = head;
        let more = self.into_temp(
            Rvalue::Op {
                op: Op::Lt,
                ty: usize_lir.clone(),
                args: vec![Operand::local(i), n],
            },
            Ty::Bool,
            span,
        );
        self.terminate(Terminator::new(
            TermKind::Switch {
                value: more,
                ty: LirTy::Bool,
                arms: vec![(1, body)],
                otherwise: done,
            },
            span,
        ));

        self.at = body;
        self.assign(
            dest.then(Projection::Index(Operand::local(i))),
            Rvalue::Use(v),
            span,
        );
        self.assign(
            Place::local(i),
            Rvalue::Op {
                op: Op::Add,
                ty: usize_lir,
                args: vec![Operand::local(i), Operand::int(1)],
            },
            span,
        );
        self.terminate(Terminator::new(TermKind::Goto(head), span));
        self.at = done;
    }

    /// `value ; count` — see the `"repeat"` arm above.
    fn lower_repeat(
        &mut self,
        value: &Expr,
        count: &Expr,
        ty: &Ty,
        span: Option<FileSpan>,
    ) -> Option<Rvalue> {
        match ty {
            // The count *is* the array's length (`infer::check_composite_body`
            // holds the two together), so the count expression has nothing left
            // to say and is not evaluated. A length monomorphization did not
            // substitute leaves no number to repeat and no program to run.
            Ty::Array { len, .. } => {
                let n = len.value()?;
                let v = self.eval(value);
                // Writing the elements out one by one is right for a short
                // array: it is a value the constant evaluator can fold, and the
                // backend emits it whole.
                //
                // It is **not** right for a long one, and the cost is not the
                // program's, it is the compiler's: every element is a separate
                // operand carried through every stage after this, so
                // `.{ 0; 1000000 }` is a million of them and the compile does
                // not finish. Past a threshold the same array is filled by a
                // loop instead, which is a fixed amount of LIR whatever the
                // length — and a loop that stores one constant to every slot is
                // the shape a backend turns back into a `memset`.
                if n <= REPEAT_UNROLL {
                    return Some(Rvalue::Aggregate {
                        kind: Aggregate::Array,
                        fields: vec![v; n as usize],
                    });
                }
                let slot = self.temp(ty.clone(), span);
                let elem_size = match ty {
                    Ty::Array { inner, .. } => self.cx.layouts.of(inner).ok().map(|l| l.size),
                    _ => None,
                };
                // A repeated value whose bytes are all the same is a `memset`,
                // which is the whole of the work: one call, whatever the length.
                // Zero is the case that matters — `.{ 0; N }` is how every
                // buffer in every program starts — and it is uniform at any
                // element size.
                match (self.uniform_byte(value, elem_size), elem_size) {
                    (Some(byte), Some(size)) => {
                        let at = self.into_temp(
                            Rvalue::Ref(Place::local(slot)),
                            Ty::Ptr {
                                mutable: true,
                                inner: Box::new(Ty::u8()),
                            },
                            span,
                        );
                        self.push(
                            LirStmtKind::Call {
                                dest: None,
                                callee: Callee::Intrinsic(Intrinsic::Memset),
                                args: vec![
                                    at,
                                    Operand::int(byte as i128),
                                    Operand::int((n.saturating_mul(size)) as i128),
                                ],
                            },
                            span,
                        );
                    }
                    // Anything else — a repeated value that is not a constant,
                    // or one whose bytes differ — is a loop. Still a fixed
                    // amount of LIR, which is the point.
                    _ => self.fill_loop(Place::local(slot), v, Operand::int(n as i128), span),
                }
                Some(Rvalue::Use(Operand::Copy(Place::local(slot))))
            }
            Ty::Slice { inner, .. } => {
                let elem = (**inner).clone();
                let stride = self.cx.stride(&elem);
                let usize_ty = self.cx.usize_ty();
                let usize_lir = self.cx.lir(&usize_ty);

                let v = self.eval(value);
                let n = self.eval(count);

                let slot = self.temp(ty.clone(), span);
                self.push(
                    LirStmtKind::Call {
                        dest: Some(Place::local(slot)),
                        callee: Callee::Intrinsic(Intrinsic::Make),
                        args: vec![n.clone()],
                    },
                    span,
                );
                let data = Place::local(slot).then(Projection::Field {
                    index: 0,
                    name: Symbol::new("ptr"),
                });

                // `i := 0; while i < n { data[i] = v; i = i + 1 }`, written out
                // as the three blocks a `while` is by this point.
                let i = self.temp(usize_ty, span);
                self.assign(Place::local(i), Rvalue::Use(Operand::int(0)), span);
                let head = self.new_block(Some("repeat".to_string()));
                let body = self.new_block(None);
                let done = self.new_block(None);
                self.terminate(Terminator::new(TermKind::Goto(head), span));

                self.at = head;
                let more = self.into_temp(
                    Rvalue::Op {
                        op: Op::Lt,
                        ty: usize_lir.clone(),
                        args: vec![Operand::local(i), n],
                    },
                    Ty::Bool,
                    span,
                );
                self.terminate(Terminator::new(
                    TermKind::Switch {
                        value: more,
                        ty: LirTy::Bool,
                        arms: vec![(1, body)],
                        otherwise: done,
                    },
                    span,
                ));

                self.at = body;
                let at = self.temp(
                    Ty::Ptr {
                        mutable: true,
                        inner: Box::new(elem),
                    },
                    span,
                );
                self.assign(
                    Place::local(at),
                    Rvalue::Offset {
                        ptr: Operand::Copy(data),
                        index: Operand::local(i),
                        stride,
                    },
                    span,
                );
                self.assign(
                    Place::local(at).then(Projection::Deref),
                    Rvalue::Use(v),
                    span,
                );
                // The counter is a length, so it cannot overflow before the
                // allocation it is walking would have: an unchecked `add` here
                // is the same instruction the bounds-checked form would leave
                // behind, without the branch that can never be taken.
                self.assign(
                    Place::local(i),
                    Rvalue::Op {
                        op: Op::Add,
                        ty: usize_lir,
                        args: vec![Operand::local(i), Operand::int(1)],
                    },
                    span,
                );
                self.terminate(Terminator::new(TermKind::Goto(head), span));

                self.at = done;
                Some(Rvalue::Use(Operand::Copy(Place::local(slot))))
            }
            // Anything else is a program that did not type-check.
            _ => None,
        }
    }

    /// The single **type** argument a call instantiated its callee with.
    fn type_argument(&self, at: IrId) -> Option<Ty> {
        let crate::sema::infer::Instantiation(args) = self.cx.meta.get(at)?;
        match args.first()? {
            crate::sema::infer::GenericArg::Ty(t) => Some(t.clone()),
            crate::sema::infer::GenericArg::Const(_) => None,
        }
    }

    // ===< Places >===

    /// `e` as an lvalue.
    ///
    /// Anything that is not one gets a slot, and the slot is the place. That is
    /// what makes `(a + b).x` need no special case anywhere downstream.
    fn place_of(&mut self, e: &Expr) -> Option<Place> {
        match &e.kind {
            ExprKind::Local(def) => self.local_of.get(def).copied().map(Place::local),
            // A `#static` is the one global with a region of its own. A `::`
            // constant *is* its value (§2.5) and has no address, so a place is
            // made for it the way one is made for any other value that needs
            // one: a slot, written once. `TABLE[1]` on a constant array is what
            // asks for this — indexing goes through `&TABLE`, and a constant
            // with nowhere to point at would be a pointer to nothing.
            ExprKind::Global(def) => match self.cx.linked.global(*def) {
                Some(g) if g.mutable => Some(Place::global(self.cx.global_id(*def))),
                _ => self.materialize(e),
            },
            ExprKind::Deref { base } => Some(self.place_of(base)?.then(Projection::Deref)),
            ExprKind::Field { base, name, .. } => {
                let bty = self.cx.ty_of(base.id);
                let index = self.field_index(&bty, name)?;
                Some(self.place_of(base)?.then(Projection::Field {
                    index,
                    name: name.clone(),
                }))
            }
            ExprKind::TupleIndex { base, index } => {
                Some(self.place_of(base)?.then(Projection::Field {
                    index: *index as u32,
                    name: Symbol::new(&index.to_string()),
                }))
            }
            _ => self.materialize(e),
        }
    }

    /// Give `e` a place by evaluating it into one.
    ///
    /// A value that is already read out of a place keeps that place; anything
    /// else gets a slot of its own, which is what makes `(a + b).x` and
    /// `TABLE[1]` need no special case downstream.
    fn materialize(&mut self, e: &Expr) -> Option<Place> {
        let ty = self.cx.ty_of(e.id);
        let span = self.cx.meta.span(e.id);
        if is_void(&ty) {
            self.eval(e);
            return None;
        }
        match self.eval(e) {
            Operand::Copy(p) => Some(p),
            other => {
                let slot = self.temp(ty, span);
                self.assign(Place::local(slot), Rvalue::Use(other), span);
                Some(Place::local(slot))
            }
        }
    }


    // ===< Types, constants and aggregates, as LIR spells them >===

    /// The aggregate kind for "build a value of this struct type".
    ///
    /// A struct literal, a tuple, a slice header and a trait object's fat
    /// pointer are one operation with four spellings in the source and one here
    /// (§7b): the destination's type says which struct, so the kind only has to
    /// name it.
    fn struct_kind(&mut self, ty: &LirTy) -> Aggregate {
        match ty {
            LirTy::Named(id) => Aggregate::Struct(*id),
            // Not an aggregate: a program that did not type-check, already
            // reported. The empty struct keeps every id in the unit resolvable.
            _ => Aggregate::Struct(self.cx.error_type()),
        }
    }

    /// The aggregate kind for building one variant of an enum.
    fn variant_kind(&mut self, ty: &Ty, index: u32) -> Option<Aggregate> {
        let lty = self.cx.lir(ty);
        let LirTy::Named(id) = lty else { return None };
        let (variant, tag, name) = {
            let def = self.cx.types.get(id.0 as usize)?;
            let Origin::Enum { variants } = &def.origin else {
                return None;
            };
            let v = variants.get(index as usize)?;
            (v.ty, v.tag, v.name.clone())
        };
        Some(Aggregate::Variant {
            ty: id,
            variant,
            index,
            tag,
            name,
        })
    }

    /// The place a variant's fields live at: the shared payload, read as the
    /// variant's own struct type (§7b).
    fn variant_place(&mut self, place: &Place, ty: &Ty, index: u32) -> Place {
        let payload = place.clone().then(Projection::Field {
            index: 1,
            name: Symbol::new("payload"),
        });
        let lty = self.cx.lir(ty);
        let LirTy::Named(id) = lty else { return payload };
        let vty = match self.cx.types.get(id.0 as usize).map(|d| &d.origin) {
            Some(Origin::Enum { variants }) => variants.get(index as usize).map(|v| v.ty),
            _ => None,
        };
        match vty {
            Some(v) => payload.then(Projection::Cast(v)),
            None => payload,
        }
    }

    /// The name a variant's `i`th field has, which is the name its own type
    /// gives it (§7b).
    ///
    /// `.rect { w, h }` has members called `w` and `h`, not `0` and `1`: the
    /// index is what a backend uses and the name is what a reader does, and a
    /// projection that names a member its type does not have is a dump that
    /// lies.
    fn variant_member(&mut self, ty: &Ty, index: u32, i: usize) -> Symbol {
        let lty = self.cx.lir(ty);
        let fallback = || Symbol::new(&i.to_string());
        let LirTy::Named(id) = lty else {
            return fallback();
        };
        let vty = match self.cx.types.get(id.0 as usize).map(|d| &d.origin) {
            Some(Origin::Enum { variants }) => variants.get(index as usize).map(|v| v.ty),
            _ => None,
        };
        vty.and_then(|v| self.cx.types.get(v.0 as usize))
            .and_then(|d| d.members.get(i))
            .map(|m| m.name.clone())
            .unwrap_or_else(fallback)
    }

    /// The width an enum's tag is switched at.
    fn tag_ty(&mut self, ty: &Ty) -> LirTy {
        let bits = match self.cx.layouts.enum_layout(ty) {
            Some(Ok(e)) => (e.tag.size * 8) as u16,
            _ => 8,
        };
        LirTy::Int {
            bits,
            signed: false,
        }
    }

    /// A constant, as something an instruction can read.
    ///
    /// A scalar is itself. Anything that needs storage — a string, a byte
    /// string, an array or a struct the evaluator folded — becomes a global and
    /// the value becomes a reference to it, because an operand carrying a blob
    /// asks every backend to invent read-only data emission on its own (§2.5).
    fn const_operand(&mut self, v: &ConstValue, ty: &Ty, span: Option<FileSpan>) -> Operand {
        match self.const_rvalue(v, ty, span) {
            Rvalue::Use(o) => o,
            other => {
                let t = self.temp(ty.clone(), span);
                self.assign(Place::local(t), other, span);
                Operand::local(t)
            }
        }
    }

    /// The same, before it is forced into an operand: a `str` is two words and
    /// is built rather than read, so it is an aggregate rather than a load.
    fn const_rvalue(&mut self, v: &ConstValue, ty: &Ty, span: Option<FileSpan>) -> Rvalue {
        let ty = self.cx.strip(ty);
        match v {
            ConstValue::Int(n) => Rvalue::Use(Operand::Const(Constant::Int(n.clone()))),
            ConstValue::Float(f) => Rvalue::Use(Operand::Const(Constant::Float(*f))),
            ConstValue::Bool(b) => Rvalue::Use(Operand::Const(Constant::Bool(*b))),
            ConstValue::Char(c) => Rvalue::Use(Operand::int(*c as i128)),
            ConstValue::Void => Rvalue::Use(Operand::Const(Constant::Undef)),
            ConstValue::Str(s) => self.text_rvalue(s.as_bytes(), &ty, span),
            ConstValue::Bytes(b) => self.text_rvalue(b, &ty, span),
            // A slice is built from its storage's address and length, the way
            // text is.
            ConstValue::Aggregate(items) if let Ty::Slice { inner, .. } = &ty => {
                let (g, len) = self.cx.slice_storage(v, items, inner);
                let lty = self.cx.lir(&ty);
                let kind = self.struct_kind(&lty);
                Rvalue::Aggregate {
                    kind,
                    fields: vec![Operand::Const(Constant::Global(g)), Operand::int(len as i128)],
                }
            }
            // A composite the evaluator folded is data, and data has an address.
            ConstValue::Aggregate(_) | ConstValue::Variant { .. } => {
                let init = self.cx.const_data(v, &ty);
                let lty = self.cx.lir(&ty);
                let key = format!("{}:{}", self.cx.key(&ty), v.display());
                let g = self.cx.data_global("data", lty, init, key);
                Rvalue::Use(Operand::Copy(Place::global(g)))
            }
        }
    }

    /// Text, at the type it is used as: the bytes for a `[N]u8`, a view of them
    /// for a `str` or a `[]u8`.
    fn text_rvalue(&mut self, bytes: &[u8], ty: &Ty, span: Option<FileSpan>) -> Rvalue {
        if let Ty::Array { .. } = ty {
            let lty = self.cx.lir(ty);
            let key = format!("array:{}", crate::parser::ast::bytes_repr(bytes));
            let g = self
                .cx
                .data_global("bytes", lty, Constant::Bytes(bytes.to_vec()), key);
            return Rvalue::Use(Operand::Copy(Place::global(g)));
        }
        let _ = span;
        let g = self.cx.bytes_global(bytes);
        let lty = self.cx.lir(ty);
        let kind = self.struct_kind(&lty);
        Rvalue::Aggregate {
            kind,
            fields: vec![
                Operand::Const(Constant::Global(g)),
                Operand::int(bytes.len() as i128),
            ],
        }
    }

    /// A text value as an operand — the `{ ptr, len }` a `str` is.
    fn text_operand(&mut self, bytes: &[u8], span: Option<FileSpan>) -> Operand {
        let ty = Ty::Slice {
            mutable: false,
            inner: Box::new(Ty::u8()),
        };
        let v = self.text_rvalue(bytes, &ty, span);
        match v {
            Rvalue::Use(o) => o,
            other => self.into_temp(other, ty, span),
        }
    }

    /// Which member of `ty` is called `name`.
    fn field_index(&self, ty: &Ty, name: &Symbol) -> Option<u32> {
        self.member_names(ty)
            .iter()
            .position(|m| m == name)
            .map(|i| i as u32)
    }

    /// The member names of an aggregate, in declaration order.
    fn member_names(&self, ty: &Ty) -> Vec<Symbol> {
        match ty {
            Ty::Ptr { inner, .. } => self.member_names(inner),
            Ty::Tuple(elems) => (0..elems.len())
                .map(|i| Symbol::new(&i.to_string()))
                .collect(),
            Ty::Slice { .. } => vec![Symbol::new("ptr"), Symbol::new("len")],
            // Already in the sorted order the layout used.
            Ty::Struct(fields) => fields.iter().map(|(n, _)| n.clone()).collect(),
            Ty::Nominal { def, .. } => match self.cx.linked.ty(*def).map(|t| &t.kind) {
                Some(TypeDefKind::Struct { members }) => {
                    members.iter().map(|m| m.name.clone()).collect()
                }
                Some(TypeDefKind::Enum { .. }) => {
                    vec![Symbol::new("tag"), Symbol::new("payload")]
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// The enum a variant name belongs to, and which variant it is.
    fn variant_index(&self, ty: &Ty, name: &Symbol) -> Option<(DefId, u32)> {
        let Ty::Nominal { def, .. } = ty else {
            return None;
        };
        let t = self.cx.linked.ty(*def)?;
        let TypeDefKind::Enum { variants } = &t.kind else {
            return None;
        };
        variants
            .iter()
            .position(|v| &v.name == name)
            .map(|i| (*def, i as u32))
    }

    // ===< Control flow >===

    fn lower_if(
        &mut self,
        e: &Expr,
        cond: &Expr,
        then: &ir::Block,
        els: Option<&ir::Block>,
    ) -> Operand {
        let span = self.cx.meta.span(e.id);
        let ty = self.cx.ty_of(e.id);
        let diverges = matches!(ty, Ty::Never);
        let slot = yields_value(&ty).then(|| self.temp(ty, span));
        let c = self.eval(cond);
        let then_b = self.new_block(Some("then".to_string()));
        let else_b = self.new_block(Some("else".to_string()));
        let join = self.new_block(Some("join".to_string()));
        self.terminate(Terminator::new(
            TermKind::Switch {
                value: c,
                ty: LirTy::Bool,
                arms: vec![(1, then_b)],
                otherwise: else_b,
            },
            span,
        ));

        self.at = then_b;
        let v = self.block_value(then);
        self.store_result(slot, v, span);
        self.goto(join, span);

        self.at = else_b;
        if let Some(b) = els {
            let v = self.block_value(b);
            self.store_result(slot, v, span);
        }
        self.goto(join, span);

        self.at = join;
        self.seal_if_never(diverges, span);
        match slot {
            Some(s) => Operand::local(s),
            None => Operand::Const(Constant::Undef),
        }
    }

    fn store_result(&mut self, slot: Option<LocalId>, v: Option<Operand>, span: Option<FileSpan>) {
        if let (Some(s), Some(v)) = (slot, v) {
            self.assign(Place::local(s), Rvalue::Use(v), span);
        }
    }

    fn lower_loop(&mut self, e: &Expr, body: &ir::Block) -> Operand {
        let span = self.cx.meta.span(e.id);
        let ty = self.cx.ty_of(e.id);
        let diverges = matches!(ty, Ty::Never);
        let slot = yields_value(&ty).then(|| self.temp(ty, span));
        let head = self.new_block(Some("loop".to_string()));
        let exit = self.new_block(Some("loop exit".to_string()));
        self.goto(head, span);
        self.loops.push(LoopCtx {
            break_to: exit,
            continue_to: head,
            result: slot,
            floor: self.scopes.len(),
        });
        self.at = head;
        self.block_value(body);
        // The back edge. A `loop` exits only through a `break`, so falling off
        // the end of the body is a jump to the top and nothing else.
        self.goto(head, span);
        self.loops.pop();
        self.at = exit;
        self.seal_if_never(diverges, span);
        match slot {
            Some(s) => Operand::local(s),
            None => Operand::Const(Constant::Undef),
        }
    }

    /// End the block the walk has just arrived in when the expression it came
    /// from was `never`.
    ///
    /// A `loop` with no `break`, an `if` whose arms both return, a `match`
    /// whose every arm diverges: each leaves the walk positioned in a block
    /// nothing jumps to. Saying so here is what keeps a dead `return` of an
    /// undefined value out of the output — and it is the same thing the `never`
    /// *call* above does, one statement earlier.
    fn seal_if_never(&mut self, diverges: bool, span: Option<FileSpan>) {
        if diverges {
            self.terminate(Terminator::new(TermKind::Unreachable, span));
        }
    }

    // ===< Match, lowered to a decision tree (§4) >===

    /// Lower a `match` into tests and edges.
    ///
    /// The **discriminant is read once**, which is §4's stated requirement and
    /// what makes this a tree rather than a chain of independent comparisons:
    /// two arms that both match `.circle` test one already-loaded number.
    ///
    /// Arms are grouped by the variant they test whenever the scrutinee is an
    /// enum and every arm's top-level pattern is a variant, a binding or `_`. A
    /// catch-all arm joins **every** group, because it matches every variant —
    /// which is also why one with a guard sends the whole match down the linear
    /// route: its failure would have to fall through to a different next arm in
    /// each group, and a test that means different things in different places is
    /// not one test.
    ///
    /// Exhaustiveness was decided earlier, on the IR, so the final
    /// `unreachable` is a guarantee rather than a hope.
    fn lower_match(&mut self, e: &Expr, scrutinee: &Expr, arms: &[ir::Arm]) -> Operand {
        let span = self.cx.meta.span(e.id);
        let ty = self.cx.ty_of(e.id);
        let sty = self.cx.ty_of(scrutinee.id);
        let diverges = matches!(ty, Ty::Never);
        let slot = yields_value(&ty).then(|| self.temp(ty, span));
        let Some(place) = self.place_of(scrutinee) else {
            return Operand::Const(Constant::Undef);
        };
        let join = self.new_block(Some("join".to_string()));

        if self.groupable(&sty, arms) {
            self.match_grouped(&place, &sty, arms, slot, join, span);
        } else {
            self.match_linear(&place, &sty, arms, slot, join, span);
        }
        self.at = join;
        self.seal_if_never(diverges, span);
        match slot {
            Some(s) => Operand::local(s),
            None => Operand::Const(Constant::Undef),
        }
    }

    /// Whether the grouped form applies — see [`Lowerer::lower_match`].
    fn groupable(&self, sty: &Ty, arms: &[ir::Arm]) -> bool {
        let Ty::Nominal { def, .. } = sty else {
            return false;
        };
        if !matches!(
            self.cx.linked.ty(*def).map(|t| &t.kind),
            Some(TypeDefKind::Enum { .. })
        ) {
            return false;
        }
        arms.iter().all(|a| match &a.pattern.kind {
            PatternKind::Variant { .. } => true,
            PatternKind::Wildcard | PatternKind::Binding { .. } => a.guard.is_none(),
            _ => false,
        })
    }

    fn match_grouped(
        &mut self,
        place: &Place,
        sty: &Ty,
        arms: &[ir::Arm],
        slot: Option<LocalId>,
        join: BlockId,
        span: Option<FileSpan>,
    ) {
        let Ty::Nominal { def, .. } = sty else { return };
        let variants: Vec<Symbol> = match self.cx.linked.ty(*def).map(|t| &t.kind) {
            Some(TypeDefKind::Enum { variants }) => {
                variants.iter().map(|v| v.name.clone()).collect()
            }
            _ => return,
        };
        // One body block per arm, entered from every group that can reach it.
        let bodies: Vec<BlockId> = arms
            .iter()
            .map(|a| {
                let label = self.pattern_label(&a.pattern);
                self.new_block(Some(format!("arm {label}")))
            })
            .collect();

        // Read once. This is the invariant §4 names, and it is why the groups
        // are built around a single switch rather than around per-arm tests.
        let disc = self.tag_of(place, sty, span);
        let switch_at = self.at;
        let default = self.new_block(Some("no variant matched".to_string()));
        // One block for "nothing matched", shared by every group. Exhaustiveness
        // was decided on the IR, so it is a guarantee rather than a hope — and
        // one block saying so reads better than one per variant saying it again.
        let dead = self.new_block(Some("unreachable".to_string()));

        let mut targets = Vec::new();
        for (i, v) in variants.iter().enumerate() {
            let head = self.new_block(Some(format!(".{v}")));
            targets.push((i as i128, head));
            let candidates: Vec<usize> = arms
                .iter()
                .enumerate()
                .filter(|(_, a)| match &a.pattern.kind {
                    PatternKind::Variant { name, .. } => name == v,
                    _ => true,
                })
                .map(|(j, _)| j)
                .collect();
            self.at = head;
            self.chain(place, sty, arms, &candidates, &bodies, Some(i as u32), dead, span);
        }
        self.at = switch_at;
        let tag_ty = self.tag_ty(sty);
        self.terminate(Terminator::new(
            TermKind::Switch {
                value: disc,
                ty: tag_ty,
                arms: targets,
                otherwise: default,
            },
            span,
        ));

        // A discriminant no variant claims cannot happen, but a catch-all arm
        // still has to be somewhere the graph can point at.
        self.at = default;
        let catch_all: Vec<usize> = arms
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                matches!(
                    a.pattern.kind,
                    PatternKind::Wildcard | PatternKind::Binding { .. }
                )
            })
            .map(|(j, _)| j)
            .collect();
        self.chain(place, sty, arms, &catch_all, &bodies, None, dead, span);

        self.emit_bodies(arms, &bodies, slot, join, span);
    }

    /// Try each candidate arm in order: test what its pattern still has to test,
    /// then its guard, then jump to its body.
    #[allow(clippy::too_many_arguments)]
    fn chain(
        &mut self,
        place: &Place,
        sty: &Ty,
        arms: &[ir::Arm],
        candidates: &[usize],
        bodies: &[BlockId],
        // The variant the switch above already selected, when there was one.
        // Its arms must **not** test the discriminant again: reading it once is
        // the invariant §4 states, and a second read is also a second block for
        // a question with a known answer.
        selected: Option<u32>,
        fail: BlockId,
        span: Option<FileSpan>,
    ) {
        for (n, &i) in candidates.iter().enumerate() {
            let next = if n + 1 < candidates.len() {
                self.new_block(None)
            } else {
                fail
            };
            match (selected, &arms[i].pattern.kind) {
                (Some(index), PatternKind::Variant { name, sub }) => {
                    self.test_payload(place, sty, index, name, sub, next)
                }
                _ => self.test_pattern(place, &arms[i].pattern, sty, next),
            }
            if let Some(g) = &arms[i].guard {
                // A failed guard falls through to the **next arm**, not to the
                // next test — which is why the candidates are a chain rather
                // than a decision on the pattern alone (§4).
                let c = self.eval(g);
                self.branch_if(c, next, span);
            }
            self.goto(bodies[i], span);
            self.at = next;
        }
    }

    fn match_linear(
        &mut self,
        place: &Place,
        sty: &Ty,
        arms: &[ir::Arm],
        slot: Option<LocalId>,
        join: BlockId,
        span: Option<FileSpan>,
    ) {
        let bodies: Vec<BlockId> = arms
            .iter()
            .map(|a| {
                let label = self.pattern_label(&a.pattern);
                self.new_block(Some(format!("arm {label}")))
            })
            .collect();
        let all: Vec<usize> = (0..arms.len()).collect();
        let dead = self.new_block(Some("unreachable".to_string()));
        self.chain(place, sty, arms, &all, &bodies, None, dead, span);
        self.emit_bodies(arms, &bodies, slot, join, span);
    }

    fn emit_bodies(
        &mut self,
        arms: &[ir::Arm],
        bodies: &[BlockId],
        slot: Option<LocalId>,
        join: BlockId,
        span: Option<FileSpan>,
    ) {
        for (i, a) in arms.iter().enumerate() {
            self.at = bodies[i];
            let v = self.eval(&a.body);
            self.store_result(slot, Some(v), span);
            self.goto(join, span);
        }
    }

    /// Emit whatever `pattern` still has to test about `place`, binding as it
    /// goes; on any failure, jump to `fail`.
    ///
    /// `ty` is the type of the value at `place`, threaded down for the reason
    /// [`Lowerer::bind_irrefutable`] gives: a pattern is matched *against* a
    /// type, and a variant payload element's own node carries none.
    fn test_pattern(&mut self, place: &Place, pattern: &Pattern, ty: &Ty, fail: BlockId) {
        let span = self.cx.meta.span(pattern.id);
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding { .. } => self.bind_irrefutable(pattern, place, ty),
            PatternKind::At {
                binding,
                pattern: inner,
            } => {
                if is_void(ty) {
                    self.test_pattern(place, inner, ty, fail);
                    return;
                }
                let id = self.new_local(Some(binding.name.clone()), ty, span);
                self.local_of.insert(binding.def, id);
                self.assign(
                    Place::local(id),
                    Rvalue::Use(Operand::Copy(place.clone())),
                    span,
                );
                self.test_pattern(place, inner, ty, fail);
            }
            PatternKind::Lit(l) => {
                // A **text** literal is not a scalar comparison. A `str` is
                // `{ ptr, len }` by the time it gets here (§7b), so `==` on it
                // would compare two addresses — which is not what
                // `s.match { "hi" => ... }` asked and is not an instruction any
                // machine has either. Its bytes are what the pattern is about.
                if let Some(bytes) = text_of(l) {
                    self.test_bytes(place, ty, &bytes, fail, span);
                    return;
                }
                let at = self.cx.lir(ty);
                let rhs = self.const_operand(&lit_value(l), ty, span);
                let c = self.into_temp(
                    Rvalue::Op {
                        op: Op::Eq,
                        ty: at,
                        args: vec![Operand::Copy(place.clone()), rhs],
                    },
                    Ty::Bool,
                    span,
                );
                self.branch_if(c, fail, span);
            }
            PatternKind::Range {
                start,
                end,
                inclusive,
            } => {
                let at = self.cx.lir(ty);
                if let Some(s) = start {
                    let rhs = self.const_operand(&lit_value(s), ty, span);
                    let c = self.into_temp(
                        Rvalue::Op {
                            op: Op::Ge,
                            ty: at.clone(),
                            args: vec![Operand::Copy(place.clone()), rhs],
                        },
                        Ty::Bool,
                        span,
                    );
                    self.branch_if(c, fail, span);
                }
                if let Some(e) = end {
                    let op = if *inclusive { Op::Le } else { Op::Lt };
                    let rhs = self.const_operand(&lit_value(e), ty, span);
                    let c = self.into_temp(
                        Rvalue::Op {
                            op,
                            ty: at,
                            args: vec![Operand::Copy(place.clone()), rhs],
                        },
                        Ty::Bool,
                        span,
                    );
                    self.branch_if(c, fail, span);
                }
            }
            PatternKind::Variant { name, sub } => {
                let Some((_, index)) = self.variant_index(ty, name) else {
                    return;
                };
                let disc = self.tag_of(place, ty, span);
                let ok = self.new_block(None);
                let tag_ty = self.tag_ty(ty);
                self.terminate(Terminator::new(
                    TermKind::Switch {
                        value: disc,
                        ty: tag_ty,
                        arms: vec![(index as i128, ok)],
                        otherwise: fail,
                    },
                    span,
                ));
                self.at = ok;
                let payload = self.variant_place(place, ty, index);
                let tys = self.variant_tys(ty, index);
                for (i, p) in sub.iter().enumerate() {
                    let name = self.variant_member(ty, index, i);
                    let f = payload.clone().then(Projection::Field {
                        index: i as u32,
                        name,
                    });
                    let t = tys.get(i).cloned().unwrap_or(Ty::Error);
                    self.test_pattern(&f, p, &t, fail);
                }
            }
            PatternKind::Tuple(ps) | PatternKind::TupleStruct { elems: ps, .. } => {
                let tys = self.member_tys(ty);
                for (i, p) in ps.iter().enumerate() {
                    let f = place.clone().then(Projection::Field {
                        index: i as u32,
                        name: Symbol::new(&i.to_string()),
                    });
                    let t = tys.get(i).cloned().unwrap_or(Ty::Error);
                    self.test_pattern(&f, p, &t, fail);
                }
            }
            PatternKind::Struct { fields, .. } => {
                let tys = self.member_tys(ty);
                for (name, p) in fields {
                    let Some(index) = self.field_index(ty, name) else {
                        continue;
                    };
                    let f = place.clone().then(Projection::Field {
                        index,
                        name: name.clone(),
                    });
                    let t = tys.get(index as usize).cloned().unwrap_or(Ty::Error);
                    self.test_pattern(&f, p, &t, fail);
                }
            }
            PatternKind::Or(ps) => {
                let done = self.new_block(None);
                let mut alts: Vec<BlockId> = ps.iter().map(|_| self.new_block(None)).collect();
                alts.push(fail);
                self.goto(alts[0], span);
                for (i, p) in ps.iter().enumerate() {
                    self.at = alts[i];
                    self.test_pattern(place, p, ty, alts[i + 1]);
                    self.goto(done, span);
                }
                self.at = done;
            }
            PatternKind::Deref(inner) => {
                let f = place.clone().then(Projection::Deref);
                let t = match ty {
                    Ty::Ptr { inner, .. } => (**inner).clone(),
                    other => other.clone(),
                };
                self.test_pattern(&f, inner, &t, fail);
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => self.test_slice(place, ty, prefix, rest, suffix, fail, span),
        }
    }

    /// What a variant pattern still has to test once the switch has already
    /// chosen the variant: its payload, and nothing else.
    fn test_payload(
        &mut self,
        place: &Place,
        sty: &Ty,
        index: u32,
        name: &Symbol,
        sub: &[Pattern],
        fail: BlockId,
    ) {
        let _ = name;
        let payload = self.variant_place(place, sty, index);
        let tys = self.variant_tys(sty, index);
        for (i, p) in sub.iter().enumerate() {
            let name = self.variant_member(sty, index, i);
            let f = payload.clone().then(Projection::Field {
                index: i as u32,
                name,
            });
            let t = tys.get(i).cloned().unwrap_or(Ty::Error);
            self.test_pattern(&f, p, &t, fail);
        }
    }

    /// A text literal pattern: a call to `core`'s byte equality.
    ///
    /// It is a **call**, not a comparison, for the reason the panic is one
    /// (§2): the answer belongs to a library. A `str` is `{ ptr, len }` by this
    /// level (§7b), so `==` on it would compare the address a view points at
    /// rather than the text it spells — two copies of `"hi"` in different
    /// buffers would not match, which is not what the pattern asked. Unrolling
    /// the bytes here instead would put a second definition of "are these bytes
    /// equal" in the compiler, free to disagree with the one `impl Eq for str`
    /// uses.
    ///
    /// The function is found by `#lang("bytes_eq")`, so `s == "hi"` and
    /// `s.match { "hi" => ... }` reach the same code.
    fn test_bytes(
        &mut self,
        place: &Place,
        ty: &Ty,
        bytes: &[u8],
        fail: BlockId,
        span: Option<FileSpan>,
    ) {
        // A `str` is a `distinct []u8` (§2.4) and a distinct is a one-member
        // struct here, so the slice is one projection in.
        let seq = self.byte_slice(place, ty);
        let callee = self.cx.lang.get("bytes_eq").map(|d| self.static_callee(d));
        let (Some(seq), Some(callee)) = (seq, callee) else {
            // Not a shape whose bytes this can reach, or a `core` with no
            // comparison in it — inference has said so already. Fail the arm
            // rather than emit a comparison that means the wrong thing.
            self.goto(fail, span);
            return;
        };
        let literal = self.text_operand(bytes, span);
        let args = vec![Operand::Copy(seq), literal];
        let Some(Rvalue::Use(same)) = self.emit_call(callee, args, Ty::Bool, span) else {
            return;
        };
        self.branch_if(same, fail, span);
    }

    /// The `{ ptr, len }` a text value's bytes live behind. `None` for anything
    /// else.
    fn byte_slice(&mut self, place: &Place, ty: &Ty) -> Option<Place> {
        match ty {
            // `str` is a `distinct []u8`, and a `distinct` is its
            // representation by this level (§9) — so there is no wrapper to
            // read through and this is the only shape text arrives in.
            Ty::Slice { .. } => Some(place.clone()),
            _ => None,
        }
    }

    /// A slice pattern: a length test, then the elements it names from each end.
    #[allow(clippy::too_many_arguments)]
    /// The place element `i` of a sequence lives at.
    ///
    /// An array kept its own shape (§7b), so the element is an ordinary
    /// projection. A slice did not: it is `{ ptr, len }`, and a struct has
    /// members rather than elements — so the element is reached through the
    /// pointer it holds, moved along by `i` and dereferenced. That is the same
    /// pointer arithmetic `a[i]` does, and doing anything else here would put
    /// an `Index` on a slice, which [`Projection::Index`] says never happens.
    fn element_place(
        &mut self,
        place: &Place,
        ty: &Ty,
        index: Operand,
        span: Option<FileSpan>,
    ) -> Place {
        let Ty::Slice { inner, mutable } = ty else {
            return place.clone().then(Projection::Index(index));
        };
        let ptr = place.clone().then(Projection::Field {
            index: 0,
            name: Symbol::new("ptr"),
        });
        let at = self.temp(
            Ty::Ptr {
                mutable: *mutable,
                inner: inner.clone(),
            },
            span,
        );
        self.assign(
            Place::local(at),
            Rvalue::Offset {
                ptr: Operand::Copy(ptr),
                index,
                stride: self.cx.stride(inner),
            },
            span,
        );
        Place::local(at).then(Projection::Deref)
    }

    fn test_slice(
        &mut self,
        place: &Place,
        ty: &Ty,
        prefix: &[Pattern],
        rest: &Option<Option<ir::Binding>>,
        suffix: &[Pattern],
        fail: BlockId,
        span: Option<FileSpan>,
    ) {
        let want = (prefix.len() + suffix.len()) as i128;
        let elem = match ty {
            Ty::Array { inner, .. } | Ty::Slice { inner, .. } => (**inner).clone(),
            _ => Ty::Error,
        };
        let len = self.sequence_len(place, ty);
        // Without a `..` the length has to be exactly what was written; with one,
        // at least that many.
        let op = if rest.is_some() { Op::Ge } else { Op::Eq };
        let usize_ty = self.cx.usize_ty();
        let usize_lir = self.cx.lir(&usize_ty);
        let c = self.into_temp(
            Rvalue::Op {
                op,
                ty: usize_lir.clone(),
                args: vec![len.clone(), Operand::int(want)],
            },
            Ty::Bool,
            span,
        );
        self.branch_if(c, fail, span);

        for (i, p) in prefix.iter().enumerate() {
            let at = Operand::int(i as i128);
            let f = self.element_place(place, ty, at, span);
            self.test_pattern(&f, p, &elem, fail);
        }
        // A suffix element is counted from the end, which is a *computed* index:
        // `len - k`. That is the run-time form, and it is why an array keeps its
        // own shape (§7b) rather than becoming a struct of members.
        for (k, p) in suffix.iter().enumerate() {
            let back = (suffix.len() - k) as i128;
            let idx = self.into_temp(
                Rvalue::Op {
                    op: Op::Sub,
                    ty: usize_lir.clone(),
                    args: vec![len.clone(), Operand::int(back)],
                },
                usize_ty.clone(),
                span,
            );
            let f = self.element_place(place, ty, idx, span);
            self.test_pattern(&f, p, &elem, fail);
        }
        // The `..` segment, when it was named: the elements between the two
        // ends, as a slice over the address of the first of them.
        if let Some(Some(b)) = rest {
            let bound = self.cx.ty_of(b.id);
            let id = self.new_local(Some(b.name.clone()), &bound, span);
            self.local_of.insert(b.def, id);
            let at = Operand::int(prefix.len() as i128);
            let first = self.element_place(place, ty, at, span);
            let ptr = self.into_temp(
                Rvalue::Ref(first),
                Ty::Ptr {
                    mutable: false,
                    inner: Box::new(elem.clone()),
                },
                span,
            );
            let n = self.into_temp(
                Rvalue::Op {
                    op: Op::Sub,
                    ty: usize_lir,
                    args: vec![len, Operand::int(want)],
                },
                usize_ty,
                span,
            );
            let bound = self.local_tys[id.0 as usize].clone();
            let lty = self.cx.lir(&bound);
            let kind = self.struct_kind(&lty);
            self.assign(
                Place::local(id),
                Rvalue::Aggregate {
                    kind,
                    fields: vec![ptr, n],
                },
                span,
            );
        }
    }

    /// How many elements a sequence has: a constant for an array, the header's
    /// second member for a slice.
    fn sequence_len(&mut self, place: &Place, ty: &Ty) -> Operand {
        match ty {
            Ty::Array { len, .. } => match len.value() {
                Some(n) => Operand::int(n as i128),
                None => Operand::Const(Constant::Undef),
            },
            _ => Operand::Copy(place.clone().then(Projection::Field {
                index: 1,
                name: Symbol::new("len"),
            })),
        }
    }

    /// A short name for the arm a block belongs to, for the dump.
    fn pattern_label(&self, p: &Pattern) -> String {
        match &p.kind {
            PatternKind::Wildcard => "_".to_string(),
            PatternKind::Binding { name, .. } => name.to_string(),
            PatternKind::Variant { name, .. } => format!(".{name}"),
            PatternKind::Lit(l) => lit_value(l).display(),
            _ => "pattern".to_string(),
        }
    }
}

/// The bytes a text literal is, or `None` for a scalar one.
///
/// A `str` literal and a `b"..."` literal are the same question at this level:
/// what the pattern tests is a run of bytes.
fn text_of(l: &Lit) -> Option<Vec<u8>> {
    match l {
        Lit::Str(s) => Some(s.as_bytes().to_vec()),
        Lit::Bytes(b) => Some(b.clone()),
        _ => None,
    }
}

/// A literal, as a constant value. The two are the same thing written twice —
/// the IR's literal carries arbitrary precision, and so does this.
fn lit_value(l: &Lit) -> ConstValue {
    match l {
        Lit::Int(n) => ConstValue::Int(n.clone()),
        Lit::Float(f) => ConstValue::Float(*f),
        Lit::Bool(b) => ConstValue::Bool(*b),
        Lit::Char(c) => ConstValue::Char(*c),
        Lit::Str(s) => ConstValue::Str(s.clone()),
        Lit::Bytes(b) => ConstValue::Bytes(b.clone()),
    }
}

fn exit_label(e: Exit) -> &'static str {
    match e {
        Exit::Return => "return",
        Exit::Break(_) => "break",
        Exit::Continue(_) => "continue",
    }
}

/// `symbol`, or the first spelling of it nothing has taken.
///
/// A `#static` written inside a function body has no path to mangle — two
/// functions may each declare `n`, and `mono::global_symbol` gives both the same
/// name. Two definitions of one symbol is what a linker refuses, so the second
/// one and every one after it gets a `Z<k>` suffix, which the mangling scheme
/// does not otherwise use. It is stable for a given program because this walk is:
/// the globals are visited in the order the linked program holds them.
fn unique(taken: &mut std::collections::HashSet<Symbol>, symbol: Symbol) -> Symbol {
    if taken.insert(symbol.clone()) {
        return symbol;
    }
    for k in 1.. {
        let candidate = Symbol::new(&format!("{symbol}Z{k}"));
        if taken.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!()
}

/// Whether a value of this type holds nothing at all.
///
/// `void` is a type the *language* has — it is what a function with no result
/// returns and what `Residual :: void` makes an `Option`'s short-circuit carry —
/// and it is not a type a *machine* has: there is no register, no slot and no
/// argument for it. So it is erased wherever a value would be held, which is
/// what keeps `let _7: void` out of a frame.
/// FNV-1a over 128 bits.
///
/// A hash and not a cryptographic one: the input is a type key the compiler
/// generated, not anything an attacker chose, and what is wanted is that two
/// different types differ — which 128 bits of any decent mixing gives with a
/// margin nobody has to argue about. The constants are the published ones.
fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u128;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Which `core` `Kind` variant a type is.
///
/// The names are `reflect.nest`'s, in the one place the compiler has to know
/// them — a variant is chosen by name here exactly as a `Location`'s members
/// are filled positionally there. `Other` is the tail rather than a panic: a
/// type this vocabulary does not name should be described as unknown, not
/// described wrongly.
fn kind_name(cx: &Cx<'_>, ty: &Ty) -> &'static str {
    match cx.strip(ty) {
        Ty::Void => "Void",
        Ty::Never => "Never",
        Ty::Bool => "Bool",
        Ty::Char => "Char",
        Ty::Int { signed: true, .. } => "Int",
        Ty::Int { signed: false, .. } => "Uint",
        Ty::Float(_) => "Float",
        Ty::Ptr { .. } => "Ptr",
        Ty::Slice { .. } => "Slice",
        Ty::Array { .. } => "Array",
        Ty::Tuple(_) => "Tuple",
        Ty::Func { .. } => "Func",
        Ty::Dyn { .. } => "Dyn",
        Ty::Nominal { def, .. } => match cx.linked.ty(def).map(|t| &t.kind) {
            Some(TypeDefKind::Enum { .. }) => "Enum",
            Some(_) => "Struct",
            None => "Other",
        },
        _ => "Other",
    }
}

fn is_void(ty: &Ty) -> bool {
    matches!(ty, Ty::Void)
}

/// Whether an expression of this type leaves a value behind to keep.
///
/// Two types do not, and they are not the same "no". A `void` expression
/// finishes and yields nothing; a `never` one does not finish — every path
/// through it returns, breaks or traps. Both mean there is no slot to allocate,
/// and the second means the block after it is unreachable, which is why the
/// callers of this terminate as well (§1: a local of type `never` is a slot no
/// machine has).
fn yields_value(ty: &Ty) -> bool {
    !matches!(ty, Ty::Void | Ty::Never)
}

/// A float's width in bits. `f80` is ten bytes of data in a sixteen-byte slot
/// (see the layout engine); the *type* is still eighty bits wide.
fn float_bits(w: crate::sema::ty::FloatWidth) -> u16 {
    use crate::sema::ty::FloatWidth::*;
    match w {
        F16 => 16,
        F32 => 32,
        F64 => 64,
        F80 => 80,
        F128 => 128,
    }
}

/// A source binary operator, as an opcode. `&&` and `||` never arrive: they
/// are control flow (§1).
fn op_of_bin(op: BinOp) -> Op {
    match op {
        BinOp::Add => Op::Add,
        BinOp::Sub => Op::Sub,
        BinOp::Mul => Op::Mul,
        BinOp::Div => Op::Div,
        BinOp::Rem => Op::Rem,
        BinOp::BitAnd => Op::BitAnd,
        BinOp::BitOr | BinOp::Or => Op::BitOr,
        BinOp::BitXor => Op::BitXor,
        BinOp::Shl => Op::Shl,
        BinOp::Shr => Op::Shr,
        BinOp::Eq => Op::Eq,
        BinOp::Ne => Op::Ne,
        BinOp::Lt => Op::Lt,
        BinOp::Le => Op::Le,
        BinOp::Gt => Op::Gt,
        BinOp::Ge => Op::Ge,
        BinOp::And => Op::BitAnd,
    }
}

/// A source unary operator, as an opcode. `&` and `&mut` are not operations —
/// they are [`Rvalue::Ref`] — and never arrive here.
fn op_of_un(op: UnOp) -> Op {
    match op {
        UnOp::Neg => Op::Neg,
        UnOp::Not => Op::Not,
        UnOp::BitNot => Op::BitNot,
        UnOp::Ref | UnOp::RefMut => Op::Not,
    }
}

/// A builtin operator, as an opcode, in the form the build asked for: checked
/// arithmetic is its own instruction rather than a flag, because a flag that
/// changes the result type is not a flag (§7d).
fn op_of_builtin(op: BuiltinOp, checked: bool) -> Op {
    match (op, checked) {
        (BuiltinOp::Add, false) => Op::Add,
        (BuiltinOp::Add, true) => Op::AddChecked,
        (BuiltinOp::Sub, false) => Op::Sub,
        (BuiltinOp::Sub, true) => Op::SubChecked,
        (BuiltinOp::Mul, false) => Op::Mul,
        (BuiltinOp::Mul, true) => Op::MulChecked,
        (BuiltinOp::Div, _) => Op::Div,
        (BuiltinOp::Rem, _) => Op::Rem,
        (BuiltinOp::BitAnd, _) => Op::BitAnd,
        (BuiltinOp::BitOr, _) => Op::BitOr,
        (BuiltinOp::BitXor, _) => Op::BitXor,
        (BuiltinOp::Shl, _) => Op::Shl,
        (BuiltinOp::Shr, _) => Op::Shr,
        (BuiltinOp::Neg, _) => Op::Neg,
        (BuiltinOp::BitNot, _) => Op::BitNot,
    }
}
