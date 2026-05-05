//! Pre-dispatch "is a newer mxnode out?" check with a sticky cache.
//!
//! Goals:
//!   - Tell the operator about a newer release without slowing down
//!     day-to-day commands. Cache hits are sub-millisecond.
//!   - Never block a non-interactive caller (CI, scripts, JSON
//!     consumers). Decision logic always returns; runtime IO is
//!     bounded by `fetch_timeout`.
//!   - Degrade silently on network / API failure — update-check is
//!     best-effort, never fatal.
//!
//! Storage lives in `mxnode.toml`'s `[update_cache]` section
//! (`last_checked_at`, `latest_tag`, `declined_tag`, `declined_at`).
//! Single source of truth, single lock. The gate uses
//! [`mxnode_state::StateStore`] for reads/writes.

use std::time::Duration;

use mxnode_core::{MxnodeFile, UpdateCacheSection};
use mxnode_github::{Client, ClientConfig, GithubError};
use mxnode_state::{StateError, StateStore};
use semver::Version;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("github error: {0}")]
    Github(#[from] GithubError),
    #[error("state error: {0}")]
    State(#[from] StateError),
    #[error("could not parse version {raw:?}: {detail}")]
    BadVersion { raw: String, detail: String },
}

/// Outcome of the pre-dispatch gate. The CLI translates this into a
/// stderr line ("→ mxnode vX.Y.Z available; run `mxnode self-update`")
/// or a Y/N prompt depending on policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No prompt — either the cache is fresh, the operator declined
    /// recently, current binary is up to date, or we couldn't reach
    /// GitHub.
    Skip(SkipReason),
    /// A newer release is available; CLI should surface it.
    Prompt(RemoteVersion),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Cache is within `Policy::ttl`; nothing to do.
    CacheFresh,
    /// Local binary already at or beyond the cached `latest_tag`.
    UpToDate,
    /// Operator pressed N for this tag within `Policy::decline_ttl`.
    Declined { tag: String },
    /// Network error or rate-limit — best-effort skip.
    FetchFailed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteVersion {
    /// Raw `v0.8.23`-style tag from GitHub.
    pub tag: String,
    /// Parsed semver (tag stripped of leading `v`).
    pub version: Version,
}

/// Tunable timing knobs. Defaults match the design: 24 h cache, 24 h
/// decline-suppression, 2 s fetch timeout.
#[derive(Debug, Clone)]
pub struct Policy {
    /// How long a cached `latest_tag` is reused without re-fetching.
    pub ttl: Duration,
    /// How long a `declined_tag` suppresses the prompt for that tag.
    pub decline_ttl: Duration,
    /// Hard ceiling on the GitHub round-trip.
    pub fetch_timeout: Duration,
    /// Repo coordinates we ask GitHub about.
    pub repo_org: String,
    pub repo_name: String,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(24 * 60 * 60),
            decline_ttl: Duration::from_secs(24 * 60 * 60),
            fetch_timeout: Duration::from_secs(2),
            repo_org: "XOXNO".to_string(),
            repo_name: "mx-node".to_string(),
        }
    }
}

/// Pure decision step — given the current binary version, the cached
/// last-fetched value, and `now`, decide whether to prompt or skip.
/// Network IO is the caller's job (see [`check_for_update`]).
pub fn decide(
    local: &Version,
    cache: &UpdateCacheSection,
    now: OffsetDateTime,
    policy: &Policy,
) -> DecideOutcome {
    // Cache miss → caller must fetch.
    let last_checked = match cache.last_checked_at {
        Some(t) => t,
        None => return DecideOutcome::NeedFetch,
    };

    let age = duration_since(now, last_checked);
    if age > policy.ttl {
        return DecideOutcome::NeedFetch;
    }

    // Cache fresh — interpret the cached tag.
    if cache.latest_tag.is_empty() {
        return DecideOutcome::Resolved(Decision::Skip(SkipReason::CacheFresh));
    }
    let remote = match parse_version(&cache.latest_tag) {
        Ok(v) => v,
        // Corrupt cache — refetch on next opportunity.
        Err(_) => return DecideOutcome::NeedFetch,
    };
    if local >= &remote.version {
        return DecideOutcome::Resolved(Decision::Skip(SkipReason::UpToDate));
    }

    // Operator declined this exact tag recently? Skip.
    if cache.declined_tag == remote.tag {
        if let Some(declined_at) = cache.declined_at {
            if duration_since(now, declined_at) <= policy.decline_ttl {
                return DecideOutcome::Resolved(Decision::Skip(SkipReason::Declined {
                    tag: remote.tag.clone(),
                }));
            }
        }
    }

    DecideOutcome::Resolved(Decision::Prompt(remote))
}

