use std::path::{Path, PathBuf};

use mxnode_core::MxnodeFile;

use crate::origin::{merge_with_origin, Origin, OriginMap};
use crate::ConfigError;

/// Which scope to read or write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `~/.config/mxnode/mxnode.toml` (preferred for human edits).
    User,
    /// `/etc/mxnode/mxnode.toml` (system-wide defaults set by packagers).
    System,
}

impl Scope {
    pub fn description(&self) -> &'static str {
        match self {
            Self::User => "user (~/.config/mxnode/mxnode.toml)",
            Self::System => "system (/etc/mxnode/mxnode.toml)",
        }
    }
}

/// Where the config-file layer's contents came from. The loader resolves the
/// first existing file in the order user → system, mirroring the precedence
/// the rest of the resolver applies (user wins over system).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// No file was found at any standard scope. Pure defaults + flags.
    None,
    File {
        scope: Scope,
        path: PathBuf,
    },
    /// Caller provided an explicit path via `LoadOptions::config_path`.
    Explicit(PathBuf),
}

#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// Skip directory probing and load this file as the config layer.
    pub config_path: Option<PathBuf>,
    /// Sparse overrides applied as the highest layer (CLI flags). Use the
    /// same TOML schema as the file. `None` skips this layer.
    pub flags_overlay: Option<toml::Value>,
}

#[derive(Debug, Clone)]
pub struct Loaded {
    /// Resolved [`MxnodeFile`] — every operator + machine-derived
    /// section after the layered merge (defaults → file → flags).
    pub file: MxnodeFile,
    pub source: ConfigSource,
    pub origins: OriginMap,
}

/// Resolve the merged config and record per-leaf origin annotations.
///
/// Failure modes:
/// - explicit path missing  → returns `ConfigError::Io`.
/// - any layer parse error  → returns `ConfigError::Parse` with that file's path.
pub fn load(opts: &LoadOptions) -> Result<Loaded, ConfigError> {
    let defaults = MxnodeFile::default();
    let defaults_value = serialize(&defaults)?;

    let (file_value, source) = read_file_layer(opts)?;
    let flags_value = opts
        .flags_overlay
        .clone()
        .unwrap_or(toml::Value::Table(toml::map::Map::new()));

    let mut merged = defaults_value.clone();
    let mut origins = OriginMap::new();
    record_origins(&defaults_value, "", Origin::Default, &mut origins);

    if let Some(file_value) = &file_value {
        merge_with_origin(
            &mut merged,
            file_value,
            "",
            layer_origin(&source),
            &mut origins,
        );
    }
    merge_with_origin(&mut merged, &flags_value, "", Origin::Flag, &mut origins);

    let file: MxnodeFile = toml::Value::try_into(merged)
        .map_err(|e| ConfigError::Invalid(format!("merged config did not match schema: {e}")))?;

    Ok(Loaded {
        file,
        source,
        origins,
    })
}

fn layer_origin(source: &ConfigSource) -> Origin {
    match source {
        ConfigSource::None => Origin::Default,
        ConfigSource::File {
            scope: Scope::User, ..
        } => Origin::User,
        ConfigSource::File {
            scope: Scope::System,
            ..
        } => Origin::System,
        ConfigSource::Explicit(_) => Origin::Explicit,
    }
}

fn record_origins(value: &toml::Value, prefix: &str, origin: Origin, out: &mut OriginMap) {
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t.iter() {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                record_origins(v, &path, origin, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), origin);
        }
    }
}

fn read_file_layer(opts: &LoadOptions) -> Result<(Option<toml::Value>, ConfigSource), ConfigError> {
    if let Some(explicit) = &opts.config_path {
        let value = parse_file(explicit)?;
        return Ok((Some(value), ConfigSource::Explicit(explicit.clone())));
    }

    // User scope first; system scope is the packager's fallback.
    if let Ok(path) = user_config_path() {
        if path.exists() {
            return Ok((
                Some(parse_file(&path)?),
                ConfigSource::File {
                    scope: Scope::User,
                    path,
                },
            ));
        }
    }
    let system = system_config_path();
    if system.exists() {
        return Ok((
            Some(parse_file(&system)?),
            ConfigSource::File {
                scope: Scope::System,
                path: system,
            },
        ));
    }

    Ok((None, ConfigSource::None))
}

