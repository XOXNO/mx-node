//! Adoption analysis: compare a [`Discovered`] unit against what mxnode
//! would render, classify the result, and produce a typed [`AdoptionReport`]
//! the CLI can act on.
//!
//! The comparison is **semantic** (via [`UnitView`]): byte-level whitespace
//! and comment differences do not constitute drift. Both the legacy
//! bash-shaped renderer and the canonical mxnode renderer parse to the same
//! `UnitView`, so a host installed by either tool adopts cleanly.

use mxnode_core::NodeIndex;
use std::path::PathBuf;

use crate::discovery::{Discovered, DiscoveredKind, UnitView};
use crate::render::{
    render_canonical_node_unit, render_canonical_proxy_unit, render_legacy_node_unit,
    render_legacy_proxy_unit, NodeUnitSpec, ProxyUnitSpec,
};
use crate::{parse_unit_text, ParseError};

/// Inputs the analyser uses to predict what mxnode *would* render for a
/// given discovered unit. The orchestrator builds this from the resolved
/// `Config` and the node's index/role.
pub struct ExpectedNode<'a> {
    pub index: NodeIndex,
    pub custom_user: &'a str,
    pub workdir: &'a std::path::Path,
    pub api_port: u16,
    pub log_level: &'a str,
    pub limit_nofile: u32,
    pub restart_sec: u32,
    pub extra_flags: &'a str,
}

pub struct ExpectedProxy<'a> {
    pub custom_user: &'a str,
    pub proxy_dir: &'a std::path::Path,
    pub limit_nofile: u32,
    pub restart_sec: u32,
}

