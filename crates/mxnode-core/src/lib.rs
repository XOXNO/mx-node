//! Shared domain types for the mxnode workspace.
//!
//! No I/O. No external services. Pure data + small helpers so the rest
//! of the workspace can be unit-tested with simple fixtures.

pub mod error;
pub mod file;
pub mod paths;
pub mod platform;
pub mod types;

// Legacy modules kept as thin re-exports during the call-site sweep.
// They are scheduled for removal once every consumer has been flipped
// to read `MxnodeFile` directly.
pub mod config {
    pub use crate::file::{
        BrandingSection, InstallSection, MetricsSection, NetworkSection, NodeOverride,
        NodeSection, OverridesSection, PathsSection, ProxySection,
    };
}
pub mod state {
    pub use crate::file::{
        HostInstall as InstallSection, InstallBinaries, InstallVersions, MigrationEntry,
        MigrationLog, MigrationResult, NodeState, ProxyState,
    };
}

pub use error::Error;
pub use file::{
    BrandingSection, HostInstall, HostState, InstallBinaries, InstallSection, InstallVersions,
    MetricsSection, MigrationEntry, MigrationLog, MigrationResult, MxnodeFile, NetworkSection,
    NodeOverride, NodeSection, NodeState, OverridesSection, PathsSection, ProxySection,
    ProxyState, SecretToken, SecretsSection, UpdateCacheSection,
};
pub use paths::Paths;
pub use platform::Platform;
pub use types::{ArtifactSource, Environment, InstallKind, NodeIndex, Role, Shard, Tag};

/// Transitional alias. `Config` was the operator-side type backed by
/// `config.toml`; the unified `MxnodeFile` now owns those sections.
/// Existing call sites read `runtime.loaded.config.X` — that resolves
/// through this alias until the sweep replaces them with `.file.X`.
pub type Config = MxnodeFile;

/// Transitional alias. `State` was the cache-derived type backed by
/// `state.toml`; its content lives under `MxnodeFile.host`. Call sites
/// that did `state.install.kind` keep compiling because `HostState`
/// exposes the same field shape (with `installed` renamed from
/// `install`).
pub type State = HostState;

/// Schema version this binary writes for `mxnode.toml`. Bumped strictly
/// monotonically; a binary refuses to act if it encounters a version
/// greater than this constant.
pub const SCHEMA_VERSION: u32 = 1;

/// Default base port for the node REST API. Index `i` listens on
/// `API_PORT_BASE + i`. Matches the bash hardcoded `OFFSET=8080`.
pub const DEFAULT_API_PORT_BASE: u16 = 8080;

/// Default proxy listen port. The bash quirk that flips `8080→8079`
/// lives in config; this is the default used when nothing overrides it.
pub const DEFAULT_PROXY_PORT: u16 = 8079;
