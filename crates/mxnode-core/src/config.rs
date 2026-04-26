use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::{ArtifactSource, Environment, NodeIndex, Role, Shard};
use crate::{DEFAULT_API_PORT_BASE, DEFAULT_PROXY_PORT, SCHEMA_VERSION};

/// Top-level config schema written to `~/.config/mxnode/config.toml` /
/// `/etc/mxnode/config.toml`. Sparse: every layer only specifies what it
/// wants to override; defaults fill in the rest.
///
/// Note: `Eq` is intentionally not derived because [`OverridesSection`]
/// can carry `toml::Value::Float` payloads (operator-supplied numeric
/// overrides), and `f64` only implements `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub network: NetworkSection,
    pub paths: PathsSection,
    pub node: NodeSection,
    pub proxy: ProxySection,
    pub install: InstallSection,
    pub overrides: OverridesSection,
    pub metrics: MetricsSection,
    pub branding: BrandingSection,
    pub nodes: Vec<NodeOverride>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            network: NetworkSection::default(),
            paths: PathsSection::default(),
            node: NodeSection::default(),
            proxy: ProxySection::default(),
            install: InstallSection::default(),
            overrides: OverridesSection::default(),
            metrics: MetricsSection::default(),
            branding: BrandingSection::default(),
            nodes: Vec::new(),
        }
    }
}

/// Operator-facing brand string rendered in the dashboard's top bar.
/// This fork ships with the XOXNO/TrustStaking banner; downstream
/// operators override via `[branding] title = "..."` in their config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrandingSection {
    pub title: String,
}

