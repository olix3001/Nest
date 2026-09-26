//! AST → IR lowering — the stage that turns the resolved, desugared, typed AST
//! into the small typed [`ir`](crate::ir) tree.
//!
//! It runs after [`super::infer`], so every expression node already carries a
//! [`Ty`] in the arena metadata; lowering reads that back rather than
//! recomputing anything. What it *does* is make structure explicit:
//!
//! - `while cond { body }` → `loop { if !cond { break }; body }`.
//! - `if match p := v { .. }` → `match v { p => then, _ => els }`.
//! - auto-deref: a field/index access whose base is a pointer gets an explicit
//!   [`ir::Expr::Deref`].
//! - `defer`: the body stays where it was written, as an
//!   [`ir::StmtKind::Defer`], rather than being copied to every exit. Running
//!   the ones already registered in reverse on each way out — and unwinding the
//!   enclosing blocks' on a `return` — is left to the CFG stage, which emits one
//!   epilogue per scope instead of one per exit path.
//!
//! - implicit coercions become explicit: an `@using` upcast turns into the field
//!   access it stands for (`e.t`, or `&e.t` through a pointer), and a `*T` →
//!   `*dyn Trait` unsizing into an [`ir::Expr::DynCast`] that keeps the erased
//!   pointee for vtable selection.
//! - a **method call** becomes an ordinary [`ir::Expr::Call`] whose `args[0]` is
//!   the receiver: whatever the `self` parameter wanted — a value, a `*Self`, a
//!   `*mut Self` — is spelled out as an `&` / `&mut` / `.*` here, using the
//!   adjustment [`super::infer`] recorded, and the call carries the
//!   [`ir::Dispatch`] that says whether the callee is a known function, a vtable
//!   slot, or a bound awaiting monomorphization.
//! - an **operator** becomes the same kind of call to the method its `#lang`
//!   trait resolved to (§6.13) — including `a[i]`, which on a user type is
//!   `Index.index(&a, i).*`, and `a < b`, which on a user type tests the
//!   `Ordering` that `Ord.cmp` returns.
//!
//! - a **closure** becomes the struct of what it captured, built where it is
//!   written, and a function of its own taking that struct first (§5.5).
//!
//! Not lowered here: `match` decision trees — arms stay structured, patterns
//! keep their full shape, and compiling them to tests is a later, CFG-level
//! pass.

use std::collections::HashMap;

use crate::common::source::{FileId, FileSpan};
use crate::common::symbol::Symbol;
use crate::parser::ast::{
    Ast, BinOp, CompositeBody, Lit, NodeId, NodeKind, RangeKind, UnOp, VariantArgs, VariantPatArgs,
    VariantPayload,
};

use super::decl::{DeclTable, Decls};
use super::def::{DefId, DefKind, DefTable, LangItems};
use super::infer::{
    ArgOrder, Coercion, DistinctRecv, DynCoerce, FuncCall, FuncCallMethod, Generics, Instantiation,
    MethodDispatch, MethodRes, RangeReported, RecvAdjust, SliceCoerce, Upcast,
};
use super::infer::{OpResolution, StaticTraitSelf};
use super::ty::Ty;
use super::{DefMeta, Resolution};
use crate::ir::{
    Arm, AssocConst, Binding, Block, DefaultValue, Dispatch, Expr, ExprKind, Function, Global,
    ImplicitCast, IrId, Member, Meta, Param, Pattern, PatternKind, Program, Recv, Stmt, StmtKind,
    TraitMethod, TypeDef, TypeDefKind, Variant,
};

/// What lowering a file produced.
pub struct Lowered {
    /// The file's IR.
    pub program: Program,
    /// Each function's **value**-parameter defaults, in declaration order, as
    /// lowering settled them — [`decl::record_defaults`] files them under the
    /// definition so a caller in another package can fill an omitted argument
    /// without the declaring file's tree.
    pub defaults: Vec<(DefId, Vec<Option<super::decl::ParamDefault>>)>,
}

/// Lower every function body in `file` to IR.
pub fn lower_file(
    defs: &DefTable,
    lang: &LangItems,
    asts: &HashMap<FileId, Ast>,
    decls: &DeclTable,
    meta: &Meta,
    sources: &crate::common::source::SourceMap,
    file: FileId,
) -> Lowered {
    let ast = &asts[&file];
    let mut lo = Lowerer {
        defs,
        decls,
        sources,
        lang,
        ast,
        asts,
        meta,
        defaults: HashMap::new(),
        recorded: Vec::new(),
        closures: Vec::new(),
        lifted: Vec::new(),
        lifted_types: Vec::new(),
    };
    // Iterate the `Func` defs of this file: each carries the name/DefId and its
    // node is the `ConstBind` whose RHS is the `FuncExpr`. A **bodyless** one is
    // emitted too, with `body: None` — an `extern("c") func` is a real symbol
    // the linker must resolve and a call to it is an ordinary call, so dropping
    // it here would lose the only record of its signature and ABI.
    let mut funcs = Vec::new();
    for def in defs.iter() {
        if def.kind != DefKind::Func || def.file != Some(file) {
            continue;
        }
        let Some(node) = def.node else { continue };
        let func = match &ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            NodeKind::FuncExpr { .. } => node,
            _ => continue,
        };
        if let Some(f) = lo.lower_function(def.id, func) {
            // A bodyless, non-`extern` function is a trait's *requirement* — a
            // signature an impl must satisfy, with no code and no symbol of its
            // own. It is reachable as a `Dispatch::Virtual` slot through its
            // def; emitting it here would put a body-less stub in the program
            // that nothing links.
            if f.body.is_some() || f.extern_abi.is_some() {
                funcs.push(f);
            }
        }
    }
    funcs.append(&mut lo.lifted);
    let mut types = lo.lower_types(file);
    types.append(&mut lo.lifted_types);
    let globals = lo.lower_globals(file);
    Lowered {
        program: Program {
            types,
            globals,
            funcs,
        },
        defaults: lo.recorded,
    }
}

struct Lowerer<'a> {
    defs: &'a DefTable,
    /// What every definition declares — see [`super::decl`].
    decls: &'a DeclTable,
    /// The program's source text, for the one construct that needs a line and a
    /// column rather than a byte span: `#caller_location`.
    sources: &'a crate::common::source::SourceMap,
    /// The `#lang` registry, consulted for the types an operator's desugaring
    /// mentions but the surface syntax never wrote — `Ordering`, for the `cmp`
    /// a user-type comparison lowers to.
    lang: &'a LangItems,
    /// The file currently being lowered. Swapped, briefly, while a **default
    /// argument** declared in another file is lowered — the default's nodes and
    /// their inferred types live in that file's arena, not this one.
    ast: &'a Ast,
    /// The whole parsed program, so a call can reach the declaration of a callee
    /// in another file to lower its defaults (a call into `core` is the common
    /// case).
    asts: &'a HashMap<FileId, Ast>,
    /// The IR's id allocator and side table. Every node built below is
    /// allocated through it and gets its span recorded in the same breath, so
    /// there is no path that produces a node with no source position.
    meta: &'a Meta,
    /// Lowered default arguments, per callee, in **value-parameter** order.
    ///
    /// Each default is lowered exactly once and cloned into the call sites that
    /// omit it, rather than re-lowered per site: the default is one expression
    /// written in one place, and anything later that walks it — the `#const`
    /// check above all — should see it once.
    defaults: HashMap<DefId, Vec<Option<Expr>>>,
    /// Where each of those lowered defaults went, per function — what
    /// [`Lowered::defaults`] carries out of here.
    recorded: Vec<(DefId, Vec<Option<super::decl::ParamDefault>>)>,
    /// The closures whose bodies are being lowered, innermost last (§5.5).
    closures: Vec<ClosureCx>,
    /// Each closure's body, lifted into a function of its own.
    lifted: Vec<Function>,
    /// Each closure's type: a struct of what it captured.
    lifted_types: Vec<TypeDef>,
}

/// A closure being lowered: how its body reaches what it captured.
struct ClosureCx {
    /// Its body's first parameter — the closure itself.
    this: DefId,
    /// That parameter's type, `*Closure`.
    this_ty: Ty,
    /// Every captured def the body may name: the member it is in, the member's
    /// type, and whether the member points at a shared local rather than being
    /// a copy.
    fields: HashMap<DefId, (Symbol, Ty, bool)>,
}

