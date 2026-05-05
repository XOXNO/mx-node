use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use mxnode_core::{HostState, MxnodeFile, SCHEMA_VERSION};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// File mode the unified `mxnode.toml` is held at — owner read/write
/// only. The file carries `[secrets].github_token`, so anything looser
/// is rejected on read with auto-`chmod` to repair.
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse mxnode.toml at {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },

    #[error("could not serialize file: {0}")]
    Serialize(#[from] toml::ser::Error),

    #[error(
        "schema version {found} is newer than this binary supports ({max}); upgrade mxnode"
    )]
    SchemaTooNew { found: u32, max: u32 },

    #[error("schema version is 0 (uninitialised or pre-v1); refusing to load")]
    SchemaTooOld,

    #[error("could not acquire lock on {path}: {source}")]
    Lock {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// File-system-backed `mxnode.toml` manager. Owns lock acquisition and
/// atomic writes on the unified document. Callers acquire a `LockGuard`
/// via `lock()`, then `load()` the current document, mutate the
/// sections they own, and `save()` the whole thing back.
///
/// On Unix the file is held at mode 0600 (owner-only). The loader
/// auto-`chmod`s anything looser and surfaces a one-line stderr notice.
pub struct StateStore {
    file_path: PathBuf,
    lock_path: PathBuf,
}

impl StateStore {
    /// `dir` is the operator's `<XDG_CONFIG_HOME>/mxnode` directory.
    /// The file lives at `<dir>/mxnode.toml`; the lock alongside.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        Self {
            file_path: dir.join("mxnode.toml"),
            lock_path: dir.join("mxnode.toml.lock"),
        }
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Backwards-compatible accessor used by error messages that still
    /// say `state.toml`. Returns the unified file's path.
    pub fn state_path(&self) -> &Path {
        &self.file_path
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// True iff the unified `mxnode.toml` exists on disk. Note that
    /// post-auto-init this is almost always true even before the
    /// operator has installed anything; for "is the host initialised"
    /// checks use [`Self::host_initialized`] instead.
    pub fn exists(&self) -> bool {
        self.file_path.exists()
    }

    /// True iff the file exists *and* the `[host]` section is
    /// populated (an install or at least one node has been recorded).
    /// This is what commands like `install` and `migrate-bash` use to
    /// guard against clobbering an existing install — the file alone
    /// is not evidence of an install since `mxnode config` writes
    /// operator-only sections too.
    pub fn host_initialized(&self) -> bool {
        match self.load_file() {
            Ok(Some(f)) => f.host.install.is_some() || !f.host.nodes.is_empty(),
            _ => false,
        }
    }

    /// Load the host inventory section (`[host]`). Returns `Ok(None)`
    /// when the file is missing **or** when the host section is empty
    /// (no install, no nodes) — the latter case preserves the
    /// pre-unified `state.toml` semantics where "file absent" meant
    /// "host not yet initialized". Callers seed defaults or run
    /// `mxnode adopt` / `migrate-bash` to populate it.
    ///
    /// Backwards-compatible with the pre-unified API: callers receive
    /// [`HostState`] (aliased as `State`) and continue to access
    /// `.install`, `.nodes`, etc. directly. The full file is available
    /// via [`Self::load_file`] when callers need the operator sections
    /// too.
    ///
    /// On Unix the file mode is checked: anything wider than 0600 is
    /// auto-tightened to 0600 with a one-line stderr notice.
    pub fn load(&self) -> Result<Option<HostState>, StateError> {
        match self.load_file()? {
            None => Ok(None),
            Some(f) if f.host.install.is_none() && f.host.nodes.is_empty() => Ok(None),
            Some(f) => Ok(Some(f.host)),
        }
    }

    /// Load the entire `MxnodeFile`. Used by callers that need the
    /// operator sections (e.g. update-cache writers, secret writers,
    /// `mxnode config show`) in addition to `[host]`.
    pub fn load_file(&self) -> Result<Option<MxnodeFile>, StateError> {
        if !self.file_path.exists() {
            return Ok(None);
        }
        ensure_mode_0600(&self.file_path)?;
        let raw = fs::read_to_string(&self.file_path).map_err(|e| StateError::Io {
            path: self.file_path.display().to_string(),
            source: e,
        })?;
        let file: MxnodeFile = toml::from_str(&raw).map_err(|e| StateError::Parse {
            path: self.file_path.display().to_string(),
            source: e,
        })?;
        if file.schema_version == 0 {
            return Err(StateError::SchemaTooOld);
        }
        if file.schema_version > SCHEMA_VERSION {
            return Err(StateError::SchemaTooNew {
                found: file.schema_version,
                max: SCHEMA_VERSION,
            });
        }
        Ok(Some(file))
    }

    /// Acquire an exclusive flock on `mxnode.toml.lock`. The returned
    /// guard must outlive any subsequent `save` call; the guard's
    /// `Drop` releases the lock automatically.
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
            _path: self.lock_path.clone(),
        })
    }

    /// Save the host inventory section (`[host]`). Reads the current
    /// file (or defaults), splices in the new `host`, writes the whole
    /// document atomically. Backwards-compatible with the pre-unified
    /// `state.toml` API.
    ///
    /// `_guard` proves the caller holds the flock; we don't inspect it
    /// but the parameter makes the API impossible to misuse without
    /// first calling `lock()`.
    pub fn save(&self, host: &HostState, guard: &LockGuard) -> Result<(), StateError> {
        let mut file = self.load_file()?.unwrap_or_default();
        file.host = host.clone();
        self.save_file(&file, guard)
    }

    /// Atomic, durable write of the full `MxnodeFile`. Always sets
    /// mode 0600 on the persisted file regardless of `umask`. Used by
    /// callers that mutate operator sections, secrets, or update-cache.
    pub fn save_file(&self, file: &MxnodeFile, _guard: &LockGuard) -> Result<(), StateError> {
        ensure_dir(self.file_path.parent())?;
        let mut stamped = file.clone();
        stamped.host.written_at = OffsetDateTime::now_utc();
        let body = toml::to_string_pretty(&stamped)?;

        let parent = self.file_path.parent().expect("file_path has a parent");
        let tmp =
            tempfile::NamedTempFile::new_in(parent).map_err(|e| io_err(&self.file_path, e))?;
        tmp.as_file()
            .set_len(0)
            .map_err(|e| io_err(&self.file_path, e))?;
        let mut handle: &File = tmp.as_file();
        handle
            .write_all(body.as_bytes())
            .map_err(|e| io_err(&self.file_path, e))?;
        handle.sync_all().map_err(|e| io_err(&self.file_path, e))?;
        // Set 0600 on the tempfile before persist so there's no
        // window where the live file is readable by anyone else.
        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(FILE_MODE);
            std::fs::set_permissions(tmp.path(), perms)
                .map_err(|e| io_err(tmp.path(), e))?;
        }
        tmp.persist(&self.file_path).map_err(|e| StateError::Io {
            path: self.file_path.display().to_string(),
            source: e.error,
        })?;
        fsync_dir(parent)?;
        Ok(())
    }

    /// Save a timestamped backup of the live file before destructive
    /// operations. Crash-durable: tempfile + rename + fsync the parent
    /// dir, mode 0600 on the result. Returns the path created.
    pub fn backup(&self) -> Result<Option<PathBuf>, StateError> {
        if !self.file_path.exists() {
            return Ok(None);
        }
        let stamp = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|e| StateError::Io {
                path: self.file_path.display().to_string(),
                source: std::io::Error::other(e),
            })?;
        let safe_stamp = stamp.replace(':', "");
        let backup_path = self
            .file_path
            .with_file_name(format!("mxnode.toml.bak.{safe_stamp}"));

        let parent = backup_path
            .parent()
            .expect("file_path is not a root, so backup_path has a parent");
        let body = fs::read(&self.file_path).map_err(|e| io_err(&self.file_path, e))?;

        let tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| io_err(&backup_path, e))?;
        {
            let handle: &File = tmp.as_file();
            (&*handle)
                .write_all(&body)
                .map_err(|e| io_err(&backup_path, e))?;
            handle.sync_all().map_err(|e| io_err(&backup_path, e))?;
        }
        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(FILE_MODE);
            std::fs::set_permissions(tmp.path(), perms)
                .map_err(|e| io_err(tmp.path(), e))?;
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

