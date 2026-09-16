//! How an id crosses from one compilation into another.
//!
//! A [`DefId`], a [`FileId`] and an [`IrId`] are indices into tables one
//! [`Session`](crate::sema::session::Session) owns, and a second session numbers
//! its tables differently — it loads other packages, in another order. So an id
//! is never written as its number. It is written as **whose** it is and **where
//! in that owner's own numbering**: an [`IdRef`]. Reading it back looks the
//! owner up in the reading session, where its tables start, and adds.
//!
//! The ids sit deep inside everything a library's metadata holds — a type names
//! its definition, a resolution names what it resolved to, a span names its
//! file — so the translation cannot be a pass over the data. It is the ids' own
//! `Serialize` and `Deserialize`, reading the translation from a thread-local
//! that [`encode`] and [`decode`] set for exactly as long as they run. Outside
//! them an id is written as its plain number, which is what any other use of
//! serde (a dump, a test) wants.

use std::cell::RefCell;
use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::common::source::FileId;
use crate::common::symbol::Symbol;
use crate::ir::IrId;
use crate::sema::def::DefId;

/// An id as it is written: its owner, and its index in the owner's numbering.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IdRef {
    /// Owned by a package: the index of the package in the metadata's package
    /// table, and the id's index among that package's own.
    Local { package: u32, index: u32 },
    /// A primitive, which every session makes for itself: named rather than
    /// numbered, because `u65536` exists in a session only once something asks.
    Builtin(Symbol),
}

/// One package's ids in a session: each kind is one contiguous run, starting at
/// its base. That is how a package loaded from metadata lays its ids out, and
/// what makes translating one an addition.
#[derive(Debug, Clone, Copy, Default)]
pub struct Bases {
    pub def: u32,
    pub file: u32,
    pub ir: u32,
}

/// What writing needs: where every id that is not the written package's own
/// comes from, and the written package's own numbering.
#[derive(Debug, Default)]
pub struct Encoding {
    /// The written package's defs, by their index in what is written.
    pub own_defs: HashMap<DefId, u32>,
    pub own_files: HashMap<FileId, u32>,
    /// The first id the written package allocated; its own ids are numbered
    /// from here.
    pub own_ir_base: u32,
    /// Every loaded package, by its index in the package table, with its runs.
    pub foreign: Vec<(u32, Bases, Counts)>,
    /// The primitives, by id.
    pub builtins: HashMap<DefId, Symbol>,
    /// Ids nothing above accounted for — a defect, reported once writing ends.
    pub unowned: Vec<String>,
    /// Every builtin written, which a reader has to create before it reads.
    pub builtins_used: Vec<Symbol>,
}

/// How many ids of each kind a package has.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Counts {
    pub defs: u32,
    pub files: u32,
    pub ir: u32,
}

/// What reading needs: where each package in the table starts in this session,
/// and the primitives by name.
#[derive(Debug, Default)]
pub struct Decoding {
    pub packages: Vec<Bases>,
    pub builtins: HashMap<Symbol, DefId>,
    pub missing_builtins: Vec<Symbol>,
}

enum Mode {
    Encode(Encoding),
    Decode(Decoding),
}

thread_local! {
    static MODE: RefCell<Option<Mode>> = const { RefCell::new(None) };
}

/// Run `f` with `encoding` as the translation, and hand the encoding back —
/// with whatever it found unaccounted for.
pub fn encode<R>(encoding: Encoding, f: impl FnOnce() -> R) -> (R, Encoding) {
    MODE.with(|m| *m.borrow_mut() = Some(Mode::Encode(encoding)));
    let result = f();
    let Some(Mode::Encode(encoding)) = MODE.with(|m| m.borrow_mut().take()) else {
        unreachable!("the encoding was set above");
    };
    (result, encoding)
}

/// Run `f` with `decoding` as the translation.
pub fn decode<R>(decoding: Decoding, f: impl FnOnce() -> R) -> (R, Decoding) {
    MODE.with(|m| *m.borrow_mut() = Some(Mode::Decode(decoding)));
    let result = f();
    let Some(Mode::Decode(decoding)) = MODE.with(|m| m.borrow_mut().take()) else {
        unreachable!("the decoding was set above");
    };
    (result, decoding)
}

