//! Discovery-driven adoption: walk `/etc/systemd/system`, classify each
//! `elrond-*.service`, compare against what mxnode would render, and either
//! produce a `State` ready for write or an actionable drift report.
//!
//! This is the shared engine behind `mxnode adopt` and `mxnode rebuild-state`.
//! `adopt` refuses on drift (unless `--force-adopt`); `rebuild-state` always
//! writes the observed reality.

use std::path::Path;

use mxnode_core::{
    Environment, InstallKind, InstallSection, NodeIndex, NodeState, Paths, ProxyState, Role, Shard,
    State,
};
use mxnode_systemd::{
    analyze_node, analyze_proxy, scan_supervisor_dir, AdoptionOutcome, AdoptionReport,
    Discovered, DiscoveredKind, ExpectedNode, ExpectedProxy,
};

/// Inputs for one adoption pass. Everything the analyser needs to predict
/// what mxnode would have rendered comes from the resolved config.
pub struct AdoptInputs<'a> {
    pub paths: &'a Paths,
    pub environment: Environment,
    pub github_org: &'a str,
    pub log_level: &'a str,
    pub limit_nofile: u32,
    pub restart_sec: u32,
    pub api_port_base: u16,
    pub extra_flags: &'a str,
}

#[derive(Debug)]
pub struct AdoptOutcome {
    pub state: State,
    pub reports: Vec<AdoptionReport>,
    /// Snapshot of the `[install]` section we wrote into `state.toml`. The
    /// CLI uses this for the human-readable summary line; tests assert on it.
    #[allow(dead_code)]
    pub install: InstallSection,
}

impl AdoptOutcome {
    /// True when every discovered unit adopted cleanly. The CLI uses this to
    /// gate `mxnode adopt` (refuses on any drift unless `--force-adopt`).
    pub fn is_clean(&self) -> bool {
        self.reports.iter().all(|r| r.is_clean()) && !self.has_drop_ins()
    }

    pub fn has_drop_ins(&self) -> bool {
        self.reports.iter().any(|r| r.has_drop_ins)
    }

    /// Drift reports only — useful for surfacing diffs to the operator.
    pub fn drift_reports(&self) -> impl Iterator<Item = &AdoptionReport> {
        self.reports.iter().filter(|r| !r.is_clean())
    }
}

/// Run the discovery + analysis pass. Reads from `/etc/systemd/system`
/// (or whatever path is passed). Does not write any files.
pub fn analyze(
    systemd_dir: &Path,
    inputs: &AdoptInputs<'_>,
    written_by: &str,
) -> Result<AdoptOutcome, mxnode_systemd::DiscoveryError> {
    let discovered = scan_supervisor_dir(systemd_dir)?;
    let mut reports: Vec<AdoptionReport> = Vec::with_capacity(discovered.len());
    let mut nodes: Vec<NodeState> = Vec::new();
    let mut proxy: Option<ProxyState> = None;
    let mut node_count: u16 = 0;
    let mut proxy_present = false;

    for d in &discovered {
        match &d.kind {
            DiscoveredKind::Node(idx) => {
                let workdir = inputs.paths.node_workdir(*idx);
                let api_port = api_port_for(inputs.api_port_base, *idx);
                let report = analyze_node(
                    d,
                    &ExpectedNode {
                        index: *idx,
                        custom_user: &inputs.paths.custom_user,
                        workdir: &workdir,
                        api_port,
                        log_level: inputs.log_level,
                        limit_nofile: inputs.limit_nofile,
                        restart_sec: inputs.restart_sec,
                        extra_flags: inputs.extra_flags,
                    },
                );
                nodes.push(node_state_from(d, &report, *idx, api_port, &workdir));
                reports.push(report);
                node_count = node_count.saturating_add(1);
            }
            DiscoveredKind::Proxy => {
                let proxy_dir = inputs.paths.elrond_proxy_root();
                let report = analyze_proxy(
                    d,
                    &ExpectedProxy {
                        custom_user: &inputs.paths.custom_user,
                        proxy_dir: &proxy_dir,
                        limit_nofile: inputs.limit_nofile,
                        restart_sec: inputs.restart_sec,
                    },
                );
                proxy = Some(ProxyState {
                    present: true,
                    unit: d.unit.clone(),
                    workdir: proxy_dir,
                    server_port: extract_proxy_port(d).unwrap_or(crate::DEFAULT_PROXY_PORT_FALLBACK),
                });
                reports.push(report);
                proxy_present = true;
            }
        }
    }

    nodes.sort_by_key(|n| n.index.get());
    let install = InstallSection::observed(
        infer_install_kind(&nodes, proxy_present),
        inputs.environment,
        inputs.github_org,
        node_count,
    );

    let mut state = State::empty(written_by);
    state.discovered = true;
    state.install = Some(install.clone());
    state.nodes = nodes;
    state.proxy = proxy;

    Ok(AdoptOutcome { state, reports, install })
}

fn api_port_for(base: u16, index: NodeIndex) -> u16 {
    base.saturating_add(index.get())
}