#[cfg(unix)]
fn ensure_mode_0600(path: &Path) -> Result<(), StateError> {
    let meta = fs::metadata(path).map_err(|e| io_err(path, e))?;
    let mode = meta.permissions().mode() & 0o777;
    // Anything readable / writable by group or others is too loose for
    // a file that holds [secrets].github_token. Tighten in place rather
    // than refusing — operators copying files from older versions or
    // misconfigured umasks shouldn't get blocked from running mxnode,
    // but we surface what we did so they notice.
    if mode & 0o077 != 0 {
        let perms = std::fs::Permissions::from_mode(FILE_MODE);
        fs::set_permissions(path, perms).map_err(|e| io_err(path, e))?;
        eprintln!(
            "→ tightened {} from mode {:o} to 600 (file may contain secrets)",
            path.display(),
            mode,
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_mode_0600(_path: &Path) -> Result<(), StateError> {
    Ok(())
}

/// Open the directory and call `fsync` on it. On Unix this is required
/// after a rename for the rename itself to be durable.
pub(crate) fn fsync_dir(dir: &Path) -> Result<(), StateError> {
    let f = File::open(dir).map_err(|e| io_err(dir, e))?;
    f.sync_all().map_err(|e| io_err(dir, e))?;
    Ok(())
}

/// RAII flock guard.
///
/// **The lock file is intentionally never deleted.** Removing it inside
/// `Drop` opens a TOCTOU race: between `unlock()` and `remove_file()`
/// another process can `open` + `flock` the same inode, and a third
/// process can `create` a fresh file at the path and lock that — both
/// then hold "the" lock simultaneously. Leaving the file in place lets
/// the kernel handle the contention via `flock(LOCK_EX)` on a single
/// inode.
pub struct LockGuard {
    file: File,
    /// Held only to keep the path printable in `Drop` diagnostics. The
    /// underscore prefix tells rustc the field exists for its side
    /// effect (RAII lifetime) rather than to be read directly.
    _path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Best-effort unlock; cannot propagate from `Drop`. Do NOT
        // remove the lock file — see the type docstring.
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxnode_core::MxnodeFile;
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
    fn save_host_then_load_round_trips() {
        use mxnode_core::{Environment, HostInstall, InstallKind};
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let guard = store.lock().unwrap();
        let mut host = HostState::empty("mxnode/test");
        // load() filters out hosts with no install + no nodes, so seed
        // one to get a non-None round-trip.
        host.install = Some(HostInstall::observed(
            InstallKind::Validators,
            Environment::Devnet,
            "multiversx",
            1,
        ));
        store.save(&host, &guard).unwrap();
        drop(guard);
        let loaded = store.load().unwrap().expect("should exist");
        assert_eq!(loaded.schema_version, host.schema_version);
        assert_eq!(loaded.written_by, host.written_by);
        assert!(loaded.install.is_some());
    }

    #[test]
    fn load_returns_none_for_uninitialised_host() {
        // A file with `[host]` defaulted (no install, no nodes) is
        // semantically equivalent to "host not initialised yet". The
        // pre-unified API returned None when state.toml was absent;
        // we preserve that contract here.
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let guard = store.lock().unwrap();
        store.save_file(&MxnodeFile::default(), &guard).unwrap();
        drop(guard);
        assert!(store.load().unwrap().is_none());
        // load_file still returns the document for callers that need
        // operator sections.
        assert!(store.load_file().unwrap().is_some());
    }

    #[test]
    fn save_file_round_trips_full_document() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let guard = store.lock().unwrap();
        let f = MxnodeFile::default();
        store.save_file(&f, &guard).unwrap();
        drop(guard);
        let loaded = store.load_file().unwrap().expect("should exist");
        assert_eq!(loaded.schema_version, f.schema_version);
        assert_eq!(loaded.network.github_org, f.network.github_org);
    }

    #[test]
    #[cfg(unix)]
    fn save_writes_file_at_mode_0600() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let guard = store.lock().unwrap();
        store.save_file(&MxnodeFile::default(), &guard).unwrap();
        drop(guard);
        let mode = fs::metadata(store.file_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "expected mode 600, got {mode:o}");
    }

    #[test]
    #[cfg(unix)]
    fn loose_mode_gets_tightened_on_load() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let guard = store.lock().unwrap();
        store.save_file(&MxnodeFile::default(), &guard).unwrap();
        drop(guard);
        // Operator (or rogue toolchain) widens the file.
        let perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(store.file_path(), perms).unwrap();
        // Load auto-fixes.
        let _ = store.load().unwrap();
        let mode = fs::metadata(store.file_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "loose mode should be auto-tightened");
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
        let _g = store.lock().unwrap();
    }

    /// Regression: `LockGuard::drop` previously removed the lock file,
    /// opening a TOCTOU race where two processes could acquire "the"
    /// lock against different inodes at the same path.
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

    #[test]
    fn fsync_dir_succeeds_for_existing_directory() {
        let dir = TempDir::new().unwrap();
        super::fsync_dir(dir.path()).expect("dir fsync succeeds on a tempdir");
    }

    #[test]
    fn schema_zero_rejected() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let f = MxnodeFile::default();
        let mut body = toml::to_string_pretty(&f).unwrap();
        body = body.replace(
            &format!("schema_version = {}", mxnode_core::SCHEMA_VERSION),
            "schema_version = 0",
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(store.file_path(), body).unwrap();
        // Tighten mode so the loader doesn't fail on perms before
        // reaching the schema check.
        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(FILE_MODE);
            std::fs::set_permissions(store.file_path(), perms).unwrap();
        }
        let err = store.load().unwrap_err();
        assert!(matches!(err, StateError::SchemaTooOld), "got {err:?}");
    }

    #[test]
    fn schema_too_new_rejected() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let f = MxnodeFile::default();
        let mut body = toml::to_string_pretty(&f).unwrap();
        body = body.replace(
            &format!("schema_version = {}", mxnode_core::SCHEMA_VERSION),
            "schema_version = 9999",
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(store.file_path(), body).unwrap();
        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(FILE_MODE);
            std::fs::set_permissions(store.file_path(), perms).unwrap();
        }
        let err = store.load().unwrap_err();
        assert!(
            matches!(err, StateError::SchemaTooNew { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn backup_creates_sibling_file() {
        let dir = TempDir::new().unwrap();
        let store = fresh_store(&dir);
        let guard = store.lock().unwrap();
        store.save_file(&MxnodeFile::default(), &guard).unwrap();
        drop(guard);
        let backup = store.backup().unwrap().expect("backup should be created");
        assert!(backup.exists());
        assert!(backup
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("mxnode.toml.bak."));
    }
}
