//! One [`Unit`] into one LLVM module.
//!
//! `design/lir.md` §10 is the list this file answers: three statements, three
//! callees, four terminators, six rvalues, the opcodes, the cast kinds and the
//! intrinsics. Most of it is a direct translation, and the places where it is
//! not are the four below.
//!
//! ## A `bool` is an `i8`, everywhere
//!
//! LLVM's `i1` is a register type whose in-memory size is a byte, and a struct
//! member that is sometimes one bit and sometimes one byte is the kind of
//! disagreement that produces a wrong offset rather than an error. So a `bool`
//! is an `i8` in every slot, every member and every argument; the `i1` a
//! comparison produces is widened immediately and never stored. LIR's layouts
//! give a `bool` one byte, and this is what agrees with them.
//!
//! ## Every access is a byte offset
//!
//! LIR decided the layout: a member has an offset, an array has a stride, a type
//! has a size and an alignment (§7b). So a projection is a `getelementptr i8` by
//! that number and nothing here re-derives it. The alternative — building LLVM
//! struct types and indexing them by member number — would mean two layout
//! engines that have to agree, and LLVM's would be the one that wins silently.
//!
//! Aggregates are still *declared* as packed structs with their padding written
//! out explicitly, so the IR is readable and a global's initializer is an
//! ordinary constant. Packed, because the padding is already there: letting LLVM
//! insert its own on top would move every member.
//!
//! ## An aggregate is never a value
//!
//! Nothing here builds an LLVM aggregate value. Assigning a struct writes each
//! member to its offset; a checked-arithmetic result writes two. That keeps
//! `Rvalue::Aggregate`, `Op { op: add_checked, .. }` and a plain store on one
//! path, and it is what a target does anyway.
//!
//! ## The collector is conservative, so there are no roots to emit
//!
//! §6 computed a precise live set at every safepoint, and this backend emits
//! none of it. That is not an omission: the runtime links a **conservative**
//! collector, which finds roots by scanning the stack itself and would ignore a
//! shadow stack if one were written. The live sets become load-bearing on the
//! day the collector is swapped for a precise or moving one, which is exactly
//! the swap the runtime shim exists to make cheap — and on that day this is
//! where `llvm.gcroot` or a statepoint lowering goes.

use std::collections::HashMap;

use inkwell::AddressSpace;
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage as LlvmLinkage, Module};
use inkwell::types::{BasicType, BasicTypeEnum, StructType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, GlobalValue, IntValue,
    PointerValue,
};

use crate::codegen::CodegenError;
use crate::sema::ty::CallConv;
use crate::lir::{
    Aggregate, Base, Callee, CastKind, Constant, Function, Global, Intrinsic, Linkage, Local, Op,
    Operand, Place, Projection, Rvalue, StmtKind, TermKind, Ty, TypeId, Unit,
};

type Result<T> = std::result::Result<T, CodegenError>;

/// A message about a unit this backend cannot emit.
fn unsupported(what: impl std::fmt::Display) -> CodegenError {
    CodegenError::Unsupported(what.to_string())
}

/// Say which function or global a failure was in.
/// A [`CallConv`] as LLVM numbers it (`llvm::CallingConv`).
///
/// The numbers are LLVM's own and are stable in its bitcode, which is why they
/// are written out rather than read from a header this compiler does not include.
fn llvm_conv(conv: CallConv) -> u32 {
    match conv {
        CallConv::C => 0,
        CallConv::Fast => 8,
        CallConv::StdCall => 64,
        CallConv::FastCall => 65,
        CallConv::Aapcs => 67,
        CallConv::AapcsVfp => 68,
        CallConv::ThisCall => 70,
        CallConv::SysV64 => 78,
        CallConv::Win64 => 79,
        CallConv::VectorCall => 80,
    }
}

fn within(who: &str, e: CodegenError) -> CodegenError {
    match e {
        CodegenError::Unsupported(m) => CodegenError::Unsupported(format!("{who}: {m}")),
        CodegenError::Failed(m) => CodegenError::Failed(format!("{who}: {m}")),
        other => other,
    }
}

/// A message about this backend or the lowering being wrong.
fn failed(what: impl std::fmt::Display) -> CodegenError {
    CodegenError::Failed(what.to_string())
}

/// Build the module for one unit.
pub fn build<'ctx>(
    context: &'ctx Context,
    unit: &Unit,
    pointer_bytes: u64,
) -> Result<Module<'ctx>> {
    let module = context.create_module(&unit.name);
    let mut cx = Cx {
        context,
        builder: context.create_builder(),
        module,
        unit,
        pointer_bytes,
        types: Vec::new(),
        globals: Vec::new(),
        funcs: Vec::new(),
    };

    // Each phase names what it was working on when it failed. A message that
    // says only "`void` is not a type a value can have" is a message somebody
    // has to bisect a program to act on.
    cx.declare_types();
    cx.declare_globals()?;
    cx.declare_funcs()?;
    cx.define_globals()?;
    for (i, f) in unit.funcs.iter().enumerate() {
        if !f.blocks.is_empty() {
            cx.define_func(i, f).map_err(|e| within(&f.name, e))?;
        }
    }
    Ok(cx.module)
}

struct Cx<'ctx, 'u> {
    context: &'ctx Context,
    builder: Builder<'ctx>,
    module: Module<'ctx>,
    unit: &'u Unit,
    pointer_bytes: u64,
    types: Vec<StructType<'ctx>>,
    globals: Vec<GlobalValue<'ctx>>,
    funcs: Vec<FunctionValue<'ctx>>,
}

// ===< Types >===

impl<'ctx> Cx<'ctx, '_> {
    /// How many bytes a value of this type occupies, by LIR's reckoning.
    ///
    /// LIR already computed every aggregate's size; the scalars are arithmetic.
    /// Nothing here asks LLVM, because LLVM's answer for a packed struct is
    /// whatever this function's answer was when the struct was built.
    fn size_of(&self, ty: &Ty) -> u64 {
        match ty {
            // A `u4096` is a legal type (§7b) and its size is its width rounded
            // up to whole bytes, which is what LIR's layout engine says too.
            Ty::Int { bits, .. } => (*bits as u64).div_ceil(8),
            Ty::Float { bits } => (*bits as u64).div_ceil(8),
            Ty::Bool => 1,
            Ty::Ptr(_) | Ty::Func { .. } => self.pointer_bytes,
            Ty::Array { len, elem } => len * self.size_of(elem),
            Ty::Named(id) => self.unit.ty(*id).layout.size,
            Ty::Void | Ty::Never => 0,
        }
    }

    /// The alignment an allocation of this type needs.
    fn align_of(&self, ty: &Ty) -> u32 {
        let n = match ty {
            Ty::Named(id) => self.unit.ty(*id).layout.align,
            Ty::Array { elem, .. } => self.align_of(elem) as u64,
            // A scalar is aligned to its own size, up to a word. The clamp is
            // what keeps a `u4096` from asking for 512-byte alignment.
            other => self.size_of(other).next_power_of_two().clamp(1, 16),
        };
        n.max(1) as u32
    }

    /// An integer type of `bits` bits.
    ///
    /// A width of zero is the one thing LLVM refuses, and it is a width no LIR
    /// type has — `void` is erased from every slot (§9) — so reaching it is a
    /// bug rather than a program.
    fn int_type(&self, bits: u32) -> Result<inkwell::types::IntType<'ctx>> {
        let bits =
            std::num::NonZero::new(bits).ok_or_else(|| failed("an integer type of no bits"))?;
        self.context
            .custom_width_int_type(bits)
            .map_err(|e| unsupported(format!("LLVM has no integer type of {bits} bits: {e}")))
    }

    /// The LLVM type a value of this LIR type is held in.
    fn llty(&self, ty: &Ty) -> Result<BasicTypeEnum<'ctx>> {
        Ok(match ty {
            Ty::Int { bits, .. } => self.int_type(*bits as u32)?.into(),
            Ty::Float { bits } => match bits {
                16 => self.context.f16_type().into(),
                32 => self.context.f32_type().into(),
                64 => self.context.f64_type().into(),
                128 => self.context.f128_type().into(),
                other => return Err(unsupported(format!("no float type of {other} bits"))),
            },
            // See the module comment: a byte, never a bit.
            Ty::Bool => self.context.i8_type().into(),
            // Opaque pointers (LLVM 15 and later): every address is `ptr`, which
            // is also what LIR says — mutability is erased and a pointee type is
            // a fact about the load, not about the address (§9).
            Ty::Ptr(_) | Ty::Func { .. } => self.context.ptr_type(AddressSpace::default()).into(),
            Ty::Array { len, elem } => self.llty(elem)?.array_type(*len as u32).into(),
            Ty::Named(id) => self
                .types
                .get(id.0 as usize)
                .copied()
                .ok_or_else(|| failed(format!("type #{} is not in this unit", id.0)))?
                .into(),
            Ty::Void | Ty::Never => {
                return Err(failed(format!("{ty:?} is not a type a value can have")));
            }
        })
    }

    /// Declare every type, then fill in every body.
    ///
    /// Two passes because a struct may hold a pointer to itself — LIR interns
    /// the id before computing the members for the same reason (§7b) — and a
    /// named LLVM struct can be created opaque and completed later.
    fn declare_types(&mut self) {
        for def in &self.unit.types {
            self.types.push(self.context.opaque_struct_type(&def.name));
        }
        for (i, def) in self.unit.types.iter().enumerate() {
            let mut fields: Vec<BasicTypeEnum<'ctx>> = Vec::new();
            let mut at = 0u64;
            // Members in offset order, with the gaps written out. A `TypeDef`
            // lists them in declaration order and the two usually agree, but a
            // layout is free to reorder and the padding has to follow the
            // offsets rather than the names.
            let mut members: Vec<_> = def.members.iter().collect();
            members.sort_by_key(|m| m.offset);
            for m in members {
                if m.offset > at {
                    fields.push(self.pad(m.offset - at));
                }
                // A member whose type this backend cannot name leaves a hole of
                // the right size: the *layout* is still right, which is what the
                // members around it depend on. Anything that reads the member
                // fails on its own, with a message about that member.
                match self.llty(&m.ty) {
                    Ok(t) => fields.push(t),
                    Err(_) => fields.push(self.pad(self.size_of(&m.ty))),
                }
                at = m.offset + self.size_of(&m.ty);
            }
            if def.layout.size > at {
                fields.push(self.pad(def.layout.size - at));
            }
            // Packed: the padding above is already explicit, and letting LLVM
            // add its own on top would move every member off its offset.
            self.types[i].set_body(&fields, true);
        }
    }

    /// `n` bytes of nothing.
    fn pad(&self, n: u64) -> BasicTypeEnum<'ctx> {
        self.context.i8_type().array_type(n as u32).into()
    }
}

