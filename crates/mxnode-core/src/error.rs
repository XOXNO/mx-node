use thiserror::Error;

/// Top-level error type used by `mxnode-core`. Other crates wrap this in their
/// own typed errors via `#[from]` or convert at boundaries.
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid environment: expected mainnet|testnet|devnet, got {0:?}")]
    InvalidEnvironment(String),

    #[error("invalid shard: expected 0|1|2|metachain|disabled|auto, got {0:?}")]
    InvalidShard(String),

    #[error("invalid role: expected validator|observer|multikey, got {0:?}")]
    InvalidRole(String),

    #[error("invalid artifact_source: expected source|release|auto, got {0:?}")]
    InvalidArtifactSource(String),

    #[error("invalid node index: {0}")]
    InvalidNodeIndex(String),

    #[error("invalid tag: {0:?}")]
    InvalidTag(String),

    #[error("schema version {found} is newer than this binary supports ({max}); upgrade mxnode")]
    SchemaTooNew { found: u32, max: u32 },
}
