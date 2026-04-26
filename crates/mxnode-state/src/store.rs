use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use mxnode_core::{State, SCHEMA_VERSION};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse state.toml at {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },

    #[error("could not serialize state: {0}")]
    Serialize(#[from] toml::ser::Error),

    #[error("state schema version {found} is newer than this binary supports ({max}); upgrade mxnode")]
    SchemaTooNew { found: u32, max: u32 },

    #[error("state schema version is 0 (uninitialised or pre-v1); refusing to load")]
    SchemaTooOld,

    #[error("could not acquire lock on {path}: {source}")]
    Lock {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// File-system-backed `state.toml` manager. Owns lock acquisition and atomic
/// writes. Callers acquire a `LockGuard` via `lock()` before any write; the
/// guard's `Drop` releases the kernel-level flock but **leaves the lock file
/// on disk** so subsequent locks contend on the same inode (avoids a TOCTOU
/// race where two processes would otherwise be able to lock different inodes
/// at the same path). See `LockGuard` docs.
pub struct StateStore {
    state_path: PathBuf,
    lock_path: PathBuf,
}

impl StateStore {
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        let state_dir = state_dir.as_ref().to_path_buf();
        Self {
            state_path: state_dir.join("state.toml"),
            lock_path: state_dir.join("state.toml.lock"),
        }
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn exists(&self) -> bool {
        self.state_path.exists()
    }

    /// Load `state.toml`. Returns `Ok(None)` if the file is missing — the
    /// caller decides whether to seed via `mxnode adopt` or rebuild.
    pub fn load(&self) -> Result<Option<State>, StateError> {
        if !self.state_path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&self.state_path).map_err(|e| StateError::Io {
            path: self.state_path.display().to_string(),
            source: e,
        })?;
        let state: State = toml::from_str(&raw).map_err(|e| StateError::Parse {
            path: self.state_path.display().to_string(),
            source: e,
        })?;
        if state.schema_version == 0 {
            // schema_version=0 means the file pre-dates the v1 introduction
            // or was written by something we don't support. We refuse rather
            // than guess at migration so future schema versions can rely on
            // 1..=CURRENT being the only legal range.
            return Err(StateError::SchemaTooOld);
        }
        if state.schema_version > SCHEMA_VERSION {
            return Err(StateError::SchemaTooNew {
                found: state.schema_version,
                max: SCHEMA_VERSION,
            });
        }
        Ok(Some(state))
    }

    /// Acquire an exclusive flock on `state.toml.lock`. The returned guard
    /// must outlive any subsequent `save` call; the guard's `Drop` releases
    /// the lock automatically.
    pub fn lock(&self) -> Result<LockGuard, StateError> {
        ensure_dir(self.lock_path.parent())?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|e| StateError::Lock {
                path: self.lock_path.display().to_string(),
                source: e,
            })?;
        file.try_lock_exclusive().map_err(|e| StateError::Lock {
            path: self.lock_path.display().to_string(),
            source: e,
        })?;
        Ok(LockGuard {
            file,
            path: self.lock_path.clone(),
        })
    }

    /// Atomic, durable write: serialize to a sibling tempfile, fsync the
    /// tempfile, rename over the target, then fsync the parent directory.
    ///
    /// The parent-dir fsync is mandatory for crash durability — without it
    /// the rename can be lost on power loss even after the file fsync
    /// returns. See `man 2 fsync` and POSIX FS guarantees.
    ///
    /// `_guard` proves the caller holds the flock; we don't inspect it but
    /// the parameter makes the API impossible to misuse without first
    /// calling `lock()`.
    pub fn save(&self, state: &State, _guard: &LockGuard) -> Result<(), StateError> {
        ensure_dir(self.state_path.parent())?;
        let mut stamped = state.clone();
        stamped.written_at = OffsetDateTime::now_utc();
        let body = toml::to_string_pretty(&stamped)?;

        let parent = self.state_path.parent().expect("state_path has a parent");
        let tmp = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| io_err(&self.state_path, e))?;
        tmp.as_file().set_len(0).map_err(|e| io_err(&self.state_path, e))?;
        let mut handle: &File = tmp.as_file();
        handle.write_all(body.as_bytes()).map_err(|e| io_err(&self.state_path, e))?;
        handle.sync_all().map_err(|e| io_err(&self.state_path, e))?;
        tmp.persist(&self.state_path).map_err(|e| StateError::Io {
            path: self.state_path.display().to_string(),
            source: e.error,
        })?;
        fsync_dir(parent)?;
        Ok(())
    }

    /// Save a timestamped backup before destructive operations (e.g. schema
    /// migration). Crash-durable: writes via a sibling tempfile, fsyncs the
    /// data, renames into place, then fsyncs the parent dir — same bar as
    /// `save()`. Returns the path created.
    pub fn backup(&self) -> Result<Option<PathBuf>, StateError> {
        if !self.state_path.exists() {
            return Ok(None);
        }
        let stamp = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|e| StateError::Io {
                path: self.state_path.display().to_string(),
                source: std::io::Error::new(std::io::ErrorKind::Other, e),
            })?;
        // Strip colons so the filename is portable across filesystems that
        // restrict them (notably FAT/exFAT external drives operators may
        // mount for offline backups).
        let safe_stamp = stamp.replace(':', "");
        let backup_path = self
            .state_path
            .with_file_name(format!("state.toml.bak.{safe_stamp}"));

        let parent = backup_path
            .parent()
            .expect("state_path is not a root, so backup_path has a parent");
        let body = fs::read(&self.state_path).map_err(|e| io_err(&self.state_path, e))?;

        let tmp = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| io_err(&backup_path, e))?;
        {
            let handle: &File = tmp.as_file();
            (&*handle)
                .write_all(&body)
                .map_err(|e| io_err(&backup_path, e))?;
            handle.sync_all().map_err(|e| io_err(&backup_path, e))?;
        }
        tmp.persist(&backup_path).map_err(|e| StateError::Io {
            path: backup_path.display().to_string(),
            source: e.error,
        })?;
        fsync_dir(parent)?;
        Ok(Some(backup_path))
    }
}

