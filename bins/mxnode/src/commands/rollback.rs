//! `mxnode rollback --node N --to-tag T`: repoint the node's `node`
//! symlink at a previously-kept binary tag and restart the unit.
//!
//! Refuses if:
//!   - the requested tag isn't in the binary store (operator must keep
//!     `binary_keep` >= the rollback target's age)
//!   - state.toml is missing (run `mxnode adopt` first)
//!
//! Records a `migrations.entries[]` row with `result = "rolled-back"` so
//! `mxnode status` and any future audit log can surface the action.

use std::path::PathBuf;
use std::sync::Arc;

use mxnode_core::{NodeIndex, NodeState, Tag};
use mxnode_state::{swap_symlink, BinStore, StateStore};
use mxnode_systemd::Ctl;
use serde::Serialize;

use crate::cli::{GlobalArgs, RollbackArgs};
use crate::errors::CliError;
use crate::events::{node_op_end, node_op_start, Outcome};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: RollbackArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let target_tag = args
        .to_tag
        .as_deref()
        .ok_or_else(|| {
            CliError::new(
                "--to-tag is required",
                "rollback needs the operator to name the kept binary tag explicitly",
                "run `mxnode status` to see available tags, then rerun with --to-tag <T>",
            )
            .json_if(global.json)
        })?;
    let target_tag: Tag = target_tag.parse().map_err(|e: mxnode_core::Error| {
        CliError::new(
            "invalid --to-tag",
            e.to_string(),
            "supply a valid version tag (e.g. v1.7.13)",
        )
        .json_if(global.json)
    })?;

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

    if args.node.is_empty() {
        return Err(CliError::new(
            "no --node specified",
            "rollback always targets explicit nodes",
            "rerun with `--node N` (repeat for more)",
        )
        .json_if(global.json));
    }

    let bin_store = BinStore::new(runtime.paths.binaries.clone());
    let binary_path = bin_store.binary_path("node", target_tag.as_str());
    if !binary_path.exists() {
        return Err(CliError::new(
            "no such kept binary",
            format!(
                "expected {} (run `ls {}/node/`)",
                binary_path.display(),
                bin_store.root().display(),
            ),
            "tag must already exist in the binary store; mxnode does not download for rollback",
        )
        .json_if(global.json));
    }

    let targets: Vec<&NodeState> = state
        .nodes
        .iter()
        .filter(|n| args.node.contains(&n.index.get()))
        .collect();
    let known: Vec<u16> = state.nodes.iter().map(|n| n.index.get()).collect();
    let missing: Vec<u16> = args
        .node
        .iter()
        .copied()
        .filter(|i| !known.contains(i))
        .collect();
    if !missing.is_empty() {
        return Err(CliError::new(
            "no such node",
            format!("state.toml has no node(s) at index {missing:?}"),
            "run `mxnode status` to list valid indices",
        )
        .json_if(global.json));
    }

    let ctl = crate::orchestrator::supervisor::build_supervisor();
    let mut results: Vec<NodeResult> = Vec::with_capacity(targets.len());
    let target_indices: Vec<NodeIndex> = targets.iter().map(|n| n.index).collect();

    for node in &targets {
        let result = perform_rollback(&ctl, &bin_store, node, &target_tag).await;
        let (ok, error) = match &result {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.clone())),
        };
        results.push(NodeResult {
            index: node.index.get(),
            unit: node.unit.clone(),
            to_tag: target_tag.to_string(),
            ok,
            error,
        });
    }

    // Record the migration entry. Even on partial failure we record what
    // we did so the operator can audit what's actually deployed.
    let now = time::OffsetDateTime::now_utc();
    let nodes_done: Vec<NodeIndex> = results
        .iter()
        .zip(targets.iter())
        .filter_map(|(r, n)| if r.ok { Some(n.index) } else { None })
        .collect();
    let nodes_failed: Vec<NodeIndex> = results
        .iter()
        .zip(targets.iter())
        .filter_map(|(r, n)| if !r.ok { Some(n.index) } else { None })
        .collect();
    let result_label = if nodes_failed.is_empty() {
        mxnode_core::state::MigrationResult::RolledBack
    } else if nodes_done.is_empty() {
        mxnode_core::state::MigrationResult::Partial
    } else {
        mxnode_core::state::MigrationResult::Partial
    };
    state
        .migrations
        .entries
        .push(mxnode_core::state::MigrationEntry {
            at: now,
            from_config: None,
            to_config: None,
            from_binary: None,
            to_binary: Some(target_tag.clone()),
            strategy: "rollback".to_string(),
            trigger: "cli".to_string(),
            result: result_label,
            duration_secs: 0,
            nodes_done,
            nodes_failed: nodes_failed.clone(),
        });

    // Persist the migration log under a single lock acquisition.
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

    let _ = target_indices; // kept for future per-node post-action verification

    let any_failed = !nodes_failed.is_empty();
    if global.json {
        let mut payload = serde_json::to_value(&RollbackReport {
            to_tag: target_tag.to_string(),
            nodes: results.clone(),
        })
        .unwrap_or(serde_json::Value::Null);
        if any_failed {
            payload["error"] = serde_json::json!({
                "summary": "rollback reported failures",
                "cause": format!("{} node(s) failed", nodes_failed.len()),
                "try": "inspect `mxnode logs --node N` for the failing index",
            });
        }
        println!("{payload}");
    } else {
        for r in &results {
            let glyph = if r.ok { "✓" } else { "✗" };
            print!("{glyph} rollback node-{} → {}", r.index, r.to_tag);
            if let Some(err) = &r.error {
                print!(" — {err}");
            }
            println!();
        }
    }

    if any_failed {
        return Err(CliError::new(
            "rollback reported failures",
            format!("{} node(s) failed", nodes_failed.len()),
            "inspect logs and rerun for the failing indices once the underlying issue is fixed",
        )
        .silent());
    }
    Ok(())
}

