//! `mxnode upgrade [--config-tag T --binary-tag T --proxy-tag T]
//!  [--strategy rolling|parallel] [--max-parallel N] [--select <expr>]
//!  [--skip-validators] [--dry-run] [--resume | --abandon]`
//!
//! The orchestration spine for upgrading nodes to a new tag set.
//! Implements the per-step transaction log (D12) so a crash mid-flight
//! can be resumed via `--resume` or abandoned via `--abandon`.
//!
//! Phase 2a scope:
//!   - resolve target tags (CLI flags > [overrides] > GitHub-latest stub)
//!   - acquire upgrade.lock + write inflight.toml
//!   - per-node rolling steps: stop → tomledit → swap symlink → start
//!   - record migrations entry on completion or rollback
//!   - `--resume` continues from the recorded `current_step`
//!   - `--abandon` writes `result = "partial"` and clears inflight
//!
//! Out of scope (Phase 2b):
//!   - real source-build (`go build`) acquirer
//!   - real release-artifact downloader with sha256/signature check
//!   - actual nonce-based readiness probe (we wait a small fixed
//!     interval instead and trust `is-active` as a smoke check)

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mxnode_core::{
    state::{MigrationEntry, MigrationResult},
    NodeIndex, NodeState, State, Tag,
};
use mxnode_rpc::NodeClient;
use mxnode_state::{
    inflight_path, swap_symlink, BinStore, Inflight, InflightCheck, InflightStep, OpKind,
    ProcessIdentity, StateStore,
};
use mxnode_systemd::Ctl; // trait used by upgrade_one_node param
use serde::Serialize;

