//! Structured operator-audit events emitted via `tracing`.
//!
//! Every state-changing command emits one event before the action and one
//! after, with stable field names so a downstream collector (journald
//! `MESSAGE_ID=` rules, Loki, ELK) can build dashboards without parsing
//! human-readable strings. Field names match the plan §"D10 — universal
//! `--json` from v0.1 + structured journald events":
//!
//!   - `op` — short verb: "start", "stop", "restart", "db.remove",
//!     "db.prune", "logs.archive", "cleanup", "metrics.start"
//!   - `node` — `NodeIndex.get()` when the op targets exactly one node
//!   - `unit` — systemd unit name when applicable
//!   - `result` — "ok" | "fail" (post-action only)
//!   - `cause` — error message when result == "fail"
//!
//! The tracing layer in `main.rs` writes to stderr by default. On Linux
//! operators can set `RUST_LOG=mxnode=info` and pipe stderr to journald
//! via the systemd unit; explicit journald-native emission lands in
//! Phase 2 once the daemon (`mxnoded`) ships.

use mxnode_core::NodeIndex;

/// Result classification recorded on completion events.
pub enum Outcome<'a> {
    Ok,
    Fail { cause: &'a str },
}

/// Emit a "starting" event before a node-targeted action.
pub fn node_op_start(op: &'static str, node: NodeIndex, unit: &str) {
    tracing::info!(
        target: "mxnode.event",
        event = "op.start",
        op,
        node = node.get(),
        unit,
    );
}

/// Emit an "ended" event after a node-targeted action.
pub fn node_op_end(op: &'static str, node: NodeIndex, unit: &str, outcome: Outcome<'_>) {
    match outcome {
        Outcome::Ok => tracing::info!(
            target: "mxnode.event",
            event = "op.end",
            op,
            node = node.get(),
            unit,
            result = "ok",
        ),
        Outcome::Fail { cause } => tracing::warn!(
            target: "mxnode.event",
            event = "op.end",
            op,
            node = node.get(),
            unit,
            result = "fail",
            cause,
        ),
    }
}

/// Emit a global event (no specific node), e.g. `cleanup` or
/// `metrics.start`. Same shape as the per-node events minus the node fields.
pub fn global_op(op: &'static str, summary: &str) {
    tracing::info!(
        target: "mxnode.event",
        event = "op.global",
        op,
        summary,
    );
}
