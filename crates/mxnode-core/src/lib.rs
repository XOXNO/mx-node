//! Shared domain types for the mxnode workspace.
//!
//! No I/O. No external services. Pure data + small helpers so the rest of
//! the workspace can be unit-tested with simple fixtures.

pub mod config;
pub mod error;
pub mod paths;
pub mod platform;
pub mod state;
pub mod types;

pub use config::Config;
pub use error::Error;
pub use paths::Paths;
pub use platform::Platform;
pub use state::{InstallSection, InstallVersions, NodeState, ProxyState, State};
pub use types::{ArtifactSource, Environment, InstallKind, NodeIndex, Role, Shard, Tag};

/// Schema version this binary writes for `state.toml` and `config.toml`.
///
/// Bumped strictly monotonically. A binary refuses to act if it encounters
/// a schema version greater than this constant.
pub const SCHEMA_VERSION: u32 = 1;

/// Minimum default base port for the node REST API. Index `i` listens on
/// `API_PORT_BASE + i`. Matches the bash hardcoded `OFFSET=8080`.
pub const DEFAULT_API_PORT_BASE: u16 = 8080;

/// Minimum default proxy listen port. The bash quirk that flips `8080→8079`
/// lives in config; this is the default used when nothing overrides it.
pub const DEFAULT_PROXY_PORT: u16 = 8079;