use crate::cli::{GlobalArgs, Strategy, UpgradeArgs, UpgradeTarget};
use crate::errors::CliError;
use crate::events::{node_op_end, node_op_start, Outcome};
use crate::orchestrator::acquirer::{
    AcquireError, Artifact, BinaryAcquirer, SourceBuildAcquirer,
};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: UpgradeArgs, global: &GlobalArgs) -> Result<(), CliError> {
    if let Some(UpgradeTarget::Proxy { proxy_tag }) = &args.target {
        return upgrade_proxy(proxy_tag.clone(), &args, global).await;
    }
    let is_squad = matches!(args.target, Some(UpgradeTarget::Squad));

    if args.resume && args.abandon {
        // clap already enforces this via conflicts_with, but defensive.
        return Err(CliError::new(
            "--resume and --abandon are mutually exclusive",
            "choose exactly one",
            "rerun with the option you want",
        )
        .json_if(global.json));
    }

    let runtime = Runtime::from_global(global)?;

    // Detect any in-flight op before we touch anything else.
    let inflight_loc = inflight_path(&runtime.paths.state);
    let identity = ProcessIdentity::current();
    let check = InflightCheck::from_path(&inflight_loc, identity).map_err(|e| {
        CliError::new(
            "failed to read inflight.toml",
            e.to_string(),
            "remove the file manually if it's corrupt",
        )
        .json_if(global.json)
    })?;

    match check {
        InflightCheck::Live { other_pid, .. } => {
            return Err(CliError::new(
                format!("another mxnode upgrade is running (pid {other_pid})"),
                "inflight.toml records a live process",
                "wait for it to finish, or run `mxnode unlock --force` after confirming it's dead",
            )
            .json_if(global.json));
        }
        InflightCheck::StaleFromDeadProcess(prev) | InflightCheck::Indeterminate(prev) => {
            return handle_stale_inflight(prev, args, global, &runtime).await;
        }
        InflightCheck::Clear => {}
    }

    if args.resume {
        return Err(CliError::new(
            "nothing to resume",
            "no inflight.toml found",
            "rerun without --resume to start a fresh upgrade",
        )
        .json_if(global.json));
    }
    if args.abandon {
        return Err(CliError::new(
            "nothing to abandon",
            "no inflight.toml found",
            "the abandon flag is only meaningful when a prior upgrade crashed mid-flight",
        )
        .json_if(global.json));
    }

    let _ = ResumePoint::Fresh; // silence dead-code on the value form

    let store = StateStore::new(&runtime.paths.state);
    let state = store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read state.toml",
                e.to_string(),
                "run `mxnode adopt` first",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no state.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode adopt` first",
            )
            .json_if(global.json)
        })?;

    let mut plan = build_plan(&args, &state, &runtime, global).await?;
    plan.is_squad = is_squad;

    if args.dry_run {
        return emit_dry_run(&plan, global);
    }

    // Persist the inflight.toml. From here on, every step writes back
    // a step marker so a crash can be picked up by `--resume`.
    let mut inflight = Inflight::new(OpKind::Upgrade, strategy_label(args.strategy), plan.selected.clone());
    inflight.target_binary_tag = Some(plan.binary_tag.clone());
    inflight.target_config_tag = plan.config_tag.clone();
    inflight.target_proxy_tag = plan.proxy_tag.clone();
    inflight.save(&inflight_loc).map_err(|e| {
        CliError::new(
            "failed to write inflight.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;

    let outcome = execute_upgrade(
        &plan,
        &state,
        &runtime,
        &inflight_loc,
        &mut inflight,
        ResumePoint::Fresh,
        global,
    )
    .await;

    // Always clear the inflight on a clean run; `execute_upgrade` returns
    // the migration entry plus per-node results regardless of partial
    // failure status.
    let result = persist_migration(&store, outcome, &inflight_loc, global);
    result
}

struct Plan {
    selected: Vec<NodeIndex>,
    binary_tag: Tag,
    config_tag: Option<Tag>,
    proxy_tag: Option<Tag>,
    strategy: Strategy,
    skip_validators: bool,
    /// Apply observer-squad-specific tomledit edits during ConfigApplied.
    /// Set when the operator runs `mxnode upgrade squad`.
    is_squad: bool,
}

async fn build_plan(
    args: &UpgradeArgs,
    state: &State,
    runtime: &Runtime,
    global: &GlobalArgs,
) -> Result<Plan, CliError> {
    use crate::orchestrator::tag_resolver::{
        resolve_binary_tag, resolve_config_tag, resolve_proxy_tag,
    };

    // Binary: priority CLI > [overrides] > GitHub-latest. We deliberately
    // skip the state.toml fallback for the *target* tag — staying on the
    // currently-installed version isn't an upgrade. Operators who want to
    // redeploy the same tag pass --binary-tag explicitly.
    let binary = resolve_binary_tag(runtime, args.binary_tag.as_deref())
        .await
        .map_err(|e| crate::commands::install::resolve_err(e, global))?;
    crate::commands::install::announce_resolved(global, "binary", &binary);
    let binary_tag = binary.tag;

    // Config + proxy are optional during an upgrade — `None` means "leave
    // untouched". Resolve only when the operator actually passed the flag
    // OR set [overrides], otherwise the upgrade flow doesn't touch them.
    let environment = state.install.as_ref().map(|i| i.environment);
    let config_tag = if args.config_tag.is_some()
        || runtime.loaded.config.overrides.configver().is_some()
    {
        let env = environment.ok_or_else(|| {
            CliError::new(
                "cannot resolve --config-tag without an environment",
                "state.install.environment is missing",
                "run `mxnode rebuild-state` or pass --config-tag explicitly",
            )
            .json_if(global.json)
        })?;
        let r = resolve_config_tag(runtime, env, args.config_tag.as_deref())
            .await
            .map_err(|e| crate::commands::install::resolve_err(e, global))?;
        crate::commands::install::announce_resolved(global, "config", &r);
        Some(r.tag)
    } else {
        None
    };
    let proxy_tag = if args.proxy_tag.is_some()
        || runtime.loaded.config.overrides.proxyver().is_some()
    {
        let r = resolve_proxy_tag(runtime, args.proxy_tag.as_deref())
            .await
            .map_err(|e| crate::commands::install::resolve_err(e, global))?;
        crate::commands::install::announce_resolved(global, "proxy", &r);
        Some(r.tag)
    } else {
        None
    };

    // Selection: default to all nodes when no --select is supplied. The
    // bash `upgrade` command also acts on all nodes by default.
    let selected: Vec<NodeIndex> = if let Some(expr) = &args.select {
        use crate::orchestrator::selector::{resolve, DefaultSelection};
        let lifecycle_args = crate::cli::LifecycleArgs {
            all: false,
            select: Some(expr.clone()),
            validators_only: false,
            observers_only: false,
            shard: None,
            node: Vec::new(),
        };
        resolve(state, &lifecycle_args, DefaultSelection::All).map_err(|e| {
            CliError::new(
                "invalid --select",
                e.to_string(),
                "see `mxnode status` for valid selectors",
            )
            .json_if(global.json)
        })?
    } else {
        let mut v: Vec<NodeIndex> = state.nodes.iter().map(|n| n.index).collect();
        v.sort();
        v
    };

    if selected.is_empty() {
        return Err(CliError::new(
            "selector matched zero nodes",
            "no nodes to upgrade",
            "run `mxnode status` to see what's installed",
        )
        .json_if(global.json));
    }

    Ok(Plan {
        selected,
        binary_tag,
        config_tag,
        proxy_tag,
        strategy: args.strategy,
        skip_validators: args.skip_validators,
        is_squad: false,
    })
}

fn strategy_label(s: Strategy) -> String {
    match s {
        Strategy::Rolling => "rolling".to_string(),
        Strategy::Parallel => "parallel".to_string(),
    }
}

fn emit_dry_run(plan: &Plan, global: &GlobalArgs) -> Result<(), CliError> {
    if global.json {
        let payload = serde_json::json!({
            "mode": "dry-run",
            "binary_tag": plan.binary_tag.to_string(),
            "config_tag": plan.config_tag.as_ref().map(|t| t.to_string()),
            "proxy_tag": plan.proxy_tag.as_ref().map(|t| t.to_string()),
            "strategy": strategy_label(plan.strategy),
            "skip_validators": plan.skip_validators,
            "selected": plan.selected.iter().map(|i| i.get()).collect::<Vec<_>>(),
        });
        println!("{payload}");
    } else {
        println!("dry-run upgrade plan:");
        println!("  binary_tag: {}", plan.binary_tag);
        if let Some(t) = &plan.config_tag {
            println!("  config_tag: {t}");
        }
        if let Some(t) = &plan.proxy_tag {
            println!("  proxy_tag:  {t}");
        }
        println!("  strategy:   {}", strategy_label(plan.strategy));
        println!(
            "  selected:   {:?}",
            plan.selected.iter().map(|i| i.get()).collect::<Vec<_>>()
        );
        if plan.skip_validators {
            println!("  skip_validators: true");
        }
    }
    Ok(())
}

/// Where the orchestrator should pick up from.
#[derive(Debug, Clone)]
enum ResumePoint {
    /// Fresh upgrade — start at the first node from the beginning.
    Fresh,
    /// Resuming a prior in-flight op. Skip nodes already in
    /// `completed`; for the recorded `current` node, restart at
    /// `current_step`. Steps are idempotent so re-running a step that
    /// already completed is a no-op (e.g. `systemctl stop` on an already
    /// stopped unit).
    From {
        completed: Vec<NodeIndex>,
        current: Option<NodeIndex>,
        step: InflightStep,
    },
}

struct UpgradeOutcome {
    binary_tag: Tag,
    started_at: time::OffsetDateTime,
    duration_secs: u64,
    nodes_done: Vec<NodeIndex>,
    nodes_failed: Vec<NodeIndex>,
    rolled_back: bool,
    per_node: Vec<NodeResult>,
}

#[derive(Debug, Clone, Serialize)]
struct NodeResult {
    index: u16,
    unit: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn execute_upgrade(
    plan: &Plan,
    state: &State,
    runtime: &Runtime,
    inflight_loc: &PathBuf,
    inflight: &mut Inflight,
    resume: ResumePoint,
    _global: &GlobalArgs,
) -> UpgradeOutcome {
    let started = time::OffsetDateTime::now_utc();
    let bin_store = BinStore::new(runtime.paths.binaries.clone());
    // Workdir for source-build clones; lives alongside the binary store
    // so an operator inspecting `<custom_home>/mxnode/build/` sees what
    // we cloned and built last.
    let build_workdir = runtime.paths.custom_home.join("mxnode/build");
    let acquirer: Arc<dyn BinaryAcquirer> = Arc::new(SourceBuildAcquirer::new(
        runtime.loaded.config.network.github_org.clone(),
        build_workdir,
    ));
    let ctl = crate::orchestrator::supervisor::build_supervisor();

    // Acquire the binary once; reuse for every node.
    let acquired = acquirer
        .acquire(Artifact::Node, &plan.binary_tag)
        .await;
    let acquired_path = match acquired {
        Ok(p) => p,
        Err(AcquireError::NotImplemented(reason)) => {
            // Phase 2b stub. Leave inflight.toml in place so `--abandon`
            // works, return a partial-failure outcome with no nodes done.
            return UpgradeOutcome {
                binary_tag: plan.binary_tag.clone(),
                started_at: started,
                duration_secs: 0,
                nodes_done: Vec::new(),
                nodes_failed: plan.selected.clone(),
                rolled_back: false,
                per_node: plan
                    .selected
                    .iter()
                    .map(|i| NodeResult {
                        index: i.get(),
                        unit: format!("elrond-node-{}.service", i.get()),
                        ok: false,
                        error: Some(format!("acquire: {reason}")),
                    })
                    .collect(),
            };
        }
        Err(e) => {
            return UpgradeOutcome {
                binary_tag: plan.binary_tag.clone(),
                started_at: started,
                duration_secs: 0,
                nodes_done: Vec::new(),
                nodes_failed: plan.selected.clone(),
                rolled_back: false,
                per_node: plan
                    .selected
                    .iter()
                    .map(|i| NodeResult {
                        index: i.get(),
                        unit: format!("elrond-node-{}.service", i.get()),
                        ok: false,
                        error: Some(format!("acquire: {e}")),
                    })
                    .collect(),
            };
        }
    };

    let installed_path = match bin_store.install_binary("node", plan.binary_tag.as_str(), &acquired_path) {
        Ok(p) => p,
        Err(e) => {
            return UpgradeOutcome {
                binary_tag: plan.binary_tag.clone(),
                started_at: started,
                duration_secs: 0,
                nodes_done: Vec::new(),
                nodes_failed: plan.selected.clone(),
                rolled_back: false,
                per_node: plan
                    .selected
                    .iter()
                    .map(|i| NodeResult {
                        index: i.get(),
                        unit: format!("elrond-node-{}.service", i.get()),
                        ok: false,
                        error: Some(format!("install_binary: {e}")),
                    })
                    .collect(),
            };
        }
    };

    let mut nodes_done: Vec<NodeIndex> = Vec::new();
    let mut nodes_failed: Vec<NodeIndex> = Vec::new();
    let mut per_node: Vec<NodeResult> = Vec::new();

    // Resolve resume position: which nodes do we skip outright, and at
    // what step do we re-enter for the in-flight node?
    let (skip_set, mut current_node_step): (Vec<NodeIndex>, Option<(NodeIndex, InflightStep)>) =
        match resume {
            ResumePoint::Fresh => (Vec::new(), None),
            ResumePoint::From {
                completed,
                current,
                step,
            } => (completed, current.map(|c| (c, step))),
        };

    // Pre-populate per_node + nodes_done for skipped nodes so the
    // migrations entry reflects what the previous run finished.
    for idx in &skip_set {
        let unit = state
            .nodes
            .iter()
            .find(|n| n.index == *idx)
            .map(|n| n.unit.clone())
            .unwrap_or_else(|| format!("elrond-node-{}.service", idx.get()));
        nodes_done.push(*idx);
        per_node.push(NodeResult {
            index: idx.get(),
            unit,
            ok: true,
            error: Some("skipped (already completed in a prior run)".to_string()),
        });
    }

    for idx in &plan.selected {
        if skip_set.contains(idx) {
            continue;
        }
        let Some(node) = state.nodes.iter().find(|n| n.index == *idx) else {
            nodes_failed.push(*idx);
            per_node.push(NodeResult {
                index: idx.get(),
                unit: format!("elrond-node-{}.service", idx.get()),
                ok: false,
                error: Some("node not found in state".to_string()),
            });
            continue;
        };
        inflight.current = Some(node.index);
        // Restart from the recorded step ONLY for the matching node.
        let starting_step = match current_node_step {
            Some((cur, step)) if cur == node.index => {
                current_node_step = None; // consume — fresh after this
                step
            }
            _ => InflightStep::Resolving,
        };
        inflight.current_step = starting_step;
        let _ = inflight.save(inflight_loc);

        match upgrade_one_node(&ctl, state, node, &installed_path, starting_step, inflight, inflight_loc, plan.is_squad).await {
            Ok(()) => {
                nodes_done.push(node.index);
                per_node.push(NodeResult {
                    index: node.index.get(),
                    unit: node.unit.clone(),
                    ok: true,
                    error: None,
                });
                inflight.completed.push(node.index);
                let _ = inflight.save(inflight_loc);
            }
            Err(e) => {
                nodes_failed.push(node.index);
                per_node.push(NodeResult {
                    index: node.index.get(),
                    unit: node.unit.clone(),
                    ok: false,
                    error: Some(e),
                });
                // First-failure stops the rolling sequence per plan §"Upgrade flow".
                break;
            }
        }
    }

    let duration_secs = (time::OffsetDateTime::now_utc() - started)
        .whole_seconds()
        .max(0) as u64;

    UpgradeOutcome {
        binary_tag: plan.binary_tag.clone(),
        started_at: started,
        duration_secs,
        nodes_done,
        nodes_failed,
        rolled_back: false,
        per_node,
    }
}

async fn upgrade_one_node(
    ctl: &Arc<dyn Ctl>,
    state: &State,
    node: &NodeState,
    installed_binary: &PathBuf,
    starting_step: InflightStep,
    inflight: &mut Inflight,
    inflight_loc: &PathBuf,
    is_squad: bool,
) -> Result<(), String> {
    use InflightStep::*;
    node_op_start("upgrade", node.index, &node.unit);

    // Each step is idempotent and runs only when the resume position is
    // at-or-before it. `step_le` defines the per-node execution order.
    if step_le(&starting_step, &Stopped) {
        inflight.current_step = Stopped;
        let _ = inflight.save(inflight_loc);
        if let Err(e) = ctl.stop(&node.unit).await {
            let cause = e.to_string();
            node_op_end("upgrade", node.index, &node.unit, Outcome::Fail { cause: &cause });
            return Err(format!("systemctl stop failed: {cause}"));
        }
    }

    if step_le(&starting_step, &ConfigApplied) {
        inflight.current_step = ConfigApplied;
        let _ = inflight.save(inflight_loc);
        if is_squad {
            apply_squad_config_edits(node).map_err(|e| {
                let cause = format!("squad config edits: {e}");
                node_op_end(
                    "upgrade",
                    node.index,
                    &node.unit,
                    Outcome::Fail { cause: &cause },
                );
                cause
            })?;
        }
    }

    if step_le(&starting_step, &BinaryReplaced) {
        inflight.current_step = BinaryReplaced;
        let _ = inflight.save(inflight_loc);
        let symlink = node.workdir.join("node");
        if let Err(e) = swap_symlink(&symlink, installed_binary) {
            let cause = e.to_string();
            node_op_end("upgrade", node.index, &node.unit, Outcome::Fail { cause: &cause });
            return Err(format!("symlink swap failed: {cause}"));
        }
    }

    if step_le(&starting_step, &Started) {
        inflight.current_step = Started;
        let _ = inflight.save(inflight_loc);
        if let Err(e) = ctl.start(&node.unit).await {
            let cause = e.to_string();
            node_op_end("upgrade", node.index, &node.unit, Outcome::Fail { cause: &cause });
            return Err(format!("systemctl start failed: {cause}"));
        }
    }

    if step_le(&starting_step, &NonceVerified) {
        inflight.current_step = NonceVerified;
        let _ = inflight.save(inflight_loc);
        // Readiness probe: wait for the node's nonce to be within K of
        // the highest known network nonce among siblings, OR for the
        // node to report `erd_is_syncing == 0` with a non-zero nonce.
        // We don't penalise upgrades on single-node hosts where there's
        // no sibling to compare against — the start succeeding plus an
        // active unit is enough.
        if let Err(e) = wait_for_node_ready(state, node).await {
            let cause = format!("readiness probe: {e}");
            node_op_end("upgrade", node.index, &node.unit, Outcome::Fail { cause: &cause });
            return Err(cause);
        }
    }

    node_op_end("upgrade", node.index, &node.unit, Outcome::Ok);
    Ok(())
}

/// Apply observer-squad-specific TOML edits to the node's config dir.
/// Mirrors the bash `observers()` flow: enable `[DbLookupExtensions]` in
/// `config.toml` and pin `DestinationShardAsObserver` in `prefs.toml`.
fn apply_squad_config_edits(node: &NodeState) -> Result<(), String> {
    use mxnode_systemd::{enable_db_lookup_extensions, set_destination_shard};
    use toml_edit::DocumentMut;

    let config_path = node.workdir.join("config/config.toml");
    if config_path.exists() {
        let body = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("read {}: {e}", config_path.display()))?;
        let mut doc: DocumentMut = body
            .parse()
            .map_err(|e| format!("parse {}: {e}", config_path.display()))?;
        enable_db_lookup_extensions(&mut doc).map_err(|e| e.to_string())?;
        std::fs::write(&config_path, doc.to_string())
            .map_err(|e| format!("write {}: {e}", config_path.display()))?;
    }

    let prefs_path = node.workdir.join("config/prefs.toml");
    if prefs_path.exists() {
        let body = std::fs::read_to_string(&prefs_path)
            .map_err(|e| format!("read {}: {e}", prefs_path.display()))?;
        let mut doc: DocumentMut = body
            .parse()
            .map_err(|e| format!("parse {}: {e}", prefs_path.display()))?;
        set_destination_shard(&mut doc, node.shard).map_err(|e| e.to_string())?;
        std::fs::write(&prefs_path, doc.to_string())
            .map_err(|e| format!("write {}: {e}", prefs_path.display()))?;
    }
    Ok(())
}

/// Total ordering of upgrade steps; `step_le(a, b)` is true when step
/// `a` happens before-or-at `b`.
fn step_le(a: &InflightStep, b: &InflightStep) -> bool {
    use InflightStep::*;
    fn rank(s: &InflightStep) -> u8 {
        match s {
            Resolving => 0,
            Stopped => 1,
            ConfigApplied => 2,
            BinaryReplaced => 3,
            Started => 4,
            NonceVerified => 5,
        }
    }
    rank(a) <= rank(b)
}

/// Probe the node's local REST API until either:
///   - `erd_nonce` is within `K` of the highest sibling's
///     `erd_probable_highest_nonce`, OR
///   - `erd_is_syncing == 0` and `erd_nonce > 0`, OR
///   - the timeout expires (returns Err)
const NONCE_LAG_TOLERANCE: u64 = 5;
const NONCE_PROBE_TIMEOUT_SECS: u64 = 5 * 60;
const NONCE_POLL_INTERVAL_SECS: u64 = 3;

async fn wait_for_node_ready(state: &State, node: &NodeState) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(NONCE_PROBE_TIMEOUT_SECS);
    let target = NodeClient::new("127.0.0.1", node.api_port)
        .map_err(|e| format!("rpc client init: {e}"))?;

    loop {
        let now = std::time::Instant::now();
        if now > deadline {
            return Err(format!(
                "node-{} did not reach ready state within {NONCE_PROBE_TIMEOUT_SECS}s",
                node.index.get(),
            ));
        }

        let target_status = match tokio::time::timeout(Duration::from_secs(2), target.status()).await {
            Ok(Ok(s)) => Some(s),
            _ => None,
        };

        if let Some(status) = target_status {
            let nonce = status.data.metrics.erd_nonce.unwrap_or(0);
            let is_syncing = status.data.metrics.erd_is_syncing.unwrap_or(1);
            if is_syncing == 0 && nonce > 0 {
                let network = highest_sibling_nonce(state, node).await;
                match network {
                    Some(net) if nonce + NONCE_LAG_TOLERANCE >= net => return Ok(()),
                    Some(net) => {
                        tracing::debug!(
                            target: "mxnode.event",
                            event = "upgrade.lag",
                            node = node.index.get(),
                            nonce,
                            network = net,
                        );
                    }
                    None => return Ok(()),
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(NONCE_POLL_INTERVAL_SECS)).await;
    }
}

/// Highest `erd_probable_highest_nonce` reported by any sibling node
/// (excluding `me`). Returns `None` when no sibling responds — the
/// caller treats that as "no comparison available, accept current state".
async fn highest_sibling_nonce(state: &State, me: &NodeState) -> Option<u64> {
    let mut highest: Option<u64> = None;
    for node in &state.nodes {
        if node.index == me.index {
            continue;
        }
        let Ok(client) = NodeClient::new("127.0.0.1", node.api_port) else {
            continue;
        };
        let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(2), client.status()).await
        else {
            continue;
        };
        let probable = status.data.metrics.erd_nonce.unwrap_or(0);
        if probable > 0 {
            highest = Some(highest.map_or(probable, |h| h.max(probable)));
        }
    }
    highest
}

fn persist_migration(
    store: &StateStore,
    outcome: UpgradeOutcome,
    inflight_loc: &PathBuf,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let entry = MigrationEntry {
        at: outcome.started_at,
        from_config: None,
        to_config: None,
        from_binary: None,
        to_binary: Some(outcome.binary_tag.clone()),
        strategy: "rolling".to_string(),
        trigger: "cli".to_string(),
        result: if outcome.rolled_back {
            MigrationResult::RolledBack
        } else if outcome.nodes_failed.is_empty() {
            MigrationResult::Ok
        } else {
            MigrationResult::Partial
        },
        duration_secs: outcome.duration_secs,
        nodes_done: outcome.nodes_done.clone(),
        nodes_failed: outcome.nodes_failed.clone(),
    };

    let guard = store.lock().map_err(|e| {
        CliError::new(
            "failed to lock state",
            e.to_string(),
            "another mxnode op may be running",
        )
        .json_if(global.json)
    })?;
    let mut state = match store.load() {
        Ok(Some(s)) => s,
        Ok(None) => {
            drop(guard);
            return Err(CliError::new(
                "state.toml went missing mid-upgrade",
                "expected the file we loaded earlier",
                "rerun `mxnode adopt` then retry",
            )
            .json_if(global.json));
        }
        Err(e) => {
            drop(guard);
            return Err(CliError::new(
                "failed to reload state.toml",
                e.to_string(),
                "remove the file manually if it's corrupt",
            )
            .json_if(global.json));
        }
    };
    state.migrations.entries.push(entry);
    if !outcome.nodes_done.is_empty() {
        // Bump the recorded binary tag so `mxnode status` reflects what's
        // actually deployed. `nodes_failed` stay on the previous tag —
        // `from_binary` of a future entry will read it from disk.
        if let Some(install) = state.install.as_mut() {
            install.versions.binary_tag = Some(outcome.binary_tag.clone());
        }
    }
    store.save(&state, &guard).map_err(|e| {
        CliError::new(
            "failed to write state.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;
    drop(guard);

    // Clear inflight.toml on terminal completion (success OR partial — the
    // operator can resume by running upgrade again with the same flags).
    let _ = Inflight::clear(inflight_loc);

    let any_failed = !outcome.nodes_failed.is_empty();
    if global.json {
        let mut payload = serde_json::json!({
            "ok": !any_failed,
            "binary_tag": outcome.binary_tag.to_string(),
            "duration_secs": outcome.duration_secs,
            "nodes_done": outcome.nodes_done.iter().map(|i| i.get()).collect::<Vec<_>>(),
            "nodes_failed": outcome.nodes_failed.iter().map(|i| i.get()).collect::<Vec<_>>(),
            "per_node": outcome.per_node,
        });
        if any_failed {
            payload["error"] = serde_json::json!({
                "summary": "upgrade reported failures",
                "cause": format!("{} of {} node(s) failed", outcome.nodes_failed.len(), outcome.per_node.len()),
                "try": "fix the failing nodes manually, then rerun for them",
            });
        }
        println!("{payload}");
    } else {
        for r in &outcome.per_node {
            let glyph = if r.ok { "✓" } else { "✗" };
            print!("{glyph} upgrade node-{}", r.index);
            if let Some(err) = &r.error {
                print!(" — {err}");
            }
            println!();
        }
    }

    if any_failed {
        return Err(CliError::new(
            "upgrade reported failures",
            "see per-node errors above",
            "fix the failing nodes manually, then rerun for them",
        )
        .silent());
    }
    Ok(())
}

async fn handle_stale_inflight(
    inflight: Inflight,
    args: UpgradeArgs,
    global: &GlobalArgs,
    runtime: &Runtime,
) -> Result<(), CliError> {
    let inflight_loc = inflight_path(&runtime.paths.state);

    if args.abandon {
        // Mark the previous run as partial in migrations[] and drop
        // inflight.toml so the next invocation starts cleanly.
        let store = StateStore::new(&runtime.paths.state);
        let guard = store.lock().map_err(|e| {
            CliError::new(
                "failed to lock state",
                e.to_string(),
                "another mxnode op may be running",
            )
            .json_if(global.json)
        })?;
        let mut state = match store.load() {
            Ok(Some(s)) => s,
            _ => {
                drop(guard);
                let _ = Inflight::clear(&inflight_loc);
                return Ok(());
            }
        };
        state.migrations.entries.push(MigrationEntry {
            at: time::OffsetDateTime::now_utc(),
            from_config: None,
            to_config: None,
            from_binary: None,
            to_binary: inflight.target_binary_tag.clone(),
            strategy: inflight.strategy.clone(),
            trigger: "abandon".to_string(),
            result: MigrationResult::Partial,
            duration_secs: 0,
            nodes_done: inflight.completed.clone(),
            nodes_failed: Vec::new(),
        });
        let _ = store.save(&state, &guard);
        drop(guard);
        let _ = Inflight::clear(&inflight_loc);
        if global.json {
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "abandoned": true,
                    "completed_nodes": inflight.completed.iter().map(|i| i.get()).collect::<Vec<_>>(),
                })
            );
        } else {
            println!(
                "abandoned previous upgrade ({} node(s) had completed)",
                inflight.completed.len(),
            );
        }
        return Ok(());
    }

    if args.resume {
        return resume_upgrade(inflight, runtime, global).await;
    }
    let _ = runtime;

    Err(CliError::new(
        "stale inflight.toml from a prior upgrade",
        format!(
            "previous run stopped at step {:?} on node {:?}; completed: {:?}",
            inflight.current_step,
            inflight.current,
            inflight.completed.iter().map(|i| i.get()).collect::<Vec<_>>(),
        ),
        "rerun with `--resume` to continue from the recorded step, or `--abandon` to clear",
    )
    .json_if(global.json))
}

async fn upgrade_proxy(
    proxy_tag: Option<String>,
    args: &UpgradeArgs,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.state);
    let mut state = store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read state.toml",
                e.to_string(),
                "run `mxnode adopt` first",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no state.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode adopt` first",
            )
            .json_if(global.json)
        })?;

    let proxy = state.proxy.as_ref().cloned().ok_or_else(|| {
        CliError::new(
            "this host has no proxy installed",
            "state.toml has no [proxy] section",
            "run `mxnode observers` or skip; standalone validators don't need a proxy",
        )
        .json_if(global.json)
    })?;

    // Resolve target proxy tag: CLI subcommand value > top-level
    // --proxy-tag > [overrides].proxyver > GitHub-latest. We deliberately
    // skip the state.toml fallback for the *target* — staying on the
    // currently-installed proxy isn't an upgrade.
    let cli_value = proxy_tag.as_deref().or(args.proxy_tag.as_deref());
    let resolved = crate::orchestrator::tag_resolver::resolve_proxy_tag(&runtime, cli_value)
        .await
        .map_err(|e| crate::commands::install::resolve_err(e, global))?;
    crate::commands::install::announce_resolved(global, "proxy", &resolved);
    let target_tag = resolved.tag;

    if args.dry_run {
        if global.json {
            println!(
                "{}",
                serde_json::json!({
                    "mode": "dry-run",
                    "target": "proxy",
                    "proxy_tag": target_tag.to_string(),
                    "unit": proxy.unit,
                })
            );
        } else {
            println!("dry-run upgrade proxy → {target_tag}");
        }
        return Ok(());
    }

    let bin_store = BinStore::new(runtime.paths.binaries.clone());
    let build_workdir = runtime.paths.custom_home.join("mxnode/build");
    let acquirer: Arc<dyn BinaryAcquirer> = Arc::new(SourceBuildAcquirer::new(
        runtime.loaded.config.network.github_org.clone(),
        build_workdir,
    ));
    let acquired = acquirer
        .acquire(Artifact::Proxy, &target_tag)
        .await
        .map_err(|e| {
            CliError::new(
                "failed to acquire proxy binary",
                e.to_string(),
                "ensure git+go are installed, or place the binary manually under \
                 {paths.binaries}/proxy/<tag>/proxy and rerun",
            )
            .json_if(global.json)
        })?;
    let installed = bin_store
        .install_binary("proxy", target_tag.as_str(), &acquired)
        .map_err(|e| {
            CliError::new(
                "failed to install proxy into binary store",
                e.to_string(),
                "ensure {paths.binaries} is writable",
            )
            .json_if(global.json)
        })?;

    let ctl = crate::orchestrator::supervisor::build_supervisor();
    let started = time::OffsetDateTime::now_utc();

    node_op_start("upgrade.proxy", NodeIndex::new(0), &proxy.unit);

    if let Err(e) = ctl.stop(&proxy.unit).await {
        let cause = e.to_string();
        node_op_end(
            "upgrade.proxy",
            NodeIndex::new(0),
            &proxy.unit,
            Outcome::Fail { cause: &cause },
        );
        return Err(CliError::new(
            "systemctl stop failed for proxy",
            cause,
            "inspect `journalctl -u elrond-proxy` for details",
        )
        .json_if(global.json));
    }

    let symlink = proxy.workdir.join("proxy");
    if let Err(e) = swap_symlink(&symlink, &installed) {
        node_op_end(
            "upgrade.proxy",
            NodeIndex::new(0),
            &proxy.unit,
            Outcome::Fail { cause: &e.to_string() },
        );
        return Err(CliError::new(
            "symlink swap failed for proxy",
            e.to_string(),
            "check that {paths.binaries}/proxy/<tag>/proxy and {custom_home}/elrond-proxy/proxy are in the same filesystem",
        )
        .json_if(global.json));
    }

    if let Err(e) = ctl.start(&proxy.unit).await {
        let cause = e.to_string();
        node_op_end(
            "upgrade.proxy",
            NodeIndex::new(0),
            &proxy.unit,
            Outcome::Fail { cause: &cause },
        );
        return Err(CliError::new(
            "systemctl start failed for proxy",
            cause,
            "inspect `journalctl -u elrond-proxy` for details",
        )
        .json_if(global.json));
    }

    node_op_end("upgrade.proxy", NodeIndex::new(0), &proxy.unit, Outcome::Ok);

    state.migrations.entries.push(MigrationEntry {
        at: started,
        from_config: None,
        to_config: None,
        from_binary: state
            .install
            .as_ref()
            .and_then(|i| i.versions.proxy_tag.clone()),
        to_binary: Some(target_tag.clone()),
        strategy: "proxy".to_string(),
        trigger: "cli".to_string(),
        result: MigrationResult::Ok,
        duration_secs: (time::OffsetDateTime::now_utc() - started)
            .whole_seconds()
            .max(0) as u64,
        nodes_done: Vec::new(),
        nodes_failed: Vec::new(),
    });
    if let Some(install) = state.install.as_mut() {
        install.versions.proxy_tag = Some(target_tag.clone());
    }
    let guard = store.lock().map_err(|e| {
        CliError::new(
            "failed to lock state",
            e.to_string(),
            "another mxnode op may be running",
        )
        .json_if(global.json)
    })?;
    store.save(&state, &guard).map_err(|e| {
        CliError::new(
            "failed to write state.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;
    drop(guard);

    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "target": "proxy",
                "proxy_tag": target_tag.to_string(),
                "unit": proxy.unit,
            })
        );
    } else {
        println!("✓ upgrade proxy → {target_tag} ({})", proxy.unit);
    }
    Ok(())
}

