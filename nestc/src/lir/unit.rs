//! The codegen-unit split (`design/lir.md` §11).
//!
//! Lowering produces **one** whole-program [`Unit`]: one type table, one list of
//! globals, one list of functions. That is the right shape to compute in — the
//! type table is the closure of what the functions mention, and a safepoint's
//! live set is a property of the finished graph — and the wrong shape to *emit*
//! from, because emitting is the part that parallelizes and a single unit is a
//! single thread.
//!
//! So the last thing the lowering does is cut the program up. The cut is a
//! **filter**, not a redesign, and it is the module note in [`super`] that makes
//! it one: a unit refers to a type by an index into its own table, to a function
//! by an index into its own list, and to a global the same way. Nothing points
//! outside, so a unit is built by walking what its functions reach, copying
//! that, and renumbering.
//!
//! # How the partition is chosen
//!
//! By **source file**, then merged. A file is what a person wrote and what a
//! person recompiles, so it is the partition that makes an incremental build
//! rebuild what changed; and `-C codegen-units=N` then merges the smallest units
//! together until there are no more than `N` of them, which is what keeps a
//! program of two hundred files from producing two hundred object files.
//!
//! It is deliberately the same shape rustc uses (one unit per module, merged
//! down to `-C codegen-units`), for the same two reasons: the merge is what
//! bounds the count, and merging the *smallest* is what keeps the units within
//! reach of each other in size, which is what decides how long the slowest
//! thread takes.
//!
//! # What a unit carries that it does not define
//!
//! A **declaration** for every function it calls and every global it reads. That
//! is the whole of what linking needs from this side: a name, a signature and a
//! symbol, which is exactly a [`Function`] with no blocks (§1) and a [`Global`]
//! whose linkage is [`Linkage::Imported`]. A backend emits those as `extern` and
//! lets the linker find the definition in whichever object file got it.
//!
//! The exception is a global **no file wrote**: a string's bytes, an array
//! constant's contents, a vtable. Nothing can name one, so there is nothing for
//! a linker to resolve and no reason for one unit to depend on another's
//! anonymous data — each unit that needs those bytes gets its own private copy
//! ([`Linkage::Internal`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::common::source::{FileId, SourceMap};

use super::{
    Aggregate, Base, Callee, Constant, FuncId, Function, Global, GlobalId, Linkage, Operand,
    Origin, Place, Program, Rvalue, Stmt, StmtKind, TermKind, Ty, TypeDef, TypeId, Unit,
};

/// Cut the whole-program unit into at most `n` codegen units.
///
/// `whole_program` says whether this compilation is every caller there will
/// ever be. It is false for a library, and then nothing is internalized: see
/// [`internalize`].
pub fn split(mut whole: Unit, n: usize, sources: &SourceMap, whole_program: bool) -> Program {
    let groups = partition(&whole, n.max(1), sources);
    if whole_program {
        internalize(&mut whole, &groups);
    }
    let only = groups.len() == 1;
    let units = groups
        .into_iter()
        .enumerate()
        .map(|(i, g)| build(&whole, &g, i, only))
        .collect();
    Program { units }
}

/// One partition: what it is called, and which of the whole program's functions
/// it defines.
struct Group {
    /// The source files whose functions it holds.
    files: Vec<String>,
    funcs: Vec<FuncId>,
}

impl Group {
    /// What the unit is called. One file names itself, two name both, and more
    /// than two name the first two and count the rest — a name made of forty
    /// file names is not a name.
    fn name(&self, only: bool) -> String {
        match self.files.len() {
            0 => "program".to_string(),
            // Everything in one unit is the whole program, and saying so reads
            // better than naming whichever file happened to sort first.
            _ if only && self.files.len() > 2 => "program".to_string(),
            1 => self.files[0].clone(),
            2 => format!("{}+{}", self.files[0], self.files[1]),
            n => format!("{}+{}+{} more", self.files[0], self.files[1], n - 2),
        }
    }
}