/// Which kind of id is being translated.
#[derive(Clone, Copy)]
enum Kind {
    Def,
    File,
    Ir,
}

fn active() -> bool {
    MODE.with(|m| m.borrow().is_some())
}

fn to_ref(kind: Kind, raw: u32) -> IdRef {
    MODE.with(|m| {
        let mut mode = m.borrow_mut();
        let Some(Mode::Encode(e)) = mode.as_mut() else {
            unreachable!("only called while encoding");
        };
        let own = match kind {
            Kind::Def => e.own_defs.get(&DefId(raw)).copied(),
            Kind::File => e.own_files.get(&FileId(raw)).copied(),
            Kind::Ir => None,
        };
        if let Some(index) = own {
            return IdRef::Local { package: 0, index };
        }
        if let Kind::Def = kind
            && let Some(name) = e.builtins.get(&DefId(raw)).cloned()
        {
            if !e.builtins_used.contains(&name) {
                e.builtins_used.push(name.clone());
            }
            return IdRef::Builtin(name);
        }
        for (package, bases, counts) in &e.foreign {
            let (base, count) = match kind {
                Kind::Def => (bases.def, counts.defs),
                Kind::File => (bases.file, counts.files),
                Kind::Ir => (bases.ir, counts.ir),
            };
            if raw >= base && raw < base + count {
                return IdRef::Local {
                    package: *package,
                    index: raw - base,
                };
            }
        }
        match kind {
            // Every IR id that is not a loaded package's is the written
            // package's: its lowering allocated it.
            Kind::Ir if raw >= e.own_ir_base => IdRef::Local {
                package: 0,
                index: raw - e.own_ir_base,
            },
            Kind::Def => {
                e.unowned.push(format!("def {raw}"));
                IdRef::Local { package: 0, index: u32::MAX }
            }
            Kind::File => {
                e.unowned.push(format!("file {raw}"));
                IdRef::Local { package: 0, index: u32::MAX }
            }
            Kind::Ir => {
                e.unowned.push(format!("IR node {raw}"));
                IdRef::Local { package: 0, index: u32::MAX }
            }
        }
    })
}

fn from_ref(kind: Kind, r: IdRef) -> Result<u32, String> {
    MODE.with(|m| {
        let mut mode = m.borrow_mut();
        let Some(Mode::Decode(d)) = mode.as_mut() else {
            unreachable!("only called while decoding");
        };
        match r {
            IdRef::Builtin(name) => match d.builtins.get(&name) {
                Some(id) => Ok(id.0),
                None => {
                    d.missing_builtins.push(name.clone());
                    Err(format!("the primitive `{name}` was not prepared"))
                }
            },
            IdRef::Local { package, index } => {
                let bases = d
                    .packages
                    .get(package as usize)
                    .ok_or_else(|| format!("package {package} is not in the table"))?;
                let base = match kind {
                    Kind::Def => bases.def,
                    Kind::File => bases.file,
                    Kind::Ir => bases.ir,
                };
                Ok(base + index)
            }
        }
    })
}

fn serialize_id<S: Serializer>(kind: Kind, raw: u32, s: S) -> Result<S::Ok, S::Error> {
    if active() {
        to_ref(kind, raw).serialize(s)
    } else {
        raw.serialize(s)
    }
}

fn deserialize_id<'de, D: Deserializer<'de>>(kind: Kind, d: D) -> Result<u32, D::Error> {
    if active() {
        let r = IdRef::deserialize(d)?;
        from_ref(kind, r).map_err(serde::de::Error::custom)
    } else {
        u32::deserialize(d)
    }
}

impl Serialize for DefId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_id(Kind::Def, self.0, s)
    }
}

impl<'de> Deserialize<'de> for DefId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_id(Kind::Def, d).map(DefId)
    }
}

impl Serialize for FileId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_id(Kind::File, self.0, s)
    }
}

impl<'de> Deserialize<'de> for FileId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_id(Kind::File, d).map(FileId)
    }
}

impl Serialize for IrId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_id(Kind::Ir, self.0, s)
    }
}

impl<'de> Deserialize<'de> for IrId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_id(Kind::Ir, d).map(IrId)
    }
}