// ===< Globals and function signatures >===

impl<'ctx> Cx<'ctx, '_> {
    fn declare_globals(&mut self) -> Result<()> {
        for g in &self.unit.globals {
            let ty = self.llty(&g.ty).map_err(|e| within(&g.name, e))?;
            let global =
                self.module
                    .add_global(ty, Some(AddressSpace::default()), g.symbol.as_str());
            global.set_linkage(match g.linkage {
                // A `#static`: one definition, and other units name it.
                Linkage::External => LlvmLinkage::External,
                // A string's bytes or a vtable — nothing can name it, so each
                // unit gets its own copy and the linker never sees the symbol
                // (§11). `Private` rather than `Internal` so the name does not
                // even reach the symbol table.
                Linkage::Internal => LlvmLinkage::Private,
                // Another unit defines it: a declaration, so no initializer.
                Linkage::Imported => LlvmLinkage::External,
            });
            global.set_constant(!g.mutable);
            global.set_alignment(self.align_of(&g.ty));
            self.globals.push(global);
        }
        Ok(())
    }

    fn define_globals(&mut self) -> Result<()> {
        for (i, g) in self.unit.globals.iter().enumerate() {
            if g.linkage == Linkage::Imported {
                continue;
            }
            let value = match &g.init {
                Some(c) => self.constant(&g.ty, c).map_err(|e| within(&g.name, e))?,
                // `init: None` means zeroed, and is the only spelling of it
                // (§9): a second one would be an overlap.
                None => self.zeroed(&g.ty).map_err(|e| within(&g.name, e))?,
            };
            let declared = self.globals[i];
            if declared.get_value_type()
                != inkwell::types::AnyType::as_any_type_enum(&value.get_type())
            {
                // A constant holding an address inside an enum payload has no
                // value of the enum's own LLVM type (§7b's payload is bytes), so
                // it is built as a packed struct with the same bytes and the
                // global is re-declared at that type. Every use reaches it
                // through a pointer, which does not carry the pointee's type.
                declared.set_name("");
                let global = self.module.add_global(
                    value.get_type(),
                    Some(AddressSpace::default()),
                    g.symbol.as_str(),
                );
                global.set_linkage(declared.get_linkage());
                global.set_constant(declared.is_constant());
                global.set_alignment(declared.get_alignment());
                declared
                    .as_pointer_value()
                    .replace_all_uses_with(global.as_pointer_value());
                unsafe { declared.delete() };
                self.globals[i] = global;
            }
            self.globals[i].set_initializer(&value);
        }
        Ok(())
    }

    /// The LLVM signature of a LIR function.
    ///
    /// The parameters are the first `params` locals — LIR puts them there — and
    /// a `void` or `never` return is LLVM's `void`. Neither erases anything
    /// further: §9 already dropped every `void` parameter on both sides of every
    /// call, so the arities here agree by construction.
    ///
    /// A `#c_vararg` declaration is the one signature whose parameters are not
    /// the whole story: they are the fixed ones, and the trailing flag is what
    /// tells LLVM to lower a call to it under the platform's variadic
    /// convention rather than the ordinary one. The two conventions differ on
    /// every target this compiler has — which register a float goes in on
    /// x86-64, whether a slot is spilled on AArch64 — so this flag is the whole
    /// of what a backend must do, and getting it wrong is silent.
    fn signature(&self, f: &Function) -> Result<inkwell::types::FunctionType<'ctx>> {
        let mut params: Vec<inkwell::types::BasicMetadataTypeEnum<'ctx>> = Vec::new();
        for local in &f.locals[..f.params] {
            params.push(self.llty(&local.ty)?.into());
        }
        let variadic = f.attrs.c_variadic;
        Ok(match &f.ret {
            Ty::Void | Ty::Never => self.context.void_type().fn_type(&params, variadic),
            other => self.llty(other)?.fn_type(&params, variadic),
        })
    }

    fn declare_funcs(&mut self) -> Result<()> {
        for f in &self.unit.funcs {
            let sig = self.signature(f).map_err(|e| within(&f.name, e))?;
            // A definition nothing outside this unit names is **internal**, and
            // that is a fact only the split can establish: language visibility
            // (`attrs.public`) says who may *write* the name, and the split is
            // free to put a private function's definition in a different unit
            // from its caller, so it is the split that marks `attrs.internal`
            // once the partition is known (§11).
            //
            // It is worth the trouble because internal is what lets LLVM change
            // the function: with every caller in front of it, `GlobalOpt`
            // promotes the calling convention to `fastcc` and rewrites the call
            // sites to match, and argument promotion and dead-argument
            // elimination need the same guarantee. A function whose address
            // escapes is left alone by all of them, which is the soundness this
            // compiler would otherwise have to argue for itself.
            //
            // An instantiation may be defined by every object that needed it,
            // and they are the same function (`FunctionAttrs::shared`). `weak_odr`
            // rather than `linkonce_odr`: the one copy the split put in this unit
            // may be the one another unit calls, and a `linkonce` definition
            // nothing in its own module uses is one LLVM is free to drop.
            let linkage = if f.blocks.is_empty() {
                LlvmLinkage::External
            } else if f.attrs.shared {
                LlvmLinkage::WeakODR
            } else if f.attrs.internal {
                // `internal`, not `private`: the symbol stays in the object's
                // local table, which is what a debugger and a profiler read a
                // frame's name out of.
                LlvmLinkage::Internal
            } else {
                LlvmLinkage::External
            };
            let value = self
                .module
                .add_function(f.symbol.as_str(), sig, Some(linkage));
            // The calling convention, on the declaration *and* on every call —
            // LLVM keeps them per site, and a site that disagrees with the
            // function it calls is a miscompile rather than a diagnostic. The
            // sites read it back off this value (see `call`), so this is the one
            // place it is decided.
            value.set_call_conventions(llvm_conv(f.attrs.conv));
            if f.blocks.is_empty() {
                // A declaration. Nothing more to say about it.
            } else {
                match f.attrs.inline {
                    crate::lir::Inline::Always => {
                        value.add_attribute(
                            inkwell::attributes::AttributeLoc::Function,
                            self.context.create_enum_attribute(
                                inkwell::attributes::Attribute::get_named_enum_kind_id(
                                    "alwaysinline",
                                ),
                                0,
                            ),
                        );
                    }
                    crate::lir::Inline::Never => {
                        value.add_attribute(
                            inkwell::attributes::AttributeLoc::Function,
                            self.context.create_enum_attribute(
                                inkwell::attributes::Attribute::get_named_enum_kind_id("noinline"),
                                0,
                            ),
                        );
                    }
                    crate::lir::Inline::Default => {}
                }
                if let Some(section) = &f.attrs.section {
                    value.set_section(Some(section.as_str()));
                }
            }
            // A `-> never` function does not return, and saying so is worth a
            // great deal: every caller's `unreachable` becomes justified rather
            // than asserted.
            if matches!(f.ret, Ty::Never) {
                value.add_attribute(
                    inkwell::attributes::AttributeLoc::Function,
                    self.context.create_enum_attribute(
                        inkwell::attributes::Attribute::get_named_enum_kind_id("noreturn"),
                        0,
                    ),
                );
            }
            self.funcs.push(value);
        }
        Ok(())
    }
}

// ===< Constants >===

