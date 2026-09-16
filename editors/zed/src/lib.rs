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
        for (name, path) in toolchain(&settings, worktree) {
            flags.push(format!("--{name}"));
            flags.push(path);
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
        // Zed starts `lsp.nest-lsp.binary.path` itself, without asking
        // `language_server_command`, so the flags never reach that server. The
        // options do, and the server reads `twig` and `nestc` from them too.
        let settings = LspSettings::for_worktree(id.as_ref(), worktree).unwrap_or_default();
        let paths = toolchain(&settings, worktree);
        let mut options = settings.initialization_options;
        if !paths.is_empty() {
            let mut map = match options.take() {
                Some(zed::serde_json::Value::Object(map)) => map,
                _ => Default::default(),
            };
            for (name, path) in paths {
                map.insert(name.to_string(), zed::serde_json::Value::String(path));
            }
            options = Some(zed::serde_json::Value::Object(map));
        }
        Ok(options)
    }
}

/// `twig` and `nestc` from `lsp.nest-lsp.settings`, as absolute paths.
fn toolchain(settings: &LspSettings, worktree: &Worktree) -> Vec<(&'static str, String)> {
    ["twig", "nestc"]
        .into_iter()
        .filter_map(|name| {
            let path = settings.settings.as_ref()?.get(name)?.as_str()?;
            Some((name, absolute(path, worktree)))
        })
        .collect()
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
