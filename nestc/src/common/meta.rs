//! A type-indexed side table: `key -> at most one value per Rust type`.
//!
//! Two stages of the compiler need the same structure for the same reason. The
//! AST keys it by [`NodeId`](crate::parser::ast::NodeId), the IR by
//! [`IrId`](crate::ir::IrId), and in both cases the point is that a *later* pass
//! records what it computed without the node ever growing a field for it — a
//! `Resolution` from name resolution, a `Ty` from inference, a span from
//! lowering, a layout from the layout pass. Nodes stay the shape their own
//! stage gave them, and adding a consumer costs nothing to the producer.
//!
//! Values are keyed by `(K, TypeId::of::<T>())`, so any number of passes annotate
//! the same node with different types and none of them collide.
//!
//! Two properties are deliberate and both stages depend on them:
//!
//! - **Interior mutability.** A pass walking a tree holds a shared borrow of it;
//!   requiring `&mut` to annotate would mean either a second walk or threading a
//!   mutable store through every visitor method.
//! - **Metadata is derived state.** [`Clone`] yields an *empty* store and
//!   `serde` skips it: a cloned or deserialized tree has not run the passes that
//!   filled the table, so carrying stale facts across would be worse than
//!   carrying none. Re-run the pass if you need them on the copy.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;

/// A side table mapping `K` to at most one value per Rust type.
///
/// ```ignore
/// store.set(node, Resolution::Def(def));   // in name resolution
/// store.set(node, ty);                     // in the type checker
/// let ty: Option<Ty> = store.get(node);    // in lowering
/// ```
pub struct MetaStore<K> {
    tables: RefCell<HashMap<TypeId, HashMap<K, Box<dyn Any>>>>,
    /// Each table's Rust type name, for a store enumerated by type
    /// ([`MetaStore::kinds`]) to say which one it does not know.
    names: RefCell<HashMap<TypeId, &'static str>>,
}

impl<K> Default for MetaStore<K> {
    fn default() -> Self {
        Self {
            tables: RefCell::new(HashMap::new()),
            names: RefCell::new(HashMap::new()),
        }
    }
}

/// Cloning yields an empty store: see the module docs on derived state.
impl<K> Clone for MetaStore<K> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl<K> fmt::Debug for MetaStore<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count: usize = self.tables.borrow().values().map(HashMap::len).sum();
        write!(f, "MetaStore({count} entries)")
    }
}

impl<K: Eq + Hash> MetaStore<K> {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a `T` to `key`, replacing and returning any previous `T`.
    pub fn set<T: Any>(&self, key: K, value: T) -> Option<T> {
        self.names
            .borrow_mut()
            .entry(TypeId::of::<T>())
            .or_insert_with(std::any::type_name::<T>);
        self.tables
            .borrow_mut()
            .entry(TypeId::of::<T>())
            .or_default()
            .insert(key, Box::new(value))
            .and_then(|old| old.downcast::<T>().ok().map(|b| *b))
    }

    /// Clone out the `T` attached to `key`. Convenient for small `Copy`/`Clone`
    /// payloads; use [`MetaStore::with`] to avoid the clone.
    pub fn get<T: Any + Clone>(&self, key: &K) -> Option<T> {
        self.with::<T, _>(key, T::clone)
    }

    /// Borrow the `T` attached to `key` and run `f` on it, returning `f`'s
    /// result (or `None` when no `T` is attached). The store's borrow is
    /// released before `f`'s result is returned.
    pub fn with<T: Any, R>(&self, key: &K, f: impl FnOnce(&T) -> R) -> Option<R> {
        let tables = self.tables.borrow();
        let value = tables.get(&TypeId::of::<T>())?.get(key)?;
        Some(f(value
            .downcast_ref::<T>()
            .expect("TypeId keys the value type")))
    }

    /// Whether any `T` is attached to `key`.
    pub fn has<T: Any>(&self, key: &K) -> bool {
        self.tables
            .borrow()
            .get(&TypeId::of::<T>())
            .is_some_and(|m| m.contains_key(key))
    }

    /// Remove and return the `T` attached to `key`.
    pub fn take<T: Any>(&self, key: &K) -> Option<T> {
        self.tables
            .borrow_mut()
            .get_mut(&TypeId::of::<T>())?
            .remove(key)
            .and_then(|b| b.downcast::<T>().ok().map(|b| *b))
    }
}

/// Enumeration, for writing a store out: what a library's metadata does with
/// the facts the passes left on a tree.
impl<K: Eq + Hash + Clone> MetaStore<K> {
    /// Every `T` in the store, with its key, in no particular order.
    pub fn entries<T: Any + Clone>(&self) -> Vec<(K, T)> {
        let tables = self.tables.borrow();
        let Some(table) = tables.get(&TypeId::of::<T>()) else {
            return Vec::new();
        };
        table
            .iter()
            .map(|(k, v)| {
                let v = v.downcast_ref::<T>().expect("TypeId keys the value type");
                (k.clone(), v.clone())
            })
            .collect()
    }

    /// Every type the store holds a non-empty table of, with its name.
    pub fn kinds(&self) -> Vec<(TypeId, &'static str)> {
        let tables = self.tables.borrow();
        let names = self.names.borrow();
        tables
            .iter()
            .filter(|(_, table)| !table.is_empty())
            .map(|(id, _)| (*id, names.get(id).copied().unwrap_or("?")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Fact(u8);

    #[derive(Debug, Clone, PartialEq)]
    struct Other(&'static str);

    #[test]
    fn facts_of_different_types_coexist_on_one_key() {
        let store = MetaStore::<u32>::new();
        store.set(1, Fact(7));
        store.set(1, Other("hi"));
        assert_eq!(store.get::<Fact>(&1), Some(Fact(7)));
        assert_eq!(store.get::<Other>(&1), Some(Other("hi")));
    }

    #[test]
    fn setting_replaces_and_returns_the_old_value() {
        let store = MetaStore::<u32>::new();
        assert_eq!(store.set(1, Fact(1)), None);
        assert_eq!(store.set(1, Fact(2)), Some(Fact(1)));
        assert_eq!(store.get::<Fact>(&1), Some(Fact(2)));
    }

    #[test]
    fn take_removes_and_has_reports() {
        let store = MetaStore::<u32>::new();
        store.set(1, Fact(3));
        assert!(store.has::<Fact>(&1));
        assert_eq!(store.take::<Fact>(&1), Some(Fact(3)));
        assert!(!store.has::<Fact>(&1));
        assert_eq!(store.take::<Fact>(&1), None);
    }

    #[test]
    fn a_fact_is_attached_to_one_key_only() {
        let store = MetaStore::<u32>::new();
        store.set(1, Fact(1));
        assert_eq!(store.get::<Fact>(&1), Some(Fact(1)));
        assert_eq!(store.get::<Fact>(&2), None);
    }

    #[test]
    fn a_shared_store_can_still_be_annotated() {
        // The whole point of the interior mutability: a pass walking a tree
        // holds `&MetaStore`, never `&mut MetaStore`.
        fn annotate(store: &MetaStore<u32>) {
            store.set(1, Fact(9));
        }
        let store = MetaStore::<u32>::new();
        annotate(&store);
        assert_eq!(store.get::<Fact>(&1), Some(Fact(9)));
    }

    #[test]
    fn cloning_drops_derived_state() {
        let store = MetaStore::<u32>::new();
        store.set(1, Fact(1));
        let copy = store.clone();
        assert_eq!(copy.get::<Fact>(&1), None);
    }
}
