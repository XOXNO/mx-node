//! `mxnode.toml` schema — single source of truth on disk.
//!
//! Replaces the old split between `mxnode.toml` (operator-edited) and
//! `mxnode.toml` (machine-derived). Operators now edit one file at
//! `<XDG_CONFIG_HOME>/mxnode/mxnode.toml`, mode 0600.
//!
//! Layout:
//!
//! ```toml
//! schema_version = 1
//!
//! # operator-owned (top-level for hand-edit)
//! [network]
//! [paths]
//! [node]
//! [proxy]
//! [install]
//! [overrides]
//! [metrics]
//! [branding]
//! [[node_overrides]]
//!
//! # machine-derived (managed by mxnode)
//! [host]
//! [host.installed]
//! [[host.nodes]]
//! [host.proxy]
//! [host.migrations]
//!
//! # secrets / cache
//! [secrets]
//! [update_cache]
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::types::{
    ArtifactSource, Environment, InstallKind, NodeIndex, Role, Shard, Tag,
};
use crate::{DEFAULT_API_PORT_BASE, DEFAULT_PROXY_PORT, SCHEMA_VERSION};

// ─────────────────────────────────────────────────────────────────────
// Top-level document
// ─────────────────────────────────────────────────────────────────────

/// On-disk shape of `mxnode.toml`. Sparse: every section has a sensible
/// `Default` so a fresh file (or unset section) round-trips cleanly.
///
/// `Eq` is intentionally not derived because [`OverridesSection`] can
/// carry `toml::Value::Float` payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MxnodeFile {
    pub schema_version: u32,

    // ── operator-owned sections ──────────────────────────────────────
    pub network: NetworkSection,
    pub paths: PathsSection,
    pub node: NodeSection,
    pub proxy: ProxySection,
    pub install: InstallSection,
    pub overrides: OverridesSection,
    pub metrics: MetricsSection,
    pub branding: BrandingSection,
    /// Per-node operator overrides matched by index. Sparse: missing
    /// nodes inherit `[node]` defaults. No collision with the host
    /// inventory's `[host.nodes]` because that array lives nested.
    pub nodes: Vec<NodeOverride>,

    // ── machine-derived ──────────────────────────────────────────────
    pub host: HostState,

    // ── secrets / cache ──────────────────────────────────────────────
    pub secrets: SecretsSection,
    pub update_cache: UpdateCacheSection,
}

