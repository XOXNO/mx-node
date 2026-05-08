use std::path::PathBuf;

use mxnode_core::{MxnodeFile, Paths};

use crate::xdg::{home_dir, xdg_config_home, xdg_runtime_dir, xdg_state_home};
use crate::ConfigError;

/// Resolve the path-shaped strings in `MxnodeFile::paths` into a typed
/// `mxnode_core::Paths`.
///
/// Resolution order for `custom_home`:
///   1. `paths.custom_home` if explicitly set in the file (multi-user
///      / shared-deploy operators override this way).
///   2. `dirs::home_dir()` — the runtime `$HOME` of whoever is
///      executing `mxnode`. This is the right answer on every
///      single-user host and is what the operator expects when they
///      type `mxnode install` with no further configuration.
///   3. Final fallback: `/home/<custom_user>`, then `/home/ubuntu`.
///      Only fires when `$HOME` is genuinely unset (rare — happens
///      with stripped systemd environments).
///
/// `custom_user` mirrors the same chain but reads `$USER` /
/// `$LOGNAME` first.
///
/// Handles `{custom_home}`, `{home}`, `{XDG_STATE_HOME}` and
/// `{XDG_RUNTIME_DIR}` placeholders in the other path strings.
pub fn resolve_paths(cfg: &MxnodeFile) -> Result<Paths, ConfigError> {
    let custom_user = resolve_custom_user(&cfg.paths.custom_user);
    let custom_home = resolve_custom_home(&cfg.paths.custom_home, &custom_user);

    // `{home}` interpolates the operator's real HOME — distinct from
    // `custom_home` in shared-deploy layouts where the operator chose
    // a non-HOME path. Falls back to `custom_home` if HOME is unset.
    let home = home_dir().unwrap_or_else(|_| custom_home.clone());
    let xdg_state = xdg_state_home().unwrap_or_else(|_| home.join(".local/state"));
    let xdg_runtime = xdg_runtime_dir().unwrap_or_else(|_| xdg_state.join("run"));
    let xdg_config = xdg_config_home().unwrap_or_else(|_| home.join(".config"));

    let interp = |raw: &str| -> PathBuf {
        let mut s = raw.to_string();
        s = s.replace("{custom_home}", &custom_home.display().to_string());
        s = s.replace("{home}", &home.display().to_string());
        s = s.replace("{XDG_STATE_HOME}", &xdg_state.display().to_string());
        s = s.replace("{XDG_RUNTIME_DIR}", &xdg_runtime.display().to_string());
        PathBuf::from(s)
    };

    Ok(Paths {
        custom_home: custom_home.clone(),
        custom_user,
        node_keys: interp(&cfg.paths.node_keys),
        binaries: interp(&cfg.paths.binaries),
        // The unified file lives under XDG_CONFIG_HOME — never under
        // `paths.custom_home` so multi-host setups sharing one HOME can
        // each have their own mxnode.toml under their own XDG dir.
        config_dir: xdg_config.join("mxnode"),
        state: interp(&cfg.paths.state),
        runtime: interp(&cfg.paths.runtime),
    })
}

/// Pick the operator's effective home directory. Order:
///   1. Explicit `paths.custom_home` from the config IF the directory
///      actually exists. Lets multi-user / shared-deploy operators
///      override away from `$HOME`.
///   2. Self-heal: explicit value that doesn't exist on disk gets
///      replaced by `$HOME` (catches stale `/home/ubuntu` schema
///      defaults persisted by older mxnode versions). One stderr
///      notice per invocation so the operator sees the swap.
///   3. Runtime `$HOME` via `dirs::home_dir()`.
///   4. `/home/<custom_user>` (`/Users/<…>` on macOS) as a last
///      resort when `$HOME` is unset.
fn resolve_custom_home(explicit: &Option<PathBuf>, custom_user: &str) -> PathBuf {
    if let Some(path) = explicit {
        if path.exists() {
            return path.clone();
        }
        if let Ok(home) = home_dir() {
            if home.exists() && home != *path {
                eprintln!(
                    "→ paths.custom_home = {} does not exist; using $HOME = {} instead (run `mxnode config set paths.custom_home <path>` to pin a different value)",
                    path.display(),
                    home.display(),
                );
                return home;
            }
        }
        // Both the explicit path and $HOME are unusable; surface the
        // explicit value so the downstream error names what the
        // operator put in their file.
        return path.clone();
    }
    home_dir().unwrap_or_else(|_| match mxnode_core::Platform::current() {
        mxnode_core::Platform::Macos => PathBuf::from(format!("/Users/{custom_user}")),
        _ => PathBuf::from(format!("/home/{custom_user}")),
    })
}

