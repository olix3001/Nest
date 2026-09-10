//! The description of the machine a program is being compiled *for*.
//!
//! Two questions in the front end already depend on the target rather than on
//! the source: how wide `isize` / `usize` are, and therefore which
//! `comptime_int` constants fit them (§1.5, §3.1). Every other target fact —
//! endianness, alignment rules, the C ABI's parameter classification — belongs
//! to layout and code generation, which do not exist yet; this type is where
//! they go when they do.
//!
//! Today there is exactly one value, [`Target::HOST_64`], and it is threaded
//! from the [`Session`](crate::sema::session::Session) through inference and the
//! IR checks rather than read out of a constant at each site. That threading is
//! the point: the value is a *parameter* everywhere it matters, so selecting a
//! target on the command line is a matter of constructing a different `Target`
//! and handing it to the session, not of hunting down assumptions.

/// The properties of a compilation target that the front end must know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// The width in bits of `isize` / `usize` and of a pointer.
    pub pointer_bits: u32,
}

impl Target {
    /// A 64-bit target — the only one the bootstrap compiles for.
    ///
    /// 64 is also the *permissive* direction for the range check on a
    /// pointer-sized constant: a constant accepted here that would not fit a
    /// 32-bit target is caught once that target can be selected, whereas
    /// assuming 32 now would refuse a program that is correct on every machine
    /// the compiler can currently produce code for.
    pub const HOST_64: Target = Target { pointer_bits: 64 };
}

impl Default for Target {
    fn default() -> Self {
        Target::HOST_64
    }
}
