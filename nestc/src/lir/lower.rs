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
use crate::common::source::FileSpan;
use crate::common::symbol::Symbol;
use crate::ir::layout::Layouts;
use crate::ir::{
    self, ConstValue, Dispatch, Expr, ExprKind, IrId, Linked, Meta, Pattern, PatternKind, StmtKind,
    TypeDefKind,
};
use crate::parser::ast::{BinOp, Lit};
use crate::sema::builtins::BuiltinOp;
use crate::sema::def::{DefId, DefTable};
use crate::sema::ty::Ty;

use super::{
    AggregateKind, Base, Block, BlockId, Callee, Constant, Function, Global, Local, LocalId,
    Operand, Origin, Place, Program, Projection, Rvalue, Stmt, StmtKind as LirStmtKind, TermKind,
    Terminator, TypeDef, TypeMember, VariantDef, Vtable, VtableId, VtableSlot,
};

/// Lower the whole monomorphized program.
pub fn lower(
    defs: &DefTable,
    meta: &Meta,
    linked: &Linked,
    layouts: &Layouts,
    options: &Options,
) -> Program {
    let mut cx = Cx {
        defs,
        meta,
        linked,
        layouts,
        options,
        program: Program::default(),
        type_index: HashMap::new(),
        vtable_index: HashMap::new(),
    };

    // A `#static` region is program-lifetime storage, so it belongs to the
    // program rather than to any function. A `::` constant is **not** here: it
    // *is* its value (§2.5), and every use of one carries that value.
    for g in linked.globals() {
        if !g.mutable {
            continue;
        }
        cx.program.globals.push(Global {
            def: g.def,
            name: g.name.clone(),
            ty: meta.ty_or_error(g.id),
            init: meta.get::<ConstValue>(g.id),
            span: meta.span(g.id),
        });
    }

    let funcs: Vec<&ir::Function> = linked.funcs().collect();
    for f in funcs {
        let func = Lowerer::new(&mut cx, f).run();
        cx.program.funcs.push(func);
    }

    // The type table last, because it is the closure of what the functions
    // turned out to mention. "Every type" is not a set anyone can enumerate —
    // the same reason layout is a query — so what LIR carries is what LIR uses.
    cx.collect_types();
    cx.program
}

/// State shared by every function's lowering: the tables that answer questions
/// about the *program* rather than about one body.
struct Cx<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    linked: &'a Linked,
    layouts: &'a Layouts<'a>,
    options: &'a Options,
    program: Program,
    /// Flattened type definitions, by [`crate::ir::mono::type_key`].
    type_index: HashMap<String, usize>,
    /// Vtables, by `(trait, concrete type key)`.
    vtable_index: HashMap<(DefId, String), VtableId>,
}