impl<'ctx> Cx<'ctx, '_> {
    /// All-zero, of the right shape.
    fn zeroed(&self, ty: &Ty) -> Result<BasicValueEnum<'ctx>> {
        Ok(match self.llty(ty)? {
            BasicTypeEnum::IntType(t) => t.const_zero().into(),
            BasicTypeEnum::FloatType(t) => t.const_zero().into(),
            BasicTypeEnum::PointerType(t) => t.const_null().into(),
            BasicTypeEnum::ArrayType(t) => t.const_zero().into(),
            BasicTypeEnum::StructType(t) => t.const_zero().into(),
            BasicTypeEnum::VectorType(t) => t.const_zero().into(),
            BasicTypeEnum::ScalableVectorType(t) => t.const_zero().into(),
        })
    }

    /// A LIR constant, as a value of `ty`.
    ///
    /// The type comes from context in every case — a slot, a member, a
    /// parameter — because a `Constant::Int` carries a number and not a width
    /// (§9). That is the same reason `Rvalue::Op` carries the type it runs at.
    fn constant(&self, ty: &Ty, c: &Constant) -> Result<BasicValueEnum<'ctx>> {
        Ok(match c {
            Constant::Int(n) => self.int_constant(ty, n)?.into(),
            Constant::Float(x) => match self.llty(ty)? {
                BasicTypeEnum::FloatType(t) => t.const_float(*x).into(),
                // A float constant settling on an integer slot is a lowering
                // bug, not something to round here.
                other => return Err(failed(format!("a float constant in a {other:?} slot"))),
            },
            Constant::Bool(b) => self.context.i8_type().const_int(*b as u64, false).into(),
            Constant::Func(id) => self
                .funcs
                .get(id.0 as usize)
                .ok_or_else(|| failed(format!("function #{} is not in this unit", id.0)))?
                .as_global_value()
                .as_pointer_value()
                .into(),
            Constant::Global(id) => self
                .globals
                .get(id.0 as usize)
                .ok_or_else(|| failed(format!("global #{} is not in this unit", id.0)))?
                .as_pointer_value()
                .into(),
            Constant::Bytes(bytes) => {
                let byte = self.context.i8_type();
                let vals: Vec<IntValue<'ctx>> = bytes
                    .iter()
                    .map(|b| byte.const_int(*b as u64, false))
                    .collect();
                byte.const_array(&vals).into()
            }
            Constant::Aggregate(fields) => self.aggregate_constant(ty, fields, None)?,
            Constant::Variant { tag, name, payload } => {
                self.variant_constant(ty, *tag, name, payload)?
            }
            // `undef` is a value the program never reads — the `()` a call
            // returns, a slot before its first write.
            Constant::Undef => match self.llty(ty)? {
                BasicTypeEnum::IntType(t) => t.get_undef().into(),
                BasicTypeEnum::FloatType(t) => t.get_undef().into(),
                BasicTypeEnum::PointerType(t) => t.get_undef().into(),
                BasicTypeEnum::ArrayType(t) => t.get_undef().into(),
                BasicTypeEnum::StructType(t) => t.get_undef().into(),
                BasicTypeEnum::VectorType(t) => t.get_undef().into(),
                BasicTypeEnum::ScalableVectorType(t) => t.get_undef().into(),
            },
        })
    }

    /// An integer constant of arbitrary precision, narrowed to `ty`'s width.
    ///
    /// LIR keeps a constant as a `BigInt` because `u4096` is a legal type and a
    /// `::` binding *is* its value (§2.5). The two-word fast path is not an
    /// optimization — `const_int_arbitrary_precision` is fine for 64 bits too —
    /// but the sign handling below is easier to be sure of on the common case.
    fn int_constant(&self, ty: &Ty, n: &num_bigint::BigInt) -> Result<IntValue<'ctx>> {
        let (bits, signed) = match ty {
            Ty::Int { bits, signed } => (*bits as u32, *signed),
            Ty::Bool => (8, false),
            // An integer constant landing in a pointer slot is a null, or an
            // address a `transmute` produced; either way the width is the
            // machine's.
            Ty::Ptr(_) | Ty::Func { .. } => (self.pointer_bytes as u32 * 8, false),
            other => {
                return Err(failed(format!("an integer constant in a {other:?} slot")));
            }
        };
        let int = self.int_type(bits)?;
        if let Ok(small) = i64::try_from(n) {
            let v = int.const_int(small as u64, signed);
            return Ok(if matches!(ty, Ty::Ptr(_) | Ty::Func { .. }) {
                // A pointer slot wants a pointer, and an integer is how the
                // constant arrived.
                self.builder
                    .build_int_to_ptr(v, self.context.ptr_type(AddressSpace::default()), "")
                    .map_err(|e| failed(e))?
                    .const_to_int(int)
            } else {
                v
            });
        }
        // Wider than a word. `to_u64_digits` gives the magnitude little-endian;
        // a negative number is two's complement at this width, which is the
        // magnitude subtracted from `1 << bits`.
        use num_traits::Signed;
        let magnitude = n.abs();
        let value = if n.is_negative() {
            (num_bigint::BigInt::from(1) << bits) - magnitude
        } else {
            magnitude
        };
        let (_, words) = value.to_u64_digits();
        Ok(int.const_int_arbitrary_precision(&words))
    }

    /// A struct constant: the members at their offsets, with the padding
    /// between them written out, exactly as [`Cx::declare_types`] declared it.
    fn aggregate_constant(
        &self,
        ty: &Ty,
        fields: &[Constant],
        // Set when the members come from a *variant*'s type while the struct
        // being built is the enum's payload.
        _within: Option<TypeId>,
    ) -> Result<BasicValueEnum<'ctx>> {
        match ty {
            Ty::Array { len, elem } => {
                if fields.len() as u64 != *len {
                    return Err(failed(format!(
                        "an array constant of {} elements for a [{len}] slot",
                        fields.len()
                    )));
                }
                let vals: Vec<BasicValueEnum<'ctx>> = fields
                    .iter()
                    .map(|c| self.constant(elem, c))
                    .collect::<Result<_>>()?;
                let elem_ty = self.llty(elem)?;
                // Elements of one LIR type can still come out as different
                // LLVM shapes (see `shaped`), and an array cannot hold those —
                // a packed struct of them back to back is the same bytes.
                if vals.iter().any(|v| v.get_type() != elem_ty) {
                    return Ok(self.context.const_struct(&vals, true).into());
                }
                Ok(const_array(elem_ty, &vals))
            }
            Ty::Named(id) => {
                let def = self.unit.ty(*id);
                let mut members: Vec<_> = def.members.iter().collect();
                members.sort_by_key(|m| m.offset);
                if fields.len() != members.len() {
                    return Err(failed(format!(
                        "`{}` has {} members and its constant has {}",
                        def.name,
                        members.len(),
                        fields.len()
                    )));
                }
                let mut values: Vec<BasicValueEnum<'ctx>> = Vec::new();
                let mut at = 0u64;
                for (m, c) in members.iter().zip(fields) {
                    // A zero-byte member contributes no bytes, the same way it
                    // takes no store above.
                    if self.size_of(&m.ty) == 0 {
                        continue;
                    }
                    if m.offset > at {
                        values.push(self.zero_bytes(m.offset - at));
                    }
                    values.push(self.constant(&m.ty, c)?);
                    at = m.offset + self.size_of(&m.ty);
                }
                if def.layout.size > at {
                    values.push(self.zero_bytes(def.layout.size - at));
                }
                Ok(self.shaped(*id, &values))
            }
            other => Err(failed(format!("an aggregate constant in a {other:?} slot"))),
        }
    }

    /// One variant of an enum, as a constant: the tag, then the payload bytes.
    ///
    /// The payload is `[N]u8` (§7b) and the variant's members are at offsets
    /// *within* it, so the bytes are assembled by hand rather than by building
    /// the variant's struct — a `[N]u8` member cannot hold a struct value, and
    /// the whole point of the byte array is that every variant fits it.
    fn variant_constant(
        &self,
        ty: &Ty,
        tag: i128,
        name: &crate::common::symbol::Symbol,
        payload: &[Constant],
    ) -> Result<BasicValueEnum<'ctx>> {
        let Ty::Named(id) = ty else {
            return Err(failed(format!("a variant constant in a {ty:?} slot")));
        };
        let def = self.unit.ty(*id);
        let mut members: Vec<_> = def.members.iter().collect();
        members.sort_by_key(|m| m.offset);
        let mut values: Vec<BasicValueEnum<'ctx>> = Vec::new();
        let mut at = 0u64;
        for (i, m) in members.iter().enumerate() {
            if m.offset > at {
                values.push(self.zero_bytes(m.offset - at));
            }
            // Member 0 is the tag; everything after it is the payload, which is
            // zero for a variant that has none.
            if i == 0 {
                values.push(
                    self.int_constant(&m.ty, &num_bigint::BigInt::from(tag))?
                        .into(),
                );
            } else if payload.is_empty() {
                values.push(self.zeroed(&m.ty)?);
            } else {
                // The payload is `[N]u8`, and what goes in it may be an
                // address, which no byte can spell. So its members are laid
                // out one by one at the variant's own offsets, and the result
                // is a packed struct of the same bytes rather than the array.
                let variant = match &def.origin {
                    crate::lir::Origin::Enum { variants } => variants.iter().find(|v| v.tag == tag),
                    _ => None,
                }
                .ok_or_else(|| failed(format!("`{}.{name}` is not a variant", def.name)))?;
                let vdef = self.unit.ty(variant.ty);
                let mut fields: Vec<_> = vdef.members.iter().collect();
                fields.sort_by_key(|f| f.offset);
                if fields.len() != payload.len() {
                    return Err(failed(format!(
                        "`{}.{name}` has {} members and its constant has {}",
                        def.name,
                        fields.len(),
                        payload.len()
                    )));
                }
                let mut inner = 0u64;
                for (f, c) in fields.iter().zip(payload) {
                    if self.size_of(&f.ty) == 0 {
                        continue;
                    }
                    if f.offset > inner {
                        values.push(self.zero_bytes(f.offset - inner));
                    }
                    values.push(self.constant(&f.ty, c)?);
                    inner = f.offset + self.size_of(&f.ty);
                }
                let size = self.size_of(&m.ty);
                if size > inner {
                    values.push(self.zero_bytes(size - inner));
                }
                at = m.offset + size;
                continue;
            }
            at = m.offset + self.size_of(&m.ty);
        }
        if def.layout.size > at {
            values.push(self.zero_bytes(def.layout.size - at));
        }
        Ok(self.shaped(*id, &values))
    }

    /// `values` as a constant of the named type `id` when they are exactly its
    /// members, and otherwise as a packed struct of the same bytes.
    ///
    /// Every gap is already an explicit run of zero bytes, so the named type
    /// has no implicit padding and packing it moves nothing.
    fn shaped(&self, id: TypeId, values: &[BasicValueEnum<'ctx>]) -> BasicValueEnum<'ctx> {
        let named = self.types[id.0 as usize];
        let exact = named.count_fields() as usize == values.len()
            && values
                .iter()
                .zip(named.get_field_types())
                .all(|(v, t)| v.get_type() == t);
        if exact {
            named.const_named_struct(values).into()
        } else {
            self.context.const_struct(values, true).into()
        }
    }

    fn zero_bytes(&self, n: u64) -> BasicValueEnum<'ctx> {
        self.context
            .i8_type()
            .array_type(n as u32)
            .const_zero()
            .into()
    }
}