/// Group the defined functions by source file, then merge the smallest groups
/// until there are at most `n`.
fn partition(whole: &Unit, n: usize, sources: &SourceMap) -> Vec<Group> {
    let mut by_file: BTreeMap<Option<FileId>, Vec<FuncId>> = BTreeMap::new();
    for (i, f) in whole.funcs.iter().enumerate() {
        // A declaration defines nothing, so it belongs to whichever units call
        // it rather than to one of its own.
        if f.blocks.is_empty() {
            continue;
        }
        by_file
            .entry(f.span.map(|s| s.file))
            .or_default()
            .push(FuncId(i as u32));
    }
    let mut groups: Vec<Group> = by_file
        .into_iter()
        .map(|(file, funcs)| Group {
            files: vec![unit_name(file, sources)],
            funcs,
        })
        .collect();
    // A program with no code at all still produces one unit: an empty object
    // file is a fact about the program, and nothing downstream should have to
    // handle "no units".
    if groups.is_empty() {
        return vec![Group {
            files: Vec::new(),
            funcs: Vec::new(),
        }];
    }
    // Merge the two smallest until the count fits. Smallest first is what keeps
    // the units comparable in size, which is what decides the slowest thread.
    while groups.len() > n {
        groups.sort_by(|a, b| {
            a.funcs
                .len()
                .cmp(&b.funcs.len())
                .then_with(|| a.files.cmp(&b.files))
        });
        let victim = groups.remove(0);
        groups[0].funcs.extend(victim.funcs);
        groups[0].files.extend(victim.files);
    }
    // Back into a stable order: a dump and an object file's name should not
    // depend on how the merge happened to walk.
    groups.sort_by(|a, b| a.funcs.first().cmp(&b.funcs.first()));
    for g in &mut groups {
        g.funcs.sort();
        g.files.sort();
        g.files.dedup();
    }
    groups
}

/// Mark every function no other unit names, so a backend can make its symbol
/// local to the object file (`FunctionAttrs::internal`).
///
/// **This is the whole of what the compiler does about calling conventions for
/// its own functions.** An ordinary Nest function is called the C way, because
/// C's is the one convention the machine's tools all agree on — and then LLVM
/// promotes an internal function whose every use is a direct call to its own
/// fast convention, rewriting the definition and every call site in one step.
/// Doing it here instead would mean answering "is this address ever taken" for
/// a vtable slot, a function pointer and a dependent package that has not been
/// compiled yet, and getting it wrong is a miscompile rather than a diagnostic
/// (§11). This is the same division rustc draws: its front end emits the C
/// convention and its partitioning internalizes.
///
/// A function stays external when:
///
/// - it is `@public` — another compilation may name it;
/// - it has an `extern` ABI, or `#offset(N)` — a linker or a C caller names it;
/// - it is [`FunctionAttrs::shared`], an instantiation several objects may each
///   define and the linker folds;
/// - its address appears in a **global's initializer** — a vtable, say, which is
///   private data every unit that needs it gets a copy of, so the reference can
///   turn up in a unit this cannot name;
/// - or any function outside its own unit refers to it.
///
/// None of those five reasons can see a **downstream package**, which is why
/// this runs only for a whole-program build. The packages compiled against a
/// library are not here, and `@public` does not name everything they can reach:
/// a trait impl's method carries no visibility of its own and is reachable
/// wherever the trait and the type are. Internalizing one is not a missed
/// optimization but a link error — the backend deletes an `internal` function
/// nothing in *this* compilation calls, and the caller arrives later.
fn internalize(whole: &mut Unit, groups: &[Group]) {
    // Which unit each defined function landed in.
    let mut home: HashMap<u32, usize> = HashMap::new();
    for (i, g) in groups.iter().enumerate() {
        for f in &g.funcs {
            home.insert(f.0, i);
        }
    }
    // Which units refer to each function, and which functions a global's
    // initializer names.
    let mut from: HashMap<u32, BTreeSet<usize>> = HashMap::new();
    for (i, g) in groups.iter().enumerate() {
        let mut refs = Refs::default();
        for f in &g.funcs {
            collect_func(whole, &whole.funcs[f.0 as usize], &mut refs);
        }
        for f in refs.funcs {
            from.entry(f).or_default().insert(i);
        }
    }
    let mut in_data = Refs::default();
    for g in &whole.globals {
        if let Some(init) = &g.init {
            collect_const(&mut in_data, init);
        }
    }
    for (i, f) in whole.funcs.iter_mut().enumerate() {
        let i = i as u32;
        // A declaration names a definition in another object, which is exactly
        // what internal linkage would hide.
        if f.blocks.is_empty() {
            continue;
        }
        if f.attrs.public
            || f.attrs.shared
            || f.extern_abi.is_some()
            || f.attrs.offset.is_some()
            || in_data.funcs.contains(&i)
        {
            continue;
        }
        let Some(&mine) = home.get(&i) else { continue };
        if from.get(&i).is_some_and(|us| us.iter().any(|&u| u != mine)) {
            continue;
        }
        f.attrs.internal = true;
    }
}

