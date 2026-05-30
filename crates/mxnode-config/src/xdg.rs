//! Single source of truth for HOME / XDG path resolution.
//!
//! Loader and resolver both consume these helpers so they cannot disagree
//! on what fallback applies when env vars are missing.

use std::path::PathBuf;

use crate::ConfigError;

/// Resolve the operator's home directory, surfacing an explicit error rather
/// than silently falling back to anything when neither `HOME` nor a platform
/// override is available.
pub fn home_dir() -> Result<PathBuf, ConfigError> {
    dirs::home_dir().ok_or_else(|| {
        ConfigError::NoHome(
            "could not determine home directory; set $HOME or run mxnode with an explicit --config <PATH>".to_string(),
        )
    })
}

/// `$XDG_CONFIG_HOME` if set and non-empty, else `$HOME/.config`.
pub fn xdg_config_home() -> Result<PathBuf, ConfigError> {
    if let Ok(s) = std::env::var("XDG_CONFIG_HOME") {
        if !s.is_empty() {
            return Ok(PathBuf::from(s));
        }
    }
    Ok(home_dir()?.join(".config"))
}

/// `$XDG_STATE_HOME` if set and non-empty, else `$HOME/.local/state`.
pub fn xdg_state_home() -> Result<PathBuf, ConfigError> {
    if let Ok(s) = std::env::var("XDG_STATE_HOME") {
        if !s.is_empty() {
            return Ok(PathBuf::from(s));
        }
    }
    Ok(home_dir()?.join(".local/state"))
}

/// `$XDG_RUNTIME_DIR` if set and non-empty, else `<state>/run`.
///
/// Falling back to a per-user state directory (not `/run`) is deliberate:
/// mxnode is unprivileged and cannot write under `/run` without root.
pub fn xdg_runtime_dir() -> Result<PathBuf, ConfigError> {
    if let Ok(s) = std::env::var("XDG_RUNTIME_DIR") {
        if !s.is_empty() {
            return Ok(PathBuf::from(s));
        }
    }
    Ok(xdg_state_home()?.join("run"))
}

/// `$XDG_STATE_HOME` only when it is explicitly set and non-empty.
///
/// Unlike [`xdg_state_home`], this does NOT fall back to `$HOME/.local/state`.
/// The resolver uses it to keep state anchored to the install owner
/// (`custom_home`) rather than the invoking user's home — running the same
/// install via `sudo` (which rewrites `$HOME`) must not silently relocate
/// `state.toml`. When the operator opts in by exporting `$XDG_STATE_HOME`,
/// that choice is honored.
pub fn xdg_state_home_explicit() -> Option<PathBuf> {
    env_path("XDG_STATE_HOME")
}

/// `$XDG_RUNTIME_DIR` only when explicitly set and non-empty. See
/// [`xdg_state_home_explicit`] for the rationale.
pub fn xdg_runtime_dir_explicit() -> Option<PathBuf> {
    env_path("XDG_RUNTIME_DIR")
}

fn env_path(var: &str) -> Option<PathBuf> {
    match std::env::var(var) {
        Ok(s) if !s.is_empty() => Some(PathBuf::from(s)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to scrub env vars deterministically inside one test.
    /// `serial_test` would be ideal but adds a dep; instead we save/restore
    /// in-process and accept that running tests in parallel risks flakes —
    /// the test below is single-threaded by virtue of touching a unique
    /// var (`XDG_CONFIG_HOME`).
    struct EnvScope {
        var: &'static str,
        previous: Option<String>,
    }

    impl EnvScope {
        fn new(var: &'static str) -> Self {
            let previous = std::env::var(var).ok();
            Self { var, previous }
        }
        fn set(&self, value: &str) {
            std::env::set_var(self.var, value);
        }
        fn unset(&self) {
            std::env::remove_var(self.var);
        }
    }

    impl Drop for EnvScope {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var(self.var, v),
                None => std::env::remove_var(self.var),
            }
        }
    }

    #[test]
    fn xdg_config_home_uses_env_when_set() {
        let scope = EnvScope::new("XDG_CONFIG_HOME");
        scope.set("/tmp/cfg-test");
        assert_eq!(xdg_config_home().unwrap(), PathBuf::from("/tmp/cfg-test"));
    }

    #[test]
    fn xdg_config_home_falls_back_to_home_dot_config() {
        let scope = EnvScope::new("XDG_CONFIG_HOME");
        scope.unset();
        let path = xdg_config_home().unwrap();
        assert!(path.ends_with(".config"));
    }

    #[test]
    fn xdg_config_home_treats_empty_env_as_unset() {
        let scope = EnvScope::new("XDG_CONFIG_HOME");
        scope.set("");
        let path = xdg_config_home().unwrap();
        assert!(
            path.ends_with(".config"),
            "empty XDG_CONFIG_HOME must fall back, got {path:?}",
        );
    }
}
