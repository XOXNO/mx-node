use std::fs;
use std::path::{Path, PathBuf};

use mxnode_core::{NodeIndex, Tag};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::process::{classify, Liveness, ProcessIdentity};
use crate::store::{fsync_dir, StateError};

/// Multi-step state-changing operation kinds. Used to label `inflight.toml`
/// so the CLI can decide whether `--resume` makes sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpKind {
    Install,
    AddNodes,
    Upgrade,
}

/// Where in the per-node sequence the operation paused. Steps run in order;
/// `--resume` continues from `current_step` for the `current` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InflightStep {
    Resolving,
    Stopped,
    ConfigApplied,
    BinaryReplaced,
    Started,
    NonceVerified,
}

/// Transaction log written to `${paths.state}/inflight.toml` while a
/// state-changing op is in progress. Deleted on clean completion. See plan
/// D12 — this is the crash-recovery primitive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inflight {
    pub op: OpKind,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// Identity (PID + birth-time token) of the writer. Both fields are used
    /// to decide whether the op is live or stale across crashes / PID reuse.
    pub identity: ProcessIdentity,
    #[serde(default)]
    pub target_config_tag: Option<Tag>,
    #[serde(default)]
    pub target_binary_tag: Option<Tag>,
    #[serde(default)]
    pub target_proxy_tag: Option<Tag>,
    pub strategy: String,
    pub selected: Vec<NodeIndex>,
    #[serde(default)]
    pub completed: Vec<NodeIndex>,
    pub current: Option<NodeIndex>,
    pub current_step: InflightStep,
}

impl Inflight {
    pub fn new(op: OpKind, strategy: impl Into<String>, selected: Vec<NodeIndex>) -> Self {
        Self {
            op,
            started_at: OffsetDateTime::now_utc(),
            identity: ProcessIdentity::current(),
            target_config_tag: None,
            target_binary_tag: None,
            target_proxy_tag: None,
            strategy: strategy.into(),
            selected,
            completed: Vec::new(),
            current: None,
            current_step: InflightStep::Resolving,
        }
    }

    /// Atomically write to `path` with full durability: write a sibling
    /// tempfile, fsync it, rename, then fsync the parent dir.
    ///
    /// Caller must hold the upgrade.lock PID-file lock; this function does
    /// not enforce that — the orchestrator does, one level up.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let parent = path.parent().expect("inflight path has a parent");
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| StateError::Io {
                path: parent.display().to_string(),
                source: e,
            })?;
        }
        let body = toml::to_string_pretty(self)?;

        let tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| StateError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        // Write + fsync the file before the rename. A bare write would let
        // the rename win the kernel's writeback race and leave the new path
        // pointing at empty bytes after a crash.
        {
            use std::io::Write;
            let mut handle = tmp.as_file();
            handle.write_all(body.as_bytes()).map_err(|e| StateError::Io {
                path: tmp.path().display().to_string(),
                source: e,
            })?;
            handle.sync_all().map_err(|e| StateError::Io {
                path: tmp.path().display().to_string(),
                source: e,
            })?;
        }
        tmp.persist(path).map_err(|e| StateError::Io {
            path: path.display().to_string(),
            source: e.error,
        })?;
        fsync_dir(parent)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Option<Self>, StateError> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path).map_err(|e| StateError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let me: Self = toml::from_str(&raw).map_err(|e| StateError::Parse {
            path: path.display().to_string(),
            source: e,
        })?;
        Ok(Some(me))
    }

    /// Remove `inflight.toml` and fsync the parent directory so the
    /// deletion is crash-durable.
    ///
    /// **Locking contract** (not enforced by the type system): the caller
    /// must already have determined that no live mxnode process is acting
    /// on this op. The two legitimate callers in v0.1 are:
    ///   - `mxnode unlock --force`, which performs the
    ///     `process::classify(&inflight.identity)` check first and refuses
    ///     when liveness is `Live`.
    ///   - The orchestrator's upgrade-completion path (Phase 2+), which
    ///     reaches this only after holding the upgrade.lock PID-file lock
    ///     for the entire op.
    ///
    /// Calling `clear` from any other context is a bug; future versions
    /// may add a runtime sentinel to assert this.
    pub fn clear(path: &Path) -> Result<(), StateError> {
        if path.exists() {
            fs::remove_file(path).map_err(|e| StateError::Io {
                path: path.display().to_string(),
                source: e,
            })?;
            // The directory metadata changed; fsync to make the deletion
            // crash-durable too.
            if let Some(parent) = path.parent() {
                fsync_dir(parent)?;
            }
        }
        Ok(())
    }
}

