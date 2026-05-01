//! Resolve `--binary-tag` / `--config-tag` / `--proxy-tag` from one of
//! three sources, in priority order:
//!
//!   1. Explicit CLI flag (operator-supplied).
//!   2. `[overrides].binaryver` / `configver` / `proxyver` from the
//!      operator's config file (power-user pinning).
//!   3. GitHub Releases — the latest published tag for the underlying
//!      repo under `[network].github_org`.
//!
//! GitHub is hit only when the first two sources are silent. The
//! `MXNODE_GITHUB_TOKEN` env var lifts the rate limit when present;
//! callers that don't need fork support can leave it unset (60 req/h is
//! plenty for a single resolve call).
//!
//! Errors carry the resolved priority chain so the operator can see
//! which source mxnode tried and why it didn't yield a tag.

use mxnode_core::{Environment, Tag};
use mxnode_github::{Client, ClientConfig, GithubError};
use thiserror::Error;

use crate::orchestrator::runtime::Runtime;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("invalid {flag}: {reason}")]
    InvalidTag { flag: &'static str, reason: String },

    #[error("github lookup failed for {org}/{repo}: {source}")]
    Github {
        org: String,
        repo: String,
        #[source]
        source: GithubError,
    },

    #[error("github returned a tag we couldn't parse ({org}/{repo} → `{tag}`): {reason}")]
    UnparseableLatest {
        org: String,
        repo: String,
        tag: String,
        reason: String,
    },
}

/// Resolution outcome — useful for telemetry and operator-facing
/// "resolved X via Y" messages so it's clear when GitHub was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Operator passed the flag explicitly.
    Cli,
    /// Pulled from `[overrides]` in config.toml.
    Override,
    /// Looked up `releases/latest` on GitHub.
    GithubLatest,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub tag: Tag,
    pub source: Source,
}

/// Resolve the binary (mx-chain-go) tag.
pub async fn resolve_binary_tag(
    runtime: &Runtime,
    cli_value: Option<&str>,
) -> Result<Resolved, ResolveError> {
    resolve(
        runtime,
        cli_value,
        runtime.loaded.config.overrides.binaryver(),
        "mx-chain-go",
        "--binary-tag",
    )
    .await
}

/// Resolve the per-environment config (`mx-chain-{env}-config`) tag.
pub async fn resolve_config_tag(
    runtime: &Runtime,
    env: Environment,
    cli_value: Option<&str>,
) -> Result<Resolved, ResolveError> {
    let repo = env.config_repo();
    resolve(
        runtime,
        cli_value,
        runtime.loaded.config.overrides.configver(),
        &repo,
        "--config-tag",
    )
    .await
}

/// Resolve the proxy (mx-chain-proxy-go) tag.
pub async fn resolve_proxy_tag(
    runtime: &Runtime,
    cli_value: Option<&str>,
) -> Result<Resolved, ResolveError> {
    resolve(
        runtime,
        cli_value,
        runtime.loaded.config.overrides.proxyver(),
        "mx-chain-proxy-go",
        "--proxy-tag",
    )
    .await
}

async fn resolve(
    runtime: &Runtime,
    cli_value: Option<&str>,
    override_value: Option<&str>,
    repo: &str,
    flag: &'static str,
) -> Result<Resolved, ResolveError> {
    if let Some(raw) = cli_value {
        let tag = parse_tag(raw, flag)?;
        return Ok(Resolved {
            tag,
            source: Source::Cli,
        });
    }
    if let Some(raw) = override_value {
        let tag = parse_tag(raw, flag)?;
        return Ok(Resolved {
            tag,
            source: Source::Override,
        });
    }
    let org = &runtime.loaded.config.network.github_org;
    fetch_latest(org, repo).await
}

fn parse_tag(raw: &str, flag: &'static str) -> Result<Tag, ResolveError> {
    raw.parse::<Tag>()
        .map_err(|e: mxnode_core::Error| ResolveError::InvalidTag {
            flag,
            reason: e.to_string(),
        })
}

async fn fetch_latest(org: &str, repo: &str) -> Result<Resolved, ResolveError> {
    let cfg = ClientConfig {
        token: std::env::var("MXNODE_GITHUB_TOKEN")
            .ok()
            .filter(|s| !s.is_empty()),
        ..ClientConfig::default()
    };
    let client = Client::new(cfg).map_err(|e| ResolveError::Github {
        org: org.to_string(),
        repo: repo.to_string(),
        source: e,
    })?;
    let release = client
        .latest_release(org, repo)
        .await
        .map_err(|e| ResolveError::Github {
            org: org.to_string(),
            repo: repo.to_string(),
            source: e,
        })?;
    let tag = release
        .tag_name
        .parse::<Tag>()
        .map_err(|e: mxnode_core::Error| ResolveError::UnparseableLatest {
            org: org.to_string(),
            repo: repo.to_string(),
            tag: release.tag_name.clone(),
            reason: e.to_string(),
        })?;
    Ok(Resolved {
        tag,
        source: Source::GithubLatest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_variants_are_distinct() {
        assert_ne!(Source::Cli, Source::Override);
        assert_ne!(Source::Override, Source::GithubLatest);
    }
}
