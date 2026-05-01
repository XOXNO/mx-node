//! Lightweight "load config + resolve paths" helper used by every command.
//!
//! Centralising it here means the surface that touches `~/.config/mxnode`,
//! environment variables, and validated config lives in one place — and
//! every command picks the same `--config` flag and the same global
//! `--no-validate` semantics for free.

use mxnode_config::{load, resolve_paths, user_config_path, ConfigSource, LoadOptions, Loaded};
use mxnode_core::Paths;

use crate::cli::GlobalArgs;
use crate::commands::init;
use crate::errors::CliError;

pub struct Runtime {
    pub loaded: Loaded,
    pub paths: Paths,
}

impl Runtime {
    /// Load config from the layered resolver and resolve the typed `Paths`.
    /// `--config <PATH>` overrides the default file lookup.
    ///
    /// First-use bootstrap: when no config file is found at any standard
    /// scope and the operator did not pass `--config <PATH>`, write a
    /// sensible default config (auto-detected `$USER`/`$HOME`, network
    /// = mainnet) and reload. The operator switches network afterwards
    /// via `mxnode config set network.environment <env>`. There is no
    /// explicit `mxnode init` command — first-use IS the init.
    ///
    /// Errors here use the 3-line summary/cause/try shape and honour
    /// global `--json`.
    pub fn from_global(global: &GlobalArgs) -> Result<Self, CliError> {
        let opts = LoadOptions {
            config_path: global.config.clone(),
            flags_overlay: None,
        };
        let mut loaded = load(&opts).map_err(|e| {
            CliError::new(
                "failed to load config",
                e.to_string(),
                "fix the file at the path shown above, or pass --config <PATH> to point at a different one",
            )
            .json_if(global.json)
        })?;
        if global.config.is_none() && matches!(loaded.source, ConfigSource::None) {
            auto_init(global)?;
            loaded = load(&opts).map_err(|e| {
                CliError::new(
                    "failed to reload config after auto-init",
                    e.to_string(),
                    "report this as a bug — the auto-init wrote a file we can't read back",
                )
                .json_if(global.json)
            })?;
        }
        let paths = resolve_paths(&loaded.config).map_err(|e| {
            CliError::new(
                "failed to resolve filesystem paths",
                e.to_string(),
                "set $HOME, $XDG_STATE_HOME, or use a config file with absolute paths under [paths]",
            )
            .json_if(global.json)
        })?;
        Ok(Runtime { loaded, paths })
    }
}

/// Stamp out a fresh config from the detected environment. Caller
/// has already verified `ConfigSource::None` so we never overwrite
/// operator state here. Banner goes to stderr (so `--json` consumers
/// see clean stdout) and is suppressed entirely under `--json`.
fn auto_init(global: &GlobalArgs) -> Result<(), CliError> {
    let target = user_config_path().map_err(|e| {
        CliError::new(
            "could not determine where to write config.toml",
            e.to_string(),
            "set $XDG_CONFIG_HOME or $HOME so mxnode can place the file under <home>/.config/mxnode/",
        )
        .json_if(global.json)
    })?;
    if !global.json {
        eprintln!(
            "→ no mxnode config found; auto-initializing {} (network=mainnet)",
            target.display(),
        );
        eprintln!(
            "  switch network with: `mxnode config set network.environment <testnet|devnet>`",
        );
    }
    init::auto_init(global)
}

/// Helper extension trait used inside command modules: lets us write
/// `e.json_if(global.json)` in any command without repeating the conditional.
pub trait CliErrorExt: Sized {
    fn json_if(self, json: bool) -> CliError;
}

impl CliErrorExt for CliError {
    fn json_if(self, json: bool) -> CliError {
        if json {
            self.json()
        } else {
            self
        }
    }
}