/// What one file's unit is called: the file's base name, without its directory,
/// because the directory the compiler ran in says nothing about the program.
fn unit_name(file: Option<FileId>, sources: &SourceMap) -> String {
    let Some(file) = file.and_then(|f| sources.file(f)) else {
        return "program".to_string();
    };
    let base = file.name.rsplit('/').next().unwrap_or(&file.name);
    base.strip_suffix(".nest").unwrap_or(base).to_string()
}

/// Everything one unit refers to, gathered as it is found.
#[derive(Default)]
struct Refs {
    funcs: BTreeSet<u32>,
    globals: BTreeSet<u32>,
    types: BTreeSet<u32>,
}

/// Build one unit: the functions it defines, a declaration for everything else
/// it names, and the types and globals under those.
fn build(whole: &Unit, group: &Group, index: usize, only: bool) -> Unit {
    let mut refs = Refs::default();
    for f in &group.funcs {
        refs.funcs.insert(f.0);
        collect_func(whole, &whole.funcs[f.0 as usize], &mut refs);
    }
    // A `#static` the source wrote belongs to the unit its file belongs to,
    // whether or not that unit's code reads it: it is a definition, and a
    // definition that nothing in its own unit mentions is still a definition.
    for (i, g) in whole.globals.iter().enumerate() {
        if g.linkage == Linkage::External && home_unit(whole, g, index, group) {
            refs.globals.insert(i as u32);
            collect_ty(whole, &g.ty, &mut refs);
            if let Some(init) = &g.init {
                collect_const(&mut refs, init);
            }
        }
    }
    // The closure: a type's members name types, a global's initializer names
    // functions and globals, and each of those brings its own.
    close(whole, &mut refs);

    // Renumbering. The order is the whole program's, so a unit's tables read in
    // the same order the program's do.
    let type_map: HashMap<u32, TypeId> = refs
        .types
        .iter()
        .enumerate()
        .map(|(i, &t)| (t, TypeId(i as u32)))
        .collect();
    let func_map: HashMap<u32, FuncId> = refs
        .funcs
        .iter()
        .enumerate()
        .map(|(i, &f)| (f, FuncId(i as u32)))
        .collect();
    let global_map: HashMap<u32, GlobalId> = refs
        .globals
        .iter()
        .enumerate()
        .map(|(i, &g)| (g, GlobalId(i as u32)))
        .collect();
    let m = Maps {
        types: type_map,
        funcs: func_map,
        globals: global_map,
    };

    let defined: BTreeSet<u32> = group.funcs.iter().map(|f| f.0).collect();
    let types = refs
        .types
        .iter()
        .map(|&t| remap_type(&whole.types[t as usize], &m))
        .collect();
    let globals = refs
        .globals
        .iter()
        .map(|&g| {
            let g = &whole.globals[g as usize];
            // Private data belongs to every unit that uses it; a `#static`
            // belongs to one, and the rest import it.
            let mine = g.linkage == Linkage::Internal || home_unit(whole, g, index, group);
            remap_global(g, &m, !mine)
        })
        .collect();
    let funcs = refs
        .funcs
        .iter()
        .map(|&f| {
            let func = &whole.funcs[f as usize];
            remap_func(func, &m, defined.contains(&f))
        })
        .collect();
    Unit {
        name: group.name(only),
        types,
        globals,
        funcs,
    }
}

