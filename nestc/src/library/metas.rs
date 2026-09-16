//! The facts the passes leave on a tree, as data a library's metadata can carry.
//!
//! A [`MetaStore`] is type-indexed, so it cannot be walked without knowing the
//! types — and the types are this list. **Every type any pass stores is here**,
//! in one of two places: [`MetaValue`], for a fact a package compiled against
//! this one reads, or [`DERIVED`], for a cache a later pass rebuilds on demand.
//! A type in neither is refused when a store is written out, by name, so a new
//! pass storing a new fact cannot quietly produce metadata without it.

use std::any::TypeId;
use std::hash::Hash;

use serde::{Deserialize, Serialize};

use crate::common::meta::MetaStore;
use crate::common::source::FileSpan;
use crate::ir::const_eval::ConstValue;
use crate::ir::{DefaultValue, ImplicitCast};
use crate::sema::def::Directive;
use crate::sema::infer::{
    ArgOrder, Coercion, ConstSlotReported, DistinctRecv, DynCoerce, Generics, IndexWrite,
    Instantiation, MethodRes, OpResolution, RangeReported, SliceCoerce, TyPathReported, Upcast,
};
use crate::sema::ty::Ty;
use crate::sema::{DefMeta, PathRes, Resolution, Signature, SpreadBase};

macro_rules! persisted {
    ($($name:ident($ty:ty)),* $(,)?) => {
        /// One fact, of any persisted type.
        #[derive(Serialize, Deserialize)]
        pub enum MetaValue {
            $($name($ty)),*
        }

        /// Every persisted fact in `store` whose key `keep` accepts.
        ///
        /// Fails, naming the type, when the store holds a type that is neither
        /// persisted nor [`DERIVED`].
        pub fn export<K: Eq + Hash + Clone>(
            store: &MetaStore<K>,
            keep: impl Fn(&K) -> bool,
        ) -> Result<Vec<(K, MetaValue)>, String> {
            let known = [$(TypeId::of::<$ty>()),*];
            for (id, name) in store.kinds() {
                if !known.contains(&id) && !DERIVED.iter().any(|d| d() == id) {
                    return Err(format!(
                        "`{name}` is stored on the tree but is neither persisted nor derived \
                         (`library::metas`)"
                    ));
                }
            }
            let mut out = Vec::new();
            $(
                for (k, v) in store.entries::<$ty>() {
                    if keep(&k) {
                        out.push((k, MetaValue::$name(v)));
                    }
                }
            )*
            Ok(out)
        }

        /// Put facts read back from metadata into `store`.
        pub fn import<K: Eq + Hash + Clone>(store: &MetaStore<K>, facts: Vec<(K, MetaValue)>) {
            for (k, v) in facts {
                match v {
                    $(MetaValue::$name(v) => { store.set(k, v); })*
                }
            }
        }
    };
}

persisted! {
    Resolution(Resolution),
    PathRes(PathRes),
    DefMeta(DefMeta),
    Signature(Signature),
    SpreadBase(SpreadBase),
    Ty(Ty),
    FileSpan(FileSpan),
    Directives(Vec<Directive>),
    ConstValue(ConstValue),
    DefaultValue(DefaultValue),
    ImplicitCast(ImplicitCast),
    Generics(Generics),
    Instantiation(Instantiation),
    MethodRes(MethodRes),
    OpResolution(OpResolution),
    Coercion(Coercion),
    SliceCoerce(SliceCoerce),
    DynCoerce(DynCoerce),
    Upcast(Upcast),
    DistinctRecv(DistinctRecv),
    ArgOrder(ArgOrder),
    IndexWrite(IndexWrite),
    RangeReported(RangeReported),
    ConstSlotReported(ConstSlotReported),
    TyPathReported(TyPathReported),
}

/// Caches, and what monomorphization decides: rebuilt by whoever asks, so a
/// library does not carry them.
pub const DERIVED: &[fn() -> TypeId] = &[
    || TypeId::of::<crate::ir::layout::Layout>(),
    || TypeId::of::<crate::ir::check::declarations::RecursiveLayout>(),
    || TypeId::of::<crate::ir::mono::Instance>(),
    || TypeId::of::<crate::ir::mono::VtableSlots>(),
    || TypeId::of::<crate::ir::mono::MemberVtables>(),
];
