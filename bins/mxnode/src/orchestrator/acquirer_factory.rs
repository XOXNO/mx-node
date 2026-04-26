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
pub fn build_acquirer(runtime: &Runtime) -> Arc<dyn BinaryAcquirer> {
    let github_org = runtime.loaded.config.network.github_org.clone();
    let workdir = runtime.paths.custom_home.join("mxnode/build");
    let token = std::env::var("MXNODE_GITHUB_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let go_override = runtime
        .loaded
        .config
        .overrides
        .goversion()
        .map(str::to_owned);

    let make_source = || {
        let mut acq = SourceBuildAcquirer::new(github_org.clone(), workdir.clone());
        if let Some(v) = &go_override {
            acq = acq.with_min_go(v.clone());
        }
        acq
    };

    match runtime.loaded.config.install.artifact_source {
        ArtifactSource::Source => Arc::new(make_source()),
        ArtifactSource::Release => Arc::new(
            ReleaseAcquirer::new(github_org, workdir).with_token(token),
        ),
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
