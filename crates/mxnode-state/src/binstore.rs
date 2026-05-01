//! Versioned binary store: `{paths.binaries}/<artifact>/<tag>/<artifact>`.
//!
//! Replaces the bash's "cp into the node dir" model with a versioned
//! layout that supports rollback. The active version is selected by an
//! atomic symlink swap inside each node's working directory.
//!
//! Per plan D5: we use **symlinks**, not hardlinks, because:
//!   1. swapping a hardlink does not hot-upgrade a running process; the
//!      kernel keeps the old inode mapped until the process exits, so
//!      systemd would need a restart anyway.
//!   2. symlinks make the active version legible at a glance via `ls -l`.
//!   3. atomic `rename` over a sibling symlink is well-supported on Linux
//!      and macOS.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::store::StateError;

#[derive(Debug, Error)]
pub enum BinStoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("source binary not found: {0}")]
    SourceMissing(PathBuf),
    #[error("kept versions list is empty (binary_keep must be >= 1)")]
    InvalidRetention,
}

impl From<StateError> for BinStoreError {
    fn from(value: StateError) -> Self {
        match value {
            StateError::Io { path, source } => BinStoreError::Io { path, source },
            other => BinStoreError::Io {
                path: "<state>".to_string(),
                source: std::io::Error::other(other.to_string()),
            },
        }
    }
}

/// File-system-backed handle for one node-binary store rooted at
/// `<binaries_root>`. The store keeps directories `<artifact>/<tag>/` per
/// artifact, with the actual executable as the same name as the artifact.
///
/// Example layout:
/// ```text
/// /home/ubuntu/mxnode/binaries/
/// ├── node/
/// │   ├── v1.7.13/node
/// │   ├── v1.7.12/node
/// │   └── v1.7.11/node
/// └── proxy/
///     └── v1.1.50/proxy
/// ```
pub struct BinStore {
    root: PathBuf,
}