/// Pick the operator's effective service-account name. Explicit
/// config wins; otherwise `$USER` → `$LOGNAME` → basename of `$HOME`
/// → `ubuntu` (the historical bash default).
fn resolve_custom_user(explicit: &Option<String>) -> String {
    if let Some(name) = explicit {
        if !name.is_empty() {
            return name.clone();
        }
    }
    std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("LOGNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            std::env::var("HOME").ok().and_then(|h| {
                std::path::Path::new(&h)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty())
            })
        })
        .unwrap_or_else(|| "ubuntu".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_resolves_custom_home_from_runtime_home() {
        // No `paths.custom_home` set → resolver picks runtime $HOME
        // instead of the historical `/home/ubuntu` schema default.
        let cfg = MxnodeFile::default();
        let p = resolve_paths(&cfg).unwrap();
        let expected_home = home_dir().unwrap_or_else(|_| PathBuf::from("/home/ubuntu"));
        assert_eq!(p.custom_home, expected_home);
        // `{custom_home}` placeholder in node_keys / binaries
        // resolves to the same value.
        assert_eq!(p.node_keys, expected_home.join("VALIDATOR_KEYS"));
        assert_eq!(p.binaries, expected_home.join("mxnode/binaries"));
    }

    #[test]
    fn explicit_custom_home_overrides_runtime_home_when_path_exists() {
        // Use a real tempdir so the existence check passes.
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = MxnodeFile::default();
        cfg.paths.custom_home = Some(tmp.path().to_path_buf());
        cfg.paths.node_keys = "{custom_home}/keys".to_string();
        let p = resolve_paths(&cfg).unwrap();
        assert_eq!(p.custom_home, tmp.path());
        assert_eq!(p.node_keys, tmp.path().join("keys"));
    }

    #[test]
    fn nonexistent_explicit_custom_home_falls_back_to_home() {
        // Stale `/home/ubuntu` (or any pinned value that doesn't
        // exist) self-heals to runtime $HOME — the operator no
        // longer has to manually `config set paths.custom_home`
        // when their config was written by an older mxnode binary.
        let mut cfg = MxnodeFile::default();
        cfg.paths.custom_home =
            Some(PathBuf::from("/var/empty/definitely-not-a-real-mxnode-home"));
        let p = resolve_paths(&cfg).unwrap();
        let expected_home = home_dir().unwrap_or_else(|_| PathBuf::from("/home/ubuntu"));
        // If $HOME exists at test time (always true on real hosts and
        // in CI), self-heal kicks in.
        if expected_home.exists() {
            assert_eq!(p.custom_home, expected_home);
        }
    }

    #[test]
    fn custom_user_falls_back_to_env() {
        let cfg = MxnodeFile::default();
        let p = resolve_paths(&cfg).unwrap();
        // Whatever the test environment's $USER is, custom_user picks
        // it up (vs. the old `/home/ubuntu`-style "ubuntu" default).
        let expected_user = std::env::var("USER")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "ubuntu".to_string());
        assert_eq!(p.custom_user, expected_user);
    }

    #[test]
    fn explicit_custom_user_wins_over_env() {
        let mut cfg = MxnodeFile::default();
        cfg.paths.custom_user = Some("validator".to_string());
        let p = resolve_paths(&cfg).unwrap();
        assert_eq!(p.custom_user, "validator");
    }
}
