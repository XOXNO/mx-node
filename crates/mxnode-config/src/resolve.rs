use std::path::PathBuf;

use mxnode_core::{Config, Paths};

use crate::xdg::{home_dir, xdg_config_home, xdg_runtime_dir, xdg_state_home};
use crate::ConfigError;

/// Resolve the path-shaped strings in `Config::paths` into a typed
/// `mxnode_core::Paths`.
///
/// Handles `{custom_home}`, `{home}`, `{XDG_STATE_HOME}` and
/// `{XDG_RUNTIME_DIR}` placeholders. Falls back to `home_dir()` /
/// `xdg_*` helpers (shared with the loader) when the placeholders are
/// substituted with the user's actual environment.
pub fn resolve_paths(cfg: &Config) -> Result<Paths, ConfigError> {
    let custom_home = cfg.paths.custom_home.clone();
    // Use the operator's actual HOME when interpolating `{home}`. Fall back
    // to `custom_home` only if `HOME` is genuinely unavailable, so
    // single-host setups where HOME is unset (e.g. systemd-run with no
    // user) still produce sane paths.
    let home = home_dir().unwrap_or_else(|_| custom_home.clone());
    let xdg_state = xdg_state_home().unwrap_or_else(|_| custom_home.join(".local/state"));
    let xdg_runtime = xdg_runtime_dir().unwrap_or_else(|_| xdg_state.join("run"));
    let xdg_config = xdg_config_home().unwrap_or_else(|_| custom_home.join(".config"));

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
        custom_user: cfg.paths.custom_user.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_resolves_to_default_paths() {
        let cfg = Config::default();
        let p = resolve_paths(&cfg).unwrap();
        assert_eq!(p.custom_home, PathBuf::from("/home/ubuntu"));
        assert_eq!(p.custom_user, "ubuntu");
        assert_eq!(p.node_keys, PathBuf::from("/home/ubuntu/VALIDATOR_KEYS"));
        assert_eq!(p.binaries, PathBuf::from("/home/ubuntu/mxnode/binaries"));
    }

    #[test]
    fn custom_home_is_interpolated_into_node_keys() {
        let mut cfg = Config::default();
        cfg.paths.custom_home = PathBuf::from("/srv/mx");
        cfg.paths.node_keys = "{custom_home}/keys".to_string();
        let p = resolve_paths(&cfg).unwrap();
        assert_eq!(p.node_keys, PathBuf::from("/srv/mx/keys"));
    }
}