/// What `decide` tells the orchestrator to do next.
#[derive(Debug, Clone)]
pub enum DecideOutcome {
    /// Use this answer directly; no GitHub call.
    Resolved(Decision),
    /// Cache is stale or absent — the orchestrator must run the fetch
    /// step (network) and then re-`decide` against the fresh cache.
    NeedFetch,
}

/// End-to-end gate: read cache → decide → fetch if needed → persist
/// updated cache → return Decision. Always best-effort: network errors
/// resolve as `Skip(FetchFailed)`, never propagate.
///
/// Holds the file lock only during the cache write — short, sub-ms.
pub async fn check_for_update(
    store: &StateStore,
    policy: &Policy,
    local_version: &str,
    token: Option<String>,
) -> Result<Decision, UpdateError> {
    let local = parse_version(local_version)?.version;
    let now = OffsetDateTime::now_utc();

    let mut file = store.load_file()?.unwrap_or_default();

    match decide(&local, &file.update_cache, now, policy) {
        DecideOutcome::Resolved(d) => Ok(d),
        DecideOutcome::NeedFetch => {
            let fetched = fetch_with_timeout(&policy.repo_org, &policy.repo_name, token, policy.fetch_timeout).await;
            let updated = match fetched {
                Ok(remote) => {
                    file.update_cache.last_checked_at = Some(now);
                    file.update_cache.latest_tag = remote.tag.clone();
                    persist_cache(store, &file)?;
                    if local >= remote.version {
                        Decision::Skip(SkipReason::UpToDate)
                    } else if file.update_cache.declined_tag == remote.tag
                        && file
                            .update_cache
                            .declined_at
                            .map(|t| duration_since(now, t) <= policy.decline_ttl)
                            .unwrap_or(false)
                    {
                        Decision::Skip(SkipReason::Declined {
                            tag: remote.tag.clone(),
                        })
                    } else {
                        Decision::Prompt(remote)
                    }
                }
                Err(e) => {
                    // Even on failure, bump `last_checked_at` so we
                    // don't hammer GitHub on a flapping network. The
                    // tag stays at whatever the cache had.
                    file.update_cache.last_checked_at = Some(now);
                    let _ = persist_cache(store, &file);
                    Decision::Skip(SkipReason::FetchFailed {
                        reason: e.to_string(),
                    })
                }
            };
            Ok(updated)
        }
    }
}

/// Persist the operator's "no, don't prompt me again for this tag"
/// answer. Caller invokes this after the Y/N prompt resolves to N.
pub fn record_decline(
    store: &StateStore,
    tag: &str,
    now: OffsetDateTime,
) -> Result<(), UpdateError> {
    let mut file = store.load_file()?.unwrap_or_default();
    file.update_cache.declined_tag = tag.to_string();
    file.update_cache.declined_at = Some(now);
    persist_cache(store, &file)
}

fn persist_cache(store: &StateStore, file: &MxnodeFile) -> Result<(), UpdateError> {
    let guard = store.lock()?;
    store.save_file(file, &guard)?;
    Ok(())
}

fn parse_version(raw: &str) -> Result<RemoteVersion, UpdateError> {
    let trimmed = raw.strip_prefix('v').unwrap_or(raw);
    let version = Version::parse(trimmed).map_err(|e| UpdateError::BadVersion {
        raw: raw.to_string(),
        detail: e.to_string(),
    })?;
    Ok(RemoteVersion {
        tag: raw.to_string(),
        version,
    })
}

fn duration_since(now: OffsetDateTime, earlier: OffsetDateTime) -> Duration {
    let delta = now - earlier;
    if delta.is_negative() {
        Duration::ZERO
    } else {
        // Convert `time::Duration` → `std::time::Duration`. Saturating:
        // we don't care about microsecond precision for a 24h gate.
        Duration::new(delta.whole_seconds().max(0) as u64, 0)
    }
}

