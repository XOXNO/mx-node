//! Lightweight "load config + resolve paths" helper used by every command.
//!
//! Centralising it here means the surface that touches `~/.config/mxnode`,
//! environment variables, and validated config lives in one place — and
//! every command picks the same `--config` flag and the same global
//! `--no-validate` semantics for free.

use mxnode_config::{load, resolve_paths, Loaded, LoadOptions};
use mxnode_core::Paths;

use crate::cli::GlobalArgs;
use crate::errors::CliError;

pub struct Runtime {
    pub loaded: Loaded,
    pub paths: Paths,
}

impl Runtime {
    /// Load config from the layered resolver and resolve the typed `Paths`.
    /// `--config <PATH>` overrides the default file lookup.
    ///
    /// Errors here use the 3-line summary/cause/try shape and honour
    /// global `--json`.
    pub fn from_global(global: &GlobalArgs) -> Result<Self, CliError> {
        let opts = LoadOptions {
            config_path: global.config.clone(),
            flags_overlay: None,
        };
        let loaded = load(&opts).map_err(|e| {
            CliError::new(
                "failed to load config",
                e.to_string(),
                "fix the file at the path shown above, or pass --config <PATH> to point at a different one",
            )
            .json_if(global.json)
        })?;
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