impl Cx<'_> {
    fn ty_of(&self, id: IrId) -> Ty {
        self.meta.ty_or_error(id)
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

    /// `usize`, as wide as this target's pointer.
    fn usize_ty(&self) -> Ty {
        Ty::int((self.layouts.pointer_size() * 8) as u16, false)
    }

    // ===< Vtables (§7b) >===

    /// The vtable for one `(trait, concrete type)` pair, built the first time a
    /// coercion asks for it.
    ///
    /// Which function fills each slot is **not** decided here: monomorphization
    /// decided it, because the instantiated method that fills a slot does not
    /// exist until that pass makes it, and re-selecting the impl would be a
    /// second implementation of a selection free to disagree with the first.
    /// What this does is turn that answer into data — a struct of function
    /// pointers, in the trait's declaration order (§7b).
    fn vtable(&mut self, slots: &crate::ir::mono::VtableSlots) -> VtableId {
        let key = (slots.trait_def, self.key(&slots.concrete));
        if let Some(id) = self.vtable_index.get(&key) {
            return *id;
        }
        let id = VtableId(self.program.vtables.len() as u32);
        self.vtable_index.insert(key, id);

        let names: Vec<Symbol> = match self.linked.ty(slots.trait_def).map(|t| &t.kind) {
            Some(TypeDefKind::Trait { methods, .. }) => {
                methods.iter().map(|m| m.name.clone()).collect()
            }
            _ => Vec::new(),
        };
        let filled = slots
            .slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let def = (*slot)?;
                let f = self.linked.get(def)?;
                let (_, symbol) = self.names(f.id, &f.name);
                Some(VtableSlot {
                    method: names.get(i).cloned().unwrap_or_else(|| f.name.clone()),
                    def,
                    symbol,
                })
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
        self.program.vtables.push(Vtable {
            id,
            trait_def: slots.trait_def,
            concrete: slots.concrete.clone(),
            symbol,
            slots: filled,
        });
        id
    }

    // ===< The flattened type table (§7b) >===

    /// Walk everything the lowered program mentions and record the aggregate
    /// behind each type, flattened to a struct.
    fn collect_types(&mut self) {
        let tys: Vec<Ty> = self
            .program
            .funcs
            .iter()
            .flat_map(|f| {
                f.locals
                    .iter()
                    .map(|l| l.ty.clone())
                    .chain(std::iter::once(f.ret.clone()))
            })
            .chain(self.program.globals.iter().map(|g| g.ty.clone()))
            .collect();
        for ty in tys {
            self.intern(&ty, 0);
        }
    }

    /// Record `ty`'s flattened definition, and its members' after it. Idempotent,
    /// and keyed by the type's mangled encoding for the same reason layout's
    /// cache is: injectivity is exactly what a key wants.
    fn intern(&mut self, ty: &Ty, depth: u32) {
        // The same guard the layout query keeps, for the same reason: a type
        // that contains itself has already been reported, and recursing on it
        // here would not fail, it would fail to terminate.
        if depth > 64 {
            return;
        }
        let key = self.key(ty);
        if self.type_index.contains_key(&key) {
            return;
        }
        // A pointer to a trait object is the **fat** pointer, and that is the
        // thing with two members: `dyn Trait` on its own is unsized and is only
        // ever reached through one (§3.4, §7b).
        if let Ty::Ptr { inner, .. } = ty
            && let Ty::Dyn(trait_def) = &**inner
        {
            let w = self.layouts.pointer_size();
            let void_ptr = Ty::Ptr {
                mutable: false,
                inner: Box::new(Ty::Void),
            };
            self.type_index.insert(key.clone(), self.program.types.len());
            self.program.types.push(TypeDef {
                key,
                name: ty.display(self.defs),
                members: vec![
                    TypeMember {
                        name: Symbol::new("data"),
                        ty: void_ptr.clone(),
                        offset: 0,
                    },
                    TypeMember {
                        name: Symbol::new("vtable"),
                        ty: void_ptr,
                        offset: w,
                    },
                ],
                layout: crate::ir::layout::Layout {
                    size: w * 2,
                    align: w,
                },
                origin: Origin::Dyn(*trait_def),
            });
            return;
        }
        // An array is not an aggregate at this level — it keeps its own shape
        // (§7b) — but its element is one, and so is a pointer's pointee.
        if let Ty::Array { inner, .. } | Ty::Ptr { inner, .. } = ty {
            let inner = (**inner).clone();
            self.intern(&inner, depth + 1);
            return;
        }
        let inner_of_slice = match ty {
            Ty::Slice { inner, .. } => Some((**inner).clone()),
            _ => None,
        };
        let Some(def) = self.flatten(ty) else {
            if let Some(inner) = inner_of_slice {
                self.intern(&inner, depth + 1);
            }
            return;
        };
        self.type_index.insert(key, self.program.types.len());
        let mut nested: Vec<Ty> = def.members.iter().map(|m| m.ty.clone()).collect();
        if let Origin::Enum { variants, .. } = &def.origin {
            nested.extend(
                variants
                    .iter()
                    .flat_map(|v| v.members.iter().map(|m| m.ty.clone())),
            );
        }
        self.program.types.push(def);
        for m in nested {
            self.intern(&m, depth + 1);
        }
    }

    /// `ty` as a struct, or `None` if it is not an aggregate.
    fn flatten(&self, ty: &Ty) -> Option<TypeDef> {
        let key = self.key(ty);
        let name = ty.display(self.defs);
        let layout = self.layouts.of(ty).ok()?;
        match ty {
            Ty::Tuple(elems) => {
                let f = self.layouts.fields(ty)?.ok()?;
                let members = elems
                    .iter()
                    .enumerate()
                    .map(|(i, t)| TypeMember {
                        name: Symbol::new(&i.to_string()),
                        ty: t.clone(),
                        offset: f.offsets[i],
                    })
                    .collect();
                Some(TypeDef {
                    key,
                    name,
                    members,
                    layout,
                    origin: Origin::Tuple,
                })
            }
            // A slice **does** flatten, and it is the interesting near-miss: it
            // is a pointer and a length, and neither is indexed by a run-time
            // value. The indexing happens through the pointer it holds (§7b).
            Ty::Slice { mutable, inner } => {
                let w = self.layouts.pointer_size();
                Some(TypeDef {
                    key,
                    name,
                    members: vec![
                        TypeMember {
                            name: Symbol::new("ptr"),
                            ty: Ty::Ptr {
                                mutable: *mutable,
                                inner: inner.clone(),
                            },
                            offset: 0,
                        },
                        TypeMember {
                            name: Symbol::new("len"),
                            ty: self.usize_ty(),
                            offset: w,
                        },
                    ],
                    layout,
                    origin: Origin::Slice,
                })
            }
            Ty::Nominal { def, .. } => {
                let t = self.linked.ty(*def)?;
                match &t.kind {
                    TypeDefKind::Struct { .. } | TypeDefKind::Distinct { .. } => {
                        let f = self.layouts.fields(ty)?.ok()?;
                        let members = self
                            .layouts
                            .member_types(ty)?
                            .into_iter()
                            .enumerate()
                            .map(|(i, (n, t))| TypeMember {
                                name: n,
                                ty: t,
                                offset: f.offsets.get(i).copied().unwrap_or(0),
                            })
                            .collect();
                        let origin = match &t.kind {
                            TypeDefKind::Distinct { .. } => Origin::Distinct(*def),
                            _ => Origin::Struct(*def),
                        };
                        Some(TypeDef {
                            key,
                            name,
                            members,
                            layout,
                            origin,
                        })
                    }
                    // An enum becomes `{ tag, payload }`. The variants do not go
                    // with the shape: a debugger showing `2` instead of `.green`
                    // is a worse debugger (§7c), so they ride on the definition,
                    // which the flattening does not touch.
                    TypeDefKind::Enum { variants } => {
                        let e = self.layouts.enum_layout(ty)?.ok()?;
                        let members = vec![
                            TypeMember {
                                name: Symbol::new("tag"),
                                ty: Ty::int((e.tag.size * 8) as u16, false),
                                offset: 0,
                            },
                            TypeMember {
                                name: Symbol::new("payload"),
                                ty: bytes_ty(e.payload.size),
                                offset: e.payload_at,
                            },
                        ];
                        let vs = variants
                            .iter()
                            .enumerate()
                            .map(|(i, v)| VariantDef {
                                name: v.name.clone(),
                                tag: i as i128,
                                members: self
                                    .layouts
                                    .variant_member_types(ty, i)
                                    .unwrap_or_default()
                                    .into_iter()
                                    .enumerate()
                                    .map(|(j, (n, t))| TypeMember {
                                        name: n,
                                        ty: t,
                                        offset: e.variants[i].offsets.get(j).copied().unwrap_or(0),
                                    })
                                    .collect(),
                                tuple: v.tuple,
                            })
                            .collect();
                        Some(TypeDef {
                            key,
                            name,
                            members,
                            layout,
                            origin: Origin::Enum {
                                def: *def,
                                variants: vs,
                            },
                        })
                    }
                    TypeDefKind::Trait { .. } => None,
                }
            }
            _ => None,
        }
    }
}

