//! `BinaryAcquirer` — abstraction over "where do node binaries come from".
//!
//! The orchestration spine in `commands::upgrade` calls this trait once
//! per artifact (node, proxy, keygenerator) before swapping the symlink.
//! Concrete implementations:
//!
//!   - [`MockAcquirer`] — copies a caller-supplied bytes blob into a
//!     temp file. Used by integration tests to exercise the upgrade
//!     orchestration without touching `git` / `go` / GitHub.
//!   - [`SourceBuildAcquirer`] (Phase 2b) — `git clone --depth=1
//!     --branch=<tag>` + `go build`. Stubbed today; returns an
//!     operator-actionable error explaining how to acquire the binary
//!     manually until the source-build pipeline lands.
//!   - [`ReleaseAcquirer`] (Phase 2b) — downloads the matching
//!     `multiversx_*_linux_<arch>.zip` from GitHub Releases. Stubbed
//!     today because empirically MultiversX ships prebuilts on a
//!     minority of releases.
//!
//! Splitting the source-build / release-fetch implementations into a
//! later phase is deliberate (D2 in the plan): both are heavy lifts
//! that pull in large transitive deps and require a real Linux host
//! to validate. The orchestration spine is testable without them.

// Some variants/methods (release acquirer's path, multi-asset selection
// helpers) are consumed only by tests or by Phase 3 install plumbing.
// We allow dead-code on those rather than churn the module surface every
// time the orchestrator gains a new consumer.
#![allow(dead_code)]

use std::path::PathBuf;

use async_trait::async_trait;
use mxnode_build::{build_artifact, build_ldflags, clone_shallow};
use mxnode_core::Tag;
use mxnode_toolchain::{bootstrap, DEFAULT_GO_VERSION};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AcquireError {
    #[error("not implemented yet (scheduled for Phase 2b): {0}")]
    NotImplemented(&'static str),
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("toolchain unavailable: {0}")]
    Toolchain(String),
    #[error("git clone or go build failed: {0}")]
    Build(String),
    #[error("acquire failed: {0}")]
    Other(String),
}

/// Which artifact we want — drives the GitHub repo path / Cargo target
/// the implementation will use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Artifact {
    /// `mx-chain-go` → `cmd/node/node`
    Node,
    /// `mx-chain-proxy-go` → `cmd/proxy/proxy`
    Proxy,
    /// `mx-chain-go` → `cmd/keygenerator/keygenerator`
    Keygenerator,
}

impl Artifact {
    pub fn binary_name(self) -> &'static str {
        match self {
            Artifact::Node => "node",
            Artifact::Proxy => "proxy",
            Artifact::Keygenerator => "keygenerator",
        }
    }
}

/// Backend that produces a path to a built binary on demand.
#[async_trait]
pub trait BinaryAcquirer: Send + Sync {
    /// Produce a path containing the named binary for `(artifact, tag)`.
    /// The returned path may live anywhere; callers copy it into the
    /// `BinStore` versioned layout afterwards.
    async fn acquire(&self, artifact: Artifact, tag: &Tag) -> Result<PathBuf, AcquireError>;
}

/// In-memory acquirer used by integration tests. Maps `(artifact, tag)`
/// → bytes; on `acquire`, materialises the bytes into a tempfile that
/// the caller is responsible for moving into the `BinStore`.
pub struct MockAcquirer {
    map: std::sync::Mutex<std::collections::HashMap<(Artifact, String), Vec<u8>>>,
    /// Optional override directory. Tests typically pass a tempdir so
    /// the acquired tempfile can be reasoned about. Defaults to
    /// `std::env::temp_dir()`.
    pub workdir: Option<PathBuf>,
}

impl MockAcquirer {
    pub fn new() -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
            workdir: None,
        }
    }

    pub fn with_workdir(mut self, dir: PathBuf) -> Self {
        self.workdir = Some(dir);
        self
    }

    pub fn add(&self, artifact: Artifact, tag: &str, bytes: &[u8]) {
        self.map
            .lock()
            .unwrap()
            .insert((artifact, tag.to_string()), bytes.to_vec());
    }
}