impl Default for MxnodeFile {
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
            host: HostState::default(),
            secrets: SecretsSection::default(),
            update_cache: UpdateCacheSection::default(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Operator-owned sections
// ─────────────────────────────────────────────────────────────────────

/// Operator-facing brand string rendered in the dashboard's top bar.
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
    /// Optional in the file, but `validate` requires it before any
    /// state-changing op.
    pub environment: Option<Environment>,
    pub github_org: String,
    /// Public gateway used by `mxnode status --watch` for trie-statistics.
    /// Empty string disables the lookup.
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

/// String fields here may contain `{custom_home}` and `{home}`
/// placeholders; `mxnode-config::resolve_paths` resolves them.
///
/// `custom_home` and `custom_user` are intentionally `Option<...>`:
/// when absent, the resolver falls back to the runtime `$HOME` /
/// `$USER` so the config never carries a stale snapshot. Operators
/// running multi-user / shared-deploy layouts set them explicitly to
/// override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_home: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_user: Option<String>,
    pub node_keys: String,
    pub binaries: String,
    pub state: String,
    pub runtime: String,
}

impl Default for PathsSection {
    fn default() -> Self {
        Self {
            custom_home: None,
            custom_user: None,
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
    pub operation_mode: Option<String>,
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
            operation_mode: None,
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
    /// Default mapping for the four-shard observer squad. Stored as
    /// wire-level shard ids (0, 1, 2, `u32::MAX` for metachain).
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverridesSection {
    pub configver: String,
    pub proxyver: String,
    pub binaryver: String,
    pub goversion: String,
    /// Dotted-key TOML overrides applied to every node's `prefs.toml`.
    pub prefs: BTreeMap<String, toml::Value>,
    /// Dotted-key TOML overrides applied to every node's `mxnode.toml`.
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

/// Per-node operator override matched by index. Sparse: missing nodes
/// inherit `[node]` defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeOverride {
    pub index: NodeIndex,
    pub role: Role,
    pub shard: Shard,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub extra_flags: String,
    #[serde(default)]
    pub operation_mode: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────
// Machine-derived: [host]
// ─────────────────────────────────────────────────────────────────────

/// Cache-derived view of what's actually installed on the host. Never
/// edited by hand. `mxnode adopt` / `import-bash` populate it; commands
/// like `install` / `upgrade` mutate it; `status` reads it.
///
/// The `schema_version` field is kept here (in addition to the one on
/// [`MxnodeFile`]) so the existing `StateStore` save/load pipeline keeps
/// validating — a follow-up step folds the version check into the
/// top-level loader and removes this duplicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostState {
    pub schema_version: u32,
    /// ISO-8601 UTC timestamp the section was last written.
    #[serde(with = "time::serde::rfc3339")]
    pub written_at: OffsetDateTime,
    pub written_by: String,
    /// True iff this snapshot came from a `rebuild_from_disk` pass.
    pub discovered: bool,
    /// `None` until adopt / import-bash records observed reality. Never
    /// fabricated — cache-derived model requires observation.
    #[serde(default)]
    pub install: Option<HostInstall>,
    #[serde(default)]
    pub nodes: Vec<NodeState>,
    #[serde(default)]
    pub proxy: Option<ProxyState>,
    #[serde(default)]
    pub migrations: MigrationLog,
}

impl Default for HostState {
    fn default() -> Self {
        Self::empty("mxnode/default")
    }
}

impl HostState {
    /// Empty inventory stamped with the writer label and current time.
    /// Used by `adopt` before discovery has filled in the install /
    /// node sections.
    pub fn empty(written_by: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            written_at: OffsetDateTime::now_utc(),
            written_by: written_by.into(),
            discovered: false,
            install: None,
            nodes: Vec::new(),
            proxy: None,
            migrations: MigrationLog::default(),
        }
    }
}

/// Observed install metadata — kind / environment / org / version
/// pinning. Renamed from the old `HostInstall` to disambiguate
/// from the operator-side [`InstallSection`] (artifact-source policy).
///
/// `#[serde(default)]` lets tag/binary lists fall back to empty when a
/// migrated legacy `mxnode.toml` omits them — only `kind`, `environment`,
/// `github_org` and `node_count` are required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInstall {
    pub kind: InstallKind,
    pub environment: Environment,
    pub github_org: String,
    pub node_count: u16,
    #[serde(default)]
    pub versions: InstallVersions,
    #[serde(default)]
    pub binaries: InstallBinaries,
}

impl HostInstall {
    /// Build from observed reality. There is intentionally no `Default`
    /// — `mxnode adopt` / `import-bash` must supply the environment,
    /// kind, and org explicitly.
    pub fn observed(
        kind: InstallKind,
        environment: Environment,
        github_org: impl Into<String>,
        node_count: u16,
    ) -> Self {
        Self {
            kind,
            environment,
            github_org: github_org.into(),
            node_count,
            versions: InstallVersions::default(),
            binaries: InstallBinaries::default(),
        }
    }
}

impl fmt::Display for HostInstall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} on {} ({} nodes, org={})",
            self.kind, self.environment, self.node_count, self.github_org,
        )
    }
}

/// Tags currently deployed across the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallVersions {
    pub config_tag: Option<Tag>,
    pub binary_tag: Option<Tag>,
    pub proxy_tag: Option<Tag>,
    #[serde(default)]
    pub go_version: String,
}

/// Per-artifact version retention list. Newest first; trimmed to
/// `install.binary_keep` after each successful upgrade.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallBinaries {
    #[serde(default)]
    pub node: Vec<Tag>,
    #[serde(default)]
    pub proxy: Vec<Tag>,
    #[serde(default)]
    pub keygenerator: Vec<Tag>,
    #[serde(default)]
    pub seednode: Vec<Tag>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    pub index: NodeIndex,
    pub role: Role,
    pub shard: Shard,
    pub display_name: String,
    pub api_port: u16,
    pub unit: String,
    /// If `--force-adopt` accepted unit drift, the verbatim on-disk unit
    /// text lives here so we round-trip it unchanged on every operation.
    #[serde(default)]
    pub unit_override: String,
    pub workdir: PathBuf,
    /// First 12 hex chars of `erd_public_key_block_sign`, used for
    /// log-archive filenames.
    #[serde(default)]
    pub last_known_pubkey: String,
    #[serde(default)]
    pub last_action: String,
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub last_action_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyState {
    pub present: bool,
    pub unit: String,
    pub workdir: PathBuf,
    pub server_port: u16,
}

/// Append-only log of upgrade attempts. Newest entries pushed last.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationLog {
    #[serde(default)]
    pub entries: Vec<MigrationEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationEntry {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub from_config: Option<Tag>,
    pub to_config: Option<Tag>,
    pub from_binary: Option<Tag>,
    pub to_binary: Option<Tag>,
    pub strategy: String,
    pub trigger: String,
    pub result: MigrationResult,
    pub duration_secs: u64,
    #[serde(default)]
    pub nodes_done: Vec<NodeIndex>,
    #[serde(default)]
    pub nodes_failed: Vec<NodeIndex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MigrationResult {
    Ok,
    RolledBack,
    Partial,
}

// ─────────────────────────────────────────────────────────────────────
// [secrets] — file-mode-protected, redacted at the type level
// ─────────────────────────────────────────────────────────────────────

/// Tokens and other plaintext credentials. The whole `mxnode.toml` is
/// expected to be mode 0600 — the loader rejects looser modes.
///
/// `Debug` is hand-written so accidental `tracing::info!("{cfg:?}")`
/// calls cannot leak the value.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecretsSection {
    /// GitHub personal-access-token used by `mxnode upgrade` and
    /// release-fetch flows. Empty string means "unset".
    pub github_token: SecretToken,
}