/// Whether this unit is where a source-written global is *defined*.
///
/// Its file decides, the same way a function's does. A global whose file is in
/// no unit — which only happens when its file defined no functions at all —
/// lands in the first unit, so that exactly one unit defines it.
fn home_unit(whole: &Unit, g: &Global, index: usize, group: &Group) -> bool {
    let Some(span) = g.span else {
        return index == 0;
    };
    let mine = group.funcs.iter().any(|f| {
        whole.funcs[f.0 as usize]
            .span
            .is_some_and(|s| s.file == span.file)
    });
    if mine {
        return true;
    }
    // No unit claims the file: the first one takes it.
    let claimed = whole
        .funcs
        .iter()
        .any(|f| !f.blocks.is_empty() && f.span.is_some_and(|s| s.file == span.file));
    !claimed && index == 0
}

/// Follow every reference until nothing new appears.
fn close(whole: &Unit, refs: &mut Refs) {
    loop {
        let before = (refs.funcs.len(), refs.globals.len(), refs.types.len());
        let types: Vec<u32> = refs.types.iter().copied().collect();
        for t in types {
            let def = &whole.types[t as usize];
            for m in &def.members {
                collect_ty(whole, &m.ty, refs);
            }
            if let Origin::Enum { variants } = &def.origin {
                for v in variants {
                    refs.types.insert(v.ty.0);
                }
            }
        }
        let globals: Vec<u32> = refs.globals.iter().copied().collect();
        for g in globals {
            let global = &whole.globals[g as usize];
            collect_ty(whole, &global.ty, refs);
            if let Some(init) = &global.init {
                collect_const(refs, init);
            }
        }
        // A declaration's signature is part of what the unit needs to call it.
        let funcs: Vec<u32> = refs.funcs.iter().copied().collect();
        for f in funcs {
            let func = &whole.funcs[f as usize];
            collect_ty(whole, &func.ret, refs);
            for l in &func.locals[..func.params.min(func.locals.len())] {
                collect_ty(whole, &l.ty, refs);
            }
        }
        if (refs.funcs.len(), refs.globals.len(), refs.types.len()) == before {
            return;
        }
    }
}

fn collect_func(whole: &Unit, f: &Function, refs: &mut Refs) {
    collect_ty(whole, &f.ret, refs);
    for l in &f.locals {
        collect_ty(whole, &l.ty, refs);
    }
    for b in &f.blocks {
        for s in &b.stmts {
            collect_stmt(whole, s, refs);
        }
        match &b.term.kind {
            TermKind::Switch { value, ty, .. } => {
                collect_operand(whole, value, refs);
                collect_ty(whole, ty, refs);
            }
            TermKind::Return(Some(v)) => collect_operand(whole, v, refs),
            _ => {}
        }
    }
}

fn collect_stmt(whole: &Unit, s: &Stmt, refs: &mut Refs) {
    match &s.kind {
        StmtKind::Assign { place, value } => {
            collect_place(whole, place, refs);
            match value {
                Rvalue::Use(o) => collect_operand(whole, o, refs),
                Rvalue::Ref(p) => collect_place(whole, p, refs),
                Rvalue::Op { ty, args, .. } => {
                    collect_ty(whole, ty, refs);
                    for a in args {
                        collect_operand(whole, a, refs);
                    }
                }
                Rvalue::Cast {
                    value, from, to, ..
                } => {
                    collect_operand(whole, value, refs);
                    collect_ty(whole, from, refs);
                    collect_ty(whole, to, refs);
                }
                Rvalue::Aggregate { kind, fields } => {
                    match kind {
                        Aggregate::Struct(t) => {
                            refs.types.insert(t.0);
                        }
                        Aggregate::Variant { ty, variant, .. } => {
                            refs.types.insert(ty.0);
                            refs.types.insert(variant.0);
                        }
                        Aggregate::Array => {}
                    }
                    for f in fields {
                        collect_operand(whole, f, refs);
                    }
                }
                Rvalue::Offset { ptr, index, .. } => {
                    collect_operand(whole, ptr, refs);
                    collect_operand(whole, index, refs);
                }
            }
        }
        StmtKind::Call { dest, callee, args } => {
            if let Some(d) = dest {
                collect_place(whole, d, refs);
            }
            match callee {
                Callee::Static(f) => {
                    refs.funcs.insert(f.0);
                }
                Callee::Indirect(o) => collect_operand(whole, o, refs),
                Callee::Intrinsic(_) => {}
            }
            for a in args {
                collect_operand(whole, a, refs);
            }
        }
        StmtKind::Drop(o) => collect_operand(whole, o, refs),
    }
}

