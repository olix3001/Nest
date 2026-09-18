//! Sizes, alignments and offsets — what a type *is* in memory.
//!
//! Everything above this point talks about types as identities: `Vec.<i32>` is
//! not `Vec.<f64>`, a `distinct Meters` is not the `f64` it stands over. Nothing
//! above this point knows how many bytes any of them takes. This is where that
//! is decided, and it is decided here rather than in code generation for two
//! reasons that pull in the same direction:
//!
//! - **The program asks.** `size_of.<T>()` is an ordinary expression a constant
//!   may be written with (§6.4) — `BUF: usize :: size_of.<Header>() * 4` is a
//!   constant whose value has to exist before there is any machine code. (It
//!   cannot yet stand inside a *type*, `[size_of.<Header>()]u8`, because a
//!   length is wanted during inference and this runs after it. That is the same
//!   ordering limit a call in a length runs into.)
//! - **One answer, one place.** Offsets are needed by the LIR lowering (a field
//!   projection), by the garbage collector's root maps, by debug info and by
//!   codegen. Each computing its own would be four chances to disagree about
//!   where a field is, and a disagreement about that is not a wrong answer, it
//!   is a wrong program.
//!
//! # A query, not a pass
//!
//! [`Layouts`] is asked about a **concrete** [`Ty`] and memoizes what it worked
//! out. It is not a pass that annotates every type up front, because "every
//! type" is not a set anyone can enumerate: `[N]T` for every `N` a program
//! mentions, every tuple, every instantiation of every generic. What *is*
//! enumerable is the types a program actually uses, and each of those arrives
//! here when something needs it.
//!
//! The types the [`TypeDef`]s describe stay **definition-relative** — a field of
//! `Pair.<T>` is a `T` — exactly as they were after lowering. Substituting the
//! use site's arguments is this module's job, and doing it here rather than by
//! rewriting the definitions is what keeps one `Pair` in the program instead of
//! one per instantiation.
//!
//! # Where the target comes back
//!
//! Phase 5 took the [`Target`] out of the type layer on purpose: `usize` is
//! `distinct uint.<PTR_BITS>` over a constant `core` supplies (§3.1), so a
//! *type* never has to ask how wide a pointer is. A **layout** does — not for
//! `usize`, whose width is already in the type, but for a pointer itself, which
//! is not a `usize` and has no width written anywhere. This module is the one
//! place allowed to ask; everything else asks this module.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::common::options::Target;
use crate::sema::def::{DefId, DefTable, DirectiveArg};
use crate::sema::ty::{FloatWidth, Ty};

use super::{Linked, Member, Meta, TypeDefKind, Variant};

/// The largest alignment this compiler will give anything.
///
/// A `u4096` is a legal type (§3.1) and a 512-byte alignment for it would be
/// absurd: alignment exists so a load can be a single instruction, and no
/// machine has a 512-byte load. Sixteen is what every ABI this targets uses for
/// its widest vector type, and it is what `#align(N)` may still exceed
/// deliberately — the cap is on what the compiler *infers*, not on what a
/// program asks for.
pub const MAX_ALIGN: u64 = 16;

/// How deep the recursion may go before giving up.
///
/// A type containing itself by value has no layout, and `check::declarations`
/// already rejects one — but it rejects a cycle through *nominals*, and a
/// generic reached through monomorphization could in principle nest deeper than
/// the stack. Failing with a message beats failing with a crash.
const DEPTH: u32 = 128;

/// What a type is in memory: how many bytes it occupies, and what its address
/// must be a multiple of.
///
/// `size` is the **stride**: the distance from one element of `[N]T` to the
/// next, which is the size including any tail padding. There is deliberately no
/// separate "data size" — every consumer here wants the stride, and carrying two
/// numbers would mean every one of them choosing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub size: u64,
    pub align: u64,
}

impl Layout {
    /// A type that occupies no space. Its alignment is still 1: an address of
    /// zero bytes is still an address, and every address is a multiple of one.
    pub const ZERO: Layout = Layout { size: 0, align: 1 };

    /// A scalar of `size` bytes, aligned to itself (capped at [`MAX_ALIGN`]).
    fn scalar(size: u64) -> Layout {
        let align = size.next_power_of_two().clamp(1, MAX_ALIGN);
        Layout {
            size: round_up(size, align),
            align,
        }
    }
}