/// `[n]u8` — what an enum's payload member is: `n` bytes every variant shares.
fn bytes_ty(n: u64) -> Ty {
    Ty::Array {
        len: crate::sema::ty::Const::Value(Box::new(crate::sema::ty::ConstArg {
            ty: Ty::int(64, false),
            value: ConstValue::Int(n.into()),
        })),
        mutable: false,
        inner: Box::new(Ty::u8()),
    }
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

/// One lexical scope: the `defer` bodies registered in it, and the ladder rungs
/// already built for it.
struct Scope {
    id: u32,
    defers: Vec<Expr>,
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
    /// Where each bound name lives.
    local_of: HashMap<DefId, LocalId>,
    blocks: Vec<PartialBlock>,
    /// The block statements are currently appended to.
    at: BlockId,
    loops: Vec<LoopCtx>,
    scopes: Vec<Scope>,
    next_scope: u32,
    /// Ladder rungs already built, by the scope they unwind and the kind of exit
    /// they continue to. This is what makes a rung once-per-kind rather than
    /// once-per-site (§3).
    rungs: HashMap<(u32, Exit), BlockId>,
    /// The slot a `return` writes, and the block that finally returns it.
    ret_slot: Option<LocalId>,
    ret_block: Option<BlockId>,
    /// The declared result type.
    ret: Ty,
}

impl<'a, 'c> Lowerer<'a, 'c> {
    fn new(cx: &'a mut Cx<'c>, f: &'c ir::Function) -> Self {
        let ret = match cx.meta.ty(f.id) {
            Some(Ty::Func { ret, .. }) => *ret,
            _ => Ty::Void,
        };
        Lowerer {
            cx,
            f,
            locals: Vec::new(),
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
        }
    }

    fn run(mut self) -> Function {
        // Parameters occupy the first slots, in declaration order: matching a
        // call's arguments to a callee's frame should be a position, not a
        // search.
        for p in &self.f.params {
            let ty = self.cx.ty_of(p.id);
            let span = self.cx.meta.span(p.id);
            let id = self.new_local(Some(p.name.clone()), ty, span);
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
        Function {
            def: self.f.def,
            name,
            symbol,
            locals: self.locals,
            params,
            ret: self.ret.clone(),
            blocks,
            extern_abi: self.f.extern_abi.clone(),
            span: self.cx.meta.span(self.f.id),
            directives: self.cx.meta.directives(self.f.id),
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
            None => self.terminate(Terminator {
                kind: TermKind::Return(value),
                span,
            }),
        }
    }

    // ===< Blocks and locals >===

    fn new_local(&mut self, name: Option<Symbol>, ty: Ty, span: Option<FileSpan>) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(Local {
            id,
            name,
            ty,
            span,
        });
        id
    }

    /// A slot no source name produced. It has none, which is honest: a debugger
    /// shows it as a slot rather than as an invented identifier (§7c).
    fn temp(&mut self, ty: Ty, span: Option<FileSpan>) -> LocalId {
        self.new_local(None, ty, span)
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
        self.blocks[at].stmts.push(Stmt { kind, span });
    }

    fn terminate(&mut self, term: Terminator) {
        let at = self.at.0 as usize;
        if self.blocks[at].term.is_none() {
            self.blocks[at].term = Some(term);
        }
    }

    fn goto(&mut self, target: BlockId, span: Option<FileSpan>) {
        self.terminate(Terminator {
            kind: TermKind::Goto(target),
            span,
        });
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
                term: b.term.unwrap_or(Terminator {
                    kind: TermKind::Unreachable,
                    span: None,
                }),
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
        let t = self.temp(ty, span);
        self.assign(Place::local(t), value, span);
        Operand::local(t)
    }

    /// Branch on `cond`, continuing in a fresh block when it is true.
    fn branch_if(&mut self, cond: Operand, on_false: BlockId, span: Option<FileSpan>) {
        let next = self.new_block(None);
        self.terminate(Terminator {
            kind: TermKind::Switch {
                value: cond,
                arms: vec![(1, next)],
                otherwise: on_false,
            },
            span,
        });
        self.at = next;
    }

    // ===< Scopes and the cleanup ladder (§3) >===

    fn push_scope(&mut self, defers: Vec<Expr>) {
        let id = self.next_scope;
        self.next_scope += 1;
        self.scopes.push(Scope { id, defers });
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
        let (id, defers) = (scope.id, scope.defers.clone());
        // A scope with nothing deferred is not a rung: it would be a block whose
        // only statement is a jump, which is a block a reader has to follow to
        // learn nothing.
        if defers.is_empty() {
            return self.rung(exit, depth - 1, floor, span);
        }
        if let Some(b) = self.rungs.get(&(id, exit)) {
            return *b;
        }
        let next = self.rung(exit, depth - 1, floor, span);
        let block = self.new_block(Some(format!("defer {} ({})", id, exit_label(exit))));
        self.rungs.insert((id, exit), block);
        let resume = self.at;
        self.at = block;
        for d in defers.iter().rev() {
            self.eval(d);
        }
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
        self.terminate(Terminator {
            kind: TermKind::Return(slot.map(Operand::local)),
            span,
        });
        self.at = resume;
        self.ret_slot = slot;
        self.ret_block = Some(b);
        b
    }

    /// Whether any scope currently open has something deferred — which is what
    /// decides whether an exit needs a ladder at all.
    fn any_defers(&self, floor: usize) -> bool {
        self.scopes[floor..].iter().any(|s| !s.defers.is_empty())
    }

    // ===< Blocks and statements >===

    /// Lower a block and hand back its value, if it has one.
    fn block_value(&mut self, b: &ir::Block) -> Option<Operand> {
        self.push_scope(b.defers.clone());
        for s in &b.stmts {
            self.stmt(s);
        }
        let value = match &b.tail {
            Some(t) if !self.ended() => Some(self.eval(t)),
            _ => None,
        };
        self.pop_scope();
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
                // are projections out of it.
                let v = self.eval(init);
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
                    self.terminate(Terminator {
                        kind: TermKind::Return(value),
                        span,
                    });
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
                let id = self.new_local(Some(name.clone()), ty.clone(), span);
                self.local_of.insert(*def, id);
                self.assign(
                    Place::local(id),
                    Rvalue::Use(Operand::Copy(from.clone())),
                    span,
                );
            }
            PatternKind::At { binding, pattern } => {
                let id = self.new_local(Some(binding.name.clone()), ty.clone(), span);
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
            ExprKind::Lit(l) => Some(Rvalue::Use(Operand::Const(Constant::Value(lit_value(l))))),
            ExprKind::Local(_)
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Index { .. }
            | ExprKind::Deref { .. } => {
                let p = self.place_of(e)?;
                Some(Rvalue::Use(Operand::Copy(p)))
            }
            ExprKind::Global(def) => Some(Rvalue::Use(self.global_operand(*def))),
            // Monomorphization replaced every one of these with the literal the
            // instantiation chose. One still here is a program that did not get
            // that far.
            ExprKind::ConstParam(_) => Some(Rvalue::Use(Operand::Const(Constant::Undef))),
            ExprKind::Ref { mutable, place } => {
                let p = self.place_of(place)?;
                Some(Rvalue::Ref {
                    mutable: *mutable,
                    place: p,
                })
            }
            ExprKind::Binary { op, lhs, rhs } => Some(self.lower_binary(*op, lhs, rhs, &ty, span)),
            ExprKind::Unary { op, operand } => {
                let v = self.eval(operand);
                Some(Rvalue::Unary {
                    op: *op,
                    operand: v,
                })
            }
            ExprKind::Tuple { elems } => {
                let fields = elems.iter().map(|x| self.eval(x)).collect();
                Some(Rvalue::Aggregate {
                    kind: AggregateKind::Tuple,
                    fields,
                })
            }
            ExprKind::Construct { def, fields } => Some(self.lower_construct(*def, fields, &ty)),
            ExprKind::Variant { name, args } => Some(self.lower_variant(name, args, &ty)),
            ExprKind::DynCast { value, .. } => {
                let data = self.eval(value);
                let vt = self.dyn_vtable(e.id);
                Some(Rvalue::Aggregate {
                    kind: AggregateKind::Dyn,
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
                    base: Base::Global(def),
                    projection: Vec::new(),
                });
            }
            // A `::` constant *is* its value (§2.5) — the evaluator produced it
            // and every use carries it, which is why constants are not in
            // `Program::globals`.
            if let Some(v) = self.cx.meta.get::<ConstValue>(g.id) {
                return Operand::Const(Constant::Value(v));
            }
        }
        if let Some(f) = self.cx.linked.get(def) {
            let (name, symbol) = self.cx.names(f.id, &f.name);
            return Operand::Const(Constant::Func { def, name, symbol });
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
            self.terminate(Terminator {
                kind: TermKind::Switch {
                    value: a,
                    arms: vec![(1, t)],
                    otherwise: f,
                },
                span,
            });
            self.at = other;
            let b = self.eval(rhs);
            self.assign(place.clone(), Rvalue::Use(b), span);
            self.goto(join, span);
            self.at = join;
            return Rvalue::Use(Operand::Copy(place));
        }
        let a = self.eval(lhs);
        let b = self.eval(rhs);
        Rvalue::Binary {
            op,
            lhs: a,
            rhs: b,
        }
    }

    fn lower_construct(&mut self, def: DefId, fields: &[(Symbol, Expr)], ty: &Ty) -> Rvalue {
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
        Rvalue::Aggregate {
            kind: AggregateKind::Struct(def),
            fields: slots,
        }
    }

    fn lower_variant(&mut self, name: &Symbol, args: &[Expr], ty: &Ty) -> Rvalue {
        let fields: Vec<Operand> = args.iter().map(|a| self.eval(a)).collect();
        let Some((def, index)) = self.variant_index(ty, name) else {
            return Rvalue::Use(Operand::Const(Constant::Undef));
        };
        Rvalue::Aggregate {
            kind: AggregateKind::Variant {
                def,
                index,
                name: name.clone(),
            },
            fields,
        }
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
        Operand::Const(Constant::Vtable(id))
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
            if self.traps(op, &ty) {
                return Some(self.checked_op(op, vals, &ty, span));
            }
            return Some(Rvalue::Builtin {
                op,
                args: vals,
                checked: false,
            });
        }

        let vals: Vec<Operand> = args.iter().map(|a| self.eval(a)).collect();
        let callee = match dispatch {
            // Which function a vtable slot holds is a property of the vtable,
            // not of the call. Reaching it is two ordinary projections and an
            // indirect call — the trait has disappeared by this level (§9).
            Dispatch::Virtual { trait_def, method } => {
                self.vtable_slot(*trait_def, *method, vals.first(), span)?
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
        self.emit_call(callee, vals, ty, span)
    }

    /// A direct callee, by the symbol monomorphization decided.
    fn static_callee(&mut self, def: DefId) -> Callee {
        match self.cx.linked.get(def) {
            Some(f) => {
                let (name, symbol) = self.cx.names(f.id, &f.name);
                Callee::Static { def, name, symbol }
            }
            // A declaration with no lowered body still has a name to call.
            None => Callee::Static {
                def,
                name: self.cx.defs.canonical_string(def),
                symbol: self.cx.defs.get(def).name.clone(),
            },
        }
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
        let f = self.into_temp(
            Rvalue::Use(Operand::Copy(slot)),
            Ty::Ptr {
                mutable: false,
                inner: Box::new(Ty::Void),
            },
            span,
        );
        Some(Callee::Indirect(f))
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
            self.terminate(Terminator {
                kind: TermKind::Unreachable,
                span,
            });
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

    // ===< The overflow setting, made real (§7d) >===

    /// Whether this operation traps on overflow in this build.
    ///
    /// Only the integer operations that *can* leave their width, and only under
    /// `overflow=trap`. A float has no overflow to trap on — it has infinities —
    /// and a bitwise operation cannot leave its width at all.
    fn traps(&self, op: BuiltinOp, ty: &Ty) -> bool {
        self.cx.options.overflow == OverflowMode::Trap
            && self.is_integer(ty)
            && matches!(
                op,
                BuiltinOp::Add
                    | BuiltinOp::Sub
                    | BuiltinOp::Mul
                    | BuiltinOp::Div
                    | BuiltinOp::Rem
                    | BuiltinOp::Neg
                    | BuiltinOp::Shl
            )
    }

    /// Whether `ty` is an integer, **through any `distinct`s over one**.
    ///
    /// `usize` is the case that makes this necessary: since §3.1 it is
    /// `distinct uint.<PTR_BITS>` declared in `core` rather than a primitive, so
    /// a plain `Ty::is_int` says no about the commonest integer type a program
    /// writes. A `distinct T` has exactly `T`'s representation (§2.4), and
    /// overflow is a property of the representation.
    fn is_integer(&self, ty: &Ty) -> bool {
        self.is_integer_at(ty, 0)
    }

    fn is_integer_at(&self, ty: &Ty, depth: u32) -> bool {
        if ty.is_int() {
            return true;
        }
        // A `distinct` over a `distinct` is legal, and a cycle among them is
        // not — but it is `check::declarations` that says so, not this.
        if depth > 16 {
            return false;
        }
        let Ty::Nominal { def, .. } = ty else {
            return false;
        };
        if !matches!(
            self.cx.linked.ty(*def).map(|t| &t.kind),
            Some(TypeDefKind::Distinct { .. })
        ) {
            return false;
        }
        match self.cx.layouts.member_types(ty).as_deref() {
            Some([(_, repr)]) => self.is_integer_at(repr, depth + 1),
            _ => false,
        }
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
        self.assign(
            Place::local(pair),
            Rvalue::Builtin {
                op,
                args,
                checked: true,
            },
            span,
        );
        let flag = Place::local(pair).then(Projection::Field {
            index: 1,
            name: Symbol::new("overflowed"),
        });
        let trap = self.new_block(Some("overflow".to_string()));
        let ok = self.new_block(None);
        self.terminate(Terminator {
            kind: TermKind::Switch {
                value: Operand::Copy(flag),
                arms: vec![(1, trap)],
                otherwise: ok,
            },
            span,
        });

        self.at = trap;
        let sink = self.temp(Ty::Never, span);
        self.assign(
            Place::local(sink),
            Rvalue::Intrinsic {
                name: Symbol::new("panic"),
                args: vec![Operand::Const(Constant::Value(ConstValue::Str(
                    "integer overflow".to_string(),
                )))],
            },
            span,
        );
        self.terminate(Terminator {
            kind: TermKind::Unreachable,
            span,
        });

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
                Some(Rvalue::Use(Operand::Const(Constant::Value(
                    ConstValue::Int(n.into()),
                ))))
            }
            // A conversion between primitives. `from` travels beside `to`
            // because what the conversion *is* — a truncation, a sign extension,
            // a rounding — depends on both, and recovering it from the operand
            // would be codegen re-deriving a type this stage already had.
            "cast" if args.len() == 1 => {
                let from = self.cx.ty_of(args[0].id);
                let v = self.eval(&args[0]);
                Some(Rvalue::Cast {
                    value: v,
                    from,
                    to: ty,
                })
            }
            // A sequence's length: a fixed array's is part of its type and a
            // slice keeps it in its second member, so neither needs code.
            "len" if args.len() == 1 => {
                let arg = self.cx.ty_of(args[0].id);
                if let Ty::Array { len, .. } = arg {
                    return len.value().map(|n| {
                        Rvalue::Use(Operand::Const(Constant::Value(ConstValue::Int(n.into()))))
                    });
                }
                let p = self.place_of(&args[0])?;
                Some(Rvalue::Use(Operand::Copy(p.then(Projection::Field {
                    index: 1,
                    name: Symbol::new("len"),
                }))))
            }
            // A composite literal. It is an intrinsic in the IR because the
            // surface form is one syntax over two types; here the array case is
            // an aggregate like any other. A **slice** literal is not: its
            // elements need storage somewhere, and where that is is an
            // allocation question rather than a shape one.
            "array" if matches!(ty, Ty::Array { .. }) => {
                let fields = args.iter().map(|a| self.eval(a)).collect();
                Some(Rvalue::Aggregate {
                    kind: AggregateKind::Array,
                    fields,
                })
            }
            _ => {
                let vals: Vec<Operand> = args.iter().map(|a| self.eval(a)).collect();
                let value = Rvalue::Intrinsic {
                    name: name.clone(),
                    args: vals,
                };
                // `panic` and its neighbours return `never`: the block ends, for
                // the same reason a call to a `-> never` function does.
                if matches!(ty, Ty::Never) {
                    let sink = self.temp(Ty::Never, span);
                    self.assign(Place::local(sink), value, span);
                    self.terminate(Terminator {
                        kind: TermKind::Unreachable,
                        span,
                    });
                    return None;
                }
                Some(value)
            }
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
            ExprKind::Global(def) => {
                let g = self.cx.linked.global(*def)?;
                g.mutable.then(|| Place {
                    base: Base::Global(*def),
                    projection: Vec::new(),
                })
            }
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
            ExprKind::Index { base, index } => {
                let i = self.eval(index);
                let bty = self.cx.ty_of(base.id);
                let p = self.place_of(base)?;
                Some(self.index_into(p, &bty, i))
            }
            _ => {
                let ty = self.cx.ty_of(e.id);
                let span = self.cx.meta.span(e.id);
                let v = self.eval(e);
                match v {
                    Operand::Copy(p) => Some(p),
                    other => {
                        let slot = self.temp(ty, span);
                        self.assign(Place::local(slot), Rvalue::Use(other), span);
                        Some(Place::local(slot))
                    }
                }
            }
        }
    }

    /// Index a sequence.
    ///
    /// An **array** is indexed directly: it kept its own shape (§7b) precisely
    /// so that `base + i * stride` stays one operation on one aggregate.
    ///
    /// A **slice** is not an aggregate you can index. It flattened into
    /// `{ ptr, len }` (§7b), and a struct has members rather than elements — so
    /// the indexing happens *through the pointer it holds*, which is the whole
    /// content of that row in §7b's table. Emitting `s[i]` on the struct would
    /// be an offset into the two-word header.
    fn index_into(&mut self, place: Place, base_ty: &Ty, index: Operand) -> Place {
        match base_ty {
            Ty::Slice { .. } => place
                .then(Projection::Field {
                    index: 0,
                    name: Symbol::new("ptr"),
                })
                .then(Projection::Index(index)),
            // A pointer to a sequence that lowering did not deref for us, and a
            // bare `*T` being walked as an array: both index through the
            // pointee, which is what `Index` on a pointer means.
            Ty::Ptr { inner, .. } => {
                let inner = (**inner).clone();
                let p = place.then(Projection::Deref);
                self.index_into(p, &inner, index)
            }
            _ => place.then(Projection::Index(index)),
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
            Ty::Nominal { def, .. } => match self.cx.linked.ty(*def).map(|t| &t.kind) {
                Some(TypeDefKind::Struct { members }) => {
                    members.iter().map(|m| m.name.clone()).collect()
                }
                Some(TypeDefKind::Distinct { repr }) => vec![repr.name.clone()],
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
        let slot = (!matches!(ty, Ty::Void)).then(|| self.temp(ty, span));
        let c = self.eval(cond);
        let then_b = self.new_block(Some("then".to_string()));
        let else_b = self.new_block(Some("else".to_string()));
        let join = self.new_block(Some("join".to_string()));
        self.terminate(Terminator {
            kind: TermKind::Switch {
                value: c,
                arms: vec![(1, then_b)],
                otherwise: else_b,
            },
            span,
        });

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
        let slot = (!matches!(ty, Ty::Void)).then(|| self.temp(ty, span));
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
        match slot {
            Some(s) => Operand::local(s),
            None => Operand::Const(Constant::Undef),
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
        let slot = (!matches!(ty, Ty::Void)).then(|| self.temp(ty, span));
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
        let disc = self.into_temp(Rvalue::Discriminant(place.clone()), Ty::int(8, false), span);
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
        self.terminate(Terminator {
            kind: TermKind::Switch {
                value: disc,
                arms: targets,
                otherwise: default,
            },
            span,
        });

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
                let id = self.new_local(Some(binding.name.clone()), ty.clone(), span);
                self.local_of.insert(binding.def, id);
                self.assign(
                    Place::local(id),
                    Rvalue::Use(Operand::Copy(place.clone())),
                    span,
                );
                self.test_pattern(place, inner, ty, fail);
            }
            PatternKind::Lit(l) => {
                let c = self.into_temp(
                    Rvalue::Binary {
                        op: BinOp::Eq,
                        lhs: Operand::Copy(place.clone()),
                        rhs: Operand::Const(Constant::Value(lit_value(l))),
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
                if let Some(s) = start {
                    let c = self.into_temp(
                        Rvalue::Binary {
                            op: BinOp::Ge,
                            lhs: Operand::Copy(place.clone()),
                            rhs: Operand::Const(Constant::Value(lit_value(s))),
                        },
                        Ty::Bool,
                        span,
                    );
                    self.branch_if(c, fail, span);
                }
                if let Some(e) = end {
                    let op = if *inclusive { BinOp::Le } else { BinOp::Lt };
                    let c = self.into_temp(
                        Rvalue::Binary {
                            op,
                            lhs: Operand::Copy(place.clone()),
                            rhs: Operand::Const(Constant::Value(lit_value(e))),
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
                let disc =
                    self.into_temp(Rvalue::Discriminant(place.clone()), Ty::int(8, false), span);
                let ok = self.new_block(None);
                self.terminate(Terminator {
                    kind: TermKind::Switch {
                        value: disc,
                        arms: vec![(index as i128, ok)],
                        otherwise: fail,
                    },
                    span,
                });
                self.at = ok;
                let payload = place.clone().then(Projection::Variant {
                    index,
                    name: name.clone(),
                });
                let tys = self.variant_tys(ty, index);
                for (i, p) in sub.iter().enumerate() {
                    let f = payload.clone().then(Projection::Field {
                        index: i as u32,
                        name: Symbol::new(&i.to_string()),
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
        let payload = place.clone().then(Projection::Variant {
            index,
            name: name.clone(),
        });
        let tys = self.variant_tys(sty, index);
        for (i, p) in sub.iter().enumerate() {
            let f = payload.clone().then(Projection::Field {
                index: i as u32,
                name: Symbol::new(&i.to_string()),
            });
            let t = tys.get(i).cloned().unwrap_or(Ty::Error);
            self.test_pattern(&f, p, &t, fail);
        }
    }

    /// A slice pattern: a length test, then the elements it names from each end.
    #[allow(clippy::too_many_arguments)]
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
        let op = if rest.is_some() {
            BinOp::Ge
        } else {
            BinOp::Eq
        };
        let c = self.into_temp(
            Rvalue::Binary {
                op,
                lhs: len.clone(),
                rhs: Operand::Const(Constant::Value(ConstValue::Int(want.into()))),
            },
            Ty::Bool,
            span,
        );
        self.branch_if(c, fail, span);

        for (i, p) in prefix.iter().enumerate() {
            let f = place
                .clone()
                .then(Projection::Index(Operand::Const(Constant::Value(
                    ConstValue::Int(i.into()),
                ))));
            self.test_pattern(&f, p, &elem, fail);
        }
        // A suffix element is counted from the end, which is a *computed* index:
        // `len - k`. That is the run-time form, and it is why an array keeps its
        // own shape (§7b) rather than becoming a struct of members.
        for (k, p) in suffix.iter().enumerate() {
            let back = (suffix.len() - k) as i128;
            let usize_ty = self.cx.usize_ty();
            let idx = self.into_temp(
                Rvalue::Builtin {
                    op: BuiltinOp::Sub,
                    args: vec![
                        len.clone(),
                        Operand::Const(Constant::Value(ConstValue::Int(back.into()))),
                    ],
                    checked: false,
                },
                usize_ty,
                span,
            );
            let f = place.clone().then(Projection::Index(idx));
            self.test_pattern(&f, p, &elem, fail);
        }
        // The `..` segment, when it was named: the elements between the two
        // ends, as a slice over the address of the first of them.
        if let Some(Some(b)) = rest {
            let ty = self.cx.ty_of(b.id);
            let id = self.new_local(Some(b.name.clone()), ty, span);
            self.local_of.insert(b.def, id);
            let first = place
                .clone()
                .then(Projection::Index(Operand::Const(Constant::Value(
                    ConstValue::Int(prefix.len().into()),
                ))));
            let ptr = self.into_temp(
                Rvalue::Ref {
                    mutable: false,
                    place: first,
                },
                Ty::Ptr {
                    mutable: false,
                    inner: Box::new(Ty::Void),
                },
                span,
            );
            let usize_ty = self.cx.usize_ty();
            let n = self.into_temp(
                Rvalue::Builtin {
                    op: BuiltinOp::Sub,
                    args: vec![
                        len,
                        Operand::Const(Constant::Value(ConstValue::Int(want.into()))),
                    ],
                    checked: false,
                },
                usize_ty,
                span,
            );
            self.assign(
                Place::local(id),
                Rvalue::Aggregate {
                    kind: AggregateKind::Slice,
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
                Some(n) => Operand::Const(Constant::Value(ConstValue::Int(n.into()))),
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