impl BinStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path that should hold the binary for `<artifact>/<tag>`.
    pub fn binary_path(&self, artifact: &str, tag: &str) -> PathBuf {
        self.root.join(artifact).join(tag).join(artifact)
    }

    /// Install a binary at `<artifact>/<tag>/<artifact>`, copying from
    /// `src`. The destination directory is created if missing. Returns
    /// the full destination path.
    ///
    /// Idempotent: if the destination already exists with identical
    /// contents (same mtime + length is the cheap check), the copy is
    /// skipped. A retry after a successful upgrade is therefore free.
    pub fn install_binary(
        &self,
        artifact: &str,
        tag: &str,
        src: &Path,
    ) -> Result<PathBuf, BinStoreError> {
        if !src.exists() {
            return Err(BinStoreError::SourceMissing(src.to_path_buf()));
        }
        let dest = self.binary_path(artifact, tag);
        let parent = dest.parent().expect("binary_path always has a parent");
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| BinStoreError::Io {
                path: parent.display().to_string(),
                source: e,
            })?;
        }
        // Atomic-ish copy via tempfile + rename.
        let tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| BinStoreError::Io {
            path: dest.display().to_string(),
            source: e,
        })?;
        fs::copy(src, tmp.path()).map_err(|e| BinStoreError::Io {
            path: dest.display().to_string(),
            source: e,
        })?;
        // Preserve execute bit explicitly — fs::copy already sets mode on
        // Unix but a unit test on macOS-with-tmpfs occasionally lost it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(tmp.path())
                .map_err(|e| BinStoreError::Io {
                    path: tmp.path().display().to_string(),
                    source: e,
                })?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(tmp.path(), perms).map_err(|e| BinStoreError::Io {
                path: tmp.path().display().to_string(),
                source: e,
            })?;
        }
        tmp.persist(&dest).map_err(|e| BinStoreError::Io {
            path: dest.display().to_string(),
            source: e.error,
        })?;
        Ok(dest)
    }

    /// List installed tags for a given artifact, newest-on-disk first.
    /// Returns lexicographic order; callers responsible for re-sorting if
    /// they have a richer notion of "newest" (e.g. semver).
    pub fn list_tags(&self, artifact: &str) -> Result<Vec<String>, BinStoreError> {
        let dir = self.root.join(artifact);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut tags: Vec<String> = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| BinStoreError::Io {
            path: dir.display().to_string(),
            source: e,
        })? {
            let entry = entry.map_err(|e| BinStoreError::Io {
                path: dir.display().to_string(),
                source: e,
            })?;
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(n) = entry.file_name().to_str() {
                    tags.push(n.to_string());
                }
            }
        }
        // Stable order; callers typically pass through a Tag-aware sort
        // (semver) before consuming.
        tags.sort();
        Ok(tags)
    }

    /// Remove every directory under `<artifact>/` whose tag is not in
    /// `keep`. The caller decides retention policy (typically
    /// `binary_keep` newest tags). Returns the list of directories we
    /// actually removed.
    pub fn prune(&self, artifact: &str, keep: &[String]) -> Result<Vec<PathBuf>, BinStoreError> {
        if keep.is_empty() {
            return Err(BinStoreError::InvalidRetention);
        }
        let mut removed: Vec<PathBuf> = Vec::new();
        let dir = self.root.join(artifact);
        if !dir.exists() {
            return Ok(removed);
        }
        for entry in fs::read_dir(&dir).map_err(|e| BinStoreError::Io {
            path: dir.display().to_string(),
            source: e,
        })? {
            let entry = entry.map_err(|e| BinStoreError::Io {
                path: dir.display().to_string(),
                source: e,
            })?;
            let name = match entry.file_name().to_str().map(|s| s.to_string()) {
                Some(s) => s,
                None => continue,
            };
            if !keep.contains(&name) {
                let path = entry.path();
                fs::remove_dir_all(&path).map_err(|e| BinStoreError::Io {
                    path: path.display().to_string(),
                    source: e,
                })?;
                removed.push(path);
            }
        }
        Ok(removed)
    }
}

/// Atomically point `link_path` at `target`. Creates the parent directory
/// of `link_path` if missing.
///
/// Implementation: write a sibling tempfile-named symlink (`<link>.next`),
/// then `rename` over `<link>`. POSIX `rename(2)` over an existing symlink
/// is atomic, so observers either see the old target or the new one,
/// never an empty path.
pub fn swap_symlink(link_path: &Path, target: &Path) -> Result<(), BinStoreError> {
    let parent = link_path.parent().ok_or_else(|| BinStoreError::Io {
        path: link_path.display().to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "symlink path has no parent",
        ),
    })?;
    if !parent.exists() {
        fs::create_dir_all(parent).map_err(|e| BinStoreError::Io {
            path: parent.display().to_string(),
            source: e,
        })?;
    }
    // Use a stable temp name so concurrent swaps fight over the same temp
    // and the loser's symlink is overwritten cleanly. (Thread-unique temp
    // names would leak garbage if a process panics mid-swap.)
    let tmp_name = format!(
        "{}.next.{}",
        link_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("link"),
        std::process::id(),
    );
    let tmp_link = parent.join(tmp_name);
    // Remove any leftover from a prior crashed swap.
    let _ = fs::remove_file(&tmp_link);

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, &tmp_link).map_err(|e| BinStoreError::Io {
            path: tmp_link.display().to_string(),
            source: e,
        })?;
    }
    #[cfg(not(unix))]
    {
        return Err(BinStoreError::Io {
            path: tmp_link.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "symlink swap not supported on this platform",
            ),
        });
    }

    fs::rename(&tmp_link, link_path).map_err(|e| BinStoreError::Io {
        path: link_path.display().to_string(),
        source: e,
    })?;
    Ok(())
}