/// The value a call produced, or `None` when it returned `void`.
fn basic<'ctx>(site: inkwell::values::CallSiteValue<'ctx>) -> Option<BasicValueEnum<'ctx>> {
    site.try_as_basic_value().basic()
}

/// `const_array`, dispatched on the element type — inkwell's is per-type.
fn const_array<'ctx>(
    elem: BasicTypeEnum<'ctx>,
    vals: &[BasicValueEnum<'ctx>],
) -> BasicValueEnum<'ctx> {
    match elem {
        BasicTypeEnum::IntType(t) => {
            let v: Vec<IntValue<'ctx>> = vals.iter().map(|x| x.into_int_value()).collect();
            t.const_array(&v).into()
        }
        BasicTypeEnum::FloatType(t) => {
            let v: Vec<_> = vals.iter().map(|x| x.into_float_value()).collect();
            t.const_array(&v).into()
        }
        BasicTypeEnum::PointerType(t) => {
            let v: Vec<PointerValue<'ctx>> = vals.iter().map(|x| x.into_pointer_value()).collect();
            t.const_array(&v).into()
        }
        BasicTypeEnum::StructType(t) => {
            let v: Vec<_> = vals.iter().map(|x| x.into_struct_value()).collect();
            t.const_array(&v).into()
        }
        BasicTypeEnum::ArrayType(t) => {
            let v: Vec<_> = vals.iter().map(|x| x.into_array_value()).collect();
            t.const_array(&v).into()
        }
        BasicTypeEnum::VectorType(t) => {
            let v: Vec<_> = vals.iter().map(|x| x.into_vector_value()).collect();
            t.const_array(&v).into()
        }
        // A scalable vector has no constant array form, and no LIR type maps
        // onto one: nothing reaches here.
        BasicTypeEnum::ScalableVectorType(t) => t.const_zero().into(),
    }
}

// ===< Function bodies >===

/// What one function's emission needs beyond the unit's.
struct FnCx<'ctx> {
    value: FunctionValue<'ctx>,
    /// One `alloca` per LIR local, in the entry block. Every local has an
    /// address because every LIR place is one; LLVM's `mem2reg` is what turns
    /// the ones that did not need it back into registers, and it is much better
    /// at that than this file would be.
    slots: Vec<PointerValue<'ctx>>,
    blocks: Vec<BasicBlock<'ctx>>,
}

impl<'ctx> Cx<'ctx, '_> {
    fn define_func(&mut self, index: usize, f: &Function) -> Result<()> {
        let value = self.funcs[index];
        let entry = self.context.append_basic_block(value, "entry");

        // Every block up front, so a jump to a later one resolves.
        let mut blocks = Vec::with_capacity(f.blocks.len());
        for b in &f.blocks {
            let name = match &b.label {
                Some(l) => format!("bb{}.{}", b.id.0, sanitize(l)),
                None => format!("bb{}", b.id.0),
            };
            blocks.push(self.context.append_basic_block(value, &name));
        }

        self.builder.position_at_end(entry);
        let mut slots = Vec::with_capacity(f.locals.len());
        for local in &f.locals {
            let ty = self.llty(&local.ty)?;
            let name = local
                .name
                .as_ref()
                .map(|n| format!("{}_{}", sanitize(n.as_str()), local.id.0))
                .unwrap_or_else(|| format!("_{}", local.id.0));
            let slot = self.builder.build_alloca(ty, &name).map_err(failed)?;
            // LIR decided the alignment; the packed struct types above have an
            // LLVM alignment of 1, so saying it here is what keeps a load of a
            // member from being a misaligned one on a target that cares.
            slot.as_instruction()
                .map(|i| i.set_alignment(self.align_of(&local.ty)));
            slots.push(slot);
        }
        // The parameters are the first locals, and a parameter arrives in a
        // register: store it into its slot so the body reads it like any other.
        for (i, _) in f.locals[..f.params].iter().enumerate() {
            let arg = value
                .get_nth_param(i as u32)
                .ok_or_else(|| failed(format!("{}: no parameter {i}", f.name)))?;
            self.builder.build_store(slots[i], arg).map_err(failed)?;
        }
        // The stack check, for a function that can reach itself (§7d). Its
        // frame is already allocated by this point — the `alloca`s above are it
        // — so comparing this frame's address against the floor is asking
        // exactly the right question: is there room for *this* call.
        let first = if f.attrs.recursive {
            let check = self.stack_check(value, blocks[0], &f.name)?;
            // `stack_check` left the builder in the block it ends with; the
            // entry block is still waiting for its terminator.
            self.builder.position_at_end(entry);
            check
        } else {
            blocks[0]
        };
        self.builder
            .build_unconditional_branch(first)
            .map_err(failed)?;

        let fx = FnCx {
            value,
            slots,
            blocks,
        };
        for (i, b) in f.blocks.iter().enumerate() {
            self.builder.position_at_end(fx.blocks[i]);
            for stmt in &b.stmts {
                self.stmt(&fx, f, &stmt.kind)?;
            }
            self.terminator(&fx, f, &b.term.kind)?;
        }
        Ok(())
    }

    fn stmt(&self, fx: &FnCx<'ctx>, f: &Function, kind: &StmtKind) -> Result<()> {
        match kind {
            StmtKind::Assign { place, value } => self.assign(fx, f, place, value),
            StmtKind::Call { dest, callee, args } => self.call(fx, f, dest.as_ref(), callee, args),
            // A release. The runtime's free, which under a collector is a hint
            // it is free to ignore — but it is the one the escape analysis (§5)
            // and a written `drop(p)` both produce, so there is one instruction
            // rather than two.
            StmtKind::Drop(operand) => {
                let v = self.operand(fx, f, operand, &Ty::ptr(Ty::Bool))?;
                let free = self.runtime("nest_free", &[self.ptr().into()], None);
                self.builder
                    .build_call(free, &[v.into()], "")
                    .map_err(failed)?;
                Ok(())
            }
        }
    }
}

/// A name LLVM will not choke on in IR text.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ===< Places, operands and assignment >===