async fn resume_upgrade(
    inflight: Inflight,
    runtime: &Runtime,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let target_tag = inflight.target_binary_tag.clone().ok_or_else(|| {
        CliError::new(
            "inflight.toml has no target_binary_tag",
            "cannot resume without a recorded tag",
            "abandon and re-run with explicit --binary-tag",
        )
        .json_if(global.json)
    })?;

    let store = StateStore::new(&runtime.paths.state);
    let state = store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read state.toml",
                e.to_string(),
                "run `mxnode adopt` first",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no state.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode adopt` first",
            )
            .json_if(global.json)
        })?;

    let plan = Plan {
        selected: inflight.selected.clone(),
        binary_tag: target_tag,
        config_tag: inflight.target_config_tag.clone(),
        proxy_tag: inflight.target_proxy_tag.clone(),
        strategy: Strategy::Rolling,
        skip_validators: false,
        is_squad: false,
    };

    let inflight_loc = inflight_path(&runtime.paths.state);
    // Re-stamp the inflight to claim ownership under our identity, then
    // proceed from the recorded step.
    let mut inflight_mut = Inflight::new(
        OpKind::Upgrade,
        inflight.strategy.clone(),
        inflight.selected.clone(),
    );
    inflight_mut.target_binary_tag = inflight.target_binary_tag.clone();
    inflight_mut.target_config_tag = inflight.target_config_tag.clone();
    inflight_mut.target_proxy_tag = inflight.target_proxy_tag.clone();
    inflight_mut.completed = inflight.completed.clone();
    inflight_mut.current = inflight.current;
    inflight_mut.current_step = inflight.current_step;
    inflight_mut.save(&inflight_loc).map_err(|e| {
        CliError::new(
            "failed to update inflight.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;

    let resume = ResumePoint::From {
        completed: inflight.completed.clone(),
        current: inflight.current,
        step: inflight.current_step,
    };

    let outcome = execute_upgrade(
        &plan,
        &state,
        runtime,
        &inflight_loc,
        &mut inflight_mut,
        resume,
        global,
    )
    .await;
    persist_migration(&store, outcome, &inflight_loc, global)
}