async fn fetch_with_timeout(
    org: &str,
    repo: &str,
    token: Option<String>,
    timeout: Duration,
) -> Result<RemoteVersion, UpdateError> {
    let cfg = ClientConfig {
        token,
        ..ClientConfig::default()
    };
    let client = Client::new(cfg)?;
    let release = tokio::time::timeout(timeout, client.latest_release(org, repo))
        .await
        .map_err(|_| UpdateError::BadVersion {
            raw: format!("{org}/{repo} latest"),
            detail: format!("fetch timed out after {timeout:?}"),
        })??;
    parse_version(&release.tag_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn cache_at(tag: &str, last: OffsetDateTime) -> UpdateCacheSection {
        UpdateCacheSection {
            last_checked_at: Some(last),
            latest_tag: tag.to_string(),
            declined_tag: String::new(),
            declined_at: None,
        }
    }

    #[test]
    fn no_cache_means_need_fetch() {
        let cache = UpdateCacheSection::default();
        let local = Version::parse("0.8.23").unwrap();
        let now = datetime!(2026-05-05 12:00 UTC);
        match decide(&local, &cache, now, &Policy::default()) {
            DecideOutcome::NeedFetch => {}
            other => panic!("expected NeedFetch, got {other:?}"),
        }
    }

    #[test]
    fn fresh_cache_with_up_to_date_local_skips() {
        let now = datetime!(2026-05-05 12:00 UTC);
        let cache = cache_at("v0.8.23", now - time::Duration::hours(1));
        let local = Version::parse("0.8.23").unwrap();
        match decide(&local, &cache, now, &Policy::default()) {
            DecideOutcome::Resolved(Decision::Skip(SkipReason::UpToDate)) => {}
            other => panic!("expected UpToDate, got {other:?}"),
        }
    }

    #[test]
    fn fresh_cache_with_newer_remote_prompts() {
        let now = datetime!(2026-05-05 12:00 UTC);
        let cache = cache_at("v0.9.0", now - time::Duration::hours(1));
        let local = Version::parse("0.8.23").unwrap();
        match decide(&local, &cache, now, &Policy::default()) {
            DecideOutcome::Resolved(Decision::Prompt(v)) => {
                assert_eq!(v.tag, "v0.9.0");
                assert_eq!(v.version, Version::parse("0.9.0").unwrap());
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn stale_cache_triggers_fetch() {
        let now = datetime!(2026-05-05 12:00 UTC);
        let cache = cache_at("v0.9.0", now - time::Duration::hours(48));
        let local = Version::parse("0.8.23").unwrap();
        match decide(&local, &cache, now, &Policy::default()) {
            DecideOutcome::NeedFetch => {}
            other => panic!("expected NeedFetch on stale cache, got {other:?}"),
        }
    }

    #[test]
    fn recently_declined_tag_skips_even_with_newer_remote() {
        let now = datetime!(2026-05-05 12:00 UTC);
        let cache = UpdateCacheSection {
            last_checked_at: Some(now - time::Duration::hours(1)),
            latest_tag: "v0.9.0".to_string(),
            declined_tag: "v0.9.0".to_string(),
            declined_at: Some(now - time::Duration::hours(2)),
        };
        let local = Version::parse("0.8.23").unwrap();
        match decide(&local, &cache, now, &Policy::default()) {
            DecideOutcome::Resolved(Decision::Skip(SkipReason::Declined { tag })) => {
                assert_eq!(tag, "v0.9.0");
            }
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[test]
    fn old_decline_does_not_suppress_after_ttl() {
        let now = datetime!(2026-05-05 12:00 UTC);
        let cache = UpdateCacheSection {
            last_checked_at: Some(now - time::Duration::hours(1)),
            latest_tag: "v0.9.0".to_string(),
            declined_tag: "v0.9.0".to_string(),
            declined_at: Some(now - time::Duration::hours(48)), // > decline_ttl
        };
        let local = Version::parse("0.8.23").unwrap();
        match decide(&local, &cache, now, &Policy::default()) {
            DecideOutcome::Resolved(Decision::Prompt(_)) => {}
            other => panic!("expected Prompt after old decline, got {other:?}"),
        }
    }

    #[test]
    fn local_newer_than_cached_is_up_to_date() {
        let now = datetime!(2026-05-05 12:00 UTC);
        let cache = cache_at("v0.8.20", now - time::Duration::minutes(30));
        let local = Version::parse("0.8.23").unwrap();
        match decide(&local, &cache, now, &Policy::default()) {
            DecideOutcome::Resolved(Decision::Skip(SkipReason::UpToDate)) => {}
            other => panic!("expected UpToDate when ahead of cache, got {other:?}"),
        }
    }

    #[test]
    fn parse_strips_v_prefix_and_handles_plain_versions() {
        assert_eq!(
            parse_version("v0.8.23").unwrap().version,
            Version::parse("0.8.23").unwrap(),
        );
        assert_eq!(
            parse_version("0.8.23").unwrap().version,
            Version::parse("0.8.23").unwrap(),
        );
        assert!(parse_version("not-a-version").is_err());
    }
}
