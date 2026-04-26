//! `mxnode db remove|prune`: destructive ops that wipe a node's working
//! data. Both require `--yes`; on a TTY both also require an interactive
//! "type the index" confirm before deleting anything.
//!
//! Refuses if the targeted unit is currently active — mxnode does not
//! pause the node for you because pausing without restarting can cause
//! validator rating loss; the operator must `mxnode stop --node N` first.

use std::io::{IsTerminal, Write};
use std::path::Path;

use mxnode_core::{NodeIndex, NodeState, State};
use mxnode_state::StateStore;
use mxnode_systemd::ActiveState;
use serde::Serialize;

use crate::cli::{DbCommand, GlobalArgs};
use crate::errors::CliError;
use crate::events::{node_op_end, node_op_start, Outcome};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

/// Top-level dispatcher for `mxnode db <subcmd>`.
pub fn run(cmd: DbCommand, global: &GlobalArgs) -> Result<(), CliError> {
    match cmd {
        DbCommand::Remove { node, yes } => run_op(DbVerb::Remove, &node, yes, global),
        DbCommand::Prune { node, epochs: _ } => run_op(DbVerb::Prune, &node, true, global),
        DbCommand::Reseed { node, yes } => run_op(DbVerb::Reseed, &node, yes, global),
    }
}

#[derive(Debug, Clone, Copy)]
enum DbVerb {
    Remove,
    Prune,
    Reseed,
}

impl DbVerb {
    fn op_label(self) -> &'static str {
        match self {
            Self::Remove => "db.remove",
            Self::Prune => "db.prune",
            Self::Reseed => "db.reseed",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Remove => "remove db/, logs/, stats/, health-records/",
            Self::Prune => "prune database (Phase 2 — not implemented)",
            Self::Reseed => "remove db then mark for reseed (Phase 2 — not implemented)",
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn run_op(
    verb: DbVerb,
    requested: &[u16],
    yes: bool,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    if matches!(verb, DbVerb::Prune | DbVerb::Reseed) {
        return Err(CliError::new(
            format!("`mxnode db {}` is not yet implemented", phase_name(verb)),
            "scheduled for Phase 2 alongside the upgrade transaction log",
            "for now use `mxnode db remove --yes --node N` or the bash get_logs flow",
        )
        .json_if(global.json));
    }

    if !yes {
        return Err(CliError::new(
            "refusing without --yes",
            "db remove permanently deletes the node's database, logs, stats, and health-records",
            "rerun with `mxnode db remove --yes --node N` to confirm intent",
        )
        .json_if(global.json));
    }

    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.state);
    let state = load_state_or_err(&store, global)?;

    let targets = pick_targets(&state, requested, global)?;

    let ctl = crate::orchestrator::supervisor::build_supervisor();
    // Refuse when any target is currently active. We could stop them
    // automatically but doing so to a validator without operator intent
    // is exactly the rating-loss failure mode the audit warned about.
    for node in &targets {
        let active = match ctl.is_active(&node.unit).await {
            Ok(s) => s,
            Err(_) => ActiveState::Unknown,
        };
        if matches!(active, ActiveState::Active | ActiveState::Activating) {
            return Err(CliError::new(
                format!("refusing: node {} is {}", node.index.get(), label(active)),
                format!("{} is still running", node.unit),
                "run `mxnode stop --node N` first; mxnode never auto-stops a live unit for db ops",
            )
            .json_if(global.json));
        }
    }

    if std::io::stdin().is_terminal() && !global.json {
        prompt_confirm(verb, &targets, global)?;
    }

    let mut results: Vec<NodeResult> = Vec::with_capacity(targets.len());
    for node in &targets {
        node_op_start(verb.op_label(), node.index, &node.unit);
        let outcome = wipe_node_data(&node.workdir);
        let (ok, error) = match &outcome {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        let event_outcome = match &error {
            None => Outcome::Ok,
            Some(e) => Outcome::Fail { cause: e.as_str() },
        };
        node_op_end(verb.op_label(), node.index, &node.unit, event_outcome);
        results.push(NodeResult {
            index: node.index.get(),
            unit: node.unit.clone(),
            ok,
            error,
        });
    }

    let any_failed = results.iter().any(|r| !r.ok);
    if global.json {
        let mut payload = serde_json::to_value(&DbReport {
            op: verb.op_label(),
            description: verb.description(),
            nodes: results.clone(),
        })
        .unwrap_or(serde_json::Value::Null);
        if any_failed {
            payload["error"] = serde_json::json!({
                "summary": format!("{} reported failures", verb.op_label()),
                "cause": format!(
                    "{} of {} node(s) failed",
                    results.iter().filter(|r| !r.ok).count(),
                    results.len(),
                ),
                "try": "inspect the working dirs manually",
            });
        }
        println!("{payload}");
    } else {
        for r in &results {
            let glyph = if r.ok { "✓" } else { "✗" };
            print!("{glyph} {} node-{}", verb.op_label(), r.index);
            if let Some(err) = &r.error {
                print!(" — {err}");
            }
            println!();
        }
    }

    if any_failed {
        return Err(CliError::new(
            format!("{} reported failures", verb.op_label()),
            "see per-node errors above",
            "fix the listed problems and rerun against the failing indices",
        )
        .silent());
    }
    Ok(())
}

fn phase_name(v: DbVerb) -> &'static str {
    match v {
        DbVerb::Prune => "prune",
        DbVerb::Reseed => "reseed",
        DbVerb::Remove => "remove",
    }
}

fn load_state_or_err(store: &StateStore, global: &GlobalArgs) -> Result<State, CliError> {
    store
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
        })
}

