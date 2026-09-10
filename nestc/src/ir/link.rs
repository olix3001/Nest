//! Linking the per-file [`Program`]s into one whole-program view.
//!
//! Lowering is per file, and everything after it is not. Reachability starts at
//! `main` and walks wherever calls go; exhaustiveness needs every variant of an
//! enum that may be declared three files away; monomorphization collects
//! instantiations across the whole compilation. None of that can be expressed
//! against a `HashMap<FileId, Program>` without every pass re-implementing the
//! same "which file was that in?" search.
//!
//! [`Linked`] is that view: every function in the compilation, `core` included,
//! keyed by the [`DefId`] that names it.
//!
//! **The per-file [`Program`]s stay**. They are what `--emit=ir` and the IR
//! snapshot tests render, and they are the only place a reader can see one
//! file's lowering on its own. Linking therefore *clones* rather than consuming
//! them. The clone is cheap in the way that matters: [`IrId`](super::IrId)s are
//! preserved, so both copies of a node name the same row in the
//! [`Meta`](super::Meta) side table, and a fact recorded against one is visible
//! through the other.
//!
//! The consequence to keep in mind is that they are *snapshots*: a pass that
//! rewrites the IR — monomorphization is the first — changes [`Linked`] and
//! leaves the per-file programs as they were at the end of lowering. That is the
//! intent, not an oversight; the per-file view is a record of what lowering
//! produced.

use std::collections::HashMap;

use crate::common::source::FileId;
use crate::sema::def::DefId;

use super::{Function, Global, Program, TypeDef};

/// Every lowered function in the compilation, keyed by its [`DefId`].
///
/// A `DefId` is unique across the whole [`DefTable`](crate::sema::def::DefTable),
/// so two files cannot collide here however they are named or imported — which
/// is exactly why the map is keyed by it rather than by a path or a symbol.
#[derive(Debug, Clone, Default)]
pub struct Linked {
    /// Every type the whole compilation declares, keyed by the [`DefId`] a
    /// [`Ty::Nominal`](crate::sema::ty::Ty::Nominal) names it with. This is what
    /// turns "which type is this" into "what is in it" for layout,
    /// exhaustiveness and the LIR aggregate flattening.
    types: HashMap<DefId, TypeDef>,
    /// Every constant and static region the compilation declares, keyed by the
    /// [`DefId`] an [`ExprKind::Global`](super::ExprKind::Global) names it with.
    /// This is what turns "which constant is this" into "what is its value" for
    /// the const evaluator, and what codegen reads to emit a `.data` region.
    globals: HashMap<DefId, Global>,
    funcs: HashMap<DefId, Function>,
    /// The file each function was lowered from, so a whole-program pass can
    /// still raise a diagnostic against the right source. A function's *nodes*
    /// carry their own [`FileSpan`](crate::common::source::FileSpan)s; this is
    /// for the cases that name the function itself rather than a point inside
    /// it.
    file_of: HashMap<DefId, FileId>,
    /// Definition order: files by [`FileId`], and within a file the order
    /// lowering emitted them.
    ///
    /// A `HashMap`'s iteration order is deliberately unspecified and varies
    /// between runs. Passes that walk every function report diagnostics as they
    /// go, so iterating the map directly would put those diagnostics in a
    /// different order on every build — which makes a test that asserts on them
    /// flake rather than fail. Iteration goes through this instead.
    order: Vec<DefId>,
    /// Declaration order for [`Linked::types`], for the same reason.
    type_order: Vec<DefId>,
    /// Declaration order for [`Linked::globals`], for the same reason. It is
    /// also the order a static's initializer would run in, which is why it has
    /// to be the source order rather than a map's.
    global_order: Vec<DefId>,
}

/// Merge every per-file [`Program`] into one whole-program view.
pub fn link(ir: &HashMap<FileId, Program>) -> Linked {
    let mut files: Vec<FileId> = ir.keys().copied().collect();
    files.sort();

    let mut linked = Linked::default();
    for file in files {
        for ty in &ir[&file].types {
            linked.type_order.push(ty.def);
            linked.file_of.insert(ty.def, file);
            linked.types.insert(ty.def, ty.clone());
        }
        for g in &ir[&file].globals {
            linked.global_order.push(g.def);
            linked.file_of.insert(g.def, file);
            linked.globals.insert(g.def, g.clone());
        }
        for func in &ir[&file].funcs {
            linked.order.push(func.def);
            linked.file_of.insert(func.def, file);
            linked.funcs.insert(func.def, func.clone());
        }
    }
    linked
}

impl Linked {
    /// The function `def` names, if it was lowered.
    ///
    /// `None` for a def that is not a function, and for a trait method that only
    /// states a signature — those are reachable as vtable slots through their
    /// def but have no code of their own, so lowering does not emit them.
    pub fn get(&self, def: DefId) -> Option<&Function> {
        self.funcs.get(&def)
    }

    /// The function `def` names, for a pass that rewrites it.
    pub fn get_mut(&mut self, def: DefId) -> Option<&mut Function> {
        self.funcs.get_mut(&def)
    }

    /// Whether `def` names a lowered function.
    pub fn contains(&self, def: DefId) -> bool {
        self.funcs.contains_key(&def)
    }

    /// The file `def` was lowered from.
    pub fn file_of(&self, def: DefId) -> Option<FileId> {
        self.file_of.get(&def).copied()
    }

    /// How many functions the program holds.
    pub fn len(&self) -> usize {
        self.funcs.len()
    }

    /// Whether the program holds no functions at all — which, since `core` is
    /// always linked in, means analysis never got as far as lowering.
    pub fn is_empty(&self) -> bool {
        self.funcs.is_empty()
    }

    /// The definition of the type `def` names, if the program declares one.
    ///
    /// `None` for a trait, for a plain type alias (`A :: B` defines no new type
    /// — every use of it resolved to `B`), and for a def that names no type at
    /// all.
    pub fn ty(&self, def: DefId) -> Option<&TypeDef> {
        self.types.get(&def)
    }

    /// Every type definition, in declaration order.
    pub fn types(&self) -> impl Iterator<Item = &TypeDef> {
        self.type_order.iter().map(|d| &self.types[d])
    }

    /// How many types the program declares.
    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    /// The constant or static region `def` names, if the program declares one.
    ///
    /// `None` for a def that names no value — a function, a type, a local — and
    /// for a **trait's** associated constant, which is a requirement an impl
    /// must satisfy rather than a definition with a value of its own; those ride
    /// on the trait's [`TypeDef`].
    pub fn global(&self, def: DefId) -> Option<&Global> {
        self.globals.get(&def)
    }

    /// Every constant and static region, in declaration order.
    pub fn globals(&self) -> impl Iterator<Item = &Global> {
        self.global_order.iter().map(|d| &self.globals[d])
    }

    /// Every function, in definition order (see [`Linked::order`]).
    pub fn funcs(&self) -> impl Iterator<Item = &Function> {
        self.order.iter().map(|d| &self.funcs[d])
    }

    /// Every function's [`DefId`], in definition order. Useful to a pass that
    /// needs to hold the ids while mutating the map.
    pub fn defs(&self) -> impl Iterator<Item = DefId> + '_ {
        self.order.iter().copied()
    }

    /// Insert a function that has no per-file program behind it — what
    /// monomorphization does with each instantiation it creates.
    pub fn insert(&mut self, file: FileId, func: Function) {
        if !self.funcs.contains_key(&func.def) {
            self.order.push(func.def);
        }
        self.file_of.insert(func.def, file);
        self.funcs.insert(func.def, func);
    }
}
