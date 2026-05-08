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
    /// Resolve the active GitHub token.
    ///
    /// Precedence: `MXNODE_GITHUB_TOKEN` env var (allows ad-hoc override
    /// without editing the file) → `[secrets].github_token` from the
    /// loaded config → `None`. Empty strings count as unset on both
    /// layers.
    pub fn github_token(&self) -> Option<String> {
        if let Ok(v) = std::env::var("MXNODE_GITHUB_TOKEN") {
            if !v.is_empty() {
                return Some(v);
            }
        }
        let t = &self.loaded.file.secrets.github_token;
        if t.is_empty() {
            None
        } else {
            Some(t.as_str().to_owned())
        }
    }

    /// Load config from the layered resolver and resolve the typed `Paths`.
    /// `--config <PATH>` overrides the default file lookup.
    ///
    /// First-use bootstrap: when no `mxnode.toml` is found at any standard
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
        let paths = resolve_paths(&loaded.file).map_err(|e| {
            CliError::new(
                "failed to resolve filesystem paths",
                e.to_string(),
                "set $HOME, $XDG_STATE_HOME, or use a config file with absolute paths under [paths]",
            )
            .json_if(global.json)
        })?;

        // One-shot self-heal: if the file persisted a `paths.custom_home`
        // that doesn't exist on this host, the resolver fell back to
        // `$HOME` for the in-memory `Paths`. Without rewriting the
        // file, every subsequent invocation re-fires the banner.
        // Drop the stale field from disk now so the next run is silent.
        // Only fires when the operator didn't pass `--config <PATH>`
        // (we don't touch their explicit file).
        if global.config.is_none() {
            heal_stale_custom_home(&loaded, &paths, global.json);
            heal_stale_custom_user(&loaded, &paths, global.json);
        }

        Ok(Runtime { loaded, paths })
    }
}

/// If `paths.custom_home` in the loaded file points at a non-existent
/// directory and the resolver swapped in `$HOME`, drop the field from
/// the file. Best effort — failures are not fatal (we already have a
/// usable in-memory `Paths`).
fn heal_stale_custom_home(loaded: &Loaded, paths: &Paths, json: bool) {
    let Some(stored) = loaded.file.paths.custom_home.as_ref() else {
        return;
    };
    if stored == &paths.custom_home {
        return; // resolver kept the explicit value; nothing to heal
    }
    if let Err(e) = remove_path_field("custom_home") {
        if !json {
            eprintln!("warn: could not heal stale paths.custom_home: {e}");
        }
    }
}

fn heal_stale_custom_user(loaded: &Loaded, paths: &Paths, json: bool) {
    let Some(stored) = loaded.file.paths.custom_user.as_ref() else {
        return;
    };
    if stored == &paths.custom_user {
        return;
    }
    if let Err(e) = remove_path_field("custom_user") {
        if !json {
            eprintln!("warn: could not heal stale paths.custom_user: {e}");
        }
    }
}

/// Drop a single key from `[paths]` in the unified `mxnode.toml`,
/// preserving operator comments and section ordering. Routes through
/// `toml_edit::DocumentMut` (same path `mxnode config set` uses) so
/// hand-edited files round-trip cleanly.
fn remove_path_field(key: &str) -> Result<(), String> {
    use toml_edit::DocumentMut;
    let target = mxnode_config::user_config_path().map_err(|e| e.to_string())?;
    let body = std::fs::read_to_string(&target).map_err(|e| format!("read {}: {e}", target.display()))?;
    let mut doc: DocumentMut = body
        .parse()
        .map_err(|e: toml_edit::TomlError| format!("parse {}: {e}", target.display()))?;
    if let Some(paths) = doc.get_mut("paths").and_then(|p| p.as_table_mut()) {
        paths.remove(key);
        // Drop the [paths] table entirely if it's now empty so we
        // don't leave an empty header behind.
        if paths.is_empty() {
            doc.remove("paths");
        }
    }
    std::fs::write(&target, doc.to_string()).map_err(|e| format!("write {}: {e}", target.display()))?;
    Ok(())
}

/// Stamp out a fresh `mxnode.toml` from the detected environment.
/// Caller has already verified `ConfigSource::None` so we never
/// overwrite operator state here. The banner goes to stderr (so
/// `--json` consumers see clean stdout) and is suppressed entirely
/// under `--json`. The interactive network prompt lives inside
/// `init::auto_init`; we surface its result here so the banner names
/// the actual network the operator picked.
fn auto_init(global: &GlobalArgs) -> Result<(), CliError> {
    let target = user_config_path().map_err(|e| {
        CliError::new(
            "could not determine where to write mxnode.toml",
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
                    "→ no mxnode.toml found; auto-initialized {} (network={})",
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