impl Default for BrandingSection {
    fn default() -> Self {
        Self {
            title: "By XOXNO ✦ TrustStaking".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSection {
    /// Optional in the file, but `Config::validate` requires it to be set
    /// before any state-changing op.
    pub environment: Option<Environment>,
    pub github_org: String,
    /// Public gateway used by `mxnode dashboard` to read
    /// `/network/trie-statistics/<shard>` (the totals operator-running
    /// observers can't compute themselves). Default points at the
    /// MultiversX-hosted gateway; override this for forks or air-gapped
    /// setups, or set it to an empty string to disable trie-stats
    /// lookups entirely.
    pub gateway: String,
}

impl Default for NetworkSection {
    fn default() -> Self {
        Self {
            environment: None,
            github_org: "multiversx".to_string(),
            gateway: "https://gateway.multiversx.com".to_string(),
        }
    }
}

/// String fields here may contain `{custom_home}` and `{home}` placeholders;
/// `mxnode-config` resolves them after merging. The core type sees raw
/// strings; resolution lives in the loader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsSection {
    pub custom_home: PathBuf,
    pub custom_user: String,
    pub node_keys: String,
    pub binaries: String,
    pub state: String,
    pub runtime: String,
}

impl Default for PathsSection {
    fn default() -> Self {
        Self {
            custom_home: PathBuf::from("/home/ubuntu"),
            custom_user: "ubuntu".to_string(),
            node_keys: "{custom_home}/VALIDATOR_KEYS".to_string(),
            binaries: "{custom_home}/mxnode/binaries".to_string(),
            state: "{XDG_STATE_HOME}/mxnode".to_string(),
            runtime: "{XDG_RUNTIME_DIR}/mxnode".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeSection {
    pub extra_flags: String,
    pub api_port_base: u16,
    pub log_level: String,
    pub limit_nofile: u32,
    pub restart_sec: u32,
    pub name_template: String,
}

impl Default for NodeSection {
    fn default() -> Self {
        Self {
            extra_flags: String::new(),
            api_port_base: DEFAULT_API_PORT_BASE,
            log_level: "*:DEBUG".to_string(),
            limit_nofile: 4096,
            restart_sec: 3,
            name_template: "mx-chain-{env}-validator-{index}".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySection {
    pub server_port: u16,
    /// Default mapping for the four-shard observer squad. Stored as wire-level
    /// shard ids (0, 1, 2, `u32::MAX` for metachain).
    pub observers_shards: Vec<u32>,
}

impl Default for ProxySection {
    fn default() -> Self {
        Self {
            server_port: DEFAULT_PROXY_PORT,
            observers_shards: vec![0, 1, 2, 4_294_967_295],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallSection {
    pub artifact_source: ArtifactSource,
    pub binary_keep: u8,
}

impl Default for InstallSection {
    fn default() -> Self {
        Self {
            artifact_source: ArtifactSource::Source,
            binary_keep: 3,
        }
    }
}

/// Power-user pinning. Empty string means "not overridden — auto-resolve".
/// We use empty-string-means-unset rather than `Option<String>` because
/// `figment` and `toml` round-trip empty strings cleanly through env vars.
///
/// `prefs` and `config` are operator-supplied dotted-key → TOML value
/// maps applied to **every node's** `prefs.toml` and `config.toml`
/// respectively after the install/upgrade orchestrator's well-known
/// edits. Single source of truth for cross-node config tweaks; previous
/// versions of the bash flow required per-node sed invocations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverridesSection {
    pub configver: String,
    pub proxyver: String,
    pub binaryver: String,
    pub goversion: String,
    /// Dotted-key TOML overrides applied to every node's `prefs.toml`.
    /// Example: `"Preferences.FullArchive" = true`.
    /// Template tokens `{index}` and `{shard}` are substituted in
    /// string values at apply time.
    pub prefs: BTreeMap<String, toml::Value>,
    /// Dotted-key TOML overrides applied to every node's `config.toml`.
    /// Example: `"DbLookupExtensions.Enabled" = true`.
    pub config: BTreeMap<String, toml::Value>,
}

impl OverridesSection {
    pub fn configver(&self) -> Option<&str> {
        non_empty(&self.configver)
    }
    pub fn proxyver(&self) -> Option<&str> {
        non_empty(&self.proxyver)
    }
    pub fn binaryver(&self) -> Option<&str> {
        non_empty(&self.binaryver)
    }
    pub fn goversion(&self) -> Option<&str> {
        non_empty(&self.goversion)
    }
    pub fn has_prefs(&self) -> bool {
        !self.prefs.is_empty()
    }
    pub fn has_config(&self) -> bool {
        !self.config.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsSection {
    pub enabled: bool,
    pub listen: String,
}

impl Default for MetricsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:9090".to_string(),
        }
    }
}

/// Optional per-node overrides matched by index. Sparse: missing nodes inherit
/// `[node]` defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeOverride {
    pub index: NodeIndex,
    pub role: Role,
    pub shard: Shard,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub extra_flags: String,
}

fn non_empty(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_invariants() {
        let cfg = Config::default();
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
        assert_eq!(cfg.network.github_org, "multiversx");
        assert_eq!(cfg.network.environment, None);
        assert_eq!(cfg.node.api_port_base, DEFAULT_API_PORT_BASE);
        assert_eq!(cfg.node.limit_nofile, 4096);
        assert_eq!(cfg.proxy.server_port, DEFAULT_PROXY_PORT);
        assert_eq!(cfg.proxy.observers_shards, vec![0, 1, 2, 4_294_967_295]);
        assert_eq!(cfg.install.artifact_source, ArtifactSource::Source);
        assert_eq!(cfg.install.binary_keep, 3);
    }

    #[test]
    fn default_round_trips_through_toml() {
        let cfg = Config::default();
        let serialized = toml::to_string(&cfg).expect("serialize");
        let parsed: Config = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn empty_overrides_report_none() {
        let o = OverridesSection::default();
        assert!(o.configver().is_none());
        assert!(o.proxyver().is_none());
        assert!(o.binaryver().is_none());
        assert!(o.goversion().is_none());
    }

    #[test]
    fn whitespace_overrides_report_none() {
        let o = OverridesSection {
            configver: "   ".to_string(),
            ..Default::default()
        };
        assert!(o.configver().is_none());
    }

    #[test]
    fn populated_override_reports_some() {
        let o = OverridesSection {
            configver: "v1.7.13.0".to_string(),
            ..Default::default()
        };
        assert_eq!(o.configver(), Some("v1.7.13.0"));
    }
}
