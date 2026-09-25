//! Where an **installed** compiler finds what it ships with.
//!
//! A release is one directory, wherever it was unpacked:
//!
//! ```text
//! bin/nestc  bin/nest-lsp  bin/twig
//! lib/nest/libnest_runtime.a  lib/nest/libgc.a
//! lib/nest/packages/core/  lib/nest/packages/std/
//! ```
//!
//! A build from the repository has none of that beside it, and keeps finding
//! `core`, `std`, the runtime and the collector where it was built: the paths
//! baked in at compile time. So each lookup asks here first and falls back to
//! those — an installed layout is used only when it is actually there.

use std::path::PathBuf;

/// `<prefix>/lib/nest/<rel>`, where `<prefix>` is the directory above the
/// running executable's, when that file exists.
pub fn shipped(rel: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let prefix = exe.parent()?.parent()?;
    let path = prefix.join("lib").join("nest").join(rel);
    path.exists().then_some(path)
}
