//! state.toml read/write, atomic rename, flock, transaction log.
//!
//! Per plan D7: this file is a **cache**, not the source of truth. The CLI
//! always treats it as a serialization of what was last observed; on any
//! drift, the orchestrator runs `rebuild_from_disk` (defined in the binary)
//! and overwrites it.

mod binstore;
mod inflight;
mod process;
mod store;

pub use binstore::{read_symlink, swap_symlink, BinStore, BinStoreError};
pub use inflight::{inflight_path, Inflight, InflightCheck, InflightStep, OpKind};
pub use process::{classify, Liveness, ProcessIdentity};
pub use store::{StateError, StateStore};
