//! Acquire the `mx-chain-{env}-config` source repo.
//!
//! Phase 3 install populates each `node-{i}/config/` directory by copying
//! files from the cloned config repo. We don't try to be clever — `git
//! clone --depth=1 --branch=<tag>` is what the bash does and what the
//! operator expects.
//!
//! Returns the path to the clone; the caller is responsible for copying
//! the contents into per-node working directories. The config repo is
//! cached per-tag under `{paths.binaries}/config-repos/<env>/<tag>` so
//! subsequent `install --add` calls don't re-clone.

use std::path::{Path, PathBuf};

use mxnode_build::clone_shallow;
use mxnode_core::{Environment, Tag};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigRepoError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("clone failed: {0}")]
    Clone(String),
}

/// Cache layout: `<binaries_root>/config-repos/<env>/<tag>/<repo-files>`.
pub fn cache_dir(binaries_root: &Path, env: Environment, tag: &Tag) -> PathBuf {
    binaries_root
        .join("config-repos")
        .join(env.as_str())
        .join(tag.as_str())
}

/// Acquire (or reuse) the config repo at `(env, tag)` for `github_org`.
/// Returns the absolute path to the cached clone. Idempotent: if the
/// cache already exists and is non-empty, the clone is skipped.
pub async fn acquire_config_repo(
    binaries_root: &Path,
    github_org: &str,
    env: Environment,
    tag: &Tag,
) -> Result<PathBuf, ConfigRepoError> {
    let dest = cache_dir(binaries_root, env, tag);
    // Cache hit: skip the clone. Treat any non-empty directory as
    // populated — operators on hand-edited hosts can prep their own
    // config dir under this path before running install.
    if dest.exists()
        && std::fs::read_dir(&dest)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false)
    {
        return Ok(dest);
    }
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|e| ConfigRepoError::Io {
            path: dest.display().to_string(),
            source: e,
        })?;
    }

    let repo_url = format!(
        "https://github.com/{}/mx-chain-{}-config.git",
        github_org,
        env.as_str()
    );
    clone_shallow(&repo_url, tag, &dest)
        .await
        .map_err(|e| ConfigRepoError::Clone(e.to_string()))?;
    Ok(dest)
}

/// Cache layout for the proxy source repo: `<binaries_root>/proxy-repos/<tag>/<repo-files>`.
/// Independent of `env` because `mx-chain-proxy-go` is the same repo across networks.
pub fn proxy_cache_dir(binaries_root: &Path, tag: &Tag) -> PathBuf {
    binaries_root.join("proxy-repos").join(tag.as_str())
}

/// Acquire (or reuse) the `mx-chain-proxy-go` source repo at `tag`. The
/// caller copies `cmd/proxy/config/*` from the returned path into the
/// proxy working directory.
pub async fn acquire_proxy_repo(
    binaries_root: &Path,
    github_org: &str,
    tag: &Tag,
) -> Result<PathBuf, ConfigRepoError> {
    let dest = proxy_cache_dir(binaries_root, tag);
    if dest.exists()
        && std::fs::read_dir(&dest)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false)
    {
        return Ok(dest);
    }
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|e| ConfigRepoError::Io {
            path: dest.display().to_string(),
            source: e,
        })?;
    }
    let repo_url = format!("https://github.com/{github_org}/mx-chain-proxy-go.git");
    clone_shallow(&repo_url, tag, &dest)
        .await
        .map_err(|e| ConfigRepoError::Clone(e.to_string()))?;
    Ok(dest)
}

/// Read the upstream `goVersion` file from a cloned config repo. Returns
/// the trimmed version string with any leading `go` prefix stripped, so
/// callers can pass the result directly to `mxnode_toolchain::ensure_go`
/// / `bootstrap`.
///
/// Bash (`update_go_version_from_config`) decodes a base64-wrapped value
/// in some upstream forks. We don't see that in the active mainnet/devnet
/// repos, so we keep this minimal and add base64 handling only when a
/// real config tag emits one.
pub fn read_go_version_from_repo(repo_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(repo_dir.join("goVersion")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.trim_start_matches("go").to_string())
}

/// Read the `binaryVersion` file at the root of a cloned config repo.
/// MultiversX config releases (`mx-chain-{env}-config`) declare which
/// `mx-chain-go` tag they pair with via this file — the bash flow
/// (`functions.cfg:git_clone`) treats it as authoritative and clones
/// the node repo at the tag it returns.
///
/// Some upstream tags prefix the value with `tags/` (the GitHub refs
/// shape); the prefix is stripped so callers receive a plain tag like
/// `v1.7.99`.
pub fn read_binary_version_from_repo(repo_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(repo_dir.join("binaryVersion")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.trim_start_matches("tags/").to_string())
}

