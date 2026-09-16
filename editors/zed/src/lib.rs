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

    fn language_server_command(&mut self, id: &LanguageServerId, worktree: &Worktree) -> Result<Command> {
        let settings = LspSettings::for_worktree(id.as_ref(), worktree).unwrap_or_default();
        let mut flags = Vec::new();
        for name in ["twig", "nestc"] {
            let path = settings.settings.as_ref().and_then(|s| s.get(name)).and_then(|v| v.as_str());
            if let Some(path) = path {
                flags.push(format!("--{name}"));
                flags.push(path.to_string());
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
        Ok(Command { command, args, env: vars })
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

zed::register_extension!(Nest);