fn pick_targets<'a>(
    state: &'a State,
    requested: &[u16],
    global: &GlobalArgs,
) -> Result<Vec<&'a NodeState>, CliError> {
    if requested.is_empty() {
        return Err(CliError::new(
            "no --node specified",
            "db ops always require explicit indices to prevent accidental fleet-wide wipes",
            "rerun with `--node N` (repeat for more) or list available indices via `mxnode status`",
        )
        .json_if(global.json));
    }
    let known: Vec<u16> = state.nodes.iter().map(|n| n.index.get()).collect();
    let missing: Vec<u16> = requested
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
    let _ = NodeIndex::new(0); // silence unused-import lint when refactor moves
    Ok(state
        .nodes
        .iter()
        .filter(|n| requested.contains(&n.index.get()))
        .collect())
}

fn wipe_node_data(workdir: &Path) -> std::io::Result<()> {
    // Match `cleanup_files` from the bash: db/, logs/, stats/, health-records/
    // are removed and recreated as empty dirs so the node restart finds the
    // expected layout.
    for sub in ["db", "logs", "stats", "health-records"] {
        let path = workdir.join(sub);
        if path.exists() {
            // Best-effort recursive remove. We don't shell to `sudo rm -rf`
            // here — the operator's CUSTOM_USER owns these dirs and has
            // write access. If they don't, the io error surfaces clearly.
            std::fs::remove_dir_all(&path)?;
        }
        std::fs::create_dir_all(&path)?;
    }
    Ok(())
}

fn prompt_confirm(verb: DbVerb, targets: &[&NodeState], global: &GlobalArgs) -> Result<(), CliError> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let indices: Vec<u16> = targets.iter().map(|n| n.index.get()).collect();
    let label = format!("{:?}", indices);
    writeln!(
        handle,
        "About to {} on node(s) {label}.",
        verb.description()
    )
    .map_err(|e| io_err(e, global))?;
    write!(
        handle,
        "Type the indices back to confirm (e.g. {label}): "
    )
    .map_err(|e| io_err(e, global))?;
    handle.flush().map_err(|e| io_err(e, global))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| io_err(e, global))?;
    let typed = line.trim();
    if typed != label {
        return Err(CliError::new(
            "confirmation mismatch",
            format!("expected {label:?}, got {typed:?}"),
            "rerun the command and type the exact index list back when prompted",
        )
        .json_if(global.json));
    }
    Ok(())
}

fn io_err(e: std::io::Error, global: &GlobalArgs) -> CliError {
    CliError::new(
        "io error during confirmation prompt",
        e.to_string(),
        "rerun in a terminal, or pass `--json` to bypass the interactive prompt (still requires --yes)",
    )
    .json_if(global.json)
}

fn label(state: ActiveState) -> &'static str {
    match state {
        ActiveState::Active => "active",
        ActiveState::Inactive => "inactive",
        ActiveState::Failed => "failed",
        ActiveState::Activating => "activating",
        ActiveState::Deactivating => "deactivating",
        ActiveState::Unknown => "unknown",
    }
}

#[derive(Debug, Clone, Serialize)]
struct DbReport {
    op: &'static str,
    description: &'static str,
    nodes: Vec<NodeResult>,
}

#[derive(Debug, Clone, Serialize)]
struct NodeResult {
    index: u16,
    unit: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