fn io_err(path: &Path, source: std::io::Error) -> StateError {
    StateError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn ensure_dir(dir: Option<&Path>) -> Result<(), StateError> {
    if let Some(d) = dir {
        if !d.exists() {
            fs::create_dir_all(d).map_err(|e| io_err(d, e))?;
        }
    }
    Ok(())
}

/// Open the directory and call `fsync` on it. On Unix this is required after
/// a rename for the rename itself to be durable. Linux + macOS both accept
/// `fsync` on a directory fd.
pub(crate) fn fsync_dir(dir: &Path) -> Result<(), StateError> {
    let f = File::open(dir).map_err(|e| io_err(dir, e))?;
    f.sync_all().map_err(|e| io_err(dir, e))?;
    Ok(())
}

/// RAII flock guard.
///
/// **The lock file is intentionally never deleted.** Removing it inside
/// `Drop` opens a TOCTOU race: between `unlock()` and `remove_file()`
/// another process can `open` + `flock` the same inode, and a third process
/// can `create` a fresh file at the path and lock that — both then hold
/// "the" lock simultaneously. Leaving the file in place lets the kernel
/// handle the contention via `flock(LOCK_EX)` on a single inode.
pub struct LockGuard {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Best-effort unlock; cannot propagate from `Drop`. Do NOT remove
        // the lock file — see the type docstring.
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxnode_core::State;
    use tempfile::TempDir;

    fn fresh_store(dir: &TempDir) -> StateStore {
        StateStore::new(dir.path())
    }

    #[test]
    fn load_returns_none_when_missing() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let guard = store.lock().unwrap();
        let s = State::empty("mxnode/test");
        store.save(&s, &guard).unwrap();
        drop(guard);
        let loaded = store.load().unwrap().expect("should exist");
        assert_eq!(loaded.schema_version, s.schema_version);
        assert_eq!(loaded.written_by, s.written_by);
    }

    #[test]
    fn second_lock_attempt_fails_while_held() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let _g1 = store.lock().unwrap();
        let g2 = store.lock();
        assert!(g2.is_err(), "second lock should fail while first is held");
    }

    #[test]
    fn lock_released_on_drop() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        {
            let _g = store.lock().unwrap();
        }
        // Now should be acquirable again.
        let _g = store.lock().unwrap();
    }

    /// Regression: `LockGuard::drop` previously removed the lock file, opening
    /// a TOCTOU race where two processes could acquire "the" lock against
    /// different inodes at the same path. The file must persist across drops.
    #[test]
    fn lock_file_persists_across_drops() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        {
            let _g = store.lock().unwrap();
        }
        assert!(
            store.lock_path().exists(),
            "lock file must remain on disk so subsequent locks contend on the same inode",
        );
    }

    /// Sanity check that the directory fsync helper works — it shouldn't
    /// error on a normal tempdir.
    #[test]
    fn fsync_dir_succeeds_for_existing_directory() {
        let dir = TempDir::new().unwrap();
        super::fsync_dir(dir.path()).expect("dir fsync succeeds on a tempdir");
    }

    #[test]
    fn schema_zero_rejected() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let state = State::empty("mxnode/test");
        let mut body = toml::to_string_pretty(&state).unwrap();
        body = body.replace(
            &format!("schema_version = {}", mxnode_core::SCHEMA_VERSION),
            "schema_version = 0",
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(store.state_path(), body).unwrap();
        let err = store.load().unwrap_err();
        assert!(matches!(err, StateError::SchemaTooOld), "got {err:?}");
    }

    #[test]
    fn schema_too_new_rejected() {
        // Build a real State so the TOML matches the schema, then bump the
        // version field in serialized form before writing.
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let state = State::empty("mxnode/future");
        let mut body = toml::to_string_pretty(&state).unwrap();
        // Replace the leading `schema_version = 1` line with a future version.
        body = body.replace(
            &format!("schema_version = {}", mxnode_core::SCHEMA_VERSION),
            "schema_version = 9999",
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(store.state_path(), body).unwrap();
        let err = store.load().unwrap_err();
        assert!(matches!(err, StateError::SchemaTooNew { .. }), "got {err:?}");
    }

    #[test]
    fn backup_creates_sibling_file() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let guard = store.lock().unwrap();
        store.save(&State::empty("mxnode/test"), &guard).unwrap();
        drop(guard);
        let backup = store.backup().unwrap().expect("backup should be created");
        assert!(backup.exists());
        assert!(backup.file_name().unwrap().to_str().unwrap().starts_with("state.toml.bak."));
    }
}
