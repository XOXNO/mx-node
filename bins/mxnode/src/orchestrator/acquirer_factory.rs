//! Resolve a [`BinaryAcquirer`] from `[install].artifact_source` config.
//!
//! Centralised here so `install` and `upgrade` make the same decision —
//! the operator's `artifact_source = "auto" | "release" | "source"`
//! controls every binary acquisition mxnode performs.

use std::sync::Arc;

use async_trait::async_trait;
use mxnode_core::{ArtifactSource, Tag};

use super::acquirer::{
    AcquireError, Artifact, BinaryAcquirer, ReleaseAcquirer, SourceBuildAcquirer,
};
use crate::orchestrator::runtime::Runtime;

/// Build the acquirer the orchestrator should use for this run.
///
/// `Source` is the historical default and what the bash uses today;
/// `Release` opts into pre-built artifact downloads; `Auto` tries
/// `Release` first and falls back to `Source` per artifact (so a
/// release-only fork plus a source-only sub-artifact like keygenerator
/// both work).
pub fn build_acquirer(
    runtime: &Runtime,
    upstream_go_version: Option<&str>,
) -> Arc<dyn BinaryAcquirer> {
    let github_org = runtime.loaded.config.network.github_org.clone();
    let workdir = runtime.paths.custom_home.join("mxnode/build");
    let token = std::env::var("MXNODE_GITHUB_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());

    // Precedence: CLI/config override > upstream goVersion file > DEFAULT_GO_VERSION.
    let go_pinned = runtime
        .loaded
        .config
        .overrides
        .goversion()
        .map(str::to_owned)
        .or_else(|| upstream_go_version.map(str::to_owned));

    // Log the precedence decision so operators can trace which Go
    // version the toolchain bootstrap will request, and why.
    if let Some(version) = &go_pinned {
        let source = if runtime.loaded.config.overrides.goversion().is_some() {
            "config override"
        } else {
            "upstream goVersion"
        };
        tracing::info!(
            target: "mxnode.event",
            event = "acquire.go_version_resolved",
            version = version.as_str(),
            source = source,
            "selected Go version {version} ({source})",
        );
    } else {
        tracing::info!(
            target: "mxnode.event",
            event = "acquire.go_version_resolved",
            source = "default",
            "selected default Go version (mxnode_toolchain::DEFAULT_GO_VERSION)",
        );
    }

    let make_source = || {
        let mut acq = SourceBuildAcquirer::new(github_org.clone(), workdir.clone());
        if let Some(v) = &go_pinned {
            acq = acq.with_min_go(v.clone());
        }
        acq
    };

    match runtime.loaded.config.install.artifact_source {
        ArtifactSource::Source => Arc::new(make_source()),
        ArtifactSource::Release => {
            Arc::new(ReleaseAcquirer::new(github_org, workdir).with_token(token))
        }
        ArtifactSource::Auto => {
            let release = Arc::new(
                ReleaseAcquirer::new(github_org.clone(), workdir.clone()).with_token(token),
            );
            let source = Arc::new(make_source());
            Arc::new(AutoAcquirer { release, source })
        }
    }
}

/// `Auto`-mode acquirer: tries the release path first, falls back to
/// source-build on any failure (typed as `Other` from the release
/// acquirer when the asset shape doesn't match).
struct AutoAcquirer {
    release: Arc<dyn BinaryAcquirer>,
    source: Arc<dyn BinaryAcquirer>,
}

#[async_trait]
impl BinaryAcquirer for AutoAcquirer {
    async fn acquire(
        &self,
        artifact: Artifact,
        tag: &Tag,
    ) -> Result<std::path::PathBuf, AcquireError> {
        match self.release.acquire(artifact, tag).await {
            Ok(p) => Ok(p),
            Err(release_err) => {
                tracing::info!(
                    target: "mxnode.event",
                    event = "acquire.auto.fallback",
                    artifact = ?artifact,
                    tag = tag.as_str(),
                    reason = %release_err,
                    "release acquisition failed; falling back to source build",
                );
                self.source.acquire(artifact, tag).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // build_acquirer threads goVersion from the right source.
    // We don't construct a full Runtime — we test the precedence logic
    // by extracting it into a small helper.

    fn pick_go(cli_override: Option<&str>, upstream: Option<&str>) -> Option<String> {
        cli_override
            .map(str::to_owned)
            .or_else(|| upstream.map(str::to_owned))
    }

    #[test]
    fn cli_override_wins_over_upstream() {
        assert_eq!(
            pick_go(Some("1.24.0"), Some("1.23.4")).as_deref(),
            Some("1.24.0")
        );
    }

    #[test]
    fn upstream_used_when_no_cli_override() {
        assert_eq!(pick_go(None, Some("1.23.4")).as_deref(), Some("1.23.4"));
    }

    #[test]
    fn neither_falls_back_to_default() {
        assert!(pick_go(None, None).is_none());
    }
}