impl fmt::Debug for SecretsSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretsSection")
            .field("github_token", &self.github_token)
            .finish()
    }
}

/// Plaintext token wrapper. Custom `Debug` redacts the value; serde
/// (de)serialization is transparent so TOML round-trips unchanged.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Render as the canonical mask (first four chars + `*` for the
    /// remainder, capped at 32). Use this in dry-run / `config show`.
    pub fn masked(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let visible_len = self.0.len().min(4);
        let visible: String = self.0.chars().take(visible_len).collect();
        let tail_len = self.0.len().saturating_sub(visible_len).min(32);
        format!("{visible}{}", "*".repeat(tail_len))
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("\"\"")
        } else {
            f.write_str("\"[redacted]\"")
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// [update_cache] — release-check memo so we don't hit GitHub every run
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateCacheSection {
    /// When the gate last fetched / consulted GitHub. `None` = never.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub last_checked_at: Option<OffsetDateTime>,
    /// Most recent `latest` tag seen. Empty when never fetched.
    pub latest_tag: String,
    /// Tag the operator declined; the gate suppresses the prompt for
    /// the same tag for `decline_ttl`.
    pub declined_tag: String,
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub declined_at: Option<OffsetDateTime>,
}

// ─────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────

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
    fn default_file_has_expected_invariants() {
        let f = MxnodeFile::default();
        assert_eq!(f.schema_version, SCHEMA_VERSION);
        assert_eq!(f.network.github_org, "multiversx");
        assert_eq!(f.network.environment, None);
        assert_eq!(f.node.api_port_base, DEFAULT_API_PORT_BASE);
        assert_eq!(f.proxy.server_port, DEFAULT_PROXY_PORT);
        assert!(f.host.install.is_none());
        assert!(f.host.nodes.is_empty());
        assert!(f.secrets.github_token.is_empty());
        assert!(f.update_cache.latest_tag.is_empty());
    }

    #[test]
    fn default_round_trips_through_toml() {
        let f = MxnodeFile::default();
        let serialized = toml::to_string(&f).expect("serialize");
        let parsed: MxnodeFile = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(f, parsed);
    }

    #[test]
    fn serialized_layout_uses_renamed_top_level_keys() {
        let mut f = MxnodeFile::default();
        f.nodes.push(NodeOverride {
            index: NodeIndex::new(0),
            role: Role::Validator,
            shard: Shard::Auto,
            display_name: String::new(),
            extra_flags: String::new(),
            operation_mode: None,
        });
        let body = toml::to_string(&f).unwrap();
        assert!(body.contains("[[nodes]]"), "body:\n{body}");
        // [host] subtree is nested under one root.
        assert!(body.contains("[host]"), "body:\n{body}");
    }

    #[test]
    fn host_state_empty_does_not_fabricate_install() {
        let h = HostState::empty("mxnode/test");
        assert!(h.install.is_none());
        assert!(h.nodes.is_empty());
        assert!(h.proxy.is_none());
        assert!(h.migrations.entries.is_empty());
        assert!(!h.discovered);
        assert_eq!(h.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn secret_token_debug_redacts_value() {
        let t = SecretToken::new("ghp_abcdefghijklmno");
        let dbg = format!("{:?}", t);
        assert!(!dbg.contains("ghp_"), "Debug leaked token: {dbg}");
        assert!(dbg.contains("[redacted]"), "Debug not redacted: {dbg}");
    }

    #[test]
    fn secret_token_empty_debug_is_empty_string() {
        let t = SecretToken::default();
        assert_eq!(format!("{:?}", t), "\"\"");
    }

    #[test]
    fn secret_token_masked_keeps_prefix_and_pads_tail() {
        let t = SecretToken::new("ghp_abcdefghijklmno");
        let masked = t.masked();
        assert!(masked.starts_with("ghp_"));
        assert!(masked.ends_with('*'));
        assert!(!masked.contains("abcd"));
    }

    #[test]
    fn secrets_section_debug_does_not_leak_token() {
        let s = SecretsSection {
            github_token: SecretToken::new("ghp_secretvalue"),
        };
        let dbg = format!("{:?}", s);
        assert!(!dbg.contains("secretvalue"), "{dbg}");
        assert!(dbg.contains("[redacted]"), "{dbg}");
    }

    #[test]
    fn empty_overrides_report_none() {
        let o = OverridesSection::default();
        assert!(o.configver().is_none());
        assert!(o.proxyver().is_none());
        assert!(o.binaryver().is_none());
        assert!(o.goversion().is_none());
    }
}