/// Outcome of analysing one [`Discovered`] unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptionOutcome {
    /// Semantically matches what mxnode would render today (legacy or canonical).
    Clean,
    /// Drift between the on-disk unit and what mxnode would render.
    Drift {
        /// Specific differences vs. the canonical render. Each item is a
        /// human-readable line of the form `"section.key: expected X, found Y"`.
        diffs: Vec<String>,
    },
    /// On-disk unit could not be parsed; usually means a corrupt or
    /// hand-edited file that's no longer valid systemd syntax.
    Unparseable(ParseError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptionReport {
    pub kind: DiscoveredKind,
    pub unit: String,
    pub path: PathBuf,
    pub outcome: AdoptionOutcome,
    /// True when the unit has drop-in `.conf` files alongside it; mxnode
    /// does not author or merge drop-ins, so adopt must surface this loudly.
    pub has_drop_ins: bool,
    pub drop_ins: Vec<String>,
    /// Verbatim file contents, preserved so `adopt --force-adopt` can store
    /// the operator's hand-edited unit unchanged in `state.nodes[].unit_override`.
    pub raw_text: String,
}

impl AdoptionReport {
    pub fn is_clean(&self) -> bool {
        matches!(self.outcome, AdoptionOutcome::Clean)
    }
}

/// Compare a [`Discovered`] node unit against the rendered expectations.
///
/// Returns `AdoptionOutcome::Clean` if the on-disk view matches *either* the
/// canonical or the legacy render — this is what makes byte-different but
/// semantically-identical units adoptable.
pub fn analyze_node(discovered: &Discovered, expected: &ExpectedNode<'_>) -> AdoptionReport {
    let DiscoveredKind::Node(_) = &discovered.kind else {
        // Caller handed us the wrong unit kind. Treat it as a drift event so
        // the CLI surfaces an explicit error rather than silently passing.
        return AdoptionReport {
            kind: discovered.kind.clone(),
            unit: discovered.unit.clone(),
            path: discovered.path.clone(),
            outcome: AdoptionOutcome::Drift {
                diffs: vec!["expected a node unit; got proxy".to_string()],
            },
            has_drop_ins: discovered.has_drop_ins,
            drop_ins: discovered.drop_ins.clone(),
            raw_text: discovered.raw_text.clone(),
        };
    };

    let spec = NodeUnitSpec {
        index: expected.index,
        custom_user: expected.custom_user,
        workdir: expected.workdir,
        api_port: expected.api_port,
        log_level: expected.log_level,
        limit_nofile: expected.limit_nofile,
        restart_sec: expected.restart_sec,
        extra_flags: expected.extra_flags,
    };
    let canonical = render_canonical_node_unit(&spec);
    let legacy = render_legacy_node_unit(&spec);
    finalize(discovered, &[canonical, legacy])
}

pub fn analyze_proxy(discovered: &Discovered, expected: &ExpectedProxy<'_>) -> AdoptionReport {
    let DiscoveredKind::Proxy = &discovered.kind else {
        return AdoptionReport {
            kind: discovered.kind.clone(),
            unit: discovered.unit.clone(),
            path: discovered.path.clone(),
            outcome: AdoptionOutcome::Drift {
                diffs: vec!["expected a proxy unit; got node".to_string()],
            },
            has_drop_ins: discovered.has_drop_ins,
            drop_ins: discovered.drop_ins.clone(),
            raw_text: discovered.raw_text.clone(),
        };
    };

    let spec = ProxyUnitSpec {
        custom_user: expected.custom_user,
        proxy_dir: expected.proxy_dir,
        limit_nofile: expected.limit_nofile,
        restart_sec: expected.restart_sec,
    };
    let canonical = render_canonical_proxy_unit(&spec);
    let legacy = render_legacy_proxy_unit(&spec);
    finalize(discovered, &[canonical, legacy])
}

fn finalize(discovered: &Discovered, candidate_texts: &[String]) -> AdoptionReport {
    let observed = &discovered.view;
    // Try each candidate render. Match the *parsed* view rather than
    // byte-comparing — that's the whole point of the discovery design.
    let mut best_diffs: Option<Vec<String>> = None;
    for candidate in candidate_texts {
        match parse_unit_text(candidate) {
            Ok(expected_view) => {
                if expected_view == *observed {
                    return AdoptionReport {
                        kind: discovered.kind.clone(),
                        unit: discovered.unit.clone(),
                        path: discovered.path.clone(),
                        outcome: AdoptionOutcome::Clean,
                        has_drop_ins: discovered.has_drop_ins,
                        drop_ins: discovered.drop_ins.clone(),
                        raw_text: discovered.raw_text.clone(),
                    };
                }
                let diffs = diff_views(&expected_view, observed);
                best_diffs = Some(match best_diffs {
                    Some(prev) if prev.len() <= diffs.len() => prev,
                    _ => diffs,
                });
            }
            Err(e) => {
                // The candidate texts are produced by our own renderers, so
                // a parse error here is a logic bug in the renderer or the
                // parser — surface it loudly via Unparseable so tests catch.
                return AdoptionReport {
                    kind: discovered.kind.clone(),
                    unit: discovered.unit.clone(),
                    path: discovered.path.clone(),
                    outcome: AdoptionOutcome::Unparseable(e),
                    has_drop_ins: discovered.has_drop_ins,
                    drop_ins: discovered.drop_ins.clone(),
                    raw_text: discovered.raw_text.clone(),
                };
            }
        }
    }

    AdoptionReport {
        kind: discovered.kind.clone(),
        unit: discovered.unit.clone(),
        path: discovered.path.clone(),
        outcome: AdoptionOutcome::Drift {
            diffs: best_diffs.unwrap_or_default(),
        },
        has_drop_ins: discovered.has_drop_ins,
        drop_ins: discovered.drop_ins.clone(),
        raw_text: discovered.raw_text.clone(),
    }
}

/// Produce a stable, line-oriented diff between an expected and observed
/// `UnitView`. Used purely for human-readable error output; ordering and
/// formatting are stable but not part of any contract.
fn diff_views(expected: &UnitView, observed: &UnitView) -> Vec<String> {
    let exp: std::collections::BTreeSet<String> = expected.flatten().collect();
    let obs: std::collections::BTreeSet<String> = observed.flatten().collect();
    let mut out: Vec<String> = Vec::new();
    for missing in exp.difference(&obs) {
        out.push(format!("- expected: {missing}"));
    }
    for extra in obs.difference(&exp) {
        out.push(format!("+ observed: {extra}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan_systemd_dir;
    use mxnode_core::NodeIndex;
    use std::path::PathBuf;

    fn write_node_unit(dir: &std::path::Path, text: &str) {
        std::fs::write(dir.join("elrond-node-0.service"), text).unwrap();
    }

    #[test]
    fn canonical_unit_adopts_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = PathBuf::from("/home/ubuntu/elrond-nodes/node-0");
        let spec = NodeUnitSpec {
            index: NodeIndex::new(0),
            custom_user: "ubuntu",
            workdir: &workdir,
            api_port: 8080,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
        };
        write_node_unit(tmp.path(), &render_canonical_node_unit(&spec));
        let discovered = scan_systemd_dir(tmp.path()).unwrap().into_iter().next().unwrap();
        let report = analyze_node(
            &discovered,
            &ExpectedNode {
                index: NodeIndex::new(0),
                custom_user: "ubuntu",
                workdir: &workdir,
                api_port: 8080,
                log_level: "*:DEBUG",
                limit_nofile: 4096,
                restart_sec: 3,
                extra_flags: "",
            },
        );
        assert_eq!(report.outcome, AdoptionOutcome::Clean);
    }

    #[test]
    fn legacy_unit_adopts_cleanly_against_canonical_expectation() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = PathBuf::from("/home/ubuntu/elrond-nodes/node-0");
        let spec = NodeUnitSpec {
            index: NodeIndex::new(0),
            custom_user: "ubuntu",
            workdir: &workdir,
            api_port: 8080,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
        };
        // On-disk: legacy bash-shape. mxnode's expected: canonical.
        write_node_unit(tmp.path(), &render_legacy_node_unit(&spec));
        let discovered = scan_systemd_dir(tmp.path()).unwrap().into_iter().next().unwrap();
        let report = analyze_node(
            &discovered,
            &ExpectedNode {
                index: NodeIndex::new(0),
                custom_user: "ubuntu",
                workdir: &workdir,
                api_port: 8080,
                log_level: "*:DEBUG",
                limit_nofile: 4096,
                restart_sec: 3,
                extra_flags: "",
            },
        );
        assert_eq!(
            report.outcome,
            AdoptionOutcome::Clean,
            "legacy must adopt cleanly so existing bash-installed hosts don't trip drift",
        );
    }

    #[test]
    fn semantic_drift_is_reported_with_diffs() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = PathBuf::from("/home/ubuntu/elrond-nodes/node-0");
        let spec = NodeUnitSpec {
            index: NodeIndex::new(0),
            custom_user: "ubuntu",
            workdir: &workdir,
            api_port: 8080,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
        };
        write_node_unit(tmp.path(), &render_canonical_node_unit(&spec));
        let discovered = scan_systemd_dir(tmp.path()).unwrap().into_iter().next().unwrap();
        // Operator config asks for a different user — drift.
        let report = analyze_node(
            &discovered,
            &ExpectedNode {
                index: NodeIndex::new(0),
                custom_user: "validator",
                workdir: &workdir,
                api_port: 8080,
                log_level: "*:DEBUG",
                limit_nofile: 4096,
                restart_sec: 3,
                extra_flags: "",
            },
        );
        match report.outcome {
            AdoptionOutcome::Drift { diffs } => {
                assert!(diffs.iter().any(|d| d.contains("Service.User")), "diffs: {diffs:?}");
            }
            other => panic!("expected drift, got {other:?}"),
        }
    }

    #[test]
    fn proxy_unit_adopts_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let proxy_dir = PathBuf::from("/home/ubuntu/elrond-proxy");
        let spec = ProxyUnitSpec {
            custom_user: "ubuntu",
            proxy_dir: &proxy_dir,
            limit_nofile: 4096,
            restart_sec: 3,
        };
        std::fs::write(
            tmp.path().join("elrond-proxy.service"),
            render_canonical_proxy_unit(&spec),
        )
        .unwrap();
        let discovered = scan_systemd_dir(tmp.path()).unwrap().into_iter().next().unwrap();
        let report = analyze_proxy(
            &discovered,
            &ExpectedProxy {
                custom_user: "ubuntu",
                proxy_dir: &proxy_dir,
                limit_nofile: 4096,
                restart_sec: 3,
            },
        );
        assert_eq!(report.outcome, AdoptionOutcome::Clean);
    }
}
