//! Starts `nest-lsp` for Nest files.
//!
//! The server is found the way a person's shell would find it, on `PATH` as the
//! worktree sees it, unless `lsp.nest-lsp.binary` in Zed's settings says where.
//! It runs with that shell's environment, so twig and `nestc` are found there
//! too, unless `lsp.nest-lsp.settings` names them:
//!
//!     "lsp": { "nest-lsp": { "settings": { "twig": "<path>", "nestc": "<path>" } } }

use zed_extension_api::settings::LspSettings;
use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

struct Nest;

impl zed::Extension for Nest {
    fn new() -> Self {
        Nest
    }

    fn language_server_command(
        &mut self,
        id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        let settings = LspSettings::for_worktree(id.as_ref(), worktree).unwrap_or_default();
        let mut flags = Vec::new();
        for name in ["twig", "nestc"] {
            let path = settings
                .settings
                .as_ref()
                .and_then(|s| s.get(name))
                .and_then(|v| v.as_str());
            if let Some(path) = path {
                flags.push(format!("--{name}"));
                flags.push(absolute(path, worktree));
            }
        }
        let binary = settings.binary;
        let (path, args, env) = match binary {
            Some(b) => (b.path, b.arguments, b.env),
            None => (None, None, None),
        };
        let command = path.or_else(|| worktree.which("nest-lsp")).ok_or(
            "`nest-lsp` is not on PATH: build it with `cargo build --release -p nest-lsp` in `nestc`, \
             or set `lsp.nest-lsp.binary.path`",
        )?;
        let mut vars = worktree.shell_env();
        vars.extend(env.unwrap_or_default());
        let mut args = args.unwrap_or_default();
        args.extend(flags);
        Ok(Command {
            command,
            args,
            env: vars,
        })
    }

    fn language_server_initialization_options(
        &mut self,
        id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        Ok(LspSettings::for_worktree(id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.initialization_options))
    }
}

/// A path from the settings, as the server has to be given it: `~/` is the home
/// directory, and a relative path is relative to the worktree.
fn absolute(path: &str, worktree: &Worktree) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = worktree
            .shell_env()
            .into_iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| v);
        if let Some(home) = home {
            return format!("{home}/{rest}");
        }
    }
    if path.starts_with('/') || path.starts_with('~') {
        return path.to_string();
    }
    format!("{}/{path}", worktree.root_path())
}

zed::register_extension!(Nest);