/// Best-effort: read the `ServerPort=` directive if it appears in the proxy
/// unit. mxnode's renderers don't include one (the proxy reads it from its
/// own `config.toml` instead), so this is mostly future-proofing for hosts
/// where the operator inlined a port.
fn extract_proxy_port(d: &Discovered) -> Option<u16> {
    d.view
        .directives
        .get("Service")
        .and_then(|kvs| kvs.iter().rev().find(|(k, _)| k == "ServerPort"))
        .and_then(|(_, v)| v.parse::<u16>().ok())
}

fn node_state_from(
    d: &Discovered,
    report: &AdoptionReport,
    idx: NodeIndex,
    api_port: u16,
    workdir: &Path,
) -> NodeState {
    NodeState {
        index: idx,
        // Phase 0 cannot reliably distinguish observer/multikey/validator
        // from the unit text alone; the role gets refined later when we
        // probe the local REST API. Default to validator — adopt will be
        // re-run once `mxnode status` populates `last_known_pubkey`.
        role: Role::Validator,
        shard: Shard::Auto,
        display_name: String::new(),
        api_port,
        unit: d.unit.clone(),
        unit_override: match &report.outcome {
            AdoptionOutcome::Drift { .. } | AdoptionOutcome::Unparseable(_) => d.raw_text.clone(),
            AdoptionOutcome::Clean => String::new(),
        },
        workdir: workdir.to_path_buf(),
        last_known_pubkey: String::new(),
        last_action: String::new(),
        last_action_at: None,
    }
}

fn infer_install_kind(nodes: &[NodeState], proxy_present: bool) -> InstallKind {
    match (nodes.len(), proxy_present) {
        (0, _) => InstallKind::Validators,
        (4, true) => InstallKind::ObserversSquad,
        (4, false) => InstallKind::MultikeySquad,
        _ => InstallKind::Validators,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use mxnode_core::{Paths, DEFAULT_API_PORT_BASE};
    use mxnode_systemd::{render_canonical_node_unit, NodeUnitSpec};

    fn paths_for(tmp: &Path) -> Paths {
        let mut p = Paths::default();
        p.custom_home = tmp.to_path_buf();
        p.binaries = tmp.join("mxnode/binaries");
        p.state = tmp.join(".local/state/mxnode");
        p.runtime = p.state.join("run");
        p.node_keys = tmp.join("VALIDATOR_KEYS");
        p
    }

    fn write_node_unit(systemd_dir: &Path, idx: NodeIndex, paths: &Paths) {
        let workdir = paths.node_workdir(idx);
        let text = render_canonical_node_unit(&NodeUnitSpec {
            index: idx,
            custom_user: &paths.custom_user,
            workdir: &workdir,
            api_port: DEFAULT_API_PORT_BASE + idx.get(),
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
        });
        std::fs::write(
            systemd_dir.join(format!("elrond-node-{}.service", idx.get())),
            text,
        )
        .unwrap();
    }

    fn inputs<'a>(paths: &'a Paths) -> AdoptInputs<'a> {
        AdoptInputs {
            paths,
            environment: Environment::Mainnet,
            github_org: "multiversx",
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            api_port_base: DEFAULT_API_PORT_BASE,
            extra_flags: "",
        }
    }

    #[test]
    fn analyze_clean_install_yields_clean_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let systemd = tmp.path().join("systemd");
        std::fs::create_dir_all(&systemd).unwrap();
        let paths = paths_for(tmp.path());
        write_node_unit(&systemd, NodeIndex::new(0), &paths);
        write_node_unit(&systemd, NodeIndex::new(1), &paths);

        let outcome = analyze(&systemd, &inputs(&paths), "mxnode/test").unwrap();
        assert!(outcome.is_clean());
        assert_eq!(outcome.state.nodes.len(), 2);
        let install = outcome.state.install.expect("install populated");
        assert_eq!(install.environment, Environment::Mainnet);
        assert_eq!(install.node_count, 2);
        assert_eq!(install.kind, InstallKind::Validators);
    }

    #[test]
    fn analyze_drift_is_surfaced_and_not_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let systemd = tmp.path().join("systemd");
        std::fs::create_dir_all(&systemd).unwrap();
        let paths = paths_for(tmp.path());
        write_node_unit(&systemd, NodeIndex::new(0), &paths);

        // Different user in the inputs → drift.
        let mut input = inputs(&paths);
        let custom_user = "validator".to_string();
        let mut altered_paths = paths.clone();
        altered_paths.custom_user = custom_user;
        input.paths = &altered_paths;

        let outcome = analyze(&systemd, &input, "mxnode/test").unwrap();
        assert!(!outcome.is_clean());
        assert_eq!(outcome.drift_reports().count(), 1);
    }

    #[test]
    fn analyze_records_drop_ins_as_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let systemd = tmp.path().join("systemd");
        std::fs::create_dir_all(&systemd).unwrap();
        let paths = paths_for(tmp.path());
        write_node_unit(&systemd, NodeIndex::new(0), &paths);
        let drop_dir = systemd.join("elrond-node-0.service.d");
        std::fs::create_dir(&drop_dir).unwrap();
        std::fs::write(drop_dir.join("override.conf"), "[Service]\nNice=10\n").unwrap();

        let outcome = analyze(&systemd, &inputs(&paths), "mxnode/test").unwrap();
        assert!(outcome.has_drop_ins());
        assert!(!outcome.is_clean(), "drop-ins must prevent clean adoption");
    }
}