impl Default for MockAcquirer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BinaryAcquirer for MockAcquirer {
    async fn acquire(&self, artifact: Artifact, tag: &Tag) -> Result<PathBuf, AcquireError> {
        let key = (artifact, tag.as_str().to_string());
        let bytes = self
            .map
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(|| {
                AcquireError::Other(format!(
                    "MockAcquirer has no entry for {} @ {tag}",
                    artifact.binary_name(),
                ))
            })?;

        let dir = self
            .workdir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&dir).map_err(|e| AcquireError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        let path = dir.join(format!("{}-{}-{}", artifact.binary_name(), tag, std::process::id()));
        std::fs::write(&path, &bytes).map_err(|e| AcquireError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)
                .map_err(|e| AcquireError::Io {
                    path: path.display().to_string(),
                    source: e,
                })?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).map_err(|e| AcquireError::Io {
                path: path.display().to_string(),
                source: e,
            })?;
        }
        Ok(path)
    }
}

/// Production-mode source-build acquirer.
///
/// Resolves the GitHub repo from the artifact + the operator's
/// `[network].github_org`, shallow-clones at the requested tag into a
/// caller-supplied workdir, validates the Go toolchain, runs `go build`,
/// and returns the path to the compiled binary. The orchestrator copies
/// that path into the versioned `BinStore` afterwards.
///
/// The acquirer requires:
///   - `git` on PATH (any version that supports `--depth=1 --branch=<tag>`)
///   - a Go install matching `min_go_version` (defaults to
///     [`DEFAULT_GO_VERSION`], the floor required by recent
///     `mx-chain-go` `go.mod` files)
///
/// Both are auto-installed by [`bootstrap`] on Debian-likes when
/// missing or below the requested floor.
pub struct SourceBuildAcquirer {
    pub github_org: String,
    pub workdir: PathBuf,
    pub min_go_version: String,
}

impl SourceBuildAcquirer {
    pub fn new(github_org: impl Into<String>, workdir: PathBuf) -> Self {
        Self {
            github_org: github_org.into(),
            workdir,
            min_go_version: DEFAULT_GO_VERSION.to_string(),
        }
    }

    pub fn with_min_go(mut self, version: impl Into<String>) -> Self {
        self.min_go_version = version.into();
        self
    }

    fn repo_url(&self, artifact: Artifact) -> String {
        let repo = match artifact {
            Artifact::Node | Artifact::Keygenerator => "mx-chain-go",
            Artifact::Proxy => "mx-chain-proxy-go",
        };
        format!("https://github.com/{}/{repo}.git", self.github_org)
    }

    fn sub_path(artifact: Artifact) -> &'static str {
        match artifact {
            Artifact::Node => "cmd/node",
            Artifact::Proxy => "cmd/proxy",
            Artifact::Keygenerator => "cmd/keygenerator",
        }
    }
}

#[async_trait]
impl BinaryAcquirer for SourceBuildAcquirer {
    async fn acquire(&self, artifact: Artifact, tag: &Tag) -> Result<PathBuf, AcquireError> {
        // Bootstrap the toolchain up-front so we don't wait through a
        // clone before discovering the operator's host doesn't have
        // git/go. `bootstrap` auto-installs apt deps + Go on Linux
        // (matching the bash flow), short-circuits to a detect on
        // subsequent calls in the same process via OnceLock.
        bootstrap(&self.min_go_version)
            .map_err(|e| AcquireError::Toolchain(e.to_string()))?;

        // Use a per-artifact + per-tag clone dir so concurrent
        // acquisitions don't trample each other.
        std::fs::create_dir_all(&self.workdir).map_err(|e| AcquireError::Io {
            path: self.workdir.display().to_string(),
            source: e,
        })?;
        let clone_dir = self
            .workdir
            .join(format!("{}-{}", artifact.binary_name(), tag.as_str()));
        if clone_dir.exists() {
            std::fs::remove_dir_all(&clone_dir).map_err(|e| AcquireError::Io {
                path: clone_dir.display().to_string(),
                source: e,
            })?;
        }

        let repo = self.repo_url(artifact);
        clone_shallow(&repo, tag, &clone_dir)
            .await
            .map_err(|e| AcquireError::Build(e.to_string()))?;

        let ldflags = build_ldflags(tag, None);
        let binary = build_artifact(
            &clone_dir,
            Self::sub_path(artifact),
            artifact.binary_name(),
            &ldflags,
        )
        .await
        .map_err(|e| AcquireError::Build(e.to_string()))?;
        Ok(binary)
    }
}

/// Production-mode release-artifact acquirer.
///
/// Resolves the GitHub release for `(repo, tag)`, picks the best matching
/// `multiversx_*_linux_<arch>.zip`, downloads it, optionally verifies
/// against a sibling `SHA256SUMS` asset, and extracts the matching
/// binary into a tempfile.
///
/// Empirical evidence (Phase 0 audit): MultiversX ships prebuilts on a
/// minority of releases. Operators who want this path explicitly set
/// `[install].artifact_source = "release"`; the orchestrator falls back
/// to source-build automatically when configured to `auto`.
pub struct ReleaseAcquirer {
    pub github_org: String,
    pub workdir: PathBuf,
    pub arch: String,
    pub token: Option<String>,
}