/// An aggregate's layout together with where each of its members sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fields {
    pub layout: Layout,
    /// Byte offset of each member, in declaration order.
    pub offsets: Vec<u64>,
}

/// Why a type has no layout.
///
/// Every variant is a statement about the *type*, not about this module: none of
/// them is a case that could be filled in later with more work here. That is
/// what makes them worth distinguishing — a caller can say which kind of mistake
/// it is looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// A generic parameter, or a type built over one. Its size depends on the
    /// instantiation, and there is not one yet.
    Generic(String),
    /// `dyn Trait` on its own: a trait object is only ever reached through a
    /// pointer, which is where the two words live.
    Unsized(String),
    /// A `comptime_int` / `comptime_float` / `comptime_str`. These have no
    /// run-time representation at all (§1.5); a value of one is a compile-time
    /// number and becomes something else on its way into memory.
    Comptime(String),
    /// An array whose length is not known, or a type the program already got
    /// wrong. A diagnostic exists for it elsewhere.
    Unknown(String),
    /// An `opaque` (§3.1): no size and no values, by construction rather than
    /// because something is missing. Its own variant because the advice is its
    /// own — reach it through a pointer — and because "has no known layout"
    /// would read as a compiler shortcoming rather than as the point of the
    /// type.
    Opaque(String),
    /// The recursion guard fired — see [`DEPTH`].
    TooDeep(String),
    /// The type is bigger than the target can address (see
    /// [`Layouts::max_size`]). Unlike every other variant this one is *not*
    /// already reported by some other check, so the stamping pass reports it.
    TooLarge(String),
}

impl LayoutError {
    pub fn message(&self) -> String {
        match self {
            LayoutError::Generic(t) => {
                format!("`{t}` has no layout until it is instantiated")
            }
            LayoutError::Unsized(t) => {
                format!("`{t}` has no size of its own; a trait object is reached through a pointer")
            }
            LayoutError::Comptime(t) => {
                format!("`{t}` is a compile-time type and has no run-time representation")
            }
            LayoutError::Opaque(t) => {
                format!("`{t}` has no size; an opaque type is only reachable through a pointer")
            }
            LayoutError::Unknown(t) => format!("`{t}` has no known layout"),
            LayoutError::TooDeep(t) => format!("`{t}` nests too deeply to lay out"),
            LayoutError::TooLarge(t) => {
                format!("`{t}` is larger than this target can address")
            }
        }
    }
}

type Result<T> = std::result::Result<T, LayoutError>;

/// The layout query.
///
/// Built once per compilation and shared. The cache is keyed by the type's
/// **mangled encoding** (`super::mono::type_key`), which is exactly the right
/// key for the same reason it is the right key for an instantiation: its one job
/// is injectivity, so two types share it precisely when they are the same type.
/// [`Ty`] itself cannot be a key — a `const` argument may hold a float, so there
/// is no `Hash`.
pub struct Layouts<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    linked: &'a Linked,
    target: Target,
    cache: RefCell<HashMap<String, Result<Layout>>>,
}