impl<'ctx> Cx<'ctx, '_> {
    fn ptr(&self) -> inkwell::types::PointerType<'ctx> {
        self.context.ptr_type(AddressSpace::default())
    }

    /// The word-sized integer type — what a length, an index and a size are.
    /// The LIR type a byte count arrives at: an unsigned word.
    fn word_ty(&self) -> Ty {
        Ty::Int {
            bits: (self.pointer_bytes * 8) as u16,
            signed: false,
        }
    }

    fn word(&self) -> inkwell::types::IntType<'ctx> {
        // The pointer width is never zero, so this cannot be the error case.
        self.int_type(self.pointer_bytes as u32 * 8)
            .expect("a pointer is at least one bit wide")
    }

    /// `ptr + n` bytes.
    fn at(&self, ptr: PointerValue<'ctx>, n: u64) -> Result<PointerValue<'ctx>> {
        if n == 0 {
            return Ok(ptr);
        }
        let off = self.word().const_int(n, false);
        // Indexing `i8` is what makes the index the byte offset LIR computed.
        unsafe {
            self.builder
                .build_gep(self.context.i8_type(), ptr, &[off], "")
                .map_err(failed)
        }
    }

    /// `ptr + index * stride` bytes, with `index` a run-time value.
    fn at_dynamic(
        &self,
        ptr: PointerValue<'ctx>,
        index: IntValue<'ctx>,
        stride: u64,
    ) -> Result<PointerValue<'ctx>> {
        let word = self.word();
        // An index arrives at whatever width its type had; the arithmetic below
        // happens at the machine's, which is the only width an address has.
        let index = self
            .builder
            .build_int_cast(index, word, "")
            .map_err(failed)?;
        let off = self
            .builder
            .build_int_mul(index, word.const_int(stride, false), "")
            .map_err(failed)?;
        unsafe {
            self.builder
                .build_gep(self.context.i8_type(), ptr, &[off], "")
                .map_err(failed)
        }
    }

    /// Walk a place to an address, keeping the type of what is at it.
    ///
    /// The type travels because every projection asks it a question the address
    /// cannot answer — which member, what stride, what to load — and because
    /// with opaque pointers the address itself carries nothing.
    fn place(&self, fx: &FnCx<'ctx>, f: &Function, p: &Place) -> Result<(PointerValue<'ctx>, Ty)> {
        let (mut ptr, mut ty) = match p.base {
            Base::Local(id) => (
                *fx.slots
                    .get(id.0 as usize)
                    .ok_or_else(|| failed(format!("{}: no local _{}", f.name, id.0)))?,
                f.locals[id.0 as usize].ty.clone(),
            ),
            Base::Global(id) => (
                self.globals
                    .get(id.0 as usize)
                    .ok_or_else(|| failed(format!("global #{} is not in this unit", id.0)))?
                    .as_pointer_value(),
                self.unit.globals[id.0 as usize].ty.clone(),
            ),
        };
        for proj in &p.projection {
            match proj {
                Projection::Field { index, name } => {
                    let Ty::Named(tid) = &ty else {
                        return Err(failed(format!("{}: `.{name}` on a {ty:?}", f.name)));
                    };
                    let def = self.unit.ty(*tid);
                    let m = def.members.get(*index as usize).ok_or_else(|| {
                        failed(format!("{}: `{}` has no member {index}", f.name, def.name))
                    })?;
                    ptr = self.at(ptr, m.offset)?;
                    ty = m.ty.clone();
                }
                Projection::Index(operand) => {
                    let elem = match &ty {
                        Ty::Array { elem, .. } => (**elem).clone(),
                        // A place never indexes a slice — an invariant test says
                        // so (§10) — but a raw pointer is indexable and arrives
                        // here from `core`'s `index` impls.
                        Ty::Ptr(inner) => (**inner).clone(),
                        other => return Err(failed(format!("{}: indexing a {other:?}", f.name))),
                    };
                    let stride = self.size_of(&elem);
                    let index = self
                        .operand(
                            fx,
                            f,
                            operand,
                            &Ty::Int {
                                bits: self.pointer_bytes as u16 * 8,
                                signed: false,
                            },
                        )?
                        .into_int_value();
                    ptr = self.at_dynamic(ptr, index, stride)?;
                    ty = elem;
                }
                Projection::Deref => {
                    let Ty::Ptr(inner) = &ty else {
                        return Err(failed(format!("{}: dereferencing a {ty:?}", f.name)));
                    };
                    let inner = (**inner).clone();
                    ptr = self
                        .builder
                        .build_load(self.ptr(), ptr, "")
                        .map_err(failed)?
                        .into_pointer_value();
                    ty = inner;
                }
                // Reading an enum's payload as one variant's type (§7b). The
                // payload is `[N]u8` and aligned for every variant, so the cast
                // is always legal and is nothing at all with opaque pointers —
                // what it changes is the *type* the walk carries from here on,
                // which is what makes the next member read find its offset.
                Projection::Cast(tid) => ty = Ty::Named(*tid),
            }
        }
        Ok((ptr, ty))
    }

    /// Load a value, or build a constant of the type the context expects.
    fn operand(
        &self,
        fx: &FnCx<'ctx>,
        f: &Function,
        op: &Operand,
        expect: &Ty,
    ) -> Result<BasicValueEnum<'ctx>> {
        match op {
            Operand::Copy(p) => {
                let (ptr, ty) = self.place(fx, f, p)?;
                let llty = self.llty(&ty)?;
                let value = self.builder.build_load(llty, ptr, "").map_err(failed)?;
                if let Some(i) = value.as_instruction_value() {
                    let _ = i.set_alignment(self.align_of(&ty));
                }
                Ok(value)
            }
            // A constant carries no type of its own (§9), so the one the context
            // asked for is the one it becomes.
            Operand::Const(c) => self.constant(expect, c),
        }
    }

    /// The type of what an operand reads, when there is one.
    fn operand_ty(&self, fx: &FnCx<'ctx>, f: &Function, op: &Operand) -> Option<Ty> {
        match op {
            Operand::Copy(p) => self.place(fx, f, p).ok().map(|(_, t)| t),
            Operand::Const(_) => None,
        }
    }

    fn store(&self, ptr: PointerValue<'ctx>, value: BasicValueEnum<'ctx>, ty: &Ty) -> Result<()> {
        let i = self.builder.build_store(ptr, value).map_err(failed)?;
        let _ = i.set_alignment(self.align_of(ty));
        Ok(())
    }

    /// `place := rvalue`.
    ///
    /// Written as "compute the address, then put the parts there" rather than
    /// "build a value, then store it", because three of the six rvalues produce
    /// something with more than one part and an LLVM aggregate value would be a
    /// detour through a form no target has.
    fn assign(&self, fx: &FnCx<'ctx>, f: &Function, place: &Place, value: &Rvalue) -> Result<()> {
        let (ptr, ty) = self.place(fx, f, place)?;
        match value {
            // Copying an **aggregate** is a `memcpy`. An array or a struct
            // moved from one place to another is a block of bytes going
            // somewhere, which is the operation every target has; going through
            // a load and a store instead asks LLVM to build a first-class value
            // of every element first — a form no machine has a register for. At
            // a megabyte that costs minutes of compile time, and at two words it
            // costs nothing either way, because the optimizer turns a short
            // `memcpy` back into the pair.
            Rvalue::Use(Operand::Copy(src)) if matches!(ty, Ty::Array { .. } | Ty::Named(_)) => {
                let (from, _) = self.place(fx, f, src)?;
                let align = self.align_of(&ty);
                let bytes = self.word().const_int(self.size_of(&ty), false);
                self.builder
                    .build_memcpy(ptr, align, from, align, bytes)
                    .map_err(failed)?;
                Ok(())
            }
            Rvalue::Use(o) => {
                let v = self.operand(fx, f, o, &ty)?;
                self.store(ptr, v, &ty)
            }
            Rvalue::Ref(p) => {
                let (addr, _) = self.place(fx, f, p)?;
                self.store(ptr, addr.into(), &ty)
            }
            Rvalue::Op { op, ty: at, args } if op.is_checked() => {
                self.checked(fx, f, *op, at, args, ptr, &ty)
            }
            Rvalue::Op { op, ty: at, args } => {
                let v = self.op(fx, f, *op, at, args)?;
                self.store(ptr, v, &ty)
            }
            Rvalue::Cast {
                value,
                kind,
                from,
                to,
            } => {
                let v = self.cast(fx, f, value, *kind, from, to)?;
                self.store(ptr, v, &ty)
            }
            Rvalue::Aggregate { kind, fields } => self.aggregate(fx, f, kind, fields, ptr, &ty),
            Rvalue::Offset {
                ptr: base,
                index,
                stride,
            } => {
                let base = self
                    .operand(fx, f, base, &Ty::ptr(Ty::Bool))?
                    .into_pointer_value();
                let index = self
                    .operand(
                        fx,
                        f,
                        index,
                        &Ty::Int {
                            bits: self.pointer_bytes as u16 * 8,
                            signed: false,
                        },
                    )?
                    .into_int_value();
                let v = self.at_dynamic(base, index, *stride)?;
                self.store(ptr, v.into(), &ty)
            }
        }
    }
}

// ===< Arithmetic >===