async fn perform_rollback(
    ctl: &Arc<dyn Ctl>,
    bin_store: &BinStore,
    node: &NodeState,
    target_tag: &Tag,
) -> Result<(), String> {
    let unit = node.unit.as_str();
    node_op_start("rollback", node.index, unit);
    let target_path = bin_store.binary_path("node", target_tag.as_str());
    let symlink = node.workdir.join("node");

    // Stop first; swapping a symlink under a running process is
    // semantically a no-op (kernel keeps the old inode mapped) and would
    // surprise the operator on the next manual restart.
    if let Err(e) = ctl.stop(unit).await {
        node_op_end(
            "rollback",
            node.index,
            unit,
            Outcome::Fail { cause: &e.to_string() },
        );
        return Err(format!("systemctl stop failed: {e}"));
    }

    if let Err(e) = swap_symlink(&symlink, &target_path) {
        node_op_end(
            "rollback",
            node.index,
            unit,
            Outcome::Fail { cause: &e.to_string() },
        );
        return Err(format!("symlink swap failed: {e}"));
    }

    if let Err(e) = ctl.start(unit).await {
        node_op_end(
            "rollback",
            node.index,
            unit,
            Outcome::Fail { cause: &e.to_string() },
        );
        return Err(format!("systemctl start failed: {e}"));
    }

    node_op_end("rollback", node.index, unit, Outcome::Ok);
    Ok(())
}

#[derive(Debug, Serialize)]
struct RollbackReport {
    to_tag: String,
    nodes: Vec<NodeResult>,
}

#[derive(Debug, Clone, Serialize)]
struct NodeResult {
    index: u16,
    unit: String,
    to_tag: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// Silence unused-warning until upgrade orchestration consumes this
#[allow(dead_code)]
fn _binary_path_unused(p: PathBuf) {
    drop(p);
}
