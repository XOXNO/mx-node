//! Layered config resolver for mxnode.
//!
//! Resolution order (lowest → highest precedence):
//!   1. Built-in defaults (`Default for Config`)
//!   2. Config file (`~/.config/mxnode/config.toml` then `/etc/mxnode/config.toml`)
//!   3. CLI flags (passed as a sparse `Override` map)
//!
//! `MXNODE_GITHUB_TOKEN` is the **only** environment variable read; secrets do
//! not belong in config files. See plan §"Configuration model", D6.

mod loader;
mod origin;
mod resolve;
mod validate;
mod xdg;

pub use loader::{
    legacy_system_config_path, legacy_user_config_path, load, system_config_path, user_config_path,
    user_config_path_or_default, ConfigSource, LoadOptions, Loaded, Scope,
};
pub use origin::Origin;
pub use resolve::resolve_paths;
pub use validate::{validate, ValidationReport};
pub use xdg::{home_dir, xdg_config_home, xdg_runtime_dir, xdg_state_home};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse TOML at {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },

    #[error("could not serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),

    #[error("could not resolve XDG / home directory: {0}")]
    NoHome(String),

    #[error("invalid config value: {0}")]
    Invalid(String),
}