impl<'a> Layouts<'a> {
    pub fn new(defs: &'a DefTable, meta: &'a Meta, linked: &'a Linked, target: Target) -> Self {
        Layouts {
            defs,
            meta,
            linked,
            target,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// How wide a pointer is on this target, in bytes.
    pub fn pointer_size(&self) -> u64 {
        (self.target.pointer_bits as u64).div_ceil(8)
    }

    /// The largest object this target can hold — the rule §7cc states as "a
    /// type has a size only if the target can address it".
    ///
    /// The ceiling is `isize::MAX`, not `usize::MAX`, and the extra bit is not
    /// caution: the **difference** of two addresses inside one object is an
    /// `isize`, so an object bigger than that has interior addresses whose
    /// distance apart cannot be expressed. `&a[n] - &a[0]` is the everyday form
    /// of that, and an array indexing operation computes it.
    ///
    /// Having a ceiling at all is what makes the arithmetic below *checkable*
    /// rather than merely unchecked: `[18446744073709551615]u64` is a type a
    /// program may write, and without a rule the only answers available are a
    /// panic in a debug compiler and a silently wrapped size in a release one.
    pub fn max_size(&self) -> u64 {
        let bits = self.target.pointer_bits.clamp(2, 64);
        (1u64 << (bits - 1)) - 1
    }

    /// `n` if the target can address an object that big, an error otherwise.
    fn within_target(&self, n: u64, ty: &Ty) -> Result<u64> {
        if n > self.max_size() {
            return Err(LayoutError::TooLarge(self.show(ty)));
        }
        Ok(n)
    }

    /// The layout of `ty`.
    pub fn of(&self, ty: &Ty) -> Result<Layout> {
        let key = super::mono::type_key(self.defs, ty);
        if let Some(hit) = self.cache.borrow().get(&key) {
            return hit.clone();
        }
        let computed = self.compute(ty, 0);
        self.cache.borrow_mut().insert(key, computed.clone());
        computed
    }

    /// The layout of an aggregate together with its members' offsets.
    ///
    /// `None` for a type that has no members to speak of — a scalar, a pointer,
    /// an array. An array's "members" are a run at a stride, which is a
    /// different question and is answered by its element's [`Layout`].
    pub fn fields(&self, ty: &Ty) -> Option<Result<Fields>> {
        match ty {
            Ty::Tuple(elems) => {
                Some(self.aggregate(ty, &elems.iter().collect::<Vec<_>>(), None, 0))
            }
            Ty::Struct(fields) => Some(self.aggregate(
                ty,
                &fields.iter().map(|(_, t)| t).collect::<Vec<_>>(),
                None,
                0,
            )),
            Ty::Nominal { def, .. } => {
                let t = self.linked.ty(*def)?;
                match &t.kind {
                    TypeDefKind::Struct { members } => {
                        Some(self.struct_fields(ty, *def, members, 0))
                    }
                    TypeDefKind::Distinct { repr } => {
                        Some(self.struct_fields(ty, *def, std::slice::from_ref(repr), 0))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// An aggregate's members as a **use site** sees them: each name paired
    /// with its type after this instantiation's arguments are substituted in.
    ///
    /// A [`TypeDef`](super::TypeDef) is definition-relative — a field of
    /// `Pair.<T>` is a `T` — and substituting is this module's job, so the
    /// substitution has to be reachable from here rather than redone by every
    /// caller. The LIR aggregate flattening (`design/lir.md` §7b) is the caller
    /// this exists for: turning a type into a struct means knowing what its
    /// members *are*, not only where they sit.
    ///
    /// `None` for anything that is not a struct or a `distinct`.
    pub fn member_types(&self, ty: &Ty) -> Option<Vec<(crate::common::symbol::Symbol, Ty)>> {
        // An anonymous struct carries its members in the type itself; there is
        // no definition to look up and nothing to substitute.
        if let Ty::Struct(fields) = ty {
            return Some(fields.clone());
        }
        let Ty::Nominal { def, .. } = ty else {
            return None;
        };
        let t = self.linked.ty(*def)?;
        let members = match &t.kind {
            TypeDefKind::Struct { members } => members.as_slice(),
            TypeDefKind::Distinct { repr } => std::slice::from_ref(repr),
            _ => return None,
        };
        let subst = self.substitution(*def, ty);
        Some(
            members
                .iter()
                .map(|m| {
                    (
                        m.name.clone(),
                        subst_ty(&subst, &self.meta.ty_or_error(m.id)),
                    )
                })
                .collect(),
        )
    }

    /// [`Layouts::member_types`] for one variant of an enum.
    pub fn variant_member_types(
        &self,
        ty: &Ty,
        variant: usize,
    ) -> Option<Vec<(crate::common::symbol::Symbol, Ty)>> {
        let Ty::Nominal { def, .. } = ty else {
            return None;
        };
        let t = self.linked.ty(*def)?;
        let TypeDefKind::Enum { variants } = &t.kind else {
            return None;
        };
        let v = variants.get(variant)?;
        let subst = self.substitution(*def, ty);
        Some(
            v.members
                .iter()
                .map(|m| {
                    (
                        m.name.clone(),
                        subst_ty(&subst, &self.meta.ty_or_error(m.id)),
                    )
                })
                .collect(),
        )
    }

    /// Where an enum's tag and payload sit, and how big each variant's payload
    /// is.
    ///
    /// The shape is `design/lir.md` §7b's: a tag member and a payload member.
    /// What is decided *here* rather than at that lowering is the tag's width
    /// and whether the payload overlaps — which is the whole content of laying
    /// an enum out.
    pub fn enum_layout(&self, ty: &Ty) -> Option<Result<EnumLayout>> {
        let Ty::Nominal { def, .. } = ty else {
            return None;
        };
        let t = self.linked.ty(*def)?;
        let TypeDefKind::Enum { variants } = &t.kind else {
            return None;
        };
        Some(self.enum_of(ty, *def, variants, 0))
    }

    // ===< The computation >===

    fn compute(&self, ty: &Ty, depth: u32) -> Result<Layout> {
        if depth > DEPTH {
            return Err(LayoutError::TooDeep(self.show(ty)));
        }
        match ty {
            // An integer's size is its width rounded up to whole bytes, and its
            // alignment is that size rounded up to a power of two (capped at
            // [`MAX_ALIGN`]); the size is then rounded up to the alignment, so
            // every integer is a self-aligned block.
            //
            // This is a **decision**, not a derivation: `u24` is a legal type
            // (§3.1) and no machine has a 3-byte load, so something has to say
            // whether it occupies three bytes or four. Four, because the
            // alternative is that `[N]u24` has a stride no load can use.
            Ty::Int { width, .. } => match width.bits() {
                Some(bits) => Ok(Layout::scalar((bits as u64).div_ceil(8))),
                None => Err(LayoutError::Generic(self.show(ty))),
            },
            // `f80` is the x87 extended format: ten bytes of data, and every ABI
            // that has it pads the slot to its alignment. Saying 16 here is
            // saying what the slot costs, which is what a layout is.
            Ty::Float(w) => Ok(match w {
                FloatWidth::F16 => Layout { size: 2, align: 2 },
                FloatWidth::F32 => Layout { size: 4, align: 4 },
                FloatWidth::F64 => Layout { size: 8, align: 8 },
                FloatWidth::F80 | FloatWidth::F128 => Layout {
                    size: 16,
                    align: 16,
                },
            }),
            Ty::Bool => Ok(Layout { size: 1, align: 1 }),
            // A `char` is a Unicode scalar value, not a byte (§3.1), so it is
            // four bytes wide however it is printed.
            Ty::Char => Ok(Layout { size: 4, align: 4 }),
            // `void` is the empty tuple and `never` is uninhabited. Neither has
            // anything to store, and a zero-sized member is a real thing to
            // write — it costs nothing and keeps its name.
            Ty::Void | Ty::Never => Ok(Layout::ZERO),
            // A pointer to a trait object is **fat**: the data pointer and the
            // vtable pointer (`design/lir.md` §7b). This is the one place a
            // pointer's size depends on what it points at.
            Ty::Ptr { inner, .. } if matches!(**inner, Ty::Dyn(_)) => Ok(self.two_words()),
            Ty::Ptr { .. } => Ok(Layout::scalar(self.pointer_size())),
            // A slice is a pointer and a length, in that order.
            Ty::Slice { .. } => Ok(self.two_words()),
            // A function value is the address of its code.
            Ty::Func { .. } => Ok(Layout::scalar(self.pointer_size())),
            Ty::Array { len, inner, .. } => {
                let Some(n) = len.value() else {
                    return Err(LayoutError::Generic(self.show(ty)));
                };
                let elem = self.compute(inner, depth + 1)?;
                // The stride is the element's size, tail padding included, which
                // is why [`Layout::size`] is the stride and not a data size: an
                // array is exactly `n` of them end to end.
                //
                // `n` is whatever the program wrote, so this product is the one
                // place in the compiler where a legal source type can exceed a
                // `u64`. It is checked rather than wrapped for the reason
                // [`Layouts::max_size`] gives: a wrapped size is a wrong answer
                // that nothing downstream can detect.
                let size = elem
                    .size
                    .checked_mul(n)
                    .ok_or_else(|| LayoutError::TooLarge(self.show(ty)))?;
                Ok(Layout {
                    size: self.within_target(size, ty)?,
                    align: elem.align,
                })
            }
            Ty::Tuple(elems) => Ok(self
                .aggregate(ty, &elems.iter().collect::<Vec<_>>(), None, depth)?
                .layout),
            // An anonymous struct lays out like a tuple of its field types. The
            // order is the sorted one [`Ty::anon_struct`] fixed, so the two
            // spellings of one type get the same offsets — the declaration
            // order a named struct promises is a promise about a *declaration*,
            // and an anonymous struct has none.
            Ty::Struct(fields) => Ok(self
                .aggregate(
                    ty,
                    &fields.iter().map(|(_, t)| t).collect::<Vec<_>>(),
                    None,
                    depth,
                )?
                .layout),
            Ty::Nominal { def, .. } => self.nominal(ty, *def, depth),
            Ty::Dyn(_) => Err(LayoutError::Unsized(self.show(ty))),
            Ty::Opaque => Err(LayoutError::Opaque(self.show(ty))),
            Ty::ComptimeInt | Ty::ComptimeFloat | Ty::ComptimeStr => {
                Err(LayoutError::Comptime(self.show(ty)))
            }
            Ty::Var(_) | Ty::Error => Err(LayoutError::Unknown(self.show(ty))),
        }
    }

    /// A pointer and one more word beside it — a slice's `(ptr, len)` and a
    /// trait object's `(data, vtable)` are the same shape.
    fn two_words(&self) -> Layout {
        let w = self.pointer_size();
        Layout {
            size: w * 2,
            align: w,
        }
    }

    fn nominal(&self, ty: &Ty, def: DefId, depth: u32) -> Result<Layout> {
        // A type parameter is a `Ty::Nominal` with no definition behind it
        // (§3.7); it is also the commonest reason a layout cannot be computed,
        // so it is worth saying which.
        let Some(t) = self.linked.ty(def) else {
            if self.defs.get(def).kind == crate::sema::def::DefKind::TypeParam {
                return Err(LayoutError::Generic(self.show(ty)));
            }
            return Err(LayoutError::Unknown(self.show(ty)));
        };
        match &t.kind {
            TypeDefKind::Struct { members } => {
                Ok(self.struct_fields(ty, def, members, depth)?.layout)
            }
            // A `distinct T` **is** `T`'s representation reinterpreted (§2.4).
            // Not "the same size as" — the same bytes, which is what makes a
            // `usize` and its `uint.<64>` interchangeable in memory and
            // different in the type system.
            TypeDefKind::Distinct { repr } => Ok(self
                .struct_fields(ty, def, std::slice::from_ref(repr), depth)?
                .layout),
            TypeDefKind::Enum { variants } => Ok(self.enum_of(ty, def, variants, depth)?.layout),
            // A trait is not a run-time type. `dyn Trait` is the type that
            // stands for one, and it is unsized.
            TypeDefKind::Trait { .. } => Err(LayoutError::Unsized(self.show(ty))),
        }
    }

    /// Lay out a nominal's members, with the use site's type arguments
    /// substituted into their declared types.
    fn struct_fields(&self, ty: &Ty, def: DefId, members: &[Member], depth: u32) -> Result<Fields> {
        let subst = self.substitution(def, ty);
        let tys: Vec<Ty> = members
            .iter()
            .map(|m| {
                let declared = self.meta.ty_or_error(m.id);
                subst_ty(&subst, &declared)
            })
            .collect();
        let refs: Vec<&Ty> = tys.iter().collect();
        let mut fields = self.aggregate(ty, &refs, Some(def), depth)?;
        // A member may over-align itself (§9). It cannot *under*-align: an
        // `#align(1)` on a field of a non-`#packed` struct would be a request to
        // put a `u64` at an odd address, which is a different thing from asking
        // for no padding and is what `#packed` is for.
        let per_field: Vec<Option<u64>> = members.iter().map(|m| self.align_of(m.id)).collect();
        if per_field.iter().any(Option::is_some) {
            fields = self.aggregate_with(ty, &refs, Some(def), &per_field, depth)?;
        }
        Ok(fields)
    }

    /// Lay out a run of types back to back, honouring the aggregate's own
    /// `#packed` / `#align`.
    fn aggregate(&self, at: &Ty, tys: &[&Ty], owner: Option<DefId>, depth: u32) -> Result<Fields> {
        let none = vec![None; tys.len()];
        self.aggregate_with(at, tys, owner, &none, depth)
    }

    /// `at` is the type being laid out, and is carried only so that a size that
    /// runs past [`Layouts::max_size`] can name it. A run of fields is not a
    /// type on its own — an enum variant's payload is one — so it is a separate
    /// argument rather than something recovered from `owner`.
    fn aggregate_with(
        &self,
        at: &Ty,
        tys: &[&Ty],
        owner: Option<DefId>,
        per_field: &[Option<u64>],
        depth: u32,
    ) -> Result<Fields> {
        let packed = owner.is_some_and(|d| self.has_directive(d, "packed"));
        let mut offset = 0u64;
        let mut align = 1u64;
        let mut offsets = Vec::with_capacity(tys.len());
        for (i, t) in tys.iter().enumerate() {
            let l = self.compute(t, depth + 1)?;
            // `#packed` removes inter-field padding: every field sits at the
            // next byte (§9). It is the one thing that can lower an alignment,
            // and that is the point of it — a wire format has no padding in it.
            let want = match (packed, per_field.get(i).copied().flatten()) {
                (true, _) => 1,
                (false, Some(n)) => l.align.max(n),
                (false, None) => l.align,
            };
            // Both of these can leave a `u64` — a struct of two
            // `[1 << 62]u64`s is a type a program may write — so both are
            // checked against the target's ceiling as they go rather than once
            // at the end, where the sum would already have wrapped.
            offset = round_up_checked(offset, want)
                .ok_or_else(|| LayoutError::TooLarge(self.show(at)))?;
            offsets.push(offset);
            offset = offset
                .checked_add(l.size)
                .ok_or_else(|| LayoutError::TooLarge(self.show(at)))?;
            self.within_target(offset, at)?;
            align = align.max(want);
        }
        // Fields are laid out in **declaration order**, and nothing is
        // reordered. That is a promise the language makes rather than a
        // limitation: §9's `#packed` is defined as removing padding, which only
        // means something if the order is the written one, and an FFI struct
        // that reordered itself would not be one.
        if let Some(n) = owner.and_then(|d| self.align_of_def(d)) {
            align = align.max(n);
        }
        let size =
            round_up_checked(offset, align).ok_or_else(|| LayoutError::TooLarge(self.show(at)))?;
        Ok(Fields {
            layout: Layout {
                size: self.within_target(size, at)?,
                align,
            },
            offsets,
        })
    }

    /// Lay out an enum: a tag, then a payload big enough for any variant.
    fn enum_of(&self, ty: &Ty, def: DefId, variants: &[Variant], depth: u32) -> Result<EnumLayout> {
        let subst = self.substitution(def, ty);
        // The tag is the smallest unsigned integer that can tell the variants
        // apart. One byte for anything up to 256 of them, which is every enum
        // anyone writes; the wider cases exist so that a generated enum does not
        // hit a wall.
        let tag = Layout::scalar(match variants.len() as u64 {
            0..=0x100 => 1,
            0x101..=0x1_0000 => 2,
            0x1_0001..=0x1_0000_0000 => 4,
            _ => 8,
        });

        let mut payload = Layout::ZERO;
        let mut payloads = Vec::with_capacity(variants.len());
        for v in variants {
            let tys: Vec<Ty> = v
                .members
                .iter()
                .map(|m| subst_ty(&subst, &self.meta.ty_or_error(m.id)))
                .collect();
            let refs: Vec<&Ty> = tys.iter().collect();
            let f = self.aggregate(ty, &refs, None, depth)?;
            payload.size = payload.size.max(f.layout.size);
            payload.align = payload.align.max(f.layout.align);
            payloads.push(f);
        }

        // The payload **overlaps**: one variant is live at a time, so the space
        // is shared. The alternative — laying the variants out end to end —
        // would make an enum as big as all of them together, which is not a
        // trade-off anyone wants for a type whose whole point is that it is one
        // of them.
        let payload_at = round_up(tag.size, payload.align);
        let mut align = tag.align.max(payload.align);
        if let Some(n) = self.align_of_def(def) {
            align = align.max(n);
        }
        let size = payload_at
            .checked_add(payload.size)
            .and_then(|n| round_up_checked(n, align))
            .ok_or_else(|| LayoutError::TooLarge(self.show(ty)))?;
        Ok(EnumLayout {
            layout: Layout {
                size: self.within_target(size, ty)?,
                align,
            },
            tag,
            payload_at,
            payload,
            variants: payloads,
        })
    }

    /// The map from a nominal's own generic parameters to the arguments this use
    /// site supplied.
    ///
    /// The definition's type is `Ty::Nominal { def, args }` over its *own*
    /// parameters — that is how [`TypeDef`](super::TypeDef) records it — so the
    /// two argument lists line up by position and the map falls out of zipping
    /// them.
    fn substitution(&self, def: DefId, ty: &Ty) -> HashMap<DefId, Ty> {
        let mut map = HashMap::new();
        let (Some(t), Ty::Nominal { args, .. }) = (self.linked.ty(def), ty) else {
            return map;
        };
        let Some(Ty::Nominal { args: params, .. }) = self.meta.ty(t.id) else {
            return map;
        };
        for (p, a) in params.iter().zip(args) {
            if let Ty::Nominal { def: pd, args } = p
                && args.is_empty()
            {
                map.insert(*pd, a.clone());
            }
        }
        map
    }

    fn has_directive(&self, def: DefId, name: &str) -> bool {
        self.defs.get(def).directives.iter().any(|d| d.is(name))
    }

    /// The `#align(N)` written on a definition, if any.
    fn align_of_def(&self, def: DefId) -> Option<u64> {
        let d = self
            .defs
            .get(def)
            .directives
            .iter()
            .find(|d| d.is("align"))?;
        match d.args.first() {
            Some(DirectiveArg::Int(n)) if *n > 0 => Some(*n as u64),
            _ => None,
        }
    }

    /// The `#align(N)` written on a member, read from its node's directives.
    fn align_of(&self, id: super::IrId) -> Option<u64> {
        let d = self
            .meta
            .directives(id)
            .into_iter()
            .find(|d| d.is("align"))?;
        match d.args.first() {
            Some(DirectiveArg::Int(n)) if *n > 0 => Some(*n as u64),
            _ => None,
        }
    }

    fn show(&self, ty: &Ty) -> String {
        ty.display(self.defs)
    }
}

/// Where an enum's parts sit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumLayout {
    pub layout: Layout,
    /// The discriminant, at offset zero.
    pub tag: Layout,
    /// Where the shared payload starts.
    pub payload_at: u64,
    /// The payload as a whole: big enough and aligned enough for every variant.
    pub payload: Layout,
    /// Each variant's own members, at offsets **relative to the payload**.
    pub variants: Vec<Fields>,
}

/// Round `n` up to the next multiple of `align` (a power of two).
///
/// Used only where both arguments are already bounded — a scalar's own size, an
/// enum tag. Anywhere a program's numbers reach, [`round_up_checked`] is the one
/// to call.
fn round_up(n: u64, align: u64) -> u64 {
    if align <= 1 {
        return n;
    }
    n.div_ceil(align) * align
}

/// [`round_up`], and `None` when the rounded value would not fit in a `u64`.
///
/// Rounding up is where a size that is merely huge becomes one that has wrapped:
/// `div_ceil` cannot overflow, but multiplying the result back by the alignment
/// can, and the product is then a *smaller* number than the input — which is the
/// worst possible failure, since every later check would pass.
fn round_up_checked(n: u64, align: u64) -> Option<u64> {
    if align <= 1 {
        return Some(n);
    }
    n.div_ceil(align).checked_mul(align)
}

/// Replace every generic parameter in `ty` by what `map` binds it to.
///
/// A member's declared type is definition-relative — a field of `Pair.<T>`
/// declared `T` *is* the type parameter — so this is what turns a definition
/// into the thing a use site holds.
///
/// Shared with the const evaluator, which needs the same substitution for the
/// same reason: `size_of.<T>()` inside a `#const` generic is asking about
/// whatever the call bound `T` to. Note that
/// [`mono`](super::mono)'s substitution is a *different* function and
/// deliberately so — it also replaces `const` parameters in widths and array
/// lengths, which needs a map this one does not have.
pub(crate) fn subst_ty(map: &HashMap<DefId, Ty>, ty: &Ty) -> Ty {
    if map.is_empty() {
        return ty.clone();
    }
    match ty {
        Ty::Nominal { def, args } if args.is_empty() => {
            map.get(def).cloned().unwrap_or_else(|| ty.clone())
        }
        Ty::Nominal { def, args } => Ty::Nominal {
            def: *def,
            args: args.iter().map(|a| subst_ty(map, a)).collect(),
        },
        Ty::Ptr { mutable, inner } => Ty::Ptr {
            mutable: *mutable,
            inner: Box::new(subst_ty(map, inner)),
        },
        Ty::Slice { mutable, inner } => Ty::Slice {
            mutable: *mutable,
            inner: Box::new(subst_ty(map, inner)),
        },
        Ty::Array {
            len,
            mutable,
            inner,
        } => Ty::Array {
            len: len.clone(),
            mutable: *mutable,
            inner: Box::new(subst_ty(map, inner)),
        },
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|e| subst_ty(map, e)).collect()),
        Ty::Func { params, ret } => Ty::Func {
            params: params.iter().map(|p| subst_ty(map, p)).collect(),
            ret: Box::new(subst_ty(map, ret)),
        },
        other => other.clone(),
    }
}