impl Lowerer<'_> {
    /// The declaration queries, over the tables this pass already holds.
    fn decls(&self) -> Decls<'_> {
        Decls::new(self.defs, self.asts, self.decls)
    }

    // ===< node construction >===
    //
    // Every IR node is built through one of these four, which is what makes the
    // span guarantee hold: an id is never allocated without recording where it
    // came from, so no later stage has to reconstruct a position — and a span
    // reconstructed at the end is a span that is wrong.

    /// Allocate an id for a node lowered out of `node`, recording its source
    /// position. The [`FileSpan`] comes off the AST node itself rather than
    /// from the file being lowered, which is what keeps a **default argument**
    /// honest: `param_default` swaps in the declaring file's arena, and the
    /// span has to follow it there.
    fn id(&self, node: NodeId) -> IrId {
        let id = self.meta.fresh();
        let n = self.ast.node(node);
        self.meta.set_span(id, FileSpan::new(n.file, n.span));
        id
    }

    fn expr(&self, node: NodeId, ty: Ty, kind: ExprKind) -> Expr {
        let id = self.id(node);
        self.meta.set_ty(id, ty);
        Expr { id, kind }
    }

    /// The type of an already-built expression.
    fn ty_of(&self, e: &Expr) -> Ty {
        self.meta.ty_or_error(e.id)
    }

    fn stmt(&self, node: NodeId, kind: StmtKind) -> Stmt {
        Stmt {
            id: self.id(node),
            kind,
        }
    }

    fn pat(&self, node: NodeId, kind: PatternKind) -> Pattern {
        Pattern {
            id: self.id(node),
            kind,
        }
    }

    /// Allocate an id for a node the source never wrote, borrowing the span of
    /// the expression it is derived from — the `.*` of an auto-deref, the `&` of
    /// a receiver adjustment, the `break` of a `while`'s guard. Pointing a
    /// synthetic node at its operand is what a reader stepping through the
    /// lowered code expects; leaving it span-less would blank the debug
    /// metadata for code that really does execute.
    fn derived(&self, from: IrId) -> IrId {
        let id = self.meta.fresh();
        if let Some(span) = self.meta.span(from) {
            self.meta.set_span(id, span);
        }
        id
    }

    /// [`Lowerer::derived`], packaged as a whole expression. Takes the source
    /// node's id rather than a borrow of it, so a caller can hand the node
    /// itself to `kind` in the same expression.
    fn derived_expr(&self, from: IrId, ty: Ty, kind: ExprKind) -> Expr {
        let id = self.derived(from);
        self.meta.set_ty(id, ty);
        Expr { id, kind }
    }

    // ===< type definitions >===

    /// Lower every type this file declares.
    ///
    /// A `Ty::Nominal` names a type without saying what is in it, so the IR
    /// carries the definitions themselves: layout, exhaustiveness and the LIR
    /// aggregate flattening all need the contents, and none of them should have
    /// to go back to the AST for them.
    ///
    /// Member types are read out of the arena rather than recomputed —
    /// `infer::stamp_member_types` resolved each one against its declaration,
    /// including aliases, `Self`, and generic parameters left as parameters.
    fn lower_types(&mut self, file: FileId) -> Vec<TypeDef> {
        let defs: Vec<DefId> = self
            .defs
            .iter()
            .filter(|d| {
                d.file == Some(file)
                    && matches!(
                        d.kind,
                        DefKind::Struct | DefKind::Enum | DefKind::TypeAlias | DefKind::Trait
                    )
            })
            .map(|d| d.id)
            .collect();
        defs.into_iter()
            .filter_map(|d| self.lower_type(d))
            .collect()
    }

    // ===< globals >===

    /// Lower every constant and static region this file declares (§2.6, §2.5).
    ///
    /// A **trait's** associated constants are excluded: they are declarations of
    /// what an impl must supply rather than definitions with a value, and they
    /// already ride on the trait's [`TypeDef`] as `AssocConst`s, in vtable
    /// order. An impl's are included — those *are* definitions, and nothing
    /// evaluated them before.
    fn lower_globals(&mut self, file: FileId) -> Vec<Global> {
        let defs: Vec<DefId> = self
            .defs
            .iter()
            .filter(|d| d.file == Some(file) && d.kind == DefKind::Const)
            .filter(|d| {
                d.parent
                    .map(|p| self.defs.get(p).kind != DefKind::Trait)
                    .unwrap_or(false)
            })
            // A `::` RHS holds either a value or a type, and `DefKind::Const`
            // cannot tell them apart on its own — `Iter :: SliceIter.<T>` is an
            // associated *type* that lands there too.
            .filter(|d| {
                d.node.is_some_and(|n| match &self.ast.node(n).kind {
                    NodeKind::ConstBind { rhs, .. } => {
                        super::is_value_rhs(self.defs, self.asts, self.ast, *rhs)
                    }
                    _ => false,
                })
            })
            .map(|d| d.id)
            .collect();
        defs.into_iter()
            .filter_map(|d| self.lower_global(d))
            .collect()
    }

    fn lower_global(&mut self, def: DefId) -> Option<Global> {
        let d = self.defs.get(def);
        let (name, mutable) = (d.name.clone(), d.mutable);
        let directives = d.directives.clone();
        let node = d.node?;
        let NodeKind::ConstBind { rhs, .. } = self.ast.node(node).kind.clone() else {
            return None;
        };
        // Two RHS shapes reach here. `A :: 5` is the value itself. `#static c ::
        // u32 := 0` and an impl's `MAX: i32 :: 100` write the *type* first and
        // the value after `:=` — and a static may write no value at all, in
        // which case the region is zeroed and there is nothing to lower.
        let init = match self.ast.node(rhs).kind.clone() {
            NodeKind::AssocConst { default, .. } => default,
            _ => Some(rhs),
        };
        let id = self.id(node);
        // The type inference stamped on the *binding*, not the initializer's:
        // `#static c: u32 :: 0` is a `u32` region however the literal on the
        // right would have defaulted on its own.
        self.meta.set_ty(id, self.ty(node));
        self.meta.set_directives(id, directives);
        // What a constant declared in a **generic impl** is generic over,
        // carried across the way a function's is: its initializer mentions the
        // impl's parameters, so it has no value until a use site says what they
        // are (see [`Generics`]).
        if let Some(g) = self.ast.meta::<Generics>(node) {
            self.meta.set(id, g);
        }
        let init = init.map(|e| self.lower_expr(e));
        Some(Global {
            id,
            def,
            name,
            init,
            mutable,
        })
    }

    fn lower_type(&mut self, def: DefId) -> Option<TypeDef> {
        let d = self.defs.get(def);
        let name = d.name.clone();
        let directives = d.directives.clone();
        let node = d.node?;
        // A type is bound by `Name :: <type expression>`; the definition is the
        // right-hand side.
        let rhs = match &self.ast.node(node).kind {
            NodeKind::ConstBind { rhs, .. } => *rhs,
            _ => node,
        };
        let (kind, generics) = match self.ast.node(rhs).kind.clone() {
            NodeKind::StructType { generics, .. } => (
                TypeDefKind::Struct {
                    members: self.struct_members(def),
                },
                generics,
            ),
            NodeKind::EnumType {
                variants, generics, ..
            } => (
                TypeDefKind::Enum {
                    variants: variants
                        .iter()
                        .filter_map(|&v| self.lower_variant(def, v))
                        .collect(),
                },
                generics,
            ),
            NodeKind::DistinctType { inner, .. } => (
                TypeDefKind::Distinct {
                    repr: self.member(inner, None, "0"),
                },
                Vec::new(),
            ),
            NodeKind::TraitType {
                members, generics, ..
            } => (self.lower_trait(def, &members), generics),
            // A plain type alias defines no new type: `A :: B` is another name
            // for `B`, and every use of it resolved to `B` long before now.
            _ => return None,
        };
        let id = self.id(rhs);
        // A type's own type is the nominal it names, over **its own** parameters
        // — the `Ty` a bare use of the name has inside its own definition. Not
        // a specialization: that is monomorphization's to make.
        let args = generics
            .iter()
            .filter_map(|&g| self.def_of(g))
            .map(|p| Ty::Nominal {
                def: p,
                args: Vec::new(),
            })
            .collect();
        self.meta.set_ty(id, Ty::Nominal { def, args });
        self.meta.set_directives(id, directives);
        Some(TypeDef {
            id,
            def,
            name,
            kind,
        })
    }

    /// A trait's members: its methods, in **declaration order**, plus the names
    /// of any associated constants.
    ///
    /// Order is what matters here — it is the layout of every vtable built for
    /// the trait, so a slot index means nothing without it. The def table's
    /// namespace is a map, so the order comes from the member nodes.
    fn lower_trait(&mut self, trait_def: DefId, members: &[NodeId]) -> TypeDefKind {
        let mut methods = Vec::new();
        let mut consts = Vec::new();
        for &m in members {
            let m = self.ast.decl_item(m);
            let NodeKind::ConstBind { pattern, rhs } = self.ast.node(m).kind.clone() else {
                continue;
            };
            let Some(name) = self.binding_name(pattern) else {
                continue;
            };
            let Some(&def) = self.defs.get(trait_def).ns.members.get(&name) else {
                continue;
            };
            match self.ast.node(rhs).kind.clone() {
                NodeKind::FuncExpr {
                    params,
                    body,
                    generics,
                    ..
                } => {
                    let sig = self
                        .ast
                        .meta::<super::Signature>(rhs)
                        .map(|s| s.0)
                        .unwrap_or(Ty::Error);
                    let recv = self.declared_recv(&params, &sig);
                    let id = self.id(rhs);
                    self.meta.set_ty(id, sig);
                    self.meta
                        .set_directives(id, self.defs.get(def).directives.clone());
                    methods.push(TraitMethod {
                        id,
                        def,
                        name,
                        recv,
                        generic: !generics.is_empty(),
                        has_default: body.is_some(),
                        sized_self: self.decls().sized_self(def),
                    });
                }
                // An associated type is a slot in the *impl*, not in the vtable.
                NodeKind::AssocType { .. } => {}
                NodeKind::AssocConst { default, .. } => {
                    let id = self.id(rhs);
                    self.meta.set_ty(id, self.ty(rhs));
                    if let Some(d) = default {
                        let e = self.lower_expr(d);
                        self.meta.set(id, DefaultValue(e));
                    }
                    consts.push(AssocConst { id, def, name });
                }
                _ => {}
            }
        }
        TypeDefKind::Trait { methods, consts }
    }

    /// How a **declared** function takes its receiver (§3.4).
    ///
    /// A trait method has no body, so it is never inferred per-function and its
    /// parameter nodes carry no types of their own. The receiver's type comes
    /// out of the signature instead — which is stamped on the `FuncExpr` — while
    /// whether there *is* a receiver is still the syntactic question of whether
    /// the first parameter is called `self`.
    fn declared_recv(&self, params: &[NodeId], sig: &Ty) -> Recv {
        let Some(&first) = params.first() else {
            return Recv::None;
        };
        let NodeKind::Param { name, .. } = self.ast.node(first).kind.clone() else {
            return Recv::None;
        };
        if name.as_str() != "self" {
            return Recv::None;
        }
        let self_ty = match sig {
            Ty::Func { params, .. } => params.first(),
            _ => None,
        };
        match self_ty {
            Some(Ty::Ptr { mutable: true, .. }) => Recv::MutPtr,
            Some(Ty::Ptr { mutable: false, .. }) => Recv::Ptr,
            _ => Recv::Value,
        }
    }

    /// The name a binding pattern introduces.
    fn binding_name(&self, pattern: NodeId) -> Option<Symbol> {
        match &self.ast.node(pattern).kind {
            NodeKind::BindingPat { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    /// The members of a struct, in **declaration order**.
    ///
    /// The def table's namespace is a map, so it cannot be the source of the
    /// order — and order is the whole point for a struct, since layout assigns
    /// offsets by it. A record's order comes from its `Field` nodes; a tuple
    /// struct's comes from the names, which *are* the positions.
    fn struct_members(&self, def: DefId) -> Vec<Member> {
        let mut out: Vec<(usize, Member)> = self
            .defs
            .get(def)
            .ns
            .members
            .values()
            .filter(|&&m| self.defs.get(m).kind == DefKind::Field)
            .filter_map(|&m| {
                let fd = self.defs.get(m);
                let at = fd.node?;
                Some((at.0, self.member(at, Some(m), fd.name.as_str())))
            })
            .collect();
        // Node ids are allocated in source order, so ordering by them is
        // ordering by where each member was written.
        out.sort_by_key(|(at, _)| *at);
        out.into_iter().map(|(_, m)| m).collect()
    }

    fn lower_variant(&mut self, enum_def: DefId, node: NodeId) -> Option<Variant> {
        let NodeKind::Variant { name, payload, .. } = self.ast.node(node).kind.clone() else {
            return None;
        };
        let def = *self.defs.get(enum_def).ns.members.get(&name)?;
        let (members, tuple) = match payload {
            VariantPayload::None => (Vec::new(), false),
            VariantPayload::Tuple(types) => (
                types
                    .iter()
                    .enumerate()
                    .map(|(i, &t)| self.member(t, None, &i.to_string()))
                    .collect(),
                true,
            ),
            VariantPayload::Record(fields) => (
                fields
                    .iter()
                    .filter_map(|&f| match &self.ast.node(f).kind {
                        NodeKind::Field { name, .. } => {
                            let name = name.clone();
                            Some(self.member(f, None, name.as_str()))
                        }
                        _ => None,
                    })
                    .collect(),
                false,
            ),
        };
        // Inference decided the tag (`infer::stamp_variant_tags`): the position
        // for a variant nobody gave a discriminant, and the written value where
        // one was. Reading it back here is what puts it on the IR, which is the
        // only place a *foreign* enum's tags can be read from.
        let tag = self
            .ast
            .meta::<crate::sema::infer::VariantTag>(node)
            .map_or(0, |t| t.0);
        Some(Variant {
            id: self.id(node),
            def,
            name,
            members,
            tuple,
            tag,
        })
    }

    /// One member, taking its type from the node inference stamped it on.
    fn member(&self, at: NodeId, def: Option<DefId>, name: &str) -> Member {
        let id = self.id(at);
        self.meta.set_ty(id, self.ty(at));
        // A member carries its own directives: `#align(N)` applies to a field as
        // much as to a type (§9), and `#raw` is only ever written on one. They
        // travel the same way a type's do — on the node, so the pass that
        // consumes them never has to reach back into the def table.
        if let Some(d) = def {
            let directives = self.defs.get(d).directives.clone();
            if !directives.is_empty() {
                self.meta.set_directives(id, directives);
            }
        }
        Member {
            id,
            def,
            name: Symbol::new(name),
        }
    }

    fn lower_function(&mut self, def: DefId, func: NodeId) -> Option<Function> {
        let NodeKind::FuncExpr {
            params,
            body,
            extern_abi,
            ..
        } = self.ast.node(func).kind.clone()
        else {
            return None;
        };
        let name = self.defs.get(def).name.clone();
        let ret = self.ty(func);
        let params: Vec<Param> = params.iter().filter_map(|&p| self.lower_param(p)).collect();
        let param_tys: Vec<Ty> = params.iter().map(|p| self.meta.ty_or_error(p.id)).collect();
        let recv = recv_of(&param_tys, &params);
        let mutating = param_tys.iter().any(grants_mutation);
        // Lower every default now, whether or not anything calls this function.
        // A default is part of the declaration: `y: i32 := read_config()` is
        // wrong the moment it is written, and leaving it to the first call site
        // would mean an uncalled function's defaults were never checked at all.
        // Each is stashed on its own parameter (see [`DefaultValue`]).
        let value_params: Vec<(usize, IrId)> = params
            .iter()
            .filter(|p| p.name.as_str() != "self")
            .enumerate()
            .map(|(i, p)| (i, p.id))
            .collect();
        let mut recorded = Vec::with_capacity(value_params.len());
        for (i, param_id) in value_params {
            recorded.push(match self.param_default(def, i) {
                Some(d) => {
                    self.meta.set(param_id, crate::ir::DefaultValue(d));
                    Some(match self.default_is_caller_location(Some(def), i) {
                        true => super::decl::ParamDefault::CallerLocation,
                        false => super::decl::ParamDefault::Value(param_id),
                    })
                }
                None => None,
            });
        }
        self.recorded.push((def, recorded));
        let body_node = body;
        let body = body.map(|b| self.lower_block(b));
        // A function's own type is its whole signature. Keeping only the return
        // type here would have made `meta.ty` mean something different for a
        // function than for every other node.
        let id = self.id(func);
        self.meta.set_ty(
            id,
            Ty::Func {
                params: param_tys,
                ret: Box::new(ret),
                c: extern_abi.is_some(),
            },
        );
        self.meta
            .set_directives(id, self.defs.get(def).directives.clone());
        // What the function is generic over, carried across from the AST so
        // monomorphization can ask the question without the AST (see
        // [`Generics`]). An empty list — the common case — is stamped too: "not
        // generic" and "never asked" are different answers, and only the first
        // one lets a later pass emit the function as it stands.
        if let Some(g) = self.ast.meta::<Generics>(func) {
            self.meta.set(id, g);
        }
        if let Some(b) = body_node {
            self.meta.set(id, self.boxed_in(b));
        }
        Some(Function {
            id,
            def,
            name,
            params,
            body,
            extern_abi,
            recv,
            mutating,
        })
    }

    fn lower_param(&self, param: NodeId) -> Option<Param> {
        let NodeKind::Param { name, .. } = self.ast.node(param).kind.clone() else {
            return None;
        };
        let def = self.def_of(param)?;
        let id = self.id(param);
        self.meta.set_ty(id, self.ty(param));
        Some(Param { id, def, name })
    }

    // ===< blocks / statements >===

    fn lower_block(&mut self, node: NodeId) -> Block {
        let (stmts, tail) = match self.ast.node(node).kind.clone() {
            NodeKind::Block { stmts, tail } => (stmts, tail),
            // A non-block body (an expression) becomes a tail-only block.
            _ => (vec![], Some(node)),
        };
        let mut out = Vec::new();
        for s in stmts {
            self.lower_stmt(s, &mut out);
        }
        let tail = tail.map(|t| Box::new(self.lower_expr(t)));
        let ty = tail.as_ref().map(|t| self.ty_of(t)).unwrap_or(Ty::Void);
        let id = self.id(node);
        self.meta.set_ty(id, ty);
        Block {
            id,
            stmts: out,
            tail,
        }
    }

    /// Lower a pattern-binding statement (`let` / `const` / synthetic `::`) to
    /// an IR `Let`.
    ///
    /// The pattern travels whole: a plain `let x := e` is a
    /// [`Pattern::Binding`], and a destructuring `let (a, b) := p` keeps its
    /// tuple/struct shape so the names it introduces survive. A `let` pattern is
    /// irrefutable, so unlike a `match` arm there is nothing to fall back to.
    fn lower_binding(&mut self, node: NodeId, pattern: NodeId, value: NodeId, out: &mut Vec<Stmt>) {
        let init = self.lower_expr(value);
        let pattern = self.lower_pattern(pattern);
        out.push(self.stmt(node, StmtKind::Let { pattern, init }));
    }

    fn lower_stmt(&mut self, node: NodeId, out: &mut Vec<Stmt>) {
        // A local item is lowered on its own, as the definition it is.
        if super::is_local_item(self.ast, node) {
            return;
        }
        match self.ast.node(node).kind.clone() {
            // A `let`/`const` local, or a `::` binding the desugarer introduced
            // in statement position (`__it`, `__try`): both bind a pattern to an
            // initializer and lower to an IR `Let`.
            NodeKind::LocalDecl { pattern, value, .. } => {
                self.lower_binding(node, pattern, value, out)
            }
            NodeKind::ConstBind { pattern, rhs } => self.lower_binding(node, pattern, rhs, out),
            NodeKind::Assign { place, value, op } => {
                let place = self.lower_expr(place);
                let value = self.lower_expr(value);
                // Compound assignment was desugared to simple `=` already; `op`
                // is `Assign` here (defensive: fall back to plain assign).
                let _ = op;
                out.push(self.stmt(node, StmtKind::Assign { place, value }));
            }
            // Control-flow exits carry no defer copies: each `defer` statement
            // holds its own body, and the CFG stage runs the ones registered
            // above the exit on each way out.
            NodeKind::Return { value } => {
                let value = value.map(|v| self.lower_expr(v));
                out.push(self.stmt(node, StmtKind::Return(value)));
            }
            NodeKind::Break { value } => {
                let value = value.map(|v| self.lower_expr(v));
                out.push(self.stmt(node, StmtKind::Break(value)));
            }
            NodeKind::Continue => out.push(self.stmt(node, StmtKind::Continue)),
            // The body stays here, at the statement that registers it: an exit
            // above this point does not run it (spec §8.4).
            NodeKind::Defer { body } => {
                let d = self.lower_expr(body);
                out.push(self.stmt(node, StmtKind::Defer(d)));
            }
            _ => {
                let e = self.lower_expr(node);
                out.push(self.stmt(node, StmtKind::Expr(e)));
            }
        }
    }

    // ===< expressions >===

    fn lower_expr(&mut self, node: NodeId) -> Expr {
        // An `@using` upcast is a coercion inference accepted at this node, not
        // anything the surface syntax wrote: make the "take `e.field`" explicit
        // around whatever the node itself lowers to.
        if let Some(up) = self.ast.meta::<Upcast>(node) {
            return self.lower_upcast(node, up);
        }
        // A comptime literal converting into a runtime type: emit the `$cast`
        // the surface syntax left implicit.
        //
        // It is marked as the compiler's own, because an inserted conversion
        // promises to be exact where a written one does not (see
        // [`ImplicitCast`]). If inference already reported this literal as out
        // of range, that travels with it so the evaluator does not report the
        // same mistake a second time.
        if let Some(c) = self.ast.meta::<Coercion>(node) {
            let value = self.lower_expr_inner(node);
            let cast = self.expr(
                node,
                c.to,
                ExprKind::Intrinsic {
                    name: Symbol::new("cast"),
                    args: vec![value],
                },
            );
            self.meta.set(cast.id, ImplicitCast);
            if self.ast.meta::<RangeReported>(node).is_some() {
                self.meta.set(cast.id, RangeReported);
            }
            return cast;
        }
        // A `[N]T` reaching a `[]T`: the view is the whole sub-slice, so emit
        // exactly what `a[..]` emits.
        if let Some(sc) = self.ast.meta::<SliceCoerce>(node) {
            let value = self.lower_expr_inner(node);
            // `a[..]`, with its two bounds written out — see [`Self::lower_slice`]
            // for why a slice never builds a `Range`.
            let index = self.bound_ty(&sc.range);
            let start =
                self.derived_expr(value.id, index.clone(), ExprKind::Lit(Lit::Int(0.into())));
            let end = self.len_expr(value.clone(), index);
            return self.expr(
                node,
                sc.to,
                ExprKind::Intrinsic {
                    name: Symbol::new("slice"),
                    args: vec![value, start, end],
                },
            );
        }
        // Likewise a `*T` → `*dyn Trait` unsizing: the fat pointer is built here,
        // not written anywhere in the source.
        if let Some(dc) = self.ast.meta::<DynCoerce>(node) {
            let value = self.lower_expr_inner(node);
            let ty = Ty::Ptr {
                mutable: matches!(self.ty(node), Ty::Ptr { mutable: true, .. }),
                inner: Box::new(dc.object),
            };
            return self.expr(
                node,
                ty,
                ExprKind::DynCast {
                    value: Box::new(value),
                    concrete: dc.concrete,
                },
            );
        }
        self.lower_expr_inner(node)
    }

    /// Wrap `node`'s own lowering in the field access its `@using` coercion
    /// stands for: `e.t` for a value, `&e.t` (keeping mutability) for a pointer.
    fn lower_upcast(&mut self, node: NodeId, up: Upcast) -> Expr {
        let ty = up.target.clone();
        let name = self.defs.get(up.field).name.clone();
        let base = self.lower_expr_inner(node);
        let base = self.autoderef(base);
        if !up.through_ptr {
            return self.expr(
                node,
                ty,
                ExprKind::Field {
                    base: Box::new(base),
                    name,
                    def: Some(up.field),
                },
            );
        }
        // Through a pointer the sub-object's address is what coerces, so the
        // field is read off the pointee and re-addressed.
        let (mutable, inner) = match &ty {
            Ty::Ptr { mutable, inner } => (*mutable, (**inner).clone()),
            _ => (false, ty.clone()),
        };
        let field = self.expr(
            node,
            inner,
            ExprKind::Field {
                base: Box::new(base),
                name,
                def: Some(up.field),
            },
        );
        self.expr(
            node,
            ty,
            ExprKind::Ref {
                mutable,
                place: Box::new(field),
            },
        )
    }

    fn lower_expr_inner(&mut self, node: NodeId) -> Expr {
        let ty = self.ty(node);
        match self.ast.node(node).kind.clone() {
            NodeKind::Block { .. } => {
                // The block expression's type is the block's own — the tail's,
                // or `void`. Not the type inference stamped on the node: a
                // block ending in `return` is typed `never` there, and taking
                // that here would make the block expression's type and the
                // block's own disagree for the same node.
                let b = self.lower_block(node);
                let ty = self.meta.ty_or_error(b.id);
                self.expr(node, ty, ExprKind::Block(b))
            }
            NodeKind::Lit(lit) => self.expr(node, ty, ExprKind::Lit(lit)),
            NodeKind::Path { .. } => self.lower_name(node, ty),
            NodeKind::Closure { .. } => self.lower_closure(node, ty),
            NodeKind::FieldAccess { base, name } => {
                // A resolved namespace member is a global reference; a resolved
                // *field* is a projection out of the base value, and carries the
                // field's own def (see [`crate::sema::fields`]).
                let def = self.resolved_def(node);
                match def.map(|d| self.defs.get(d).kind) {
                    Some(DefKind::Field) | None => {}
                    Some(_) => return self.lower_name(node, ty),
                }
                let base = self.lower_expr(base);
                let base = self.autoderef(base);
                self.expr(
                    node,
                    ty,
                    ExprKind::Field {
                        base: Box::new(base),
                        name,
                        def,
                    },
                )
            }
            NodeKind::TupleIndex { base, index } => {
                let base = self.lower_expr(base);
                let base = self.autoderef(base);
                // On a tuple struct this is a field projection out of a nominal
                // type, and `fields` bound it to the field's def: emit the same
                // `Expr::Field` a named access does, so every later stage reads
                // the offset off the struct's own definition (directives
                // included) rather than re-deriving it positionally.
                if let Some(def) = self.resolved_def(node) {
                    return self.expr(
                        node,
                        ty,
                        ExprKind::Field {
                            base: Box::new(base),
                            name: Symbol::new(&index.to_string()),
                            def: Some(def),
                        },
                    );
                }
                self.expr(
                    node,
                    ty,
                    ExprKind::TupleIndex {
                        base: Box::new(base),
                        index,
                    },
                )
            }
            NodeKind::Call { callee, args } => self.lower_call(node, callee, &args, ty),
            NodeKind::GenericApply { base, .. } => self.lower_expr(base),
            // An arithmetic operator that resolved through an operator trait
            // lowers to a **uniform** call to the chosen method — the same shape
            // for a primitive `i32 + i32` and a user `Vec3 + Vec3` (§6). The
            // `builtin` tag lets codegen recognize the primitive case in O(1).
            NodeKind::Binary { op, lhs, rhs } => match self.ast.meta::<OpResolution>(node) {
                // A comparison resolves to `Eq.eq` / `Ord.cmp`, whose results
                // are not the comparison's own — they need the test around them.
                Some(res) if is_comparison(op) => self.lower_cmp(node, res, op, lhs, rhs),
                Some(res) => {
                    let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
                    self.op_call(node, res, args, ty)
                }
                // `&&` / `||` and the comparisons of the numeric core dispatch
                // on nothing: they stay primitive (§6.13).
                None => {
                    let lhs = self.lower_expr(lhs);
                    let rhs = self.lower_expr(rhs);
                    self.expr(
                        node,
                        ty,
                        ExprKind::Binary {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                    )
                }
            },
            // `&x` / `&mut x` are the built-in pointer operations, not trait
            // calls (§6.13); `-x` and `~x` are, and lower to the same uniform
            // call shape a binary operator does.
            NodeKind::Unary { op, operand } => match op {
                UnOp::Ref | UnOp::RefMut => {
                    let place = self.lower_expr(operand);
                    self.expr(
                        node,
                        ty,
                        ExprKind::Ref {
                            mutable: matches!(op, UnOp::RefMut),
                            place: Box::new(place),
                        },
                    )
                }
                // `-128` is **one** literal, not a negation of `128` (§1.5).
                //
                // Inference already reads it that way: `infer_unary` negates the
                // recorded value before the range check, which is the only
                // reason the minimum of a signed type is writable at all. This
                // is the same reading, carried through to what runs. Without it
                // `-2147483648` type-checks as an `i32` and then *traps* under
                // `overflow=trap` (§7d), because the negation it lowered to is a
                // `sub_checked` whose operand is the one value the type cannot
                // hold — the compiler rejecting the literal it just accepted, at
                // run time.
                //
                // Only for a primitive result. A `distinct` type or a user type
                // with its own `Neg` impl has a function on the other end, and
                // folding the sign into the literal would skip it.
                UnOp::Neg
                    if (ty.is_int() || ty.is_float())
                        && matches!(
                            self.ast.node(operand).kind,
                            NodeKind::Lit(Lit::Int(_) | Lit::Float(_))
                        ) =>
                {
                    let lit = match self.ast.node(operand).kind.clone() {
                        NodeKind::Lit(Lit::Int(n)) => Lit::Int(-n),
                        NodeKind::Lit(Lit::Float(f)) => Lit::Float(-f),
                        _ => unreachable!("guarded by the match arm above"),
                    };
                    self.expr(node, ty, ExprKind::Lit(lit))
                }
                _ => match self.ast.meta::<OpResolution>(node) {
                    Some(res) => {
                        let args = vec![self.lower_expr(operand)];
                        self.op_call(node, res, args, ty)
                    }
                    None => {
                        let operand = self.lower_expr(operand);
                        self.expr(
                            node,
                            ty,
                            ExprKind::Unary {
                                op,
                                operand: Box::new(operand),
                            },
                        )
                    }
                },
            },
            NodeKind::Deref { base } => {
                let base = self.lower_expr(base);
                self.expr(
                    node,
                    ty,
                    ExprKind::Deref {
                        base: Box::new(base),
                    },
                )
            }
            // Indexing goes through `Index` / `IndexMut`, which hand back a
            // *pointer* to the element: `a[i]` is `index(&a, i).*` (§6.13). The
            // built-in sequences included — their impls are in `core` and their
            // members are `#intrinsic`, so the call becomes the operation on
            // the way through `lower_call`.
            //
            // The `None` arm is a base whose type was already in error: there is
            // no impl to name, and inference has reported it.
            NodeKind::Index { base, index } => match self.ast.meta::<OpResolution>(node) {
                Some(res) => self.lower_index_call(node, res, base, index, ty),
                None => self.expr(node, ty, ExprKind::Error),
            },
            NodeKind::Slice { base, range } => self.lower_slice(node, base, range, ty),
            // A range is not an intrinsic: it is a value of the `#lang("range")`
            // enum, one variant per surface form so the bound count and the
            // `..<` / `..=` distinction survive lowering.
            NodeKind::Range { start, end, kind } => {
                let closed = kind == RangeKind::Closed;
                let (name, args) = match (start, end) {
                    (None, None) => ("full", Vec::new()),
                    (Some(s), None) => ("from", vec![self.lower_expr(s)]),
                    (None, Some(e)) => (
                        if closed { "to_inclusive" } else { "to" },
                        vec![self.lower_expr(e)],
                    ),
                    (Some(s), Some(e)) => (
                        if closed { "inclusive" } else { "exclusive" },
                        vec![self.lower_expr(s), self.lower_expr(e)],
                    ),
                };
                self.expr(
                    node,
                    ty,
                    ExprKind::Variant {
                        name: Symbol::new(name),
                        args,
                    },
                )
            }
            NodeKind::Tuple { elems } => {
                let elems = elems.iter().map(|&e| self.lower_expr(e)).collect();
                self.expr(node, ty, ExprKind::Tuple { elems })
            }
            NodeKind::If { cond, then, els } => {
                let cond = self.lower_expr(cond);
                let then = self.lower_block(then);
                let els = els.map(|e| self.lower_block(e));
                self.expr(
                    node,
                    ty,
                    ExprKind::If {
                        cond: Box::new(cond),
                        then,
                        els,
                    },
                )
            }
            NodeKind::IfMatch {
                pattern,
                value,
                then,
                els,
            } => {
                // `if match p := v { then } else { els }` -> a two-arm match.
                let scrutinee = Box::new(self.lower_expr(value));
                let then_block = self.lower_block(then);
                let then_ty = self.meta.ty_or_error(then_block.id);
                let pattern = self.lower_pattern(pattern);
                let body = self.expr(then, then_ty.clone(), ExprKind::Block(then_block));
                let mut arms = vec![Arm {
                    id: self.id(then),
                    pattern,
                    guard: None,
                    body,
                }];
                let else_body = match els {
                    Some(e) => {
                        let b = self.lower_block(e);
                        let bty = self.meta.ty_or_error(b.id);
                        self.expr(e, bty, ExprKind::Block(b))
                    }
                    None => self.expr(node, Ty::Void, ExprKind::Tuple { elems: vec![] }),
                };
                arms.push(Arm {
                    id: self.id(els.unwrap_or(node)),
                    pattern: self.pat(node, PatternKind::Wildcard),
                    guard: None,
                    body: else_body,
                });
                let _ = then_ty;
                self.expr(node, ty, ExprKind::Match { scrutinee, arms })
            }
            NodeKind::MatchExpr { scrutinee, arms } => {
                let scrutinee = Box::new(self.lower_expr(scrutinee));
                let arms = arms
                    .iter()
                    .filter_map(|&a| self.lower_match_arm(a))
                    .collect();
                self.expr(node, ty, ExprKind::Match { scrutinee, arms })
            }
            NodeKind::Loop { body } => {
                let body = self.lower_block(body);
                self.expr(node, ty, ExprKind::Loop { body })
            }
            NodeKind::While { cond, body } => self.lower_while(node, cond, body, ty),
            NodeKind::VariantLit { name, args } => {
                let args = self.lower_variant_args(&args);
                self.expr(node, ty, ExprKind::Variant { name, args })
            }
            NodeKind::CompositeLit { body, .. } => self.lower_composite(node, &body, ty),
            NodeKind::Arg { value, .. } => self.lower_expr(value),
            // A nested item — a local `func`, `struct`, or `import` written
            // among a block's statements — is a *definition*, not a step the
            // block runs. It was collected and lowered on its own (a nested
            // `func` becomes its own [`Function`]), so here it contributes
            // nothing to the enclosing body.
            NodeKind::Decl { .. }
            | NodeKind::ConstBind { .. }
            | NodeKind::NamespaceExpr { .. }
            | NodeKind::ImplBlock { .. }
            | NodeKind::Import { .. } => {
                self.expr(node, Ty::Void, ExprKind::Tuple { elems: Vec::new() })
            }
            // Closures and nested-function *values* are the one expression form
            // that has no IR yet: they need a captured environment, which is a
            // representation decision the IR does not make (see the module doc).
            _ => self.expr(node, ty, ExprKind::Error),
        }
    }

    // ===< calls >===

    /// Lower a call. A method call (`recv.m(args)`) and a free call
    /// (`f(args)`) become the **same** [`Expr::Call`]: the difference is that
    /// the method's receiver is `args[0]`, adjusted to what its `self` parameter
    /// wants, and that its [`Dispatch`] may be virtual or generic.
    fn lower_call(&mut self, node: NodeId, callee: NodeId, args: &[NodeId], ty: Ty) -> Expr {
        // `recv.m.<T>(x)` — the resolution sits on the field access the
        // turbofish wraps.
        let head = match self.ast.node(callee).kind.clone() {
            NodeKind::GenericApply { base, .. } => base,
            _ => callee,
        };
        if let Some(m) = self.ast.meta::<FuncCallMethod>(head) {
            return self.lower_func_call_method(node, head, m, args, ty);
        }
        if let Some(res) = self.ast.meta::<MethodRes>(head) {
            return self.lower_method_call(node, head, res, args, ty);
        }
        // `Pair(1, 2)` is not a call at all: a callee naming a type constructs
        // it (§3.3). It builds the same `Expr::Construct` the positional
        // composite literal `.{1, 2}` builds — a tuple struct has one
        // representation in the IR regardless of which syntax reached it.
        // Named arguments were bound to their parameters during inference; take
        // the order it recorded so the IR is positional, always.
        // Named arguments were bound and omitted defaults left as holes during
        // inference; take the slots it recorded, or the arguments as written when
        // the call needed neither.
        let written: Vec<Option<NodeId>>;
        let slots: &[Option<NodeId>] = match self.ast.meta::<ArgOrder>(head) {
            Some(o) => {
                written = o.args;
                &written
            }
            None => {
                written = args.iter().copied().map(Some).collect();
                &written
            }
        };
        if let Some(def) = self.construct_target(head, &ty) {
            // A tuple struct's fields are positions, not parameters: they take no
            // defaults and reject named arguments, so every slot is written.
            let fields = self
                .lower_args(None, slots, node)
                .into_iter()
                .enumerate()
                .map(|(i, e)| (Symbol::new(&i.to_string()), e))
                .collect();
            return self.expr(
                node,
                ty,
                ExprKind::Construct {
                    def: Some(def),
                    fields,
                },
            );
        }
        let target = self.resolved_def(head);
        // A call to an `#intrinsic` declaration has no body to call: the
        // compiler supplies it, so the call *is* the operation (§6.4). It lowers
        // to an [`ExprKind::Intrinsic`] keyed by the declaration's tag — which is
        // why the tag, and not the function's name or path, is what a later
        // stage keys on.
        if let Some(tag) = target.and_then(|d| self.defs.get(d).intrinsic_tag()) {
            let args = self.lower_args(target, slots, node);
            let e = self.lower_intrinsic(node, tag, args, ty);
            // An intrinsic carries its generic arguments too, and for the one
            // reason nothing else needs them: `size_of.<T>()` mentions `T`
            // **nowhere** in its signature (`func <T> () -> usize`), so the type
            // it is asking about exists only in the instantiation. Without this
            // the layout query would have nothing to be asked about.
            self.carry_instantiation(head, &e);
            return e;
        }
        // A call on a `Func` value: the callee is the value, and what it
        // reaches is decided by its type once monomorphization knows it (§5.5).
        let dispatch = match (self.ast.meta::<StaticTraitSelf>(head), target) {
            // `Trait.member(args)` whose `Self` is a type parameter: which impl
            // it reaches is monomorphization's to say (see `StaticTraitSelf`).
            (Some(st), Some(method)) => Dispatch::Generic {
                trait_def: st.trait_def,
                method,
                self_ty: st.self_ty,
                trait_args: st.trait_args,
            },
            _ => match self.ast.meta::<FuncCall>(head) {
                Some(FuncCall) => Dispatch::Func {
                    self_ty: self.ty(head),
                },
                None => Dispatch::Static,
            },
        };
        let callee = Box::new(self.lower_expr(callee));
        let args = self.lower_args(target, slots, node);
        let call = self.expr(
            node,
            ty,
            ExprKind::Call {
                callee,
                args,
                builtin: None,
                dispatch,
            },
        );
        self.carry_instantiation(head, &call);
        call
    }

    /// Build the IR node for an intrinsic call, applying the two foldings that
    /// happen here rather than in codegen.
    fn lower_intrinsic(&mut self, node: NodeId, tag: Symbol, mut args: Vec<Expr>, ty: Ty) -> Expr {
        // `len(a)` folds on a fixed array — the length is part of the type, so
        // there is nothing left to compute at run time. This is the whole of
        // `core`'s `.len()` methods once they are inlined.
        if tag.as_str() == "len" && args.len() == 1 {
            let base = args.remove(0);
            let base = self.autoderef(base);
            return self.len_expr(base, ty);
        }
        // An explicit `cast.<*dyn Trait>(p)` builds the same fat pointer the
        // implicit coercion does; it is a spelling of the unsizing, not a
        // reinterpretation of bits, so it lowers to the same node (§3.4).
        if tag.as_str() == "cast" && args.len() == 1 {
            if let Some(cast) = self.dyn_cast(args[0].clone(), &ty) {
                return cast;
            }
        }
        self.expr(node, ty, ExprKind::Intrinsic { name: tag, args })
    }

    /// Lower a call's argument slots, filling each `None` — a parameter the call
    /// left out — with the callee's default for that position (§5.2).
    ///
    /// This is where a default becomes real. Inference deliberately left the
    /// hole: filling it there would mean inferring the default expression once
    /// per call site, stamping conflicting types on the one set of AST nodes the
    /// declaration owns. Here there is no such conflict — the default is lowered
    /// once against its declaration and the result is *cloned* into each site,
    /// so a default like `.{}` still builds a fresh value per call.
    fn lower_args(
        &mut self,
        callee: Option<DefId>,
        slots: &[Option<NodeId>],
        at: NodeId,
    ) -> Vec<Expr> {
        let mut out = Vec::with_capacity(slots.len());
        for (i, slot) in slots.iter().enumerate() {
            match slot {
                Some(a) => out.push(self.lower_expr(*a)),
                None => {
                    // `#caller_location` is the one default that cannot be
                    // lowered once and cloned: its whole value is *which call
                    // site asked*. The cache below would hand every caller the
                    // declaration's position, so it is built here, from `at`.
                    if self.default_is_caller_location(callee, i) {
                        out.push(self.caller_location(at));
                        continue;
                    }
                    let d = callee.and_then(|c| self.param_default(c, i));
                    // A hole with no default behind it means inference and
                    // lowering disagree about the signature; a typed `Error`
                    // keeps the IR well-formed rather than dropping an argument
                    // and silently changing the call's arity. It has no syntax
                    // anywhere, so it is the one node with no span to record.
                    out.push(d.unwrap_or_else(|| {
                        let id = self.meta.fresh();
                        self.meta.set_ty(id, Ty::Error);
                        Expr {
                            id,
                            kind: ExprKind::Error,
                        }
                    }));
                }
            }
        }
        out
    }

    /// Whether `def`'s `i`-th value parameter defaults to `#caller_location`.
    fn default_is_caller_location(&self, def: Option<DefId>, i: usize) -> bool {
        let Some(def) = def else { return false };
        // What lowering recorded, first: a callee in another package has this
        // written down and has no syntax to read it off.
        if let Some(d) = self.decls().param_default(def, i) {
            return matches!(d, super::decl::ParamDefault::CallerLocation);
        }
        let Some((file, _)) = self.decls().func(def) else {
            return false;
        };
        self.decls()
            .param_default_nodes(def)
            .and_then(|d| d.get(i).copied().flatten())
            .is_some_and(|d| matches!(self.asts[&file].node(d).kind, NodeKind::CallerLocation))
    }

    /// Build the `Location` value for the call site `at` (§5.2).
    ///
    /// This is an ordinary struct construction with three constant fields, not a
    /// new IR node: every stage after this one — the `#const` check, the const
    /// evaluator, codegen — already knows what a `Construct` of literals is, and
    /// a location is exactly that once the position is resolved.
    fn caller_location(&mut self, at: NodeId) -> Expr {
        let node = self.ast.node(at);
        let (file, span) = (node.file, node.span);
        let ty = match self.lang.get("location") {
            Some(def) => Ty::Nominal {
                def: self.defs.resolve_alias(def),
                args: Vec::new(),
            },
            // Inference already reported the missing lang item.
            None => return self.expr(at, Ty::Error, ExprKind::Error),
        };
        let Ty::Nominal { def, .. } = &ty else {
            unreachable!()
        };
        let def = *def;
        let (name, line, column) = match self.sources.file(file) {
            Some(src) => {
                let lc = src.line_col(span.start);
                (src.name.clone(), lc.line, lc.column)
            }
            // A file with no recorded text (an in-memory test fixture that was
            // never added to the map) still gets a well-formed value.
            None => (String::new(), 0, 0),
        };
        // The literal's type is `str`, the `#lang("str")` item — found by tag,
        // like the location type itself.
        let str_ty = match self.lang.get("str") {
            Some(d) => Ty::Nominal {
                def: self.defs.resolve_alias(d),
                args: Vec::new(),
            },
            None => Ty::Error,
        };
        let u32_ty = Ty::int(32, false);
        let fields = vec![
            (
                Symbol::new("file"),
                self.expr(at, str_ty, ExprKind::Lit(Lit::Str(name))),
            ),
            (
                Symbol::new("line"),
                self.expr(at, u32_ty.clone(), ExprKind::Lit(Lit::Int(line.into()))),
            ),
            (
                Symbol::new("column"),
                self.expr(at, u32_ty, ExprKind::Lit(Lit::Int(column.into()))),
            ),
        ];
        self.expr(
            at,
            ty,
            ExprKind::Construct {
                def: Some(def),
                fields,
            },
        )
    }

    /// The lowered default of `def`'s `i`-th **value** parameter, if it has one.
    ///
    /// `self` is excluded from the numbering, matching how inference counts the
    /// arguments of a method call.
    fn param_default(&mut self, def: DefId, i: usize) -> Option<Expr> {
        if let Some(cached) = self.defaults.get(&def) {
            return cached.get(i).cloned().flatten();
        }
        // The recorded default, first: it is the only one a callee in another
        // package has, and it is the same expression this would lower — the one
        // that was lowered where it was written, kept on the parameter it
        // belongs to and carried by the library along with its type and span.
        if let Some(super::decl::ParamDefault::Value(id)) = self.decls().param_default(def, i) {
            return self
                .meta
                .get::<crate::ir::DefaultValue>(id)
                .map(|crate::ir::DefaultValue(e)| e);
        }
        let (file, _) = self.decls().func(def)?;
        let slots = self.decls().param_default_nodes(def)?;
        // Lower in the *declaring* file's context: the default's nodes, and the
        // types inference stamped on them, live in that arena.
        let saved = std::mem::replace(&mut self.ast, &self.asts[&file]);
        let lowered: Vec<Option<Expr>> = slots
            .into_iter()
            .map(|s| s.map(|n| self.lower_expr(n)))
            .collect();
        self.ast = saved;

        let out = lowered.get(i).cloned().flatten();
        self.defaults.insert(def, lowered);
        out
    }

    /// Append a `Field` read off the spread temporary for every field `def`
    /// declares that the literal did not write.
    ///
    /// Declaration order, because the IR's field list is read positionally by
    /// everything downstream and two compilations should agree.
    fn fill_from_spread(&mut self, spread: NodeId, def: DefId, out: &mut Vec<(Symbol, Expr)>) {
        let written: Vec<Symbol> = out.iter().map(|(n, _)| n.clone()).collect();
        for name in self.record_field_names(def) {
            if written.contains(&name) {
                continue;
            }
            let base = self.lower_expr(spread);
            let field = self.field_def(def, &name);
            let fty = self.field_ty_of(def, &name);
            let read = self.expr(
                spread,
                fty,
                ExprKind::Field {
                    base: Box::new(base),
                    name: name.clone(),
                    def: field,
                },
            );
            out.push((name, read));
        }
    }

    /// The fields `def` declares, in declaration order.
    fn record_field_names(&self, def: DefId) -> Vec<Symbol> {
        self.decls().record_field_names(def)
    }

    /// The `DefKind::Field` def named `name` on struct `def`.
    fn field_def(&self, def: DefId, name: &Symbol) -> Option<DefId> {
        let m = *self.defs.get(def).ns.members.get(name)?;
        (self.defs.get(m).kind == DefKind::Field).then_some(m)
    }

    /// The declared type of `def`'s field `name`, as inference stamped it.
    fn field_ty_of(&self, def: DefId, name: &Symbol) -> Ty {
        let Some(field) = self.field_def(def, name) else {
            return Ty::Error;
        };
        if let Some(t) = self.decls().field_ty(field) {
            return t;
        }
        let d = self.defs.get(field);
        let (Some(file), Some(node)) = (d.file, d.node) else {
            return Ty::Error;
        };
        // Inference stamps a member's declared type on the `Field` node itself —
        // the node the member's def points at — not on the type node inside it.
        self.asts[&file].meta::<Ty>(node).unwrap_or(Ty::Error)
    }

    /// The struct a call's callee names, when the callee is a type rather than a
    /// function — the construction form. `None` for an ordinary call.
    fn construct_target(&self, callee: NodeId, ty: &Ty) -> Option<DefId> {
        if !matches!(self.ast.node(callee).kind, NodeKind::Path { .. }) {
            return None;
        }
        let def = self.resolved_def(callee)?;
        if self.defs.get(def).kind != DefKind::Struct {
            return None;
        }
        // Name the def the *type* settled on, not the one the path resolved to:
        // an alias resolves to its target here the same way it does elsewhere.
        match ty {
            Ty::Nominal { def, .. } => Some(*def),
            _ => None,
        }
    }

    /// Lower `recv.m(args)` using the resolution inference stamped on the
    /// `recv.m` node.
    ///
    /// The receiver is not a field being read: it is the first *argument*, and
    /// whatever the `self` parameter asked for — a value, a `*Self`, a
    /// `*mut Self` — is spelled out here as an explicit `&` / `&mut` / `.*`, so
    /// nothing downstream has to re-derive an implicit adjustment.
    /// `f.call(t)` (§5.5): a call of `f` itself, whose one argument is the
    /// tuple the LIR spreads ([`crate::ir::SpreadArgs`]).
    fn lower_func_call_method(
        &mut self,
        node: NodeId,
        callee: NodeId,
        m: FuncCallMethod,
        args: &[NodeId],
        ty: Ty,
    ) -> Expr {
        let NodeKind::FieldAccess { base, .. } = self.ast.node(callee).kind.clone() else {
            return self.expr(node, ty, ExprKind::Error);
        };
        let mut value = self.lower_expr(base);
        if m.deref {
            value = self.autoderef(value);
        }
        let dispatch = match self.ty_of(&value) {
            Ty::Func { .. } => Dispatch::Static,
            self_ty => Dispatch::Func { self_ty },
        };
        let slots: Vec<Option<NodeId>> = args.iter().copied().map(Some).collect();
        let args = self.lower_args(None, &slots, node);
        let call = self.expr(
            node,
            ty,
            ExprKind::Call {
                callee: Box::new(value),
                args,
                builtin: None,
                dispatch,
            },
        );
        self.meta.set(call.id, crate::ir::SpreadArgs);
        call
    }

    fn lower_method_call(
        &mut self,
        node: NodeId,
        callee: NodeId,
        res: MethodRes,
        args: &[NodeId],
        ty: Ty,
    ) -> Expr {
        let NodeKind::FieldAccess { base, .. } = self.ast.node(callee).kind.clone() else {
            return self.expr(node, ty, ExprKind::Error);
        };
        let mut recv = self.lower_expr(base);
        // The method was found on the type this `distinct` type is distinct from
        // (§2.4). Representations are identical, so reaching it is a
        // reinterpretation — but the method's `self` is typed as the
        // representation, so say so before the `&` / `.*` adjustment runs.
        if let Some(d) = self.ast.meta::<DistinctRecv>(base) {
            recv = self.expr(
                base,
                d.repr.clone(),
                ExprKind::Intrinsic {
                    name: Symbol::new("cast"),
                    args: vec![recv],
                },
            );
            // The compiler's, not the program's: a `distinct` type and its
            // representation are the same bits, so this conversion cannot lose
            // anything, and nothing about it was written down.
            self.meta.set(recv.id, ImplicitCast);
        }
        let written: Vec<Option<NodeId>>;
        let slots: &[Option<NodeId>] = match self.ast.meta::<ArgOrder>(callee) {
            Some(o) => {
                written = o.args;
                &written
            }
            None => {
                written = args.iter().copied().map(Some).collect();
                &written
            }
        };
        let mut call_args = vec![self.adjust_recv(recv, res.adjust, &res.self_ty)];
        let lowered = self.lower_args(Some(res.method), slots, node);
        call_args.extend(lowered);
        // A method may be `#intrinsic` too — `x.wrapping_add(y)` is one (§3.1) —
        // and it lowers the same way a free intrinsic call does, with the
        // receiver as the first argument.
        if let Some(tag) = self.defs.get(res.method).intrinsic_tag() {
            return self.lower_intrinsic(node, tag, call_args, ty);
        }
        // Inference recorded the instantiated signature on this node; falling
        // back to a reconstruction keeps the IR typed if it did not.
        let callee_ty = match self.ty(callee) {
            f @ Ty::Func { .. } => f,
            _ => Ty::Func {
                params: call_args.iter().map(|a| self.ty_of(a)).collect(),
                ret: Box::new(ty.clone()),
                c: false,
            },
        };
        let dispatch = match &res.dispatch {
            MethodDispatch::Static => Dispatch::Static,
            MethodDispatch::Virtual(trait_def) => Dispatch::Virtual {
                trait_def: *trait_def,
                method: res.method,
            },
            MethodDispatch::Generic { trait_def, args } => Dispatch::Generic {
                trait_def: *trait_def,
                method: res.method,
                self_ty: res.self_ty.clone(),
                trait_args: args.clone(),
            },
        };
        let head = callee;
        let callee = self.expr(callee, callee_ty, ExprKind::Global(res.method));
        let call = self.expr(
            node,
            ty,
            ExprKind::Call {
                callee: Box::new(callee),
                args: call_args,
                builtin: None,
                dispatch,
            },
        );
        self.carry_instantiation(head, &call);
        call
    }

    /// Carry a call site's generic arguments across from the AST node inference
    /// stamped them on (see [`Instantiation`]).
    ///
    /// They land on the **call**, not on the callee expression, because that is
    /// the node monomorphization is holding when it asks: it walks calls, and
    /// each one it finds has to answer "which instantiation of the callee is
    /// this?" without a second lookup.
    fn carry_instantiation(&self, head: NodeId, call: &Expr) {
        // **Not when the callee was redirected.** A static trait call
        // (`FromResidual.from_residual(r)`) resolves by name to the *trait's*
        // bodyless declaration, and the solver then stamps the impl member that
        // won — which `lower_name` is what points the call at. The arguments
        // recorded here are the ones that declaration has: `FromResidual.<R>`
        // owns one parameter, and `impl <T, E> FromResidual.<E> for
        // Result.<T, E>` has two.
        //
        // Handing the shorter list to monomorphization left `T` bound to
        // nothing: the emitted `from_residual` returned `Result.<T, E>` with a
        // parameter still in it, and building an `.err` of a type that does not
        // exist lowered to `return undef` — so every `.?` propagating an error
        // returned garbage. Dropping the list makes mono re-derive it from the
        // signature, which is the narrow case `args_from_signature` is sound
        // for: both signatures are concrete and every parameter appears in them.
        if self.redirected_to_impl(head) {
            return;
        }
        if let Some(inst) = self.ast.meta::<Instantiation>(head) {
            self.meta.set(call.id, inst);
        }
    }

    /// Whether the solver pointed this callee at an impl member other than the
    /// declaration its name resolved to.
    fn redirected_to_impl(&self, head: NodeId) -> bool {
        let Some(res) = self.ast.meta::<OpResolution>(head) else {
            return false;
        };
        self.resolved_def(head) != Some(res.method)
    }

    /// Lower an operator to a uniform call to its resolved method. The callee is
    /// the trait/impl method as a global; its function type is reconstructed
    /// from the (already lowered) argument and result types so the IR stays
    /// fully typed. `builtin` carries through the primitive-op tag for codegen.
    ///
    /// Operands are lowered by the caller on purpose: one that coerces (a
    /// `comptime_int` literal, say) presents its *converted* type to the call,
    /// so the reconstructed signature has to come from the lowered arguments.
    fn op_call(&mut self, node: NodeId, res: OpResolution, args: Vec<Expr>, ty: Ty) -> Expr {
        // An impl member may be `#intrinsic`, and then the call *is* the
        // operation (§6.4) — there is no body on the other end of it. `core`'s
        // `Index` / `IndexMut` on the built-in sequences are written that way,
        // which is what keeps `a[i]` one construct for every type while still
        // compiling to an address computation for the two the machine knows.
        //
        // The same question a direct call asks in `lower_call`, asked here
        // because an operator never goes through that path.
        if let Some(tag) = self.defs.get(res.method).intrinsic_tag() {
            return self.lower_intrinsic(node, tag, args, ty);
        }
        let callee_ty = Ty::Func {
            params: args.iter().map(|a| self.ty_of(a)).collect(),
            ret: Box::new(ty.clone()),
            c: false,
        };
        let callee = self.expr(node, callee_ty, ExprKind::Global(res.method));
        self.expr(
            node,
            ty,
            ExprKind::Call {
                callee: Box::new(callee),
                args,
                builtin: res.builtin,
                dispatch: Dispatch::Static,
            },
        )
    }

    /// Lower a comparison that resolved to a user impl (§6.13).
    ///
    /// `==` is the method itself and `!=` its negation; the four relations all
    /// go through the *one* `Ord.cmp`, testing the `Ordering` it returns. That
    /// test is a `match`, not a primitive compare on the enum: the arms are what
    /// say which orderings count, and a decision-tree pass turns them into the
    /// discriminant check later.
    fn lower_cmp(
        &mut self,
        node: NodeId,
        res: OpResolution,
        op: BinOp,
        lhs: NodeId,
        rhs: NodeId,
    ) -> Expr {
        let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            let call = self.op_call(node, res, args, Ty::Bool);
            return match op {
                BinOp::Ne => self.expr(
                    node,
                    Ty::Bool,
                    ExprKind::Unary {
                        op: UnOp::Not,
                        operand: Box::new(call),
                    },
                ),
                _ => call,
            };
        }
        let ordering = match self.lang.get("ordering") {
            Some(d) => Ty::Nominal {
                def: self.defs.resolve_alias(d),
                args: Vec::new(),
            },
            None => Ty::Error,
        };
        let call = self.op_call(node, res, args, ordering);
        // Which single `Ordering` answers the relation, and whether landing on
        // it means `true`: `a < b` is "`.less`, yes"; `a >= b` is "`.less`, no".
        let (variant, hit) = match op {
            BinOp::Lt => ("less", true),
            BinOp::Ge => ("less", false),
            BinOp::Gt => ("greater", true),
            _ => ("greater", false),
        };
        let arms = vec![
            Arm {
                id: self.id(node),
                pattern: self.pat(
                    node,
                    PatternKind::Variant {
                        name: Symbol::new(variant),
                        sub: Vec::new(),
                    },
                ),
                guard: None,
                body: self.expr(node, Ty::Bool, ExprKind::Lit(Lit::Bool(hit))),
            },
            Arm {
                id: self.id(node),
                pattern: self.pat(node, PatternKind::Wildcard),
                guard: None,
                body: self.expr(node, Ty::Bool, ExprKind::Lit(Lit::Bool(!hit))),
            },
        ];
        self.expr(
            node,
            Ty::Bool,
            ExprKind::Match {
                scrutinee: Box::new(call),
                arms,
            },
        )
    }

    /// Lower `a[i]` on a type that indexes through `Index` / `IndexMut`: the
    /// trait method takes the container by pointer and hands back a pointer to
    /// the element, so the surface `a[i]` is `index(&a, i).*` (§6.13).
    fn lower_index_call(
        &mut self,
        node: NodeId,
        res: OpResolution,
        base: NodeId,
        index: NodeId,
        ty: Ty,
    ) -> Expr {
        // `IndexMut` is the write side; its `self` and its result are `*mut`.
        let mut mutable = self
            .lang
            .get("index_mut")
            .map(|t| self.defs.resolve_alias(t))
            == Some(res.trait_def);
        let base_node = base;
        let base = self.lower_expr(base);
        // A built-in sequence goes through `Index` whether it is being read or
        // written (`core/iter/slice.nest` says why), so the declared `-> *Self.Output`
        // is not the whole answer: what the element pointer **permits** follows
        // the receiver, which is the one thing no signature in the language can
        // state. It is the same gap `make.<[]T>(n)` has, and it is filled the
        // same way — here, where the receiver's type is in hand.
        //
        // A `[]mut T` yields a `*mut T` however immutably the binding holding it
        // was declared, because that is what a `[]mut T` *is* (§2.3). An array's
        // elements belong to whatever holds the array, so the pointer is mutable
        // and the mutability check walks to the base to decide — which is
        // exactly the rule it applied to `a[i]` before this went through a
        // trait.
        let recv_ty = self.ty_of(&base);
        let seq = match &recv_ty {
            Ty::Ptr { inner, .. } => (**inner).clone(),
            other => other.clone(),
        };
        match seq {
            Ty::Slice { mutable: m, .. } => mutable = m,
            // An array's elements are part of whatever holds the array, so
            // there is nothing in the *type* to read the permission off. What
            // decides is whether this `a[i]` is being written, which only the
            // statement knew — see [`IndexWrite`].
            Ty::Array { .. } => {
                mutable = self
                    .ast
                    .meta::<crate::sema::infer::IndexWrite>(node)
                    .is_some()
            }
            _ => {}
        }
        // The *receiver* is only read even on the write path: a sequence's
        // header is never written by indexing it, and asking for `&mut s` would
        // refuse every `s: []mut T` the program did not also declare `mut`.
        let recv_mut = mutable && !matches!(seq, Ty::Slice { .. } | Ty::Array { .. });
        let recv = match recv_ty {
            // Already a pointer (an auto-deref site): pass it straight through.
            Ty::Ptr { .. } => base,
            other => {
                let ptr = Ty::Ptr {
                    mutable: recv_mut,
                    inner: Box::new(other),
                };
                self.expr(
                    base_node,
                    ptr,
                    ExprKind::Ref {
                        mutable: recv_mut,
                        place: Box::new(base),
                    },
                )
            }
        };
        let index = self.lower_expr(index);
        let elem_ptr = Ty::Ptr {
            mutable,
            inner: Box::new(ty.clone()),
        };
        let call = self.op_call(node, res, vec![recv, index], elem_ptr);
        self.expr(
            node,
            ty,
            ExprKind::Deref {
                base: Box::new(call),
            },
        )
    }

    /// `while cond { body }` -> `loop { if <not cond> { break }; <body...> }`.
    fn lower_while(&mut self, node: NodeId, cond: NodeId, body: NodeId, ty: Ty) -> Expr {
        let cond_expr = self.lower_expr(cond);
        let not_cond = self.expr(
            cond,
            Ty::Bool,
            ExprKind::Unary {
                op: UnOp::Not,
                operand: Box::new(cond_expr),
            },
        );
        // The guard and its `break` are synthetic — nothing in the source is
        // spelled `break` — so they take the condition's span: that is the
        // expression whose value decides whether the jump happens.
        let break_id = self.id(cond);
        self.meta.set_ty(break_id, Ty::Void);
        let break_block = Block {
            id: break_id,
            stmts: vec![self.stmt(cond, StmtKind::Break(None))],
            tail: None,
        };
        let guard_if = self.expr(
            cond,
            Ty::Void,
            ExprKind::If {
                cond: Box::new(not_cond),
                then: break_block,
                els: None,
            },
        );
        let guard = self.stmt(cond, StmtKind::Expr(guard_if));
        let mut body_block = self.lower_block(body);
        body_block.stmts.insert(0, guard);
        // A loop body yields nothing.
        if let Some(tail) = body_block.tail.take() {
            let id = self.derived(tail.id);
            body_block.stmts.push(Stmt {
                id,
                kind: StmtKind::Expr(*tail),
            });
        }
        // A loop body yields nothing, whatever its tail used to say.
        self.meta.set_ty(body_block.id, Ty::Void);
        self.expr(node, ty, ExprKind::Loop { body: body_block })
    }

    fn lower_variant_args(&mut self, args: &VariantArgs) -> Vec<Expr> {
        match args {
            VariantArgs::None => vec![],
            VariantArgs::Tuple(ids) => ids.iter().map(|&a| self.lower_expr(a)).collect(),
            VariantArgs::Record(ids) => ids
                .iter()
                .map(|&f| match self.ast.node(f).kind.clone() {
                    NodeKind::FieldInit { value, .. } => self.lower_expr(value),
                    _ => self.lower_expr(f),
                })
                .collect(),
        }
    }

    /// Lower a composite literal to the construct its **type** calls for, not
    /// the syntax it was written with: `P { .. }` and `.{ .. }` both become an
    /// `Expr::Construct` for a struct, an `Expr::Tuple` for a tuple, `$array`
    /// for an array or slice. Inference has already checked the body fits, so
    /// the type is the authority here.
    fn lower_composite(&mut self, node: NodeId, body: &CompositeBody, ty: Ty) -> Expr {
        match body {
            CompositeBody::Named { fields, spread } => {
                let mut lowered: Vec<(Symbol, Expr)> = fields
                    .iter()
                    .filter_map(|&f| match self.ast.node(f).kind.clone() {
                        NodeKind::FieldInit { name, value } => Some((name, self.lower_expr(value))),
                        _ => None,
                    })
                    .collect();
                match &ty {
                    Ty::Nominal { def, .. } => {
                        let def = *def;
                        // `..rest` supplies every field the literal did not
                        // write. It is expanded **here**, and not in desugaring,
                        // because the fields it fills come from the literal's
                        // type — which `.{ x: 5, ..rest }` does not have until
                        // inference has settled it. `rest` is already bound to a
                        // temporary, so each read is a field of a name rather
                        // than a re-evaluation.
                        if let Some(s) = spread {
                            self.fill_from_spread(*s, def, &mut lowered);
                        }
                        self.expr(
                            node,
                            ty,
                            ExprKind::Construct {
                                def: Some(def),
                                fields: lowered,
                            },
                        )
                    }
                    // An anonymous struct (§3.8): the same construction with no
                    // declaration behind it. A `..rest` spread is not expanded
                    // here — the fields it would fill come from a *declaration*,
                    // and there is none — so it is refused in inference.
                    Ty::Struct(_) => self.expr(
                        node,
                        ty,
                        ExprKind::Construct {
                            def: None,
                            fields: lowered,
                        },
                    ),
                    // Named fields on a non-struct: already diagnosed.
                    _ => self.expr(node, ty, ExprKind::Error),
                }
            }
            CompositeBody::Positional(elems) => {
                let elems: Vec<Expr> = elems.iter().map(|&e| self.lower_expr(e)).collect();
                match &ty {
                    Ty::Tuple(_) => self.expr(node, ty, ExprKind::Tuple { elems }),
                    // A tuple struct's members are positional but it is still a
                    // nominal construction; name the fields by their index.
                    Ty::Nominal { def, .. } => {
                        let def = *def;
                        let fields = elems
                            .into_iter()
                            .enumerate()
                            .map(|(i, e)| (Symbol::new(&i.to_string()), e))
                            .collect();
                        self.expr(
                            node,
                            ty,
                            ExprKind::Construct {
                                def: Some(def),
                                fields,
                            },
                        )
                    }
                    Ty::Array { .. } | Ty::Slice { .. } => self.expr(
                        node,
                        ty,
                        ExprKind::Intrinsic {
                            name: Symbol::new("array"),
                            args: elems,
                        },
                    ),
                    _ => self.expr(node, ty, ExprKind::Error),
                }
            }
            CompositeBody::Repeat { value, count } => {
                let args = vec![self.lower_expr(*value), self.lower_expr(*count)];
                self.expr(
                    node,
                    ty,
                    ExprKind::Intrinsic {
                        name: Symbol::new("repeat"),
                        args,
                    },
                )
            }
        }
    }

    // ===< match arms / patterns >===

    fn lower_match_arm(&mut self, node: NodeId) -> Option<Arm> {
        let NodeKind::MatchArm {
            pattern,
            guard,
            body,
        } = self.ast.node(node).kind.clone()
        else {
            return None;
        };
        Some(Arm {
            id: self.id(node),
            pattern: self.lower_pattern(pattern),
            guard: guard.map(|g| self.lower_expr(g)),
            body: self.lower_expr(body),
        })
    }

    fn lower_pattern(&mut self, node: NodeId) -> Pattern {
        let kind = match self.ast.node(node).kind.clone() {
            NodeKind::WildcardPat => PatternKind::Wildcard,
            NodeKind::BindingPat { name, .. } => match self.def_of(node) {
                Some(def) => PatternKind::Binding { def, name },
                None => PatternKind::Wildcard,
            },
            NodeKind::LitPat(lit) => PatternKind::Lit(lit),
            NodeKind::VariantPat { name, args } => PatternKind::Variant {
                name,
                sub: self.lower_variant_pat_args(&args),
            },
            NodeKind::TuplePat { elems } => {
                PatternKind::Tuple(elems.iter().map(|&e| self.lower_pattern(e)).collect())
            }
            NodeKind::OrPat { alternatives } => PatternKind::Or(
                alternatives
                    .iter()
                    .map(|&a| self.lower_pattern(a))
                    .collect(),
            ),
            NodeKind::AtPat { name, pattern } => match self.def_of(node) {
                Some(def) => PatternKind::At {
                    binding: Binding {
                        id: self.id(node),
                        def,
                        name,
                    },
                    pattern: Box::new(self.lower_pattern(pattern)),
                },
                None => return self.lower_pattern(pattern),
            },
            NodeKind::RefPat { pattern } => {
                PatternKind::Deref(Box::new(self.lower_pattern(pattern)))
            }
            NodeKind::StructPat { path, fields, rest } => PatternKind::Struct {
                def: path.and_then(|p| self.resolved_def(p)),
                fields: fields
                    .iter()
                    .filter_map(|&f| self.lower_field_pat(f))
                    .collect(),
                rest,
            },
            NodeKind::TupleStructPat { path, elems, rest } => PatternKind::TupleStruct {
                def: self.resolved_def(path),
                elems: elems.iter().map(|&e| self.lower_pattern(e)).collect(),
                rest,
            },
            NodeKind::SlicePat { elems, rest } => {
                // The `..` splits the element patterns: those before it match
                // from the front, those after it from the back.
                let split = rest.as_ref().map_or(elems.len(), |r| r.at.min(elems.len()));
                let lowered: Vec<Pattern> = elems.iter().map(|&e| self.lower_pattern(e)).collect();
                let (prefix, suffix) = lowered.split_at(split);
                PatternKind::Slice {
                    prefix: prefix.to_vec(),
                    // The rest's own binding lives on the `SlicePat` node.
                    rest: rest.map(|r| {
                        r.name.zip(self.def_of(node)).map(|(name, def)| Binding {
                            id: self.id(node),
                            def,
                            name,
                        })
                    }),
                    suffix: suffix.to_vec(),
                }
            }
            NodeKind::RangePat { start, end, kind } => PatternKind::Range {
                start: start.and_then(|s| self.lit_of(s)),
                end: end.and_then(|e| self.lit_of(e)),
                inclusive: kind == RangeKind::Closed,
            },
            // A glob pattern is an import form, not a value test.
            _ => PatternKind::Wildcard,
        };
        self.pat(node, kind)
    }

    /// Lower one `FieldPat` of a struct pattern to its `name → sub-pattern`
    /// pair; the `{ name }` shorthand binds the field under its own name.
    fn lower_field_pat(&mut self, f: NodeId) -> Option<(Symbol, Pattern)> {
        let NodeKind::FieldPat { name, pattern, .. } = self.ast.node(f).kind.clone() else {
            return None;
        };
        let sub = match pattern {
            Some(p) => self.lower_pattern(p),
            None => {
                let kind = match self.def_of(f) {
                    Some(def) => PatternKind::Binding {
                        def,
                        name: name.clone(),
                    },
                    None => PatternKind::Wildcard,
                };
                self.pat(f, kind)
            }
        };
        Some((name, sub))
    }

    /// The literal a range-pattern bound names, if it is one.
    fn lit_of(&self, node: NodeId) -> Option<Lit> {
        match self.ast.node(node).kind.clone() {
            NodeKind::LitPat(l) | NodeKind::Lit(l) => Some(l),
            _ => None,
        }
    }

    fn lower_variant_pat_args(&mut self, args: &VariantPatArgs) -> Vec<Pattern> {
        match args {
            VariantPatArgs::None => vec![],
            VariantPatArgs::Tuple(ids) => ids.iter().map(|&p| self.lower_pattern(p)).collect(),
            VariantPatArgs::Record { fields, .. } => fields
                .iter()
                .map(|&f| match self.ast.node(f).kind.clone() {
                    NodeKind::FieldPat {
                        pattern: Some(p), ..
                    } => self.lower_pattern(p),
                    NodeKind::FieldPat {
                        name,
                        pattern: None,
                        ..
                    } => {
                        let kind = match self.def_of(f) {
                            Some(def) => PatternKind::Binding { def, name },
                            None => PatternKind::Wildcard,
                        };
                        self.pat(f, kind)
                    }
                    _ => self.pat(f, PatternKind::Wildcard),
                })
                .collect(),
        }
    }

    // ===< names / helpers >===

    fn lower_name(&mut self, node: NodeId, ty: Ty) -> Expr {
        if let Some(def) = self.resolved_def(node)
            && let Some(e) = self.captured(node, def, &ty)
        {
            return e;
        }
        // A static trait call (`Trait.member(args)`) resolves by name to the
        // trait's bodyless *declaration*; the solver stamped which impl won, so
        // point at that impl's member instead (see `infer::open_trait_self`).
        match (self.ast.meta::<OpResolution>(node), self.resolved_def(node)) {
            (Some(res), _) => self.global_or_local(node, res.method, ty),
            (None, Some(def)) => self.global_or_local(node, def, ty),
            (None, None) => self.expr(node, ty, ExprKind::Error),
        }
    }

    /// A use of `def` inside a closure that captured it: the closure's member,
    /// read through the closure — and, for a shared local, through the pointer
    /// the member holds (§5.5).
    fn captured(&self, node: NodeId, def: DefId, ty: &Ty) -> Option<Expr> {
        let cx = self.closures.last()?;
        let (name, fty, shared) = cx.fields.get(&def)?.clone();
        let this = self.expr(node, cx.this_ty.clone(), ExprKind::Local(cx.this));
        let closure_ty = match &cx.this_ty {
            Ty::Ptr { inner, .. } => (**inner).clone(),
            other => other.clone(),
        };
        let base = self.expr(
            node,
            closure_ty,
            ExprKind::Deref {
                base: Box::new(this),
            },
        );
        let member = self.expr(
            node,
            fty,
            ExprKind::Field {
                base: Box::new(base),
                name,
                def: None,
            },
        );
        if !shared {
            return Some(member);
        }
        Some(self.expr(
            node,
            ty.clone(),
            ExprKind::Deref {
                base: Box::new(member),
            },
        ))
    }

    /// A read of `def` where a closure is being made: what the enclosing code
    /// sees it as, which inside another closure is that closure's member.
    fn use_of(&self, node: NodeId, def: DefId, ty: &Ty) -> Expr {
        self.captured(node, def, ty)
            .unwrap_or_else(|| self.global_or_local(node, def, ty.clone()))
    }

    /// Lower a closure (§5.5): lift its body into its own function, give it a
    /// struct of what it captured, and answer the value that fills that struct.
    ///
    /// A copy is its value where the closure is made. A shared local is its
    /// address — the local lives in a cell (see [`crate::ir::Boxed`]), and the
    /// address *is* the cell, so the closure and the code around it read and
    /// write the one place.
    fn lower_closure(&mut self, node: NodeId, ty: Ty) -> Expr {
        let NodeKind::Closure {
            captures,
            params,
            body,
            ..
        } = self.ast.node(node).kind.clone()
        else {
            return self.expr(node, ty, ExprKind::Error);
        };
        let (Some(defs), Some(sig)) = (
            self.ast.meta::<super::ClosureDefs>(node),
            self.ast.meta::<super::infer::ClosureSig>(node),
        ) else {
            return self.expr(node, ty, ExprKind::Error);
        };
        let shared = self
            .ast
            .meta::<super::Captures>(node)
            .map(|c| c.0)
            .unwrap_or_default();
        let mut fields: HashMap<DefId, (Symbol, Ty, bool)> = HashMap::new();
        let mut members = Vec::new();
        let mut values = Vec::new();
        let mut taken: Vec<Symbol> = Vec::new();
        let mut member_name = |name: &Symbol| {
            let mut n = name.clone();
            let mut i = 1;
            while taken.contains(&n) {
                n = Symbol::new(&format!("{name}#{i}"));
                i += 1;
            }
            taken.push(n.clone());
            n
        };
        for &c in &captures {
            let (Some(inner), Some(outer)) = (self.def_of(c), self.resolved_def(c)) else {
                continue;
            };
            let t = self.ty(c);
            let name = member_name(&self.defs.get(inner).name);
            values.push((name.clone(), self.use_of(c, outer, &t)));
            fields.insert(inner, (name.clone(), t.clone(), false));
            members.push((name, t));
        }
        // A `<const N>` of a function around the closure, read in its body, is
        // a copy made where the closure is: there `N` still stands for what
        // monomorphization substitutes, and the closure's type — generic over
        // the function's type parameters only — could not carry it.
        for (n, d) in self.const_params_read(body) {
            let t = self.ty(n);
            let name = member_name(&self.defs.get(d).name);
            values.push((name.clone(), self.use_of(n, d, &t)));
            fields.insert(d, (name.clone(), t.clone(), false));
            members.push((name, t));
        }
        for (d, t) in shared.iter().zip(&sig.shared) {
            let mutable = self.defs.get(*d).mutable;
            let fty = Ty::Ptr {
                mutable,
                inner: Box::new(t.clone()),
            };
            let name = member_name(&self.defs.get(*d).name);
            let place = self.use_of(node, *d, t);
            let addr = self.expr(
                node,
                fty.clone(),
                ExprKind::Ref {
                    mutable,
                    place: Box::new(place),
                },
            );
            values.push((name.clone(), addr));
            fields.insert(*d, (name.clone(), fty.clone(), true));
            members.push((name, fty));
        }
        // The closure's type, over the same parameters as the function around
        // it — what `ty` is, with each of them standing for itself.
        let own = Ty::Nominal {
            def: defs.ty,
            args: sig
                .generics
                .iter()
                .map(|&p| Ty::Nominal {
                    def: p,
                    args: Vec::new(),
                })
                .collect(),
        };
        let type_id = self.id(node);
        self.meta.set_ty(type_id, own.clone());
        let type_members = members
            .iter()
            .map(|(name, t)| {
                let id = self.id(node);
                self.meta.set_ty(id, t.clone());
                Member {
                    id,
                    def: None,
                    name: name.clone(),
                }
            })
            .collect();
        self.lifted_types.push(TypeDef {
            id: type_id,
            def: defs.ty,
            name: self.defs.get(defs.ty).name.clone(),
            kind: TypeDefKind::Struct {
                members: type_members,
            },
        });
        // The body, as `call(self: *Closure, params...)`.
        let this_ty = Ty::Ptr {
            mutable: false,
            inner: Box::new(own),
        };
        let this_id = self.id(node);
        self.meta.set_ty(this_id, this_ty.clone());
        let mut fparams = vec![Param {
            id: this_id,
            def: defs.this,
            name: Symbol::new("self"),
        }];
        fparams.extend(params.iter().filter_map(|&p| self.lower_param(p)));
        let param_tys: Vec<Ty> = fparams
            .iter()
            .map(|p| self.meta.ty_or_error(p.id))
            .collect();
        self.closures.push(ClosureCx {
            this: defs.this,
            this_ty,
            fields,
        });
        let block = self.lower_block(body);
        self.closures.pop();
        let fid = self.id(node);
        self.meta.set_ty(
            fid,
            Ty::Func {
                params: param_tys,
                ret: Box::new(sig.ret.clone()),
                c: false,
            },
        );
        self.meta.set(
            fid,
            Generics {
                params: sig.generics.clone(),
                own: 0,
            },
        );
        self.meta.set(fid, self.boxed_in(body));
        self.lifted.push(Function {
            id: fid,
            def: defs.call,
            name: Symbol::new("call"),
            params: fparams,
            body: Some(block),
            extern_abi: None,
            recv: Recv::Ptr,
            mutating: false,
        });
        self.expr(
            node,
            ty,
            ExprKind::Construct {
                def: Some(defs.ty),
                fields: values,
            },
        )
    }

    /// Each `<const N>` read anywhere under `node`, with the first node that
    /// reads it, in the order they are first read.
    fn const_params_read(&self, node: NodeId) -> Vec<(NodeId, DefId)> {
        let mut out: Vec<(NodeId, DefId)> = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if let Some(d) = self.resolved_def(n)
                && self.defs.get(d).kind == DefKind::ConstParam
                && !out.iter().any(|&(_, seen)| seen == d)
            {
                out.push((n, d));
            }
            let mut kids = self.ast.node(n).kind.children();
            kids.reverse();
            stack.extend(kids);
        }
        out
    }

    /// Every local a closure written anywhere under `node` shares — what the
    /// function `node` is the body of keeps in cells (see [`crate::ir::Boxed`]).
    fn boxed_in(&self, node: NodeId) -> crate::ir::Boxed {
        let mut out: Vec<DefId> = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if let Some(super::Captures(defs)) = self.ast.meta::<super::Captures>(n) {
                for d in defs {
                    if !out.contains(&d) {
                        out.push(d);
                    }
                }
            }
            stack.extend(self.ast.node(n).kind.children());
        }
        crate::ir::Boxed(out)
    }

    fn global_or_local(&self, node: NodeId, def: DefId, ty: Ty) -> Expr {
        let kind = match self.defs.get(def).kind {
            DefKind::Local | DefKind::Param => ExprKind::Local(def),
            // A `<const N>` parameter has no storage to load from; it stands for
            // whatever value monomorphization substitutes.
            DefKind::ConstParam => ExprKind::ConstParam(def),
            _ => ExprKind::Global(def),
        };
        let e = self.expr(node, ty, kind);
        // A read of a constant a generic `impl` declares carries what it bound
        // that impl's parameters to, the way a call carries a callee's (see
        // [`Lowerer::carry_instantiation`]). Without it `u8.MAX` and `u16.MAX`
        // are the same global with nothing to tell them apart.
        if let Some(inst) = self.ast.meta::<Instantiation>(node) {
            self.meta.set(e.id, inst);
        }
        e
    }

    /// The type def a `TypePath`/`Path` type node names, if it is a struct/enum.
    fn ty(&self, node: NodeId) -> Ty {
        self.ast.meta::<Ty>(node).unwrap_or(Ty::Error)
    }

    fn def_of(&self, node: NodeId) -> Option<DefId> {
        self.ast.meta::<DefMeta>(node).map(|m| m.0)
    }

    fn resolved_def(&self, node: NodeId) -> Option<DefId> {
        match self.ast.meta::<Resolution>(node)? {
            Resolution::Def(d) => Some(self.defs.resolve_alias(d)),
            _ => None,
        }
    }

    // ===< synthetic nodes >===
    //
    // These four build IR the source never wrote, so there is no AST node to
    // take a span from; each borrows its operand's, via
    // [`Lowerer::derived_expr`].

    /// The element count of an array or slice, however it was spelled (`a.len`
    /// or `$len(a)`).
    ///
    /// A fixed `[N]T` whose `N` is already known folds to the literal right here
    /// — the length is part of the type, so there is nothing to compute.
    /// Everything else keeps the `$len` intrinsic for a later stage to read (a
    /// slice header's length) or substitute (`[N]T` still generic in `N`,
    /// resolved at monomorphization).
    fn len_expr(&self, base: Expr, ty: Ty) -> Expr {
        let from = base.id;
        if let Ty::Array { len, .. } = self.meta.ty_or_error(from)
            && let Some(n) = len.value()
        {
            return self.derived_expr(from, ty, ExprKind::Lit(Lit::Int(n.into())));
        }
        self.derived_expr(
            from,
            ty,
            ExprKind::Intrinsic {
                name: Symbol::new("len"),
                args: vec![base],
            },
        )
    }

    /// `a[i..<j]`, as the two bounds it names.
    ///
    /// **A slice never builds a `Range`.** The parser produces a `Slice` node
    /// only when the index is *syntactically* a range
    /// (`parse_index_or_slice`), so which of the six forms it is — `..`, `a..`,
    /// `..<b`, `..=b`, `a..<b`, `a..=b` — is known right here. Constructing the
    /// enum anyway would hand a backend a six-way branch on a tag, to recover a
    /// fact this function already had; `design/lir.md` §10 says an intrinsic is
    /// one instruction or one runtime call, and a range-taking `$slice` is
    /// neither.
    ///
    /// Both bounds come out **exclusive and present**, so everything downstream
    /// has one convention instead of a flag: a missing start is `0`, a missing
    /// end is the sequence's length, and `..=b` is `b + 1`.
    fn lower_slice(&mut self, node: NodeId, base: NodeId, range: NodeId, ty: Ty) -> Expr {
        let value = self.lower_expr(base);
        let NodeKind::Range { start, end, kind } = self.ast.node(range).kind else {
            // The parser does not build a `Slice` over anything else, so this is
            // a tree that was already wrong.
            return self.expr(node, ty, ExprKind::Error);
        };
        let index = self.bound_ty(&self.ty(range));
        let start = match start {
            Some(s) => self.lower_expr(s),
            None => self.derived_expr(value.id, index.clone(), ExprKind::Lit(Lit::Int(0.into()))),
        };
        let end = match end {
            Some(e) => {
                let e = self.lower_expr(e);
                // `..=b` includes `b`, and every bound below this line is
                // exclusive. One convention, settled here.
                if kind == RangeKind::Closed {
                    let one =
                        self.derived_expr(e.id, index.clone(), ExprKind::Lit(Lit::Int(1.into())));
                    self.derived_expr(
                        e.id,
                        index.clone(),
                        ExprKind::Binary {
                            op: BinOp::Add,
                            lhs: Box::new(e),
                            rhs: Box::new(one),
                        },
                    )
                } else {
                    e
                }
            }
            None => self.len_expr(value.clone(), index),
        };
        self.expr(
            node,
            ty,
            ExprKind::Intrinsic {
                name: Symbol::new("slice"),
                args: vec![value, start, end],
            },
        )
    }

    /// The type a range's bounds have: the `T` of the `Range.<T>` inference gave
    /// it, which is the index type of the sequence being sliced.
    fn bound_ty(&self, range: &Ty) -> Ty {
        match range {
            Ty::Nominal { args, .. } => args.first().cloned().unwrap_or(Ty::Error),
            _ => Ty::Error,
        }
    }

    /// Apply the receiver adjustment a method call implies, spelling out in the
    /// IR what the surface `x.m()` left to the type checker (§3.4).
    fn adjust_recv(&self, recv: Expr, adjust: RecvAdjust, self_ty: &Ty) -> Expr {
        let from = recv.id;
        match adjust {
            RecvAdjust::None => recv,
            RecvAdjust::Ref { mutable } => self.derived_expr(
                from,
                self_ty.clone(),
                ExprKind::Ref {
                    mutable,
                    place: Box::new(recv),
                },
            ),
            RecvAdjust::Deref => self.derived_expr(
                from,
                self_ty.clone(),
                ExprKind::Deref {
                    base: Box::new(recv),
                },
            ),
        }
    }

    /// The [`ExprKind::DynCast`] an explicit `$cast.<*dyn Trait>(p)` denotes,
    /// when that is what it is: a pointer value reaching a
    /// pointer-to-trait-object type. Any other `$cast` is a real conversion and
    /// stays an intrinsic.
    fn dyn_cast(&self, value: Expr, to: &Ty) -> Option<Expr> {
        let Ty::Ptr { inner, .. } = to else {
            return None;
        };
        if !matches!(**inner, Ty::Dyn { .. }) {
            return None;
        }
        let Ty::Ptr {
            inner: concrete, ..
        } = self.ty_of(&value)
        else {
            return None;
        };
        Some(self.derived_expr(
            value.id,
            to.clone(),
            ExprKind::DynCast {
                value: Box::new(value),
                concrete: *concrete,
            },
        ))
    }

    /// Wrap `base` in an explicit [`ExprKind::Deref`] if its type is a pointer,
    /// so auto-deref field/index access is spelled out in the IR (§3.2).
    fn autoderef(&self, base: Expr) -> Expr {
        let Ty::Ptr { inner, .. } = self.ty_of(&base) else {
            return base;
        };
        self.derived_expr(
            base.id,
            *inner,
            ExprKind::Deref {
                base: Box::new(base),
            },
        )
    }
}