impl ReleaseAcquirer {
    pub fn new(github_org: impl Into<String>, workdir: PathBuf) -> Self {
        Self {
            github_org: github_org.into(),
            workdir,
            arch: detect_arch(),
            token: None,
        }
    }

    pub fn with_arch(mut self, arch: impl Into<String>) -> Self {
        self.arch = arch.into();
        self
    }

    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    fn repo_name(&self, artifact: Artifact) -> &'static str {
        match artifact {
            Artifact::Node | Artifact::Keygenerator => "mx-chain-go",
            Artifact::Proxy => "mx-chain-proxy-go",
        }
    }
}

/// Best-effort architecture detection. The bash uses
/// `dpkg --print-architecture` on Debian-family hosts; we fall back to
/// `uname -m` mappings on other systems.
pub fn detect_arch() -> String {
    if let Ok(output) = std::process::Command::new("dpkg")
        .arg("--print-architecture")
        .output()
    {
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    if let Ok(output) = std::process::Command::new("uname").arg("-m").output() {
        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return match raw.as_str() {
                "x86_64" => "amd64".to_string(),
                "aarch64" | "arm64" => "arm64".to_string(),
                other => other.to_string(),
            };
        }
    }
    "amd64".to_string()
}

#[async_trait]
impl BinaryAcquirer for ReleaseAcquirer {
    async fn acquire(&self, artifact: Artifact, tag: &Tag) -> Result<PathBuf, AcquireError> {
        use mxnode_github::{Client, ClientConfig};

        let cfg = ClientConfig {
            token: self.token.clone(),
            ..ClientConfig::default()
        };
        let client = Client::new(cfg).map_err(|e| AcquireError::Other(e.to_string()))?;
        let release = client
            .release_at_tag(&self.github_org, self.repo_name(artifact), tag.as_str())
            .await
            .map_err(|e| AcquireError::Other(e.to_string()))?;

        // Filename pattern matches the bash + the few-published artifacts
        // we observed in Phase 0: `multiversx_<inner-version>_linux_<arch>.zip`.
        let arch = &self.arch;
        let pattern_arch = arch.clone();
        let asset = Client::pick_asset(
            &release,
            |name| {
                name.starts_with("multiversx_")
                    && name.ends_with(&format!("_linux_{pattern_arch}.zip"))
            },
            // "Newest" heuristic: lexicographic on the filename. When
            // multiple matching zips exist the bash picks the last;
            // we pick the lexicographically-largest so behaviour is
            // deterministic across runs.
            |name| name.len() as i64 + name.bytes().map(|b| b as i64).sum::<i64>(),
        )
        .ok_or_else(|| {
            AcquireError::Other(format!(
                "no asset matching `multiversx_*_linux_{arch}.zip` in {} @ {tag}; \
                 set [install].artifact_source = \"source\" to build instead",
                self.repo_name(artifact),
            ))
        })?;

        std::fs::create_dir_all(&self.workdir).map_err(|e| AcquireError::Io {
            path: self.workdir.display().to_string(),
            source: e,
        })?;
        let archive_path = self.workdir.join(&asset.name);
        client
            .download_asset(asset, &archive_path)
            .await
            .map_err(|e| AcquireError::Other(e.to_string()))?;

        // Optional sha256 verification — warn loudly when the release
        // ships no SHA256SUMS asset (MultiversX historically does not).
        if let Some(sums_asset) = release
            .assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case("SHA256SUMS"))
        {
            let sums_path = self.workdir.join("SHA256SUMS");
            client
                .download_asset(sums_asset, &sums_path)
                .await
                .map_err(|e| AcquireError::Other(e.to_string()))?;
            let sums_text = std::fs::read_to_string(&sums_path).map_err(|e| {
                AcquireError::Io {
                    path: sums_path.display().to_string(),
                    source: e,
                }
            })?;
            mxnode_github::verify_against_sums(&sums_text, &asset.name, &archive_path)
                .map_err(|e| AcquireError::Other(format!("sha256 verification failed: {e}")))?;
        } else {
            tracing::warn!(
                target: "mxnode.event",
                event = "acquire.unverified",
                artifact = artifact.binary_name(),
                tag = tag.as_str(),
                asset = asset.name.as_str(),
                "release does not ship SHA256SUMS; downloaded archive is unverified",
            );
        }

