//! Every choice a build makes *for* the compiler, in one place.
//!
//! Two kinds of thing live here and they are deliberately one struct. A
//! [`Target`] fact — how wide a pointer is — is not negotiable: it describes the
//! machine. An [`OverflowMode`] is a policy: the same program on the same
//! machine may be built either way. What they share is the only thing that
//! matters to the compiler's plumbing: **both are inputs**, decided before the
//! first file is read, constant for the whole compilation, and read by passes
//! that must never decide them for themselves.
//!
//! Profiles (`debug`, `release`, …) are **not** here and will not be. Choosing
//! that a debug build traps on overflow is the build tool's job; `nestc` is
//! handed the resolved answer. That is what keeps the compiler from growing an
//! opinion about what a "release build" is, and what lets the same knob be set
//! from a profile, a command line, or a test.
//!
//! Adding a knob is: a field here, a `-C` key in `main.rs`, and a reader
//! wherever the choice is actually made. Nothing in between has to learn it.

/// The properties of a compilation target that the front end must know.
///
/// Today it is one number, because one number is all the front end can
/// currently ask about. Endianness, alignment rules and the C ABI's parameter
/// classification belong here too, once layout and code generation exist to
/// need them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// The width in bits of `isize` / `usize` and of a pointer.
    pub pointer_bits: u32,
    /// The operating system being targeted, as a program sees it.
    pub os: &'static str,
    /// The processor architecture being targeted.
    pub arch: &'static str,
}

/// The operating systems `-C os=` accepts. A fixed list rather than free text
/// for the same reason an unknown `-C` key is an error: a typo would otherwise
/// compile a different program silently.
pub const OSES: &[&str] = &["linux", "macos", "windows", "freebsd", "none"];

/// The architectures `-C arch=` accepts.
pub const ARCHES: &[&str] = &["x86_64", "aarch64", "riscv64", "wasm32"];

/// The build profiles `-C profile=` accepts.
///
/// This is the profile's **name**, carried so a program can read it, and
/// nothing more. It is not a contradiction of the note above: what that refuses
/// is the *compiler* deriving behaviour from a profile — deciding for itself
/// that a release build wraps on overflow. Handing the name through to source,
/// where a program may branch on it, decides nothing here.
pub const PROFILES: &[&str] = &["debug", "release"];

impl Target {
    /// A 64-bit target — the only one the bootstrap compiles for.
    ///
    /// 64 is also the *permissive* direction for the range check on a
    /// pointer-sized constant: a constant accepted here that would not fit a
    /// 32-bit target is caught once that target can be selected, whereas
    /// assuming 32 now would refuse a program that is correct on every machine
    /// the compiler can currently produce code for.
    pub const HOST_64: Target = Target {
        pointer_bits: 64,
        os: "linux",
        arch: "x86_64",
    };
}

impl Default for Target {
    fn default() -> Self {
        Target::HOST_64
    }
}

/// What a **run-time** integer operation does when its result does not fit.
///
/// It says nothing about compile time. A constant is refused when it overflows
/// whatever this is set to (§2.5): a `::` binding *is* its value, so a compiler
/// that wrapped one would be choosing a different number than the source names,
/// and there is no running program for the other answer to happen in. The two
/// rules do not contradict each other — they answer different questions, and
/// Rust splits them the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverflowMode {
    /// The program panics. The default, and what a debug build wants: an
    /// overflow is a bug, and the build that is for finding bugs should say so.
    #[default]
    Trap,
    /// The result wraps, two's complement. What a release build may choose, and
    /// what the arithmetic on the machine does anyway.
    Wrap,
}

impl OverflowMode {
    fn parse(value: &str) -> Option<OverflowMode> {
        match value {
            "trap" => Some(OverflowMode::Trap),
            "wrap" => Some(OverflowMode::Wrap),
            _ => None,
        }
    }

    /// The spelling `-C overflow=` accepts, for round-tripping and diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            OverflowMode::Trap => "trap",
            OverflowMode::Wrap => "wrap",
        }
    }
}

/// Everything a build decides for the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub target: Target,
    pub overflow: OverflowMode,
    /// The build profile's name; see [`PROFILES`].
    pub profile: &'static str,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            target: Target::default(),
            overflow: OverflowMode::default(),
            profile: "debug",
        }
    }
}

/// Match `value` against a fixed list, returning the `'static` spelling.
fn one_of(list: &[&'static str], key: &str, value: &str) -> Result<&'static str, String> {
    list.iter().copied().find(|v| *v == value).ok_or_else(|| {
        format!("`{key}` must be one of {}, not `{value}`", list.join(", "))
    })
}

impl Options {
    /// Apply one `key=value` setting, as `-C key=value` supplies it.
    ///
    /// An unknown key or an unparseable value is an **error**, never a silent
    /// default: these arrive from a build tool translating a profile, and a
    /// typo there would otherwise change what is built without saying so.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "overflow" => {
                self.overflow = OverflowMode::parse(value)
                    .ok_or_else(|| format!("`overflow` must be `trap` or `wrap`, not `{value}`"))?;
            }
            "pointer-width" => {
                let bits: u32 = value
                    .parse()
                    .map_err(|_| format!("`pointer-width` must be a number, not `{value}`"))?;
                if !matches!(bits, 16 | 32 | 64) {
                    return Err(format!("`pointer-width` must be 16, 32 or 64, not {bits}"));
                }
                self.target.pointer_bits = bits;
            }
            "os" => self.target.os = one_of(OSES, "os", value)?,
            "arch" => self.target.arch = one_of(ARCHES, "arch", value)?,
            "profile" => self.profile = one_of(PROFILES, "profile", value)?,
            other => return Err(format!("unknown setting `{other}`")),
        }
        Ok(())
    }

    /// The settings as `-C` pairs, one per line — what `-C print=options`
    /// prints, so a build tool can check what its profile actually resolved to.
    pub fn render(&self) -> String {
        format!(
            "arch={}\noverflow={}\nos={}\npointer-width={}\nprofile={}\n",
            self.target.arch,
            self.overflow.name(),
            self.target.os,
            self.target.pointer_bits,
            self.profile
        )
    }
}
