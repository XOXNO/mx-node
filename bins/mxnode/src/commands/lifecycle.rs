//! Shared backend for `mxnode start|stop|restart`.
//!
//! Each command is a thin wrapper that picks an op + selector default and
//! lets this module drive the systemctl side. We keep the implementation
//! in one place so structured event emission, error UX, and JSON-output
//! shape stay identical across the three commands.

use std::sync::Arc;
use std::time::Duration;

use mxnode_core::{NodeIndex, State};
use mxnode_state::StateStore;
use mxnode_systemd::{ActiveState, Ctl};
use serde::Serialize;

use crate::cli::{GlobalArgs, LifecycleArgs, RestartArgs, Strategy};
use crate::errors::CliError;
use crate::events::{node_op_end, node_op_start, Outcome};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};
use crate::orchestrator::selector::{resolve, SelectorError};

/// Verb supported by this module — matches the systemctl op we'll fire.
#[derive(Debug, Clone, Copy)]
enum Verb {
    Start,
    Stop,
    Restart,
}

impl Verb {
    fn op_label(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
        }
    }
}

pub fn run_start(args: LifecycleArgs, global: &GlobalArgs) -> Result<(), CliError> {
    drive(Verb::Start, args, Strategy::Parallel, 1, global)
}

pub fn run_stop(args: LifecycleArgs, global: &GlobalArgs) -> Result<(), CliError> {
    drive(Verb::Stop, args, Strategy::Parallel, 1, global)
}

pub fn run_restart(args: RestartArgs, global: &GlobalArgs) -> Result<(), CliError> {
    drive(
        Verb::Restart,
        args.select,
        args.strategy,
        args.max_parallel.max(1),
        global,
    )
}

#[tokio::main(flavor = "current_thread")]
async fn drive(
    verb: Verb,
    args: LifecycleArgs,
    strategy: Strategy,
    max_parallel: u16,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.state);
    let state = load_state_or_err(&store, global)?;

    // No selector → every node, matching what an operator means by
    // `mxnode stop` / `start` / `restart` typed bare. Operators who want
    // a subset still pass `--node N`, `--shard X`, `--validators-only`,
    // `--observers-only`, or `--select <expr>`. The clap `ArgGroup` on
    // `LifecycleArgs` keeps "at most one of these" enforcement so the
    // CLI fails fast if multiple selectors are mixed.
    let indices = resolve(&state, &args).map_err(|e| selector_error_to_cli(e, global))?;
    if indices.is_empty() {
        return Err(CliError::new(
            "selector matched zero nodes",
            "no nodes satisfied the supplied filters",
            "run `mxnode status` to see what's installed",
        )
        .json_if(global.json));
    }

    let nodes: Vec<NodeStateRef> = indices
        .iter()
        .filter_map(|idx| state.nodes.iter().find(|n| n.index == *idx))
        .map(|n| NodeStateRef {
            index: n.index,
            unit: n.unit.clone(),
        })
        .collect();

    // Pick the supervisor backend based on the current platform —
    // SystemctlCtl on Linux, LaunchdCtl on macOS. `Arc` so each spawned
    // task gets a refcounted handle without any unsafe lifetime tricks.
    let ctl: Arc<dyn Ctl> = crate::orchestrator::supervisor::build_supervisor();

    let mut results: Vec<NodeResult> = Vec::with_capacity(nodes.len());
    match strategy {
        Strategy::Rolling => {
            for node in &nodes {
                results.push(run_one(ctl.as_ref(), verb, node).await);
            }
        }
        Strategy::Parallel => {
            // Bound concurrency: split into chunks of `max_parallel`.
            for chunk in nodes.chunks(max_parallel as usize) {
                let mut set = tokio::task::JoinSet::new();
                for node in chunk {
                    let ctl_clone = Arc::clone(&ctl);
                    let node_handle = node.clone();
                    set.spawn(async move { run_one(ctl_clone.as_ref(), verb, &node_handle).await });
                }
                while let Some(joined) = set.join_next().await {
                    match joined {
                        Ok(r) => results.push(r),
                        Err(e) => results.push(NodeResult {
                            index: NodeIndex::new(0),
                            unit: String::new(),
                            ok: false,
                            error: Some(format!("task panic: {e}")),
                            after: ActiveStateView::Unknown,
                        }),
                    }
                }
            }
        }
    }

    // Stable order in output regardless of join order.
    results.sort_by_key(|r| r.index.get());

    let any_failed = results.iter().any(|r| !r.ok);
    if global.json {
        let payload = LifecycleReport {
            op: verb.op_label(),
            strategy: strategy_label(strategy),
            nodes: results
                .iter()
                .map(|r| NodeReport {
                    index: r.index.get(),
                    unit: r.unit.clone(),
                    ok: r.ok,
                    error: r.error.clone(),
                    after: r.after.label(),
                })
                .collect(),
        };
        let mut value = serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
        if any_failed {
            value["error"] = serde_json::json!({
                "summary": format!("{} reported failures", verb.op_label()),
                "cause": format!(
                    "{} of {} node(s) failed",
                    results.iter().filter(|r| !r.ok).count(),
                    results.len(),
                ),
                "try": "inspect `mxnode logs --node N` for the failing index",
            });
        }
        println!("{value}");
    } else {
        for r in &results {
            let glyph = if r.ok { "✓" } else { "✗" };
            // After-state suffix used to read `(unknown)` for the
            // entire ~second after a restart while launchctl's
            // `state = waiting` propagated, which made every success
            // look half-broken. We now only surface the after-state
            // when it actively contradicts the verb (e.g. Start
            // ended up Inactive/Failed) or when there was an error;
            // a clean `✓` already conveys the supervisor acked the
            // command.
            print!("{glyph} {} {}", verb.op_label(), r.unit);
            if r.ok {
                if let Some(suffix) = unexpected_state_suffix(verb, r.after) {
                    print!("  {}", suffix);
                }
            } else if let Some(err) = &r.error {
                print!(" — {err}");
            }
            println!();
        }
    }

    if any_failed {
        return Err(CliError::new(
            format!("{} reported failures", verb.op_label()),
            format!(
                "{} of {} node(s) failed",
                results.iter().filter(|r| !r.ok).count(),
                results.len(),
            ),
            "inspect `mxnode logs --node N` for the failing index",
        )
        .silent());
    }
    Ok(())
}