impl<'ctx> Cx<'ctx, '_> {
    /// Widen the `i1` a comparison produces into the byte a `bool` is.
    fn as_bool(&self, v: IntValue<'ctx>) -> Result<BasicValueEnum<'ctx>> {
        Ok(self
            .builder
            .build_int_z_extend(v, self.context.i8_type(), "")
            .map_err(failed)?
            .into())
    }

    /// One primitive operation, at the type it runs at.
    fn op(
        &self,
        fx: &FnCx<'ctx>,
        f: &Function,
        op: Op,
        at: &Ty,
        args: &[Operand],
    ) -> Result<BasicValueEnum<'ctx>> {
        if args.len() != op.arity() {
            return Err(failed(format!(
                "{}: {} takes {} operands and has {}",
                f.name,
                op.name(),
                op.arity(),
                args.len()
            )));
        }
        let vals: Vec<BasicValueEnum<'ctx>> = args
            .iter()
            .map(|a| self.operand(fx, f, a, at))
            .collect::<Result<_>>()?;
        let signed = matches!(at, Ty::Int { signed: true, .. });
        let float = matches!(at, Ty::Float { .. });
        let b = &self.builder;

        if float {
            let x = vals[0].into_float_value();
            let y = || vals[1].into_float_value();
            use inkwell::FloatPredicate as P;
            return Ok(match op {
                Op::Add => b.build_float_add(x, y(), "").map_err(failed)?.into(),
                Op::Sub => b.build_float_sub(x, y(), "").map_err(failed)?.into(),
                Op::Mul => b.build_float_mul(x, y(), "").map_err(failed)?.into(),
                Op::Div => b.build_float_div(x, y(), "").map_err(failed)?.into(),
                Op::Rem => b.build_float_rem(x, y(), "").map_err(failed)?.into(),
                Op::Neg => b.build_float_neg(x, "").map_err(failed)?.into(),
                // Ordered comparisons: a NaN compares false, which is IEEE and
                // is what every language that does not special-case NaN wants.
                Op::Eq => {
                    self.as_bool(b.build_float_compare(P::OEQ, x, y(), "").map_err(failed)?)?
                }
                Op::Ne => {
                    self.as_bool(b.build_float_compare(P::UNE, x, y(), "").map_err(failed)?)?
                }
                Op::Lt => {
                    self.as_bool(b.build_float_compare(P::OLT, x, y(), "").map_err(failed)?)?
                }
                Op::Le => {
                    self.as_bool(b.build_float_compare(P::OLE, x, y(), "").map_err(failed)?)?
                }
                Op::Gt => {
                    self.as_bool(b.build_float_compare(P::OGT, x, y(), "").map_err(failed)?)?
                }
                Op::Ge => {
                    self.as_bool(b.build_float_compare(P::OGE, x, y(), "").map_err(failed)?)?
                }
                other => {
                    return Err(failed(format!(
                        "{}: {} is not an operation on floats",
                        f.name,
                        other.name()
                    )));
                }
            });
        }

        let x = vals[0].into_int_value();
        let y = || vals[1].into_int_value();
        use inkwell::IntPredicate as P;
        Ok(match op {
            // Wrapping, by definition (§10): `overflow=wrap`, `#unsafe` and
            // `wrapping_add` are all this instruction, so no `nsw`/`nuw` flag is
            // set and LLVM may not assume the overflow did not happen.
            Op::Add => b.build_int_add(x, y(), "").map_err(failed)?.into(),
            Op::Sub => b.build_int_sub(x, y(), "").map_err(failed)?.into(),
            Op::Mul => b.build_int_mul(x, y(), "").map_err(failed)?.into(),
            // The divide by zero is already checked and trapped before this
            // (§7d), so the instruction has no undefined case left.
            Op::Div if signed => b.build_int_signed_div(x, y(), "").map_err(failed)?.into(),
            Op::Div => b.build_int_unsigned_div(x, y(), "").map_err(failed)?.into(),
            Op::Rem if signed => b.build_int_signed_rem(x, y(), "").map_err(failed)?.into(),
            Op::Rem => b.build_int_unsigned_rem(x, y(), "").map_err(failed)?.into(),
            Op::Neg => b.build_int_neg(x, "").map_err(failed)?.into(),
            Op::BitAnd => b.build_and(x, y(), "").map_err(failed)?.into(),
            Op::BitOr => b.build_or(x, y(), "").map_err(failed)?.into(),
            Op::BitXor => b.build_xor(x, y(), "").map_err(failed)?.into(),
            Op::BitNot => b.build_not(x, "").map_err(failed)?.into(),
            Op::Shl => b.build_left_shift(x, y(), "").map_err(failed)?.into(),
            // Arithmetic for a signed type, logical for an unsigned one: the
            // type is what decides, which is why `Op` carries it.
            Op::Shr => b
                .build_right_shift(x, y(), signed, "")
                .map_err(failed)?
                .into(),
            // A `bool` is a byte holding 0 or 1, so its negation is a flip of
            // the low bit rather than a bitwise complement.
            Op::Not => b
                .build_xor(x, self.context.i8_type().const_int(1, false), "")
                .map_err(failed)?
                .into(),
            Op::Eq => self.as_bool(b.build_int_compare(P::EQ, x, y(), "").map_err(failed)?)?,
            Op::Ne => self.as_bool(b.build_int_compare(P::NE, x, y(), "").map_err(failed)?)?,
            Op::Lt if signed => {
                self.as_bool(b.build_int_compare(P::SLT, x, y(), "").map_err(failed)?)?
            }
            Op::Le if signed => {
                self.as_bool(b.build_int_compare(P::SLE, x, y(), "").map_err(failed)?)?
            }
            Op::Gt if signed => {
                self.as_bool(b.build_int_compare(P::SGT, x, y(), "").map_err(failed)?)?
            }
            Op::Ge if signed => {
                self.as_bool(b.build_int_compare(P::SGE, x, y(), "").map_err(failed)?)?
            }
            Op::Lt => self.as_bool(b.build_int_compare(P::ULT, x, y(), "").map_err(failed)?)?,
            Op::Le => self.as_bool(b.build_int_compare(P::ULE, x, y(), "").map_err(failed)?)?,
            Op::Gt => self.as_bool(b.build_int_compare(P::UGT, x, y(), "").map_err(failed)?)?,
            Op::Ge => self.as_bool(b.build_int_compare(P::UGE, x, y(), "").map_err(failed)?)?,
            Op::AddChecked | Op::SubChecked | Op::MulChecked => {
                return Err(failed("a checked operation reached the plain path"));
            }
        })
    }

    /// Checked arithmetic: the value and whether it overflowed, written to the
    /// two members of the `(T, bool)` this assigns to.
    ///
    /// LLVM's `with.overflow` intrinsics return exactly that pair, which is why
    /// LIR's checked opcodes have the shape they do — the result type is not a
    /// flag on an instruction, it is what the instruction produces (§7d).
    #[allow(clippy::too_many_arguments)]
    fn checked(
        &self,
        fx: &FnCx<'ctx>,
        f: &Function,
        op: Op,
        at: &Ty,
        args: &[Operand],
        dest: PointerValue<'ctx>,
        dest_ty: &Ty,
    ) -> Result<()> {
        let Ty::Int { signed, .. } = at else {
            return Err(failed(format!(
                "{}: {} at {at:?}, which is not an integer",
                f.name,
                op.name()
            )));
        };
        let name = match (op, signed) {
            (Op::AddChecked, true) => "llvm.sadd.with.overflow",
            (Op::AddChecked, false) => "llvm.uadd.with.overflow",
            (Op::SubChecked, true) => "llvm.ssub.with.overflow",
            (Op::SubChecked, false) => "llvm.usub.with.overflow",
            (Op::MulChecked, true) => "llvm.smul.with.overflow",
            (Op::MulChecked, false) => "llvm.umul.with.overflow",
            _ => return Err(failed("not a checked operation")),
        };
        let llat = self.llty(at)?;
        let decl = inkwell::intrinsics::Intrinsic::find(name)
            .and_then(|i| i.get_declaration(&self.module, &[llat]))
            .ok_or_else(|| failed(format!("LLVM has no `{name}` for {at:?}")))?;
        let lhs = self.operand(fx, f, &args[0], at)?;
        let rhs = self.operand(fx, f, &args[1], at)?;
        let pair = self
            .builder
            .build_call(decl, &[lhs.into(), rhs.into()], "")
            .map_err(failed)?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| failed(format!("`{name}` returned nothing")))?
            .into_struct_value();
        let value = self
            .builder
            .build_extract_value(pair, 0, "")
            .map_err(failed)?;
        let flag = self
            .builder
            .build_extract_value(pair, 1, "")
            .map_err(failed)?
            .into_int_value();
        let flag = self.as_bool(flag)?;

        // The destination is the `(T, bool)` tuple, and its two members are at
        // offsets the type table decided — not at 0 and `size_of(T)`, which is
        // the same number only until a type with padding turns up.
        let Ty::Named(tid) = dest_ty else {
            return Err(failed(format!(
                "{}: {} assigns to a {dest_ty:?}",
                f.name,
                op.name()
            )));
        };
        let def = self.unit.ty(*tid);
        let (v_member, f_member) = (
            def.members
                .first()
                .ok_or_else(|| failed(format!("`{}` has no value member", def.name)))?,
            def.members
                .get(1)
                .ok_or_else(|| failed(format!("`{}` has no overflow member", def.name)))?,
        );
        self.store(self.at(dest, v_member.offset)?, value, &v_member.ty)?;
        self.store(self.at(dest, f_member.offset)?, flag, &f_member.ty)
    }

    /// A conversion, by the kind LIR recorded (§10).
    ///
    /// The widths compared here are **LLVM's**, not LIR's, and the difference
    /// matters in exactly one place: a `bool` is one bit to LIR and one byte
    /// here, so `bool -> u8` is recorded as a zero extension and is nothing at
    /// all to emit. Comparing the widths that the instruction will actually run
    /// at is what turns that into an identity instead of an illegal `zext i8 to
    /// i8`.
    fn cast(
        &self,
        fx: &FnCx<'ctx>,
        f: &Function,
        value: &Operand,
        kind: CastKind,
        from: &Ty,
        to: &Ty,
    ) -> Result<BasicValueEnum<'ctx>> {
        let v = self.operand(fx, f, value, from)?;
        let target = self.llty(to)?;
        let b = &self.builder;
        Ok(match kind {
            CastKind::Truncate
            | CastKind::ZeroExtend
            | CastKind::SignExtend
            | CastKind::Reinterpret => {
                let x = v.into_int_value();
                let t = target.into_int_type();
                match x.get_type().get_bit_width().cmp(&t.get_bit_width()) {
                    std::cmp::Ordering::Greater => {
                        b.build_int_truncate(x, t, "").map_err(failed)?.into()
                    }
                    std::cmp::Ordering::Less if matches!(kind, CastKind::SignExtend) => {
                        b.build_int_s_extend(x, t, "").map_err(failed)?.into()
                    }
                    std::cmp::Ordering::Less => {
                        b.build_int_z_extend(x, t, "").map_err(failed)?.into()
                    }
                    // Same width: a register is a register, and signedness is
                    // not a property an LLVM integer has.
                    std::cmp::Ordering::Equal => x.into(),
                }
            }
            CastKind::FloatTruncate => b
                .build_float_trunc(v.into_float_value(), target.into_float_type(), "")
                .map_err(failed)?
                .into(),
            CastKind::FloatExtend => b
                .build_float_ext(v.into_float_value(), target.into_float_type(), "")
                .map_err(failed)?
                .into(),
            CastKind::FloatToInt { signed: true } => b
                .build_float_to_signed_int(v.into_float_value(), target.into_int_type(), "")
                .map_err(failed)?
                .into(),
            CastKind::FloatToInt { signed: false } => b
                .build_float_to_unsigned_int(v.into_float_value(), target.into_int_type(), "")
                .map_err(failed)?
                .into(),
            CastKind::IntToFloat { signed: true } => b
                .build_signed_int_to_float(v.into_int_value(), target.into_float_type(), "")
                .map_err(failed)?
                .into(),
            CastKind::IntToFloat { signed: false } => b
                .build_unsigned_int_to_float(v.into_int_value(), target.into_float_type(), "")
                .map_err(failed)?
                .into(),
            CastKind::PtrToInt => b
                .build_ptr_to_int(v.into_pointer_value(), target.into_int_type(), "")
                .map_err(failed)?
                .into(),
            CastKind::IntToPtr => b
                .build_int_to_ptr(v.into_int_value(), self.ptr(), "")
                .map_err(failed)?
                .into(),
            // With opaque pointers there is nothing to change.
            CastKind::PtrCast => v,
            CastKind::Unknown => {
                return Err(failed(format!(
                    "{}: no instruction for {from:?} -> {to:?}",
                    f.name
                )));
            }
        })
    }

    /// Build an aggregate by writing each part to its offset.
    fn aggregate(
        &self,
        fx: &FnCx<'ctx>,
        f: &Function,
        kind: &Aggregate,
        fields: &[Operand],
        dest: PointerValue<'ctx>,
        dest_ty: &Ty,
    ) -> Result<()> {
        match kind {
            Aggregate::Struct(tid) => {
                let def = self.unit.ty(*tid);
                for (i, value) in fields.iter().enumerate() {
                    let m = def.members.get(i).ok_or_else(|| {
                        failed(format!("{}: `{}` has no member {i}", f.name, def.name))
                    })?;
                    // **A member of no size is written by writing nothing.**
                    // `void` is the one that turns up — `Option`'s `Residual`, a
                    // `ControlFlow.<void, T>`'s `stop` payload — and §9 erases
                    // `void` from slots, parameters and arguments but not yet
                    // from a type's members, so the operand arrives as `undef`
                    // against a type no register holds. This is not a special
                    // case for `void`, though: a zero-byte member has nothing to
                    // store whatever it is.
                    if self.size_of(&m.ty) == 0 {
                        continue;
                    }
                    let v = self.operand(fx, f, value, &m.ty)?;
                    self.store(self.at(dest, m.offset)?, v, &m.ty)?;
                }
                Ok(())
            }
            Aggregate::Array => {
                let Ty::Array { elem, .. } = dest_ty else {
                    return Err(failed(format!(
                        "{}: an array built into a {dest_ty:?}",
                        f.name
                    )));
                };
                let stride = self.size_of(elem);
                for (i, value) in fields.iter().enumerate() {
                    let v = self.operand(fx, f, value, elem)?;
                    self.store(self.at(dest, i as u64 * stride)?, v, elem)?;
                }
                Ok(())
            }
            // One variant of an enum: the tag, then the variant's own members at
            // offsets *within* the payload. Both numbers come from the type
            // table — the enum's for the tag and the payload, the variant's for
            // what is inside it (§7b).
            Aggregate::Variant {
                ty,
                variant,
                tag,
                name,
                ..
            } => {
                let def = self.unit.ty(*ty);
                let tag_member = def
                    .members
                    .first()
                    .ok_or_else(|| failed(format!("`{}` has no tag", def.name)))?;
                let tag_value =
                    self.int_constant(&tag_member.ty, &num_bigint::BigInt::from(*tag))?;
                self.store(
                    self.at(dest, tag_member.offset)?,
                    tag_value.into(),
                    &tag_member.ty,
                )?;
                if fields.is_empty() {
                    return Ok(());
                }
                let payload = def.members.get(1).ok_or_else(|| {
                    failed(format!(
                        "`{}.{name}` has a payload and `{}` has no payload member",
                        def.name, def.name
                    ))
                })?;
                let vdef = self.unit.ty(*variant);
                for (i, value) in fields.iter().enumerate() {
                    let m = vdef.members.get(i).ok_or_else(|| {
                        failed(format!("{}: `{}` has no member {i}", f.name, vdef.name))
                    })?;
                    // As above: nothing to store, so nothing is stored.
                    if self.size_of(&m.ty) == 0 {
                        continue;
                    }
                    let v = self.operand(fx, f, value, &m.ty)?;
                    self.store(self.at(dest, payload.offset + m.offset)?, v, &m.ty)?;
                }
                Ok(())
            }
        }
    }
}

