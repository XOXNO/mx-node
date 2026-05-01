//! Minimal GitHub release client used by the mxnode CLI.
//!
//! Phase 0 ships read paths only:
//!   - latest release of `mx-chain-{env}-config` and `mx-chain-go`
//!   - `/rate_limit` for `mxnode config validate --strict`
//!
//! ETag caching lands in Phase 2 alongside `mxnode upgrade`.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GithubError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("github returned {status}: {body}")]
    Status { status: u16, body: String },

    #[error("rate limit exhausted; resets at unix {resets_at} (used {used}/{limit}); add a token or wait")]
    RateLimited {
        used: u64,
        limit: u64,
        resets_at: u64,
    },

    #[error("io error: {0}")]
    Io(#[source] std::io::Error),
}

/// Bare-minimum release shape; full asset details land later.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimit {
    pub resources: RateLimitResources,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitResources {
    pub core: RateLimitBucket,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitBucket {
    pub limit: u64,
    pub used: u64,
    pub remaining: u64,
    pub reset: u64,
}

/// Configuration injected by the CLI on construction.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub api_base: String,
    pub token: Option<String>,
    pub user_agent: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.github.com".to_string(),
            token: None,
            user_agent: format!("mxnode/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

pub struct Client {
    cfg: ClientConfig,
    http: reqwest::Client,
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Result<Self, GithubError> {
        let http = reqwest::Client::builder()
            .user_agent(&cfg.user_agent)
            .build()?;
        Ok(Self { cfg, http })
    }

    /// `GET /repos/{org}/{repo}/releases/latest`.
    pub async fn latest_release(&self, org: &str, repo: &str) -> Result<Release, GithubError> {
        let url = format!(
            "{}/repos/{}/{}/releases/latest",
            self.cfg.api_base, org, repo
        );
        let resp = self.send(&url).await?;
        let release: Release = resp.json().await?;
        Ok(release)
    }

    /// `GET /rate_limit`.
    pub async fn rate_limit(&self) -> Result<RateLimit, GithubError> {
        let url = format!("{}/rate_limit", self.cfg.api_base);
        let resp = self.send(&url).await?;
        let rl: RateLimit = resp.json().await?;
        Ok(rl)
    }

    async fn send(&self, url: &str) -> Result<reqwest::Response, GithubError> {
        let mut req = self.http.get(url);
        if let Some(token) = &self.cfg.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(GithubError::Status { status, body });
        }
        Ok(resp)
    }

    /// Fetch the release for `org/repo @ tag`. Mirrors `latest_release`
    /// but pinned to the operator's chosen tag.
    pub async fn release_at_tag(
        &self,
        org: &str,
        repo: &str,
        tag: &str,
    ) -> Result<Release, GithubError> {
        let url = format!(
            "{}/repos/{}/{}/releases/tags/{}",
            self.cfg.api_base, org, repo, tag,
        );
        let resp = self.send(&url).await?;
        let release: Release = resp.json().await?;
        Ok(release)
    }

    /// Pick the release asset whose name matches `predicate`. Returns the
    /// "best" match using the supplied scorer (higher is better).
    /// Operators on hosts where multiple matching zips exist (the bash
    /// observed `multiversx_*_linux_amd64.zip` with several inner-version
    /// suffixes per release) get the deterministic pick: highest scorer.
    pub fn pick_asset<F, S>(release: &Release, predicate: F, scorer: S) -> Option<&ReleaseAsset>
    where
        F: Fn(&str) -> bool,
        S: Fn(&str) -> i64,
    {
        release
            .assets
            .iter()
            .filter(|a| predicate(&a.name))
            .max_by_key(|a| scorer(&a.name))
    }

    /// Download `asset.browser_download_url` to `dest`. Streams to disk
    /// so we don't hold the whole archive in memory. Returns the bytes
    /// length we wrote.
    pub async fn download_asset(
        &self,
        asset: &ReleaseAsset,
        dest: &Path,
    ) -> Result<u64, GithubError> {
        let mut req = self.http.get(&asset.browser_download_url);
        if let Some(token) = &self.cfg.token {
            req = req.bearer_auth(token);
        }
        let mut resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(GithubError::Status { status, body });
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(GithubError::Io)?;
        }
        let mut file = std::fs::File::create(dest).map_err(GithubError::Io)?;
        let mut total: u64 = 0;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk).map_err(GithubError::Io)?;
            total += chunk.len() as u64;
        }
        file.sync_all().map_err(GithubError::Io)?;
        Ok(total)
    }
}