/// Read the `proxyVersion` file at the root of a cloned config repo.
/// Mirrors `binaryVersion`: when the config release ships a
/// `proxyVersion` file, bash (`functions.cfg:git_clone_proxy`) uses it
/// to pin the proxy tag. Networks without a paired proxy release omit
/// the file — callers fall through to GitHub-latest or an override.
pub fn read_proxy_version_from_repo(repo_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(repo_dir.join("proxyVersion")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.trim_start_matches("tags/").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn cache_dir_layout_is_per_env_and_tag() {
        let root = PathBuf::from("/srv/mxnode/binaries");
        let tag = Tag::from_str("v1.7.13").unwrap();
        let dir = cache_dir(&root, Environment::Mainnet, &tag);
        assert_eq!(
            dir,
            PathBuf::from("/srv/mxnode/binaries/config-repos/mainnet/v1.7.13"),
        );
    }

    #[tokio::test]
    async fn acquire_returns_cache_when_dir_already_populated() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Environment::Testnet;
        let tag = Tag::from_str("v0.0.0").unwrap();
        let cached = cache_dir(tmp.path(), env, &tag);
        std::fs::create_dir_all(&cached).unwrap();
        std::fs::write(cached.join("placeholder"), b"already cloned").unwrap();
        let result = acquire_config_repo(tmp.path(), "myfork", env, &tag)
            .await
            .unwrap();
        assert_eq!(result, cached);
    }

    #[tokio::test]
    async fn proxy_repo_returns_cache_when_dir_already_populated() {
        let tmp = tempfile::tempdir().unwrap();
        let tag = Tag::from_str("v1.1.51").unwrap();
        let cached = proxy_cache_dir(tmp.path(), &tag);
        std::fs::create_dir_all(&cached).unwrap();
        std::fs::write(cached.join("placeholder"), b"already cloned").unwrap();
        let result = acquire_proxy_repo(tmp.path(), "myfork", &tag)
            .await
            .unwrap();
        assert_eq!(result, cached);
    }

    #[test]
    fn proxy_cache_dir_layout_is_per_tag() {
        let root = std::path::PathBuf::from("/srv/mxnode/binaries");
        let tag = Tag::from_str("v1.1.51").unwrap();
        let dir = proxy_cache_dir(&root, &tag);
        assert_eq!(
            dir,
            std::path::PathBuf::from("/srv/mxnode/binaries/proxy-repos/v1.1.51")
        );
    }

    #[test]
    fn read_go_version_from_repo_handles_plain_text() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("goVersion"), "1.23.4\n").unwrap();
        let v = read_go_version_from_repo(tmp.path()).unwrap();
        assert_eq!(v, "1.23.4");
    }

    #[test]
    fn read_go_version_from_repo_strips_leading_go_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("goVersion"), "go1.23.4").unwrap();
        let v = read_go_version_from_repo(tmp.path()).unwrap();
        assert_eq!(v, "1.23.4");
    }

    #[test]
    fn read_go_version_from_repo_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_go_version_from_repo(tmp.path()).is_none());
    }

    #[test]
    fn read_binary_version_from_repo_handles_plain_tag() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("binaryVersion"), "v1.7.99\n").unwrap();
        assert_eq!(
            read_binary_version_from_repo(tmp.path()).unwrap(),
            "v1.7.99"
        );
    }

    #[test]
    fn read_binary_version_from_repo_strips_tags_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("binaryVersion"), "tags/v1.7.99").unwrap();
        assert_eq!(
            read_binary_version_from_repo(tmp.path()).unwrap(),
            "v1.7.99"
        );
    }

    #[test]
    fn read_binary_version_from_repo_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_binary_version_from_repo(tmp.path()).is_none());
    }

    #[test]
    fn read_binary_version_from_repo_returns_none_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("binaryVersion"), "   \n").unwrap();
        assert!(read_binary_version_from_repo(tmp.path()).is_none());
    }

    #[test]
    fn read_proxy_version_from_repo_handles_plain_tag() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("proxyVersion"), "v1.1.66").unwrap();
        assert_eq!(
            read_proxy_version_from_repo(tmp.path()).unwrap(),
            "v1.1.66"
        );
    }

    #[test]
    fn read_proxy_version_from_repo_strips_tags_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("proxyVersion"), "tags/v1.1.66\n").unwrap();
        assert_eq!(
            read_proxy_version_from_repo(tmp.path()).unwrap(),
            "v1.1.66"
        );
    }

    #[test]
    fn read_proxy_version_from_repo_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_proxy_version_from_repo(tmp.path()).is_none());
    }
}