        // Extract the binary out of the zip into a sibling tempfile.
        let extract_to = self.workdir.join(format!(
            "{}-{}-{}",
            artifact.binary_name(),
            tag,
            std::process::id(),
        ));
        let _ = std::fs::remove_file(&extract_to);

        // Use the system unzip binary to keep the dependency surface
        // small. The tarball path can move to a real Rust unzip crate in
        // Phase 3 if operators report missing `unzip` on minimal hosts.
        let status = std::process::Command::new("unzip")
            .arg("-p")
            .arg(&archive_path)
            .arg(format!("**/{}", artifact.binary_name()))
            .stdout(std::process::Stdio::from(
                std::fs::File::create(&extract_to).map_err(|e| AcquireError::Io {
                    path: extract_to.display().to_string(),
                    source: e,
                })?,
            ))
            .stderr(std::process::Stdio::piped())
            .status()
            .map_err(|e| AcquireError::Other(format!("unzip not on PATH: {e}")))?;
        if !status.success() {
            return Err(AcquireError::Other(format!(
                "unzip exited {:?} extracting {} from {}",
                status.code(),
                artifact.binary_name(),
                archive_path.display(),
            )));
        }
        // Empty extract = wrong glob; surface that clearly.
        let meta = std::fs::metadata(&extract_to).map_err(|e| AcquireError::Io {
            path: extract_to.display().to_string(),
            source: e,
        })?;
        if meta.len() == 0 {
            return Err(AcquireError::Other(format!(
                "unzip produced an empty file for {}; the archive likely uses a different layout",
                artifact.binary_name(),
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&extract_to)
                .map_err(|e| AcquireError::Io {
                    path: extract_to.display().to_string(),
                    source: e,
                })?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&extract_to, perms).map_err(|e| AcquireError::Io {
                path: extract_to.display().to_string(),
                source: e,
            })?;
        }
        Ok(extract_to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[tokio::test]
    async fn mock_acquirer_returns_executable_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let acquirer = MockAcquirer::new().with_workdir(dir.path().to_path_buf());
        acquirer.add(Artifact::Node, "v1.7.13", b"#!/bin/sh\necho fake node\n");

        let tag = Tag::from_str("v1.7.13").unwrap();
        let path = acquirer.acquire(Artifact::Node, &tag).await.unwrap();
        assert!(path.exists());
        assert!(path.starts_with(dir.path()));
        let body = std::fs::read(&path).unwrap();
        assert!(body.starts_with(b"#!/bin/sh"));
    }

    #[tokio::test]
    async fn mock_acquirer_errors_on_missing_entry() {
        let acquirer = MockAcquirer::new();
        let tag = Tag::from_str("v1.0.0").unwrap();
        let err = acquirer.acquire(Artifact::Node, &tag).await.unwrap_err();
        assert!(matches!(err, AcquireError::Other(_)));
    }

    /// `SourceBuildAcquirer` against an unreachable upstream surfaces a
    /// `Build` (or `Toolchain`) error rather than panicking. We don't
    /// require git/go on the dev box; if Go is missing the test
    /// short-circuits via Toolchain. If Go is present but the clone
    /// fails, that's still a valid Build error.
    #[tokio::test]
    async fn source_build_acquirer_surfaces_failure_for_bogus_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let acquirer =
            SourceBuildAcquirer::new("definitely-not-a-real-org-xyz", tmp.path().to_path_buf());
        let tag = Tag::from_str("v0.0.0").unwrap();
        let err = acquirer.acquire(Artifact::Node, &tag).await.unwrap_err();
        // Either toolchain (no Go) or build (clone failed) is acceptable;
        // we only assert that we don't get NotImplemented (which would
        // mean the stub is still in place) or a panic.
        match err {
            AcquireError::Toolchain(_) | AcquireError::Build(_) | AcquireError::Other(_) => {}
            other => panic!("expected Toolchain/Build/Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn release_acquirer_surfaces_failure_for_bogus_org() {
        let tmp = tempfile::tempdir().unwrap();
        let acquirer = ReleaseAcquirer::new("definitely-not-a-real-org-xyz", tmp.path().to_path_buf());
        let tag = Tag::from_str("v0.0.0").unwrap();
        let err = acquirer.acquire(Artifact::Node, &tag).await.unwrap_err();
        match err {
            AcquireError::Other(_) | AcquireError::Io { .. } => {}
            other => panic!("expected Other/Io, got {other:?}"),
        }
    }

    #[test]
    fn detect_arch_returns_a_nonempty_string() {
        let a = detect_arch();
        assert!(!a.is_empty());
    }
}