fn collect_operand(whole: &Unit, o: &Operand, refs: &mut Refs) {
    match o {
        Operand::Copy(p) => collect_place(whole, p, refs),
        Operand::Const(c) => collect_const(refs, c),
    }
}

fn collect_const(refs: &mut Refs, c: &Constant) {
    match c {
        Constant::Func(f) => {
            refs.funcs.insert(f.0);
        }
        Constant::Global(g) => {
            refs.globals.insert(g.0);
        }
        Constant::Aggregate(items) | Constant::Variant { payload: items, .. } => {
            for i in items {
                collect_const(refs, i);
            }
        }
        _ => {}
    }
}

fn collect_place(whole: &Unit, p: &Place, refs: &mut Refs) {
    if let Base::Global(g) = p.base {
        refs.globals.insert(g.0);
    }
    for proj in &p.projection {
        match proj {
            super::Projection::Index(i) => collect_operand(whole, i, refs),
            super::Projection::Cast(t) => {
                refs.types.insert(t.0);
            }
            _ => {}
        }
    }
}

fn collect_ty(whole: &Unit, ty: &Ty, refs: &mut Refs) {
    match ty {
        Ty::Named(id) => {
            if refs.types.insert(id.0) {
                let def = &whole.types[id.0 as usize];
                for m in &def.members {
                    collect_ty(whole, &m.ty, refs);
                }
                if let Origin::Enum { variants } = &def.origin {
                    for v in variants {
                        collect_ty(whole, &Ty::Named(v.ty), refs);
                    }
                }
            }
        }
        Ty::Ptr(inner) => collect_ty(whole, inner, refs),
        Ty::Array { elem, .. } => collect_ty(whole, elem, refs),
        Ty::Func { params, ret } => {
            for p in params {
                collect_ty(whole, p, refs);
            }
            collect_ty(whole, ret, refs);
        }
        _ => {}
    }
}

// ===< Renumbering >===

struct Maps {
    types: HashMap<u32, TypeId>,
    funcs: HashMap<u32, FuncId>,
    globals: HashMap<u32, GlobalId>,
}

impl Maps {
    fn ty(&self, id: TypeId) -> TypeId {
        self.types.get(&id.0).copied().unwrap_or(TypeId(0))
    }

    fn func(&self, id: FuncId) -> FuncId {
        self.funcs.get(&id.0).copied().unwrap_or(FuncId(0))
    }

    fn global(&self, id: GlobalId) -> GlobalId {
        self.globals.get(&id.0).copied().unwrap_or(GlobalId(0))
    }
}

fn remap_type(t: &TypeDef, m: &Maps) -> TypeDef {
    let mut t = t.clone();
    t.id = m.ty(t.id);
    for member in &mut t.members {
        member.ty = remap_ty(&member.ty, m);
    }
    if let Origin::Enum { variants } = &mut t.origin {
        for v in variants {
            v.ty = m.ty(v.ty);
        }
    }
    t
}

fn remap_global(g: &Global, m: &Maps, imported: bool) -> Global {
    let mut g = g.clone();
    g.ty = remap_ty(&g.ty, m);
    if imported {
        // A unit that only *refers* to a definition carries no copy of its
        // contents: two units emitting the same bytes under one linker-visible
        // symbol is two definitions, which is what a linker refuses.
        g.init = None;
        g.linkage = Linkage::Imported;
    } else {
        g.init = g.init.as_ref().map(|c| remap_const(c, m));
    }
    g
}