/// Compute the sha256 of a file as a lowercase hex string.
pub fn sha256_file(path: &Path) -> Result<String, GithubError> {
    let mut file = std::fs::File::open(path).map_err(GithubError::Io)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(GithubError::Io)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Verify a downloaded artifact against the contents of a SHA256SUMS-style
/// asset. Lines look like `<hex>  <filename>` (two spaces, sha256sum
/// convention) or `<hex> *<filename>` (binary mode). Returns `Ok(true)`
/// on a match, `Ok(false)` when no entry matches `expected_filename`,
/// and `Err` on hash-mismatch.
pub fn verify_against_sums(
    sums_text: &str,
    expected_filename: &str,
    actual_path: &Path,
) -> Result<bool, GithubError> {
    let actual_hash = sha256_file(actual_path)?;
    for line in sums_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Either "<hash><spaces><name>" or "<hash><spaces>*<name>".
        let mut parts = line.splitn(2, char::is_whitespace);
        let hex = match parts.next() {
            Some(h) => h.trim(),
            None => continue,
        };
        let rest = parts.next().unwrap_or("").trim_start();
        let name = rest.trim_start_matches('*').trim();
        if name == expected_filename {
            if hex.eq_ignore_ascii_case(&actual_hash) {
                return Ok(true);
            }
            return Err(GithubError::Status {
                status: 0,
                body: format!(
                    "sha256 mismatch for {expected_filename}: expected {hex}, got {actual_hash}"
                ),
            });
        }
    }
    Ok(false)
}

/// Path returned by [`Client::download_asset`] is just a sibling helper
/// for callers that prefer building paths themselves.
pub fn asset_dest(dir: &Path, asset_name: &str) -> PathBuf {
    dir.join(asset_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sha256_file_matches_known_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"hello world").unwrap();
        f.sync_all().unwrap();
        let h = sha256_file(&path).unwrap();
        assert_eq!(
            h,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        );
    }

    #[test]
    fn verify_against_sums_accepts_matching_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.zip");
        std::fs::write(&path, b"hello world").unwrap();
        let sums = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9  blob.zip\n";
        assert!(verify_against_sums(sums, "blob.zip", &path).unwrap());
    }

    #[test]
    fn verify_against_sums_rejects_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.zip");
        std::fs::write(&path, b"hello world").unwrap();
        let sums = "0000000000000000000000000000000000000000000000000000000000000000  blob.zip\n";
        let err = verify_against_sums(sums, "blob.zip", &path).unwrap_err();
        match err {
            GithubError::Status { body, .. } => assert!(body.contains("sha256 mismatch")),
            other => panic!("expected mismatch error, got {other:?}"),
        }
    }

    #[test]
    fn verify_against_sums_returns_false_when_filename_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.zip");
        std::fs::write(&path, b"hello world").unwrap();
        let sums = "deadbeef  unrelated.tar\n";
        assert!(!verify_against_sums(sums, "blob.zip", &path).unwrap());
    }

    #[test]
    fn verify_against_sums_handles_binary_mode_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.zip");
        std::fs::write(&path, b"hello world").unwrap();
        // sha256sum -b puts a `*` before the filename.
        let sums = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9 *blob.zip\n";
        assert!(verify_against_sums(sums, "blob.zip", &path).unwrap());
    }

    #[test]
    fn pick_asset_chooses_by_scorer() {
        let release = Release {
            tag_name: "v1".to_string(),
            body: String::new(),
            assets: vec![
                ReleaseAsset {
                    name: "multiversx_v1.0_linux_amd64.zip".to_string(),
                    browser_download_url: String::new(),
                    size: 0,
                },
                ReleaseAsset {
                    name: "multiversx_v1.7_linux_amd64.zip".to_string(),
                    browser_download_url: String::new(),
                    size: 0,
                },
                ReleaseAsset {
                    name: "unrelated.txt".to_string(),
                    browser_download_url: String::new(),
                    size: 0,
                },
            ],
        };
        // Pick the highest by inner version (scorer just sums non-zero
        // digit chars to fake "newest").
        let picked = Client::pick_asset(
            &release,
            |n| n.starts_with("multiversx_") && n.ends_with("_linux_amd64.zip"),
            |n| n.chars().filter(|c| c.is_ascii_digit()).count() as i64,
        );
        assert_eq!(
            picked.map(|a| a.name.as_str()),
            Some("multiversx_v1.7_linux_amd64.zip"),
        );
    }
}