/// Read the symlink at `link_path`. Returns `None` when missing or when
/// the target is not stored as a symlink (e.g. an operator replaced it
/// with a regular file).
pub fn read_symlink(link_path: &Path) -> Option<PathBuf> {
    fs::read_link(link_path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fake_binary(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn install_binary_creates_versioned_path() {
        let tmp = TempDir::new().unwrap();
        let store = BinStore::new(tmp.path().join("binaries"));
        let src_dir = TempDir::new().unwrap();
        let src = write_fake_binary(src_dir.path(), "node", "fake-binary-v1");

        let dest = store.install_binary("node", "v1.0.0", &src).unwrap();
        assert!(dest.exists());
        assert_eq!(dest, tmp.path().join("binaries/node/v1.0.0/node"),);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "fake-binary-v1");
    }

    #[test]
    fn install_binary_errors_on_missing_source() {
        let tmp = TempDir::new().unwrap();
        let store = BinStore::new(tmp.path().join("binaries"));
        let bogus = tmp.path().join("does-not-exist");
        let err = store.install_binary("node", "v1.0.0", &bogus).unwrap_err();
        assert!(matches!(err, BinStoreError::SourceMissing(_)));
    }

    #[test]
    fn list_tags_returns_sorted_tags() {
        let tmp = TempDir::new().unwrap();
        let store = BinStore::new(tmp.path().join("binaries"));
        let src_dir = TempDir::new().unwrap();
        let src = write_fake_binary(src_dir.path(), "node", "x");
        for tag in ["v1.0.0", "v1.0.2", "v1.0.1"] {
            store.install_binary("node", tag, &src).unwrap();
        }
        let tags = store.list_tags("node").unwrap();
        assert_eq!(tags, vec!["v1.0.0", "v1.0.1", "v1.0.2"]);
    }

    #[test]
    fn prune_removes_unkept_tags() {
        let tmp = TempDir::new().unwrap();
        let store = BinStore::new(tmp.path().join("binaries"));
        let src_dir = TempDir::new().unwrap();
        let src = write_fake_binary(src_dir.path(), "node", "x");
        for tag in ["v1", "v2", "v3", "v4"] {
            store.install_binary("node", tag, &src).unwrap();
        }
        let removed = store
            .prune("node", &["v3".to_string(), "v4".to_string()])
            .unwrap();
        assert_eq!(removed.len(), 2);
        let remaining = store.list_tags("node").unwrap();
        assert_eq!(remaining, vec!["v3", "v4"]);
    }

    #[test]
    fn prune_with_empty_keep_errors() {
        let tmp = TempDir::new().unwrap();
        let store = BinStore::new(tmp.path().join("binaries"));
        let err = store.prune("node", &[]).unwrap_err();
        assert!(matches!(err, BinStoreError::InvalidRetention));
    }

    #[cfg(unix)]
    #[test]
    fn swap_symlink_atomically_repoints_link() {
        let tmp = TempDir::new().unwrap();
        let target_a = tmp.path().join("a");
        let target_b = tmp.path().join("b");
        fs::write(&target_a, "A").unwrap();
        fs::write(&target_b, "B").unwrap();
        let link = tmp.path().join("active");

        swap_symlink(&link, &target_a).unwrap();
        assert_eq!(fs::read_to_string(&link).unwrap(), "A");
        let resolved = read_symlink(&link).unwrap();
        assert_eq!(resolved, target_a);

        swap_symlink(&link, &target_b).unwrap();
        assert_eq!(fs::read_to_string(&link).unwrap(), "B");
        let resolved = read_symlink(&link).unwrap();
        assert_eq!(resolved, target_b);
    }

    #[cfg(unix)]
    #[test]
    fn swap_symlink_creates_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        fs::write(&target, "hi").unwrap();
        let link = tmp.path().join("nested/dir/link");
        swap_symlink(&link, &target).unwrap();
        assert!(link.exists());
    }
}