/// Decide whether the post-action active state contradicts the
/// command badly enough to warrant calling out.
///
/// We deliberately stay silent on the common "ack but transitional"
/// cases (post-restart `Activating`/`Unknown`, post-stop the brief
/// `Deactivating`) — the operator already saw `✓` and can run
/// `mxnode status` for live state. Only the surprising cases get a
/// suffix.
fn unexpected_state_suffix(verb: Verb, after: ActiveStateView) -> Option<&'static str> {
    match (verb, after) {
        (Verb::Start, ActiveStateView::Inactive)
        | (Verb::Start, ActiveStateView::Failed)
        | (Verb::Restart, ActiveStateView::Inactive)
        | (Verb::Restart, ActiveStateView::Failed) => Some("(supervisor reports unit not running)"),
        (Verb::Stop, ActiveStateView::Active) => Some("(supervisor reports unit still active)"),
        _ => None,
    }
}

fn strategy_label(s: Strategy) -> &'static str {
    match s {
        Strategy::Rolling => "rolling",
        Strategy::Parallel => "parallel",
    }
}

#[derive(Debug)]
struct NodeResult {
    index: NodeIndex,
    unit: String,
    ok: bool,
    error: Option<String>,
    after: ActiveStateView,
}

/// Lightweight handle used inside the parallel path so we don't lifetime-
/// extend the `&NodeState` reference into a `tokio::spawn`.
#[derive(Debug, Clone)]
struct NodeStateRef {
    index: NodeIndex,
    unit: String,
}

async fn run_one(ctl: &dyn Ctl, verb: Verb, node: &NodeStateRef) -> NodeResult {
    node_op_start(verb.op_label(), node.index, &node.unit);
    let action = match verb {
        Verb::Start => ctl.start(&node.unit).await,
        Verb::Stop => ctl.stop(&node.unit).await,
        Verb::Restart => ctl.restart(&node.unit).await,
    };
    let (ok, error) = match action {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    // After-state probe: short timeout so we don't hang on a stuck unit.
    let after = match tokio::time::timeout(Duration::from_secs(2), ctl.is_active(&node.unit)).await
    {
        Ok(Ok(state)) => ActiveStateView::from(state),
        _ => ActiveStateView::Unknown,
    };
    let outcome = match &error {
        None => Outcome::Ok,
        Some(e) => Outcome::Fail { cause: e.as_str() },
    };
    node_op_end(verb.op_label(), node.index, &node.unit, outcome);
    NodeResult {
        index: node.index,
        unit: node.unit.clone(),
        ok,
        error,
        after,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveStateView {
    Active,
    Inactive,
    Failed,
    Activating,
    Deactivating,
    Unknown,
}

impl From<ActiveState> for ActiveStateView {
    fn from(s: ActiveState) -> Self {
        match s {
            ActiveState::Active => Self::Active,
            ActiveState::Inactive => Self::Inactive,
            ActiveState::Failed => Self::Failed,
            ActiveState::Activating => Self::Activating,
            ActiveState::Deactivating => Self::Deactivating,
            ActiveState::Unknown => Self::Unknown,
        }
    }
}

impl ActiveStateView {
    fn label(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Failed => "failed",
            Self::Activating => "activating",
            Self::Deactivating => "deactivating",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Serialize)]
struct LifecycleReport {
    op: &'static str,
    strategy: &'static str,
    nodes: Vec<NodeReport>,
}

#[derive(Debug, Serialize)]
struct NodeReport {
    index: u16,
    unit: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    after: &'static str,
}

fn load_state_or_err(store: &StateStore, global: &GlobalArgs) -> Result<State, CliError> {
    store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read state.toml",
                e.to_string(),
                "run `mxnode install` to set up nodes",
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

fn selector_error_to_cli(e: SelectorError, global: &GlobalArgs) -> CliError {
    let summary = match &e {
        SelectorError::NodeMissing { .. } => "no such node",
        SelectorError::BadExpression(_) => "invalid --select expression",
        SelectorError::EmptyState => "no nodes recorded",
    };
    CliError::new(
        summary,
        e.to_string(),
        "run `mxnode status` to list valid selectors",
    )
    .json_if(global.json)
}