// ===< Calls, intrinsics and terminators >===

impl<'ctx> Cx<'ctx, '_> {
    /// Declare a runtime function, or find the one already declared.
    ///
    /// The runtime is a small C shim (`runtime/`): the allocator is the
    /// collector's, the panic path is an abort, and swapping the collector is a
    /// link-time choice rather than a change here.
    /// Emit the prologue stack check and return the block the entry should jump
    /// to instead of the body's first.
    ///
    /// `nest_stack_floor` is the lowest address a frame may start at, written
    /// once by the runtime at startup (see `nest_runtime.c`). A **zero** floor
    /// means the runtime could not work one out, and the comparison is false
    /// for every address, so the check disables itself without a second branch.
    ///
    /// The address compared is this frame's own, taken with `llvm.frameaddress`
    /// rather than an `alloca` of its own — an `alloca` would be a slot the
    /// function then carries for the life of the call, and the frame pointer is
    /// the number actually wanted.
    fn stack_check(
        &self,
        value: FunctionValue<'ctx>,
        body: BasicBlock<'ctx>,
        name: &str,
    ) -> Result<BasicBlock<'ctx>> {
        let word = self.word();
        let floor_global = match self.module.get_global("nest_stack_floor") {
            Some(g) => g,
            None => {
                let g = self.module.add_global(word, None, "nest_stack_floor");
                g.set_linkage(LlvmLinkage::External);
                g
            }
        };
        let check = self.context.append_basic_block(value, "stack.check");
        let overflow = self.context.append_basic_block(value, "stack.overflow");
        self.builder.position_at_end(check);

        let frame = self
            .module
            .get_function("llvm.frameaddress.p0")
            .unwrap_or_else(|| {
                let sig = self.ptr().fn_type(&[self.context.i32_type().into()], false);
                self.module.add_function("llvm.frameaddress.p0", sig, None)
            });
        let here = self
            .builder
            .build_call(frame, &[self.context.i32_type().const_zero().into()], "")
            .map_err(failed)?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| failed(format!("{name}: frameaddress returned nothing")))?
            .into_pointer_value();
        let here = self
            .builder
            .build_ptr_to_int(here, word, "")
            .map_err(failed)?;
        let floor = self
            .builder
            .build_load(word, floor_global.as_pointer_value(), "")
            .map_err(failed)?
            .into_int_value();
        let low = self
            .builder
            .build_int_compare(inkwell::IntPredicate::ULT, here, floor, "")
            .map_err(failed)?;
        self.builder
            .build_conditional_branch(low, overflow, body)
            .map_err(failed)?;

        self.builder.position_at_end(overflow);
        let report = self.runtime("nest_stack_overflow", &[], None);
        report.add_attribute(
            inkwell::attributes::AttributeLoc::Function,
            self.context.create_enum_attribute(
                inkwell::attributes::Attribute::get_named_enum_kind_id("noreturn"),
                0,
            ),
        );
        self.builder.build_call(report, &[], "").map_err(failed)?;
        self.builder.build_unreachable().map_err(failed)?;
        Ok(check)
    }

    fn runtime(
        &self,
        name: &str,
        params: &[inkwell::types::BasicMetadataTypeEnum<'ctx>],
        ret: Option<BasicTypeEnum<'ctx>>,
    ) -> FunctionValue<'ctx> {
        if let Some(existing) = self.module.get_function(name) {
            return existing;
        }
        let sig = match ret {
            Some(t) => t.fn_type(params, false),
            None => self.context.void_type().fn_type(params, false),
        };
        self.module
            .add_function(name, sig, Some(LlvmLinkage::External))
    }

    fn call(
        &self,
        fx: &FnCx<'ctx>,
        f: &Function,
        dest: Option<&Place>,
        callee: &Callee,
        args: &[Operand],
    ) -> Result<()> {
        match callee {
            Callee::Static(id) => {
                let target = &self.unit.funcs[id.0 as usize];
                let value = self.funcs[id.0 as usize];
                let mut built: Vec<BasicMetadataValueEnum<'ctx>> = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    // Past the fixed parameters of a `#c_vararg` callee there is
                    // no parameter to take a type from, so the argument's own is
                    // the only one there is — which is exactly what C does, and
                    // why the front end has already promoted it.
                    let want = match target.locals.get(i) {
                        Some(l) if i < target.params => l.ty.clone(),
                        _ if target.attrs.c_variadic && i >= target.params => {
                            self.operand_ty(fx, f, a).ok_or_else(|| {
                                failed(format!("{}: a variadic argument with no type", f.name))
                            })?
                        }
                        Some(l) => l.ty.clone(),
                        None => {
                            return Err(failed(format!(
                                "{}: `{}` takes no argument {i}",
                                f.name, target.name
                            )));
                        }
                    };
                    built.push(self.operand(fx, f, a, &want)?.into());
                }
                let site = self.builder.build_call(value, &built, "").map_err(failed)?;
                // The callee's convention, read off the declaration: a call site
                // LLVM leaves at the default would pass its arguments one way
                // and the callee would read them another.
                site.set_call_convention(value.get_call_conventions());
                self.take(fx, f, dest, basic(site))
            }
            Callee::Indirect(operand) => {
                // The signature comes from the pointer's own type, which is the
                // only place it is written down at this level.
                let ty = self.operand_ty(fx, f, operand).ok_or_else(|| {
                    failed(format!("{}: an indirect call through a constant", f.name))
                })?;
                let (params, ret) = match &ty {
                    Ty::Func { params, ret } => (params.clone(), (**ret).clone()),
                    Ty::Ptr(inner) => match &**inner {
                        Ty::Func { params, ret } => (params.clone(), (**ret).clone()),
                        other => return Err(failed(format!("{}: calling a {other:?}", f.name))),
                    },
                    other => return Err(failed(format!("{}: calling a {other:?}", f.name))),
                };
                let pointer = self.operand(fx, f, operand, &ty)?.into_pointer_value();
                let mut metadata: Vec<inkwell::types::BasicMetadataTypeEnum<'ctx>> = Vec::new();
                for p in &params {
                    metadata.push(self.llty(p)?.into());
                }
                let sig = match &ret {
                    Ty::Void | Ty::Never => self.context.void_type().fn_type(&metadata, false),
                    other => self.llty(other)?.fn_type(&metadata, false),
                };
                let mut built: Vec<BasicMetadataValueEnum<'ctx>> = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    let want = params.get(i).cloned().ok_or_else(|| {
                        failed(format!("{}: the callee takes no argument {i}", f.name))
                    })?;
                    built.push(self.operand(fx, f, a, &want)?.into());
                }
                let site = self
                    .builder
                    .build_indirect_call(sig, pointer, &built, "")
                    .map_err(failed)?;
                self.take(fx, f, dest, basic(site))
            }
            Callee::Intrinsic(i) => self.intrinsic(fx, f, dest, i, args),
        }
    }

    /// Put a call's result where it goes, if it has one and anybody wanted it.
    fn take(
        &self,
        fx: &FnCx<'ctx>,
        f: &Function,
        dest: Option<&Place>,
        value: Option<BasicValueEnum<'ctx>>,
    ) -> Result<()> {
        let (Some(place), Some(value)) = (dest, value) else {
            return Ok(());
        };
        let (ptr, ty) = self.place(fx, f, place)?;
        self.store(ptr, value, &ty)
    }

    fn intrinsic(
        &self,
        fx: &FnCx<'ctx>,
        f: &Function,
        dest: Option<&Place>,
        which: &Intrinsic,
        args: &[Operand],
    ) -> Result<()> {
        let word = self.word();
        match which {
            // The last instruction, which no library can write (§6.10).
            Intrinsic::Trap => {
                let trap = self.runtime("nest_trap", &[], None);
                let site = self.builder.build_call(trap, &[], "").map_err(failed)?;
                site.add_attribute(
                    inkwell::attributes::AttributeLoc::Function,
                    self.context.create_enum_attribute(
                        inkwell::attributes::Attribute::get_named_enum_kind_id("noreturn"),
                        0,
                    ),
                );
                Ok(())
            }
            // `new.<T>()` — one `T`'s worth of collected memory. The size comes
            // from the destination's pointee, which is where the type argument
            // ended up by this level.
            Intrinsic::New => {
                let place =
                    dest.ok_or_else(|| failed(format!("{}: `new` with no destination", f.name)))?;
                let (ptr, ty) = self.place(fx, f, place)?;
                let Ty::Ptr(inner) = &ty else {
                    return Err(failed(format!("{}: `new` assigns to a {ty:?}", f.name)));
                };
                let size = word.const_int(self.size_of(inner), false);
                let value = self.alloc(size)?;
                self.store(ptr, value.into(), &ty)
            }
            // `make.<[]T>(n)` — `n` elements, and the slice header over them.
            Intrinsic::Make => {
                let place =
                    dest.ok_or_else(|| failed(format!("{}: `make` with no destination", f.name)))?;
                let (ptr, ty) = self.place(fx, f, place)?;
                let (elem, ptr_member, len_member) = self.slice_shape(&ty, &f.name)?;
                let len = self
                    .operand(fx, f, &args[0], &len_member.ty)?
                    .into_int_value();
                let len_word = self.builder.build_int_cast(len, word, "").map_err(failed)?;
                let bytes = self
                    .builder
                    .build_int_mul(len_word, word.const_int(self.size_of(&elem), false), "")
                    .map_err(failed)?;
                let data = self.alloc(bytes)?;
                self.store(
                    self.at(ptr, ptr_member.offset)?,
                    data.into(),
                    &ptr_member.ty,
                )?;
                self.store(self.at(ptr, len_member.offset)?, len.into(), &len_member.ty)
            }
            // One call, whatever the length — see [`Intrinsic::Memset`]. LLVM
            // lowers it to the target's own fill, and recognizes a zero one as
            // the zeroing it is.
            Intrinsic::Memset => {
                let dest = self
                    .operand(fx, f, &args[0], &Ty::ptr(Ty::Bool))?
                    .into_pointer_value();
                let byte = self
                    .operand(
                        fx,
                        f,
                        &args[1],
                        &Ty::Int {
                            bits: 8,
                            signed: false,
                        },
                    )?
                    .into_int_value();
                let len = self
                    .operand(fx, f, &args[2], &self.word_ty())?
                    .into_int_value();
                self.builder
                    .build_memset(dest, 1, byte, len)
                    .map_err(failed)?;
                Ok(())
            }
            // The source and destination are addresses and the length is in
            // bytes, so both are the LLVM builtin directly. The alignment given
            // is 1: these arrive from slices of any element type, and claiming
            // more than a byte would be claiming something the caller never
            // promised.
            Intrinsic::Memcpy => {
                let dest = self
                    .operand(fx, f, &args[0], &Ty::ptr(Ty::Bool))?
                    .into_pointer_value();
                let src = self
                    .operand(fx, f, &args[1], &Ty::ptr(Ty::Bool))?
                    .into_pointer_value();
                let len = self
                    .operand(fx, f, &args[2], &self.word_ty())?
                    .into_int_value();
                self.builder
                    .build_memcpy(dest, 1, src, 1, len)
                    .map_err(failed)?;
                Ok(())
            }
            Intrinsic::GcCollect => {
                let collect = self.runtime("nest_gc_collect", &[], None);
                self.builder.build_call(collect, &[], "").map_err(failed)?;
                Ok(())
            }
            // Both are statements *about* a conservative collector that it does
            // not need: it scans the stack, so a value still in a slot is
            // already alive and a pinned one already never moves. They become
            // instructions the day the collector does.
            Intrinsic::GcKeepAlive | Intrinsic::GcPin => Ok(()),
            // The object becomes a root until `drop` releases it, which is
            // the runtime's to track: `nest_free` is where a drop ends up.
            Intrinsic::GcLeak => {
                let v = self.operand(fx, f, &args[0], &Ty::ptr(Ty::Bool))?;
                let leak = self.runtime("nest_gc_leak", &[self.ptr().into()], None);
                self.builder
                    .build_call(leak, &[v.into()], "")
                    .map_err(failed)?;
                Ok(())
            }
            // "Reinterpret as whatever this slot holds" — so the bytes are
            // written to the destination and read back at its type, which is
            // exactly what the operation says and needs no instruction.
            Intrinsic::Transmute => {
                let place = dest.ok_or_else(|| {
                    failed(format!("{}: `transmute` with no destination", f.name))
                })?;
                let (ptr, ty) = self.place(fx, f, place)?;
                let from = self
                    .operand_ty(fx, f, &args[0])
                    .unwrap_or_else(|| ty.clone());
                if self.size_of(&from) != self.size_of(&ty) {
                    return Err(failed(format!(
                        "{}: `transmute` between {} and {} bytes",
                        f.name,
                        self.size_of(&from),
                        self.size_of(&ty)
                    )));
                }
                let v = self.operand(fx, f, &args[0], &from)?;
                self.store(ptr, v, &from)
            }
            // One left that is more than "one instruction or one runtime call",
            // which is what §10 claims of an intrinsic. `slice`, `array` and
            // `repeat` used to be on this list and are now lowered in LIR —
            // which is where this one belongs too, rather than being invented
            // here and then invented differently by the next backend.
            Intrinsic::EmbedFile => Err(unsupported(format!(
                "{}: `${}` needs a lowering LIR does not yet give it",
                f.name,
                which.name()
            ))),
            Intrinsic::Unknown(name) => Err(failed(format!(
                "{}: `${name}` reached a backend by name",
                f.name
            ))),
        }
    }

    /// `nest_alloc(bytes)` — the collector's allocator, behind the shim.
    fn alloc(&self, bytes: IntValue<'ctx>) -> Result<PointerValue<'ctx>> {
        let alloc = self.runtime("nest_alloc", &[self.word().into()], Some(self.ptr().into()));
        Ok(self
            .builder
            .build_call(alloc, &[bytes.into()], "")
            .map_err(failed)?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| failed("`nest_alloc` returned nothing"))?
            .into_pointer_value())
    }

    /// The element type and the two members of a slice's `{ ptr, len }` (§7b).
    fn slice_shape(
        &self,
        ty: &Ty,
        who: &str,
    ) -> Result<(Ty, crate::lir::TypeMember, crate::lir::TypeMember)> {
        let Ty::Named(tid) = ty else {
            return Err(failed(format!("{who}: a slice operation on a {ty:?}")));
        };
        let def = self.unit.ty(*tid);
        let (p, l) = (
            def.members
                .first()
                .ok_or_else(|| failed(format!("`{}` has no `ptr`", def.name)))?,
            def.members
                .get(1)
                .ok_or_else(|| failed(format!("`{}` has no `len`", def.name)))?,
        );
        let Ty::Ptr(elem) = &p.ty else {
            return Err(failed(format!("`{}`'s `ptr` is a {:?}", def.name, p.ty)));
        };
        Ok(((**elem).clone(), p.clone(), l.clone()))
    }

    fn terminator(&self, fx: &FnCx<'ctx>, f: &Function, kind: &TermKind) -> Result<()> {
        match kind {
            TermKind::Goto(b) => {
                self.builder
                    .build_unconditional_branch(fx.blocks[b.0 as usize])
                    .map_err(failed)?;
            }
            // One form for every branch there is (§10), `bool` included: a
            // one-bit switch is fine everywhere, and having no second form is
            // what makes there be nothing to keep in step.
            TermKind::Switch {
                value,
                ty,
                arms,
                otherwise,
            } => {
                let v = self.operand(fx, f, value, ty)?.into_int_value();
                let mut cases = Vec::with_capacity(arms.len());
                for (arm, target) in arms {
                    let c = self.int_constant(ty, &num_bigint::BigInt::from(*arm))?;
                    cases.push((c, fx.blocks[target.0 as usize]));
                }
                self.builder
                    .build_switch(v, fx.blocks[otherwise.0 as usize], &cases)
                    .map_err(failed)?;
            }
            TermKind::Return(Some(o)) => {
                let v = self.operand(fx, f, o, &f.ret)?;
                self.builder.build_return(Some(&v)).map_err(failed)?;
            }
            TermKind::Return(None) => {
                self.builder.build_return(None).map_err(failed)?;
            }
            // A guarantee, not an assertion: an exhaustive match's fallback and
            // what follows a call to a `-> never` function (§2).
            TermKind::Unreachable => {
                self.builder.build_unreachable().map_err(failed)?;
            }
        }
        let _ = fx.value;
        Ok(())
    }
}