fn remap_func(f: &Function, m: &Maps, defined: bool) -> Function {
    let mut f = f.clone();
    f.ret = remap_ty(&f.ret, m);
    if !defined {
        // Everything but the signature belongs to the unit that defines it.
        f.blocks.clear();
        f.locals.truncate(f.params);
    }
    for l in &mut f.locals {
        l.ty = remap_ty(&l.ty, m);
    }
    for b in &mut f.blocks {
        for s in &mut b.stmts {
            remap_stmt(s, m);
        }
        if let TermKind::Switch { value, ty, .. } = &mut b.term.kind {
            remap_operand(value, m);
            *ty = remap_ty(ty, m);
        }
        if let TermKind::Return(Some(v)) = &mut b.term.kind {
            remap_operand(v, m);
        }
    }
    f
}

fn remap_stmt(s: &mut Stmt, m: &Maps) {
    match &mut s.kind {
        StmtKind::Assign { place, value } => {
            remap_place(place, m);
            match value {
                Rvalue::Use(o) => remap_operand(o, m),
                Rvalue::Ref(p) => remap_place(p, m),
                Rvalue::Op { ty, args, .. } => {
                    *ty = remap_ty(ty, m);
                    for a in args {
                        remap_operand(a, m);
                    }
                }
                Rvalue::Cast {
                    value, from, to, ..
                } => {
                    remap_operand(value, m);
                    *from = remap_ty(from, m);
                    *to = remap_ty(to, m);
                }
                Rvalue::Aggregate { kind, fields } => {
                    match kind {
                        Aggregate::Struct(t) => *t = m.ty(*t),
                        Aggregate::Variant { ty, variant, .. } => {
                            *ty = m.ty(*ty);
                            *variant = m.ty(*variant);
                        }
                        Aggregate::Array => {}
                    }
                    for f in fields {
                        remap_operand(f, m);
                    }
                }
                Rvalue::Offset { ptr, index, .. } => {
                    remap_operand(ptr, m);
                    remap_operand(index, m);
                }
            }
        }
        StmtKind::Call { dest, callee, args } => {
            if let Some(d) = dest {
                remap_place(d, m);
            }
            match callee {
                Callee::Static(f) => *f = m.func(*f),
                Callee::Indirect(o) => remap_operand(o, m),
                Callee::Intrinsic(_) => {}
            }
            for a in args {
                remap_operand(a, m);
            }
        }
        StmtKind::Drop(o) => remap_operand(o, m),
    }
}

fn remap_operand(o: &mut Operand, m: &Maps) {
    match o {
        Operand::Copy(p) => remap_place(p, m),
        Operand::Const(c) => *c = remap_const(c, m),
    }
}

fn remap_const(c: &Constant, m: &Maps) -> Constant {
    match c {
        Constant::Func(f) => Constant::Func(m.func(*f)),
        Constant::Global(g) => Constant::Global(m.global(*g)),
        Constant::Aggregate(items) => {
            Constant::Aggregate(items.iter().map(|i| remap_const(i, m)).collect())
        }
        Constant::Variant { tag, name, payload } => Constant::Variant {
            tag: *tag,
            name: name.clone(),
            payload: payload.iter().map(|i| remap_const(i, m)).collect(),
        },
        other => other.clone(),
    }
}

fn remap_place(p: &mut Place, m: &Maps) {
    if let Base::Global(g) = &mut p.base {
        *g = m.global(*g);
    }
    for proj in &mut p.projection {
        match proj {
            super::Projection::Index(i) => remap_operand(i, m),
            super::Projection::Cast(t) => *t = m.ty(*t),
            _ => {}
        }
    }
}

fn remap_ty(ty: &Ty, m: &Maps) -> Ty {
    match ty {
        Ty::Named(id) => Ty::Named(m.ty(*id)),
        Ty::Ptr(inner) => Ty::ptr(remap_ty(inner, m)),
        Ty::Array { len, elem } => Ty::Array {
            len: *len,
            elem: Box::new(remap_ty(elem, m)),
        },
        Ty::Func { params, ret } => Ty::Func {
            params: params.iter().map(|p| remap_ty(p, m)).collect(),
            ret: Box::new(remap_ty(ret, m)),
        },
        other => other.clone(),
    }
}