/// How a lowered parameter list takes its receiver: the first parameter, if it
/// is named `self` (§3.4). Nest writes the receiver as an ordinary parameter, so
/// this is the one place that decides what counts as a method.
fn recv_of(param_tys: &[Ty], params: &[Param]) -> Recv {
    let Some(first) = params.first() else {
        return Recv::None;
    };
    if first.name.as_str() != "self" {
        return Recv::None;
    }
    match &param_tys[0] {
        Ty::Ptr { mutable: true, .. } => Recv::MutPtr,
        Ty::Ptr { mutable: false, .. } => Recv::Ptr,
        _ => Recv::Value,
    }
}

/// Whether a parameter of this type lets the callee write into the caller's
/// storage — a `*mut T` or a `[]mut T`, or one reached through a read-only
/// pointer or slice to such a thing (`*[]mut T` still hands out the mutable
/// view). Arrays, tuples and named types are *values*: their own `mut` marks
/// only the local copy.
fn grants_mutation(ty: &Ty) -> bool {
    match ty {
        Ty::Ptr { mutable, inner } | Ty::Slice { mutable, inner } => {
            *mutable || grants_mutation(inner)
        }
        _ => false,
    }
}

/// Whether a [`BinOp`] is one of the six comparisons, which reach `Eq` / `Ord`
/// by a method name that is not the operator's own.
fn is_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
    )
}