/// Helper used by the CLI before running a state-changing op: detects an
/// existing `inflight.toml`, classifies the previous run, returns advice.
#[derive(Debug)]
pub enum InflightCheck {
    Clear,
    /// Previous mxnode crashed; safe to `--resume` or `--abandon`.
    StaleFromDeadProcess(Inflight),
    /// Another mxnode is currently running the recorded op. Refuse.
    Live { other_pid: u32, inflight: Inflight },
    /// PID exists but we couldn't confirm whether it's the same process
    /// (kernel denied us, or birth-time was unreadable). Caller should
    /// refuse rather than stomp.
    Indeterminate(Inflight),
}

impl InflightCheck {
    pub fn from_path(path: &Path, current: ProcessIdentity) -> Result<Self, StateError> {
        let Some(inflight) = Inflight::load(path)? else {
            return Ok(InflightCheck::Clear);
        };
        if inflight.identity.pid == current.pid && inflight.identity.started_token == current.started_token {
            // Our own process re-entering; treat as clear (we'll overwrite).
            return Ok(InflightCheck::Clear);
        }
        match classify(&inflight.identity) {
            Liveness::Live => Ok(InflightCheck::Live {
                other_pid: inflight.identity.pid,
                inflight,
            }),
            Liveness::Stale => Ok(InflightCheck::StaleFromDeadProcess(inflight)),
            Liveness::Unknown => Ok(InflightCheck::Indeterminate(inflight)),
        }
    }
}

/// Stable path helper matching `Paths::inflight_file`.
pub fn inflight_path(state_dir: impl AsRef<Path>) -> PathBuf {
    state_dir.as_ref().join("inflight.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxnode_core::NodeIndex;
    use tempfile::TempDir;

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = inflight_path(dir.path());
        let inflight = Inflight::new(
            OpKind::Upgrade,
            "rolling",
            vec![NodeIndex::new(0), NodeIndex::new(1)],
        );
        inflight.save(&path).unwrap();
        let loaded = Inflight::load(&path).unwrap().unwrap();
        assert_eq!(loaded.op, OpKind::Upgrade);
        assert_eq!(loaded.strategy, "rolling");
        assert_eq!(loaded.selected.len(), 2);
        assert_eq!(loaded.current_step, InflightStep::Resolving);
        assert_eq!(loaded.identity.pid, std::process::id());
    }

    #[test]
    fn clear_removes_file() {
        let dir = TempDir::new().unwrap();
        let path = inflight_path(dir.path());
        Inflight::new(OpKind::Install, "rolling", vec![]).save(&path).unwrap();
        assert!(path.exists());
        Inflight::clear(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn check_clear_when_no_file() {
        let dir = TempDir::new().unwrap();
        let path = inflight_path(dir.path());
        let check = InflightCheck::from_path(&path, ProcessIdentity::current()).unwrap();
        assert!(matches!(check, InflightCheck::Clear));
    }

    #[test]
    fn check_stale_when_pid_dead() {
        let dir = TempDir::new().unwrap();
        let path = inflight_path(dir.path());
        let mut inflight = Inflight::new(OpKind::Upgrade, "rolling", vec![]);
        // Use an absurdly high pid that is virtually guaranteed not to exist.
        inflight.identity = ProcessIdentity {
            pid: u32::MAX - 1,
            started_token: 0,
        };
        inflight.save(&path).unwrap();
        let check = InflightCheck::from_path(&path, ProcessIdentity::current()).unwrap();
        assert!(
            matches!(check, InflightCheck::StaleFromDeadProcess(_) | InflightCheck::Indeterminate(_)),
            "got {check:?}",
        );
    }

    /// PID reuse detection: write an inflight with our own PID but a
    /// tampered birth-time token. The classifier must mark it stale.
    #[test]
    fn check_stale_when_pid_reused_with_different_token() {
        let dir = TempDir::new().unwrap();
        let path = inflight_path(dir.path());
        let mut inflight = Inflight::new(OpKind::Upgrade, "rolling", vec![]);
        // Only meaningful when birth-time capture worked; on platforms
        // without /proc the token is 0 and this test degenerates to the
        // legacy PID-only behaviour.
        if inflight.identity.started_token == 0 {
            return;
        }
        inflight.identity.started_token = inflight.identity.started_token.wrapping_add(1);
        inflight.save(&path).unwrap();
        let check = InflightCheck::from_path(&path, ProcessIdentity::current()).unwrap();
        assert!(matches!(check, InflightCheck::StaleFromDeadProcess(_)), "got {check:?}");
    }
}