fn parse_file(path: &Path) -> Result<toml::Value, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    toml::from_str(&raw).map_err(|e| ConfigError::Parse {
        path: path.display().to_string(),
        source: e,
    })
}

fn serialize(cfg: &MxnodeFile) -> Result<toml::Value, ConfigError> {
    let raw = toml::to_string(cfg)?;
    toml::from_str::<toml::Value>(&raw).map_err(|e| ConfigError::Parse {
        path: "<defaults>".to_string(),
        source: e,
    })
}

/// `~/.config/mxnode/mxnode.toml` honoring `XDG_CONFIG_HOME`. Returns
/// the resolution error if neither HOME nor `XDG_CONFIG_HOME` is set —
/// callers must surface this to the operator instead of silently using
/// a default.
pub fn user_config_path() -> Result<PathBuf, ConfigError> {
    Ok(crate::xdg::xdg_config_home()?
        .join("mxnode")
        .join("mxnode.toml"))
}

/// Convenience alias used by commands that just want a writable path
/// without caring whether HOME or XDG resolved it.
pub fn user_config_path_or_default() -> Result<PathBuf, ConfigError> {
    user_config_path()
}

pub fn system_config_path() -> PathBuf {
    PathBuf::from("/etc/mxnode/mxnode.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn load_with_no_file_returns_defaults() {
        let opts = LoadOptions::default();
        let loaded = load(&opts).unwrap();
        assert_eq!(loaded.source, ConfigSource::None);
        assert_eq!(loaded.file, MxnodeFile::default());
        // Every leaf should be marked as Default.
        for origin in loaded.origins.values() {
            assert_eq!(*origin, Origin::Default);
        }
    }

    #[test]
    fn load_explicit_file_wins_over_defaults() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "test.toml",
            r#"
[network]
environment = "testnet"
github_org = "myfork"
"#,
        );
        let opts = LoadOptions {
            config_path: Some(path.clone()),
            flags_overlay: None,
        };
        let loaded = load(&opts).unwrap();
        assert!(matches!(loaded.source, ConfigSource::Explicit(_)));
        assert_eq!(
            loaded.file.network.environment,
            Some(mxnode_core::Environment::Testnet)
        );
        assert_eq!(loaded.file.network.github_org, "myfork");
        // Origin tracking: keys we set should be Explicit; untouched should stay Default.
        assert_eq!(
            loaded.origins.get("network.environment"),
            Some(&Origin::Explicit)
        );
        assert_eq!(
            loaded.origins.get("network.github_org"),
            Some(&Origin::Explicit)
        );
        assert_eq!(
            loaded.origins.get("node.api_port_base"),
            Some(&Origin::Default)
        );
    }

    #[test]
    fn flags_layer_wins_over_file() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "test.toml",
            r#"
[network]
environment = "testnet"
github_org = "fromfile"
"#,
        );
        let mut overlay = toml::map::Map::new();
        let mut net = toml::map::Map::new();
        net.insert(
            "github_org".to_string(),
            toml::Value::String("fromflag".to_string()),
        );
        overlay.insert("network".to_string(), toml::Value::Table(net));

        let opts = LoadOptions {
            config_path: Some(path.clone()),
            flags_overlay: Some(toml::Value::Table(overlay)),
        };
        let loaded = load(&opts).unwrap();
        assert_eq!(loaded.file.network.github_org, "fromflag");
        // environment came from the file, not the flag, so still Explicit.
        assert_eq!(
            loaded.file.network.environment,
            Some(mxnode_core::Environment::Testnet)
        );
        assert_eq!(
            loaded.origins.get("network.github_org"),
            Some(&Origin::Flag)
        );
        assert_eq!(
            loaded.origins.get("network.environment"),
            Some(&Origin::Explicit)
        );
    }

    #[test]
    fn invalid_toml_in_explicit_path_returns_parse_error() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "broken.toml", "not = [valid");
        let opts = LoadOptions {
            config_path: Some(path),
            flags_overlay: None,
        };
        let err = load(&opts).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }
}
