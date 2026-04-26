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
//! subsequent `add-nodes` calls don't re-clone.

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
    if dest.exists() && std::fs::read_dir(&dest)
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

    let repo_url = format!("https://github.com/{}/mx-chain-{}-config.git", github_org, env.as_str());
    clone_shallow(&repo_url, tag, &dest)
        .await
        .map_err(|e| ConfigRepoError::Clone(e.to_string()))?;
    Ok(dest)
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
        let result = acquire_config_repo(tmp.path(), "myfork", env, &tag).await.unwrap();
        assert_eq!(result, cached);
    }
}
