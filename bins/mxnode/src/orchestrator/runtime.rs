//! Lightweight "load config + resolve paths" helper used by every command.
//!
//! Centralising it here means the surface that touches `~/.config/mxnode`,
//! environment variables, and validated config lives in one place — and
//! every command picks the same `--config` flag and the same global
//! `--no-validate` semantics for free.

use std::fs;
use std::path::Path;

use mxnode_config::{
    legacy_system_config_path, legacy_user_config_path, load, resolve_paths, user_config_path,
    ConfigSource, LoadOptions, Loaded,
};
use mxnode_core::{MxnodeFile, Paths};
use mxnode_state::StateStore;

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
        // First-run migration: if no `mxnode.toml` exists yet but the
        // legacy `config.toml` and/or `state.toml` are on disk, merge
        // them into a unified `mxnode.toml` at 0600 before anything
        // else runs. Subsequent invocations skip this entirely.
        if global.config.is_none() {
            if let Err(e) = pre_migrate_legacy_files(global.json) {
                if !global.json {
                    eprintln!("→ legacy file migration skipped: {e}");
                }
            }
        }

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

/// Fold `config.toml` + `state.toml` into a unified `mxnode.toml` on
/// first run after upgrading. Runs **before** the layered loader so
/// the rest of `from_global` sees a single-file world. Renames the
/// originals to `*.legacy` so subsequent invocations skip this branch.
/// Best effort — failures surface as a stderr warning, not a fatal
/// error.
fn pre_migrate_legacy_files(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let new_path = user_config_path()?;
    if new_path.exists() {
        return Ok(()); // already unified
    }

    // Legacy file probes. We can't ask the loader for paths here
    // because resolution depends on a config we haven't read yet —
    // hardcode the standard XDG locations.
    let legacy_config = legacy_user_config_path()?;
    let legacy_system = legacy_system_config_path();
    let legacy_state = mxnode_config::xdg_state_home()
        .ok()
        .map(|p| p.join("mxnode/state.toml"));

    let have_legacy = legacy_config.exists()
        || legacy_system.exists()
        || legacy_state.as_ref().map(|p| p.exists()).unwrap_or(false);
    if !have_legacy {
        return Ok(()); // nothing to migrate
    }

    // Build the unified document. Operator sections come from
    // config.toml (or system fallback); host inventory comes from
    // state.toml; missing pieces stay at default.
    let mut unified = MxnodeFile::default();
    if legacy_config.exists() {
        let body = fs::read_to_string(&legacy_config)?;
        unified = toml::from_str(&body)?;
    } else if legacy_system.exists() {
        let body = fs::read_to_string(&legacy_system)?;
        unified = toml::from_str(&body)?;
    }
    if let Some(ref state_path) = legacy_state {
        if state_path.exists() {
            let body = fs::read_to_string(state_path)?;
            let host: mxnode_core::HostState = toml::from_str(&body)?;
            unified.host = host;
        }
    }

    // Write at 0600. StateStore handles directory creation, atomic
    // rename, and the file mode for us.
    let parent = new_path.parent().ok_or("mxnode.toml has no parent directory")?;
    let store = StateStore::new(parent);
    let guard = store.lock()?;
    store.save_file(&unified, &guard)?;
    drop(guard);

    rename_to_legacy(&legacy_config);
    rename_to_legacy(&legacy_system);
    if let Some(state_path) = legacy_state {
        rename_to_legacy(&state_path);
    }

    if !json {
        eprintln!(
            "→ migrated config.toml + state.toml into {} (mode 0600)",
            new_path.display(),
        );
        eprintln!("  legacy files renamed to *.legacy; safe to delete after verifying.");
    }
    Ok(())
}

fn rename_to_legacy(path: &Path) {
    if !path.exists() {
        return;
    }
    let target = path.with_extension(format!(
        "{}.legacy",
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("toml")
    ));
    let _ = fs::rename(path, target);
}

/// Stamp out a fresh config from the detected environment. Caller
/// has already verified `ConfigSource::None` so we never overwrite
/// operator state here. The banner goes to stderr (so `--json`
/// consumers see clean stdout) and is suppressed entirely under
/// `--json`. The interactive network prompt lives inside
/// `init::auto_init`; we surface its result here so the banner names
/// the actual network the operator picked rather than the historical
/// "mainnet" placeholder.
fn auto_init(global: &GlobalArgs) -> Result<(), CliError> {
    let target = user_config_path().map_err(|e| {
        CliError::new(
            "could not determine where to write config.toml",
            e.to_string(),
            "set $XDG_CONFIG_HOME or $HOME so mxnode can place the file under <home>/.config/mxnode/",
        )
        .json_if(global.json)
    })?;
    let chosen = init::auto_init(global)?;
    if !global.json {
        match chosen {
            Some(network) => {
                eprintln!(
                    "→ no mxnode config found; auto-initialized {} (network={})",
                    target.display(),
                    network,
                );
                eprintln!(
                    "  switch network later with: `mxnode config set network.environment <testnet|devnet>`",
                );
            }
            None => {
                // Concurrent writer raced us — config already exists.
                // Stay silent here; the load() that runs next will pick
                // up whatever they wrote.
            }
        }
    }
    Ok(())
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
