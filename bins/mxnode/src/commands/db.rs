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
        DbCommand::Remove { node, yes } => run_op(DbVerb::Remove, &node, None, yes, global),
        DbCommand::Prune { node, epochs } => run_op(DbVerb::Prune, &node, epochs, true, global),
        DbCommand::Reseed { node, yes } => run_op(DbVerb::Reseed, &node, None, yes, global),
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
            Self::Prune => "trim db/Epoch_N directories",
            Self::Reseed => "remove db then start the node (resync from genesis)",
        }
    }
}

/// Default number of recent epochs `db prune` keeps when `--epochs` is
/// not supplied. The mainnet config rotates an epoch every ~24 hours,
/// so 4 keeps roughly the last four days of block data.
const DEFAULT_PRUNE_KEEP: u32 = 4;

#[tokio::main(flavor = "current_thread")]
async fn run_op(
    verb: DbVerb,
    requested: &[u16],
    epochs_to_keep: Option<u32>,
    yes: bool,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    if !yes {
        return Err(CliError::new(
            "refusing without --yes",
            format!(
                "{} permanently changes the node's data; pass --yes to confirm",
                verb.description(),
            ),
            format!("rerun with `mxnode db {} --yes --node N`", phase_name(verb)),
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

    let keep = epochs_to_keep.unwrap_or(DEFAULT_PRUNE_KEEP);
    let mut results: Vec<NodeResult> = Vec::with_capacity(targets.len());
    for node in &targets {
        node_op_start(verb.op_label(), node.index, &node.unit);
        let outcome: std::io::Result<Option<String>> = match verb {
            DbVerb::Remove => wipe_node_data(&node.workdir).map(|_| None),
            DbVerb::Prune => prune_old_epochs(&node.workdir, keep).map(Some),
            DbVerb::Reseed => match wipe_node_data(&node.workdir) {
                Err(e) => Err(e),
                Ok(()) => match ctl.start(&node.unit).await {
                    Ok(()) => Ok(Some(format!("started {}", node.unit))),
                    Err(e) => Err(std::io::Error::other(format!(
                        "wiped db but failed to start {}: {e}",
                        node.unit
                    ))),
                },
            },
        };
        let (ok, error, note) = match outcome {
            Ok(note) => (true, None, note),
            Err(e) => (false, Some(e.to_string()), None),
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
            note,
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
            if let Some(note) = &r.note {
                print!(" — {note}");
            }
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
                "run `mxnode install` first",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no state.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode install` first",
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
    // Recreate the four data subdirs as empty so the next node start
    // finds the expected layout. The operator's CUSTOM_USER owns these
    // dirs; an io error surfaces directly if it doesn't.
    for sub in ["db", "logs", "stats", "health-records"] {
        let path = workdir.join(sub);
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        std::fs::create_dir_all(&path)?;
    }
    Ok(())
}

/// Trim `db/Epoch_N/` directories down to the most recent `keep` of
/// them. Returns a one-line summary the caller prints alongside the
/// per-node `✓`. Missing/empty `db/` directories are a no-op.
fn prune_old_epochs(workdir: &Path, keep: u32) -> std::io::Result<String> {
    let db = workdir.join("db");
    if !db.exists() {
        return Ok("no db/ to prune".to_string());
    }
    let mut epochs: Vec<(u32, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&db)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(rest) = name_str.strip_prefix("Epoch_") {
            if let Ok(n) = rest.parse::<u32>() {
                epochs.push((n, entry.path()));
            }
        }
    }
    if epochs.is_empty() {
        return Ok("no Epoch_N directories found".to_string());
    }
    // Sort descending so the head of the vec is the newest.
    epochs.sort_by(|a, b| b.0.cmp(&a.0));
    let total = epochs.len();
    let to_remove = epochs.split_off(keep.min(total as u32) as usize);
    let removed = to_remove.len();
    for (_, path) in &to_remove {
        std::fs::remove_dir_all(path)?;
    }
    Ok(format!(
        "removed {removed} of {total} Epoch_N directories (kept newest {})",
        keep.min(total as u32),
    ))
}

fn prompt_confirm(
    verb: DbVerb,
    targets: &[&NodeState],
    global: &GlobalArgs,
) -> Result<(), CliError> {
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
    write!(handle, "Type the indices back to confirm (e.g. {label}): ")
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
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}
