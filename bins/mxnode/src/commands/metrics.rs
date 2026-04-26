//! `mxnode metrics --port N`: tiny Prometheus exporter over plain HTTP.
//!
//! v0.1 deliberately does not depend on `hyper`/`axum` for this — the
//! exposition format is plain text and we only handle one route, so a
//! minimal `tokio::net::TcpListener` reader suffices. Keeps the binary
//! small and avoids a full HTTP framework's transitive deps.
//!
//! Metrics emitted (Prometheus text exposition format v0.0.4):
//!   - `mxnode_node_active{index="N",unit="elrond-node-N.service"}` 0|1
//!   - `mxnode_node_nonce{index="N"}` u64
//!   - `mxnode_node_health{index="N"}` 0=failed 1=lagging 2=ok
//!   - `mxnode_state_schema_version` u32
//!   - `mxnode_node_count` u32

use std::sync::Arc;
use std::time::Duration;

use mxnode_core::State;
use mxnode_rpc::NodeClient;
use mxnode_state::StateStore;
use mxnode_systemd::ActiveState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::cli::{GlobalArgs, MetricsArgs};
use crate::errors::CliError;
use crate::events::global_op;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

const PROBE_TIMEOUT: Duration = Duration::from_millis(2_000);

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: MetricsArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = Arc::new(StateStore::new(&runtime.paths.state));
    if !store.exists() {
        return Err(CliError::new(
            "no state.toml on this host",
            format!("expected {}", store.state_path().display()),
            "run `mxnode install` first",
        )
        .json_if(global.json));
    }

    let bind = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&bind).await.map_err(|e| {
        CliError::new(
            "failed to bind metrics port",
            format!("{bind}: {e}"),
            "either choose a free port via --port, or stop whatever is using this one",
        )
        .json_if(global.json)
    })?;
    global_op("metrics.start", &bind);
    if !global.json {
        println!("mxnode metrics listening on http://{bind}/metrics");
    } else {
        println!(
            "{}",
            serde_json::json!({"ok": true, "listening": format!("http://{bind}/metrics")})
        );
    }

    loop {
        let (mut socket, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("metrics: accept failed: {e}");
                continue;
            }
        };
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            // Drain the request line + headers up to the first \r\n\r\n; we
            // don't care about the path because we only serve /metrics.
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let body = match render_metrics(&store).await {
                Ok(b) => b,
                Err(e) => format!("# error: {e}\n"),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
    }
}

async fn render_metrics(store: &StateStore) -> Result<String, String> {
    let state = store
        .load()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "state.toml went missing".to_string())?;
    let mut out = String::with_capacity(2048);
    out.push_str("# HELP mxnode_state_schema_version state.toml schema version\n");
    out.push_str("# TYPE mxnode_state_schema_version gauge\n");
    out.push_str(&format!(
        "mxnode_state_schema_version {}\n",
        state.schema_version
    ));
    out.push_str("# HELP mxnode_node_count number of nodes recorded in state.toml\n");
    out.push_str("# TYPE mxnode_node_count gauge\n");
    out.push_str(&format!("mxnode_node_count {}\n", state.nodes.len()));

    let active_map = probe_active(&state).await;
    let nonce_map = probe_nonces(&state).await;

    out.push_str("# HELP mxnode_node_active 1 if the systemd unit is active\n");
    out.push_str("# TYPE mxnode_node_active gauge\n");
    for node in &state.nodes {
        let v = active_map.get(node.index.get() as usize).copied().unwrap_or(0);
        out.push_str(&format!(
            "mxnode_node_active{{index=\"{}\",unit=\"{}\"}} {}\n",
            node.index.get(),
            escape_label(&node.unit),
            v,
        ));
    }
    out.push_str("# HELP mxnode_node_nonce latest erd_nonce from /node/status (0 when unreachable)\n");
    out.push_str("# TYPE mxnode_node_nonce gauge\n");
    for node in &state.nodes {
        let nonce = nonce_map
            .get(node.index.get() as usize)
            .copied()
            .flatten()
            .unwrap_or(0);
        out.push_str(&format!(
            "mxnode_node_nonce{{index=\"{}\"}} {}\n",
            node.index.get(),
            nonce,
        ));
    }
    out.push_str("# HELP mxnode_node_health 0=failed 1=lagging 2=ok 3=unknown\n");
    out.push_str("# TYPE mxnode_node_health gauge\n");
    for node in &state.nodes {
        let active = active_map.get(node.index.get() as usize).copied().unwrap_or(0);
        let nonce = nonce_map
            .get(node.index.get() as usize)
            .copied()
            .flatten();
        let health = match (active, nonce) {
            (1, Some(n)) if n > 0 => 2, // ok
            (1, _) => 1,                // lagging
            (0, _) => 0,                // failed/inactive
            _ => 3,                     // unknown
        };
        out.push_str(&format!(
            "mxnode_node_health{{index=\"{}\"}} {}\n",
            node.index.get(),
            health,
        ));
    }
    Ok(out)
}

/// Per-node `is-active` probe in parallel. Result vec is indexed by
/// `node.index.get() as usize`; entries default to 0 (inactive/unknown).
async fn probe_active(state: &State) -> Vec<u8> {
    let max_idx = state.nodes.iter().map(|n| n.index.get() as usize).max().unwrap_or(0);
    let mut result = vec![0u8; max_idx + 1];
    let ctl = crate::orchestrator::supervisor::build_supervisor();
    let mut set: JoinSet<(usize, ActiveState)> = JoinSet::new();
    for node in &state.nodes {
        let unit = node.unit.clone();
        let idx = node.index.get() as usize;
        let ctl_clone = Arc::clone(&ctl);
        set.spawn(async move {
            let state = ctl_clone
                .is_active(&unit)
                .await
                .unwrap_or(ActiveState::Unknown);
            (idx, state)
        });
    }
    while let Some(joined) = set.join_next().await {
        if let Ok((idx, state)) = joined {
            if idx < result.len() {
                result[idx] = if state == ActiveState::Active { 1 } else { 0 };
            }
        }
    }
    result
}

async fn probe_nonces(state: &State) -> Vec<Option<u64>> {
    let max_idx = state.nodes.iter().map(|n| n.index.get() as usize).max().unwrap_or(0);
    let mut result: Vec<Option<u64>> = vec![None; max_idx + 1];
    let mut set: JoinSet<(usize, Option<u64>)> = JoinSet::new();
    for node in &state.nodes {
        let port = node.api_port;
        let idx = node.index.get() as usize;
        set.spawn(async move {
            let nonce = match NodeClient::new("127.0.0.1", port) {
                Ok(c) => match tokio::time::timeout(PROBE_TIMEOUT, c.status()).await {
                    Ok(Ok(s)) => s.data.metrics.erd_nonce,
                    _ => None,
                },
                Err(_) => None,
            };
            (idx, nonce)
        });
    }
    while let Some(joined) = set.join_next().await {
        if let Ok((idx, nonce)) = joined {
            if idx < result.len() {
                result[idx] = nonce;
            }
        }
    }
    result
}

/// Prometheus exposition spec: `\\` and `"` must be escaped inside label
/// values; newlines too. Unit names mxnode produces never contain these,
/// but operators on hand-edited hosts might.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_label_escapes_quotes_and_backslash() {
        assert_eq!(
            escape_label("normal-unit.service"),
            "normal-unit.service",
        );
        assert_eq!(
            escape_label("weird\"unit\\name"),
            "weird\\\"unit\\\\name",
        );
        assert_eq!(escape_label("with\nnewline"), "with\\nnewline");
    }
}
