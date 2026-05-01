use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::types::{Environment, InstallKind, NodeIndex, Role, Shard, Tag};
use crate::SCHEMA_VERSION;
use std::fmt;

/// Cache-derived-from-disk view of what's actually installed on the host.
///
/// Per plan D7: this file is **not** the source of truth. `mxnode-state`
/// rebuilds it from `/etc/systemd/system/elrond-*.service` plus the legacy
/// dotfiles whenever drift is detected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub schema_version: u32,
    /// ISO-8601 UTC timestamp the file was last written.
    #[serde(with = "time::serde::rfc3339")]
    pub written_at: OffsetDateTime,
    pub written_by: String,
    /// True iff this serialization came from a `rebuild_from_disk` pass.
    pub discovered: bool,
    /// `None` until `mxnode adopt`/`migrate-from-bash` populates it. Writing a
    /// `Some(_)` value with fabricated defaults would lie about the host —
    /// the cache-derived model requires that we only ever record observed
    /// reality.
    #[serde(default)]
    pub install: Option<InstallSection>,
    #[serde(default)]
    pub nodes: Vec<NodeState>,
    #[serde(default)]
    pub proxy: Option<ProxyState>,
    #[serde(default)]
    pub migrations: MigrationLog,
}

impl State {
    /// Empty state stamped with the current schema and time. Used by `adopt`
    /// before discovery has filled in the install/node sections. Crucially,
    /// the install section starts as `None` — we never fabricate "mainnet
    /// validators" defaults.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallSection {
    pub kind: InstallKind,
    pub environment: Environment,
    pub github_org: String,
    pub node_count: u16,
    pub versions: InstallVersions,
    pub binaries: InstallBinaries,
}

impl InstallSection {
    /// Build an `InstallSection` from observed reality. There is intentionally
    /// no `Default` impl — `mxnode adopt` / `migrate-from-bash` must supply
    /// the environment, kind, and org explicitly.
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

impl fmt::Display for InstallSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} on {} ({} nodes, org={})",
            self.kind, self.environment, self.node_count, self.github_org,
        )
    }
}

/// Tags currently deployed across the host. `proxy_tag` is `None` when no
/// proxy is installed (e.g. multikey squads). `go_version` is non-empty only
/// when at least one binary was source-built.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallVersions {
    pub config_tag: Option<Tag>,
    pub binary_tag: Option<Tag>,
    pub proxy_tag: Option<Tag>,
    #[serde(default)]
    pub go_version: String,
}

/// Per-artifact version retention list. Newest first; trimmed to
/// `Config::install.binary_keep` after each successful upgrade.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallBinaries {
    #[serde(default)]
    pub node: Vec<Tag>,
    #[serde(default)]
    pub proxy: Vec<Tag>,
    #[serde(default)]
    pub keygenerator: Vec<Tag>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    pub index: NodeIndex,
    pub role: Role,
    pub shard: Shard,
    pub display_name: String,
    pub api_port: u16,
    pub unit: String,
    /// If `--force-adopt` accepted unit drift, the verbatim on-disk unit text
    /// lives here so we round-trip it unchanged on every operation.
    #[serde(default)]
    pub unit_override: String,
    pub workdir: PathBuf,
    /// First 12 hex chars of `erd_public_key_block_sign`, used for log-archive
    /// filenames (matches the bash `get_logs` convention).
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

/// Append-only log of upgrade attempts. Newest entries pushed last; readers
/// in `mxnode status` slice the tail.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_state_is_serializable() {
        let s = State::empty("mxnode/test");
        let toml_str = toml::to_string(&s).expect("serialize");
        let back: State = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(s.schema_version, back.schema_version);
        assert_eq!(s.written_by, back.written_by);
        assert_eq!(s.discovered, back.discovered);
    }

    #[test]
    fn empty_state_does_not_fabricate_install() {
        let s = State::empty("mxnode/test");
        assert_eq!(s.schema_version, SCHEMA_VERSION);
        assert!(s.nodes.is_empty());
        assert!(s.proxy.is_none());
        assert!(s.migrations.entries.is_empty());
        assert!(!s.discovered);
        assert!(
            s.install.is_none(),
            "empty State must NOT fabricate install metadata; cache-derived model requires observation",
        );
    }

    #[test]
    fn install_section_observed_records_supplied_values() {
        let install = InstallSection::observed(
            InstallKind::ObserversSquad,
            Environment::Testnet,
            "myfork",
            4,
        );
        assert_eq!(install.environment, Environment::Testnet);
        assert_eq!(install.kind, InstallKind::ObserversSquad);
        assert_eq!(install.github_org, "myfork");
        assert_eq!(install.node_count, 4);
    }
}
