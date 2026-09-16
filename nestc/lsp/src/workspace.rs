//! Which command line analyzes a file.
//!
//! A file belongs to the package whose `nest.toml` is nearest above it, and that
//! package is the root of a workspace. twig decides everything about how it is
//! compiled: `twig build --deps` makes its dependencies' libraries, and
//! `twig metadata` says each target's `nestc` command line, which is analyzed as
//! it is. The one change made to it is that a binary reads its own package's
//! library from source, since that is the source being edited.
//!
//! A file with no manifest above it is analyzed on its own, against the `core`
//! and `std` that ship with the compiler.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

pub const MANIFEST: &str = "nest.toml";

/// What `twig metadata` prints.
#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub root: PathBuf,
    pub packages: Vec<Package>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub name: String,
    pub dir: PathBuf,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub entry: PathBuf,
    pub lib: bool,
    pub args: Vec<String>,
}

/// The directory of the manifest nearest above `file`.
pub fn find_root(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|dir| dir.join(MANIFEST).is_file())
        .map(Path::to_path_buf)
}

/// What gets a workspace ready to analyze.
pub trait Toolchain: Send + Sync {
    /// Build what the package at `root` depends on, and say how it is compiled.
    fn prepare(&self, root: &Path) -> Result<Metadata, String>;
}

/// twig, found at `program`, finding `nestc` at `nestc` when that is given and
/// the way it always does otherwise.
pub struct Twig {
    pub program: String,
    pub nestc: Option<String>,
}

impl Toolchain for Twig {
    fn prepare(&self, root: &Path) -> Result<Metadata, String> {
        self.run(root, &["build", "--deps"])?;
        let out = self.run(root, &["metadata"])?;
        serde_json::from_slice(&out).map_err(|e| format!("`twig metadata` printed something unreadable: {e}"))
    }
}

impl Twig {
    fn run(&self, root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
        let mut command = Command::new(&self.program);
        command.args(args).current_dir(root);
        if let Some(nestc) = &self.nestc {
            command.env("NESTC", nestc);
        }
        let out = command
            .output()
            .map_err(|e| format!("could not run `{}` in `{}`: {e}", self.program, root.display()))?;
        eprintln!("nest-lsp: `{} {}` in `{}`: {}", self.program, args.join(" "), root.display(), out.status);
        if !out.status.success() {
            return Err(format!(
                "`twig {}` failed:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim_end()
            ));
        }
        Ok(out.stdout)
    }
}

/// The command lines that may compile `file`, a file of the root package, the
/// likeliest first: a binary it is the root of, then the package's library, then
/// the other binaries.
pub fn candidates(meta: &Metadata, file: &Path) -> Vec<Vec<String>> {
    let Some(root) = meta.packages.iter().find(|p| p.dir == meta.root) else {
        return Vec::new();
    };
    let lib = root.targets.iter().find(|t| t.lib);
    let bins = || root.targets.iter().filter(|t| !t.lib);
    let own = |t: &Target| match lib {
        Some(lib) if !t.lib => from_source(&t.args, &root.name, &lib.entry),
        _ => t.args.clone(),
    };
    let mut out: Vec<Vec<String>> = bins().filter(|t| t.entry == file).map(own).collect();
    out.extend(lib.map(own));
    out.extend(bins().filter(|t| t.entry != file).map(own));
    out
}

/// `args` with the library `name` read from its root file `entry` rather than
/// from where it would be built.
fn from_source(args: &[String], name: &str, entry: &Path) -> Vec<String> {
    let prefix = format!("{name}=");
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--extern" && args.get(i + 1).is_some_and(|a| a.starts_with(&prefix)) {
            out.push("--package".to_string());
            out.push(format!("{name}={}", entry.display()));
            i += 2;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn target(entry: &str, lib: bool, args: &[&str]) -> Target {
        Target { entry: PathBuf::from(entry), lib, args: strings(args) }
    }

    /// The binary a file is the root of comes first, then the library, and a
    /// binary reads its own package's library from source.
    #[test]
    fn a_binary_s_root_is_analyzed_as_that_binary() {
        let meta = Metadata {
            root: PathBuf::from("/app"),
            packages: vec![Package {
                name: "app".to_string(),
                dir: PathBuf::from("/app"),
                targets: vec![
                    target("/app/src/lib.nest", true, &["/app/src/lib.nest", "--extern", "std=/app/build/debug/deps/std.nlib"]),
                    target("/app/src/a.nest", false, &["/app/src/a.nest", "--extern", "app=/app/build/debug/app.nlib"]),
                    target("/app/src/b.nest", false, &["/app/src/b.nest", "--extern", "app=/app/build/debug/app.nlib"]),
                ],
            }],
        };
        let found = candidates(&meta, Path::new("/app/src/b.nest"));
        assert_eq!(found, vec![
            strings(&["/app/src/b.nest", "--package", "app=/app/src/lib.nest"]),
            strings(&["/app/src/lib.nest", "--extern", "std=/app/build/debug/deps/std.nlib"]),
            strings(&["/app/src/a.nest", "--package", "app=/app/src/lib.nest"]),
        ]);
    }
}
