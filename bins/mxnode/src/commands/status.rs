//! `mxnode status [--watch]`: load state, probe each node's REST API
//! concurrently with a per-probe timeout, render a compact table with a
//! health column. `--json` produces a stable schema. `--watch` repaints
//! every `--interval` seconds.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use mxnode_core::HostState;
use mxnode_rpc::{NodeClient, NodeMetrics};
use mxnode_state::StateStore;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::cli::{GlobalArgs, StatusArgs, StatusFormat};
use crate::errors::CliError;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

/// Per-probe timeout. Lower than the rpc client's default 5s so a firewalled
/// host doesn't make `status` feel hung.
const PROBE_TIMEOUT: Duration = Duration::from_millis(2_000);

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: StatusArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.config_dir);

    if args.watch {
        // Repaint indefinitely; ctrl-c breaks out of the read loop.
        loop {
            paint_once(&store, &args, global).await?;
            tokio::time::sleep(Duration::from_secs(args.interval.max(1))).await;
            // Move cursor home + clear screen between repaints when stdout
            // is a TTY. Plain mode prints a blank line separator instead.
            if std::io::stdout().is_terminal() && !no_color_env() {
                let _ = std::io::stdout().write_all(b"\x1b[2J\x1b[H");
            } else {
                println!();
            }
        }
    } else {
        paint_once(&store, &args, global).await
    }
}

/// Honour the [NO_COLOR](https://no-color.org/) convention. Any
/// non-empty value disables colour; absent or empty allows colour
/// when stdout is a TTY.
fn no_color_env() -> bool {
    std::env::var_os("NO_COLOR")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

async fn paint_once(
    store: &StateStore,
    args: &StatusArgs,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let state = store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read mxnode.toml",
                e.to_string(),
                "run `mxnode install` to set up nodes",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no mxnode.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode install` to set up nodes",
            )
            .json_if(global.json)
        })?;

    let probes = probe_all(&state).await;

    let format = if global.json {
        StatusFormat::Json
    } else {
        args.format
    };
    match format {
        StatusFormat::Json => render_json(&state, &probes),
        StatusFormat::Table => render_table(&state, &probes, !no_color_env()),
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Ok,
    Lagging,
    Failed,
    Unknown,
}

impl Health {
    fn glyph(&self) -> char {
        match self {
            Self::Ok => '✓',
            Self::Lagging => '!',
            Self::Failed => '✗',
            Self::Unknown => '?',
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Lagging => "lagging",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// ANSI colour code; only emitted when stdout is a TTY and the
    /// `NO_COLOR` env var is not set (https://no-color.org/).
    fn color(&self) -> &'static str {
        match self {
            Self::Ok => "\x1b[32m",
            Self::Lagging => "\x1b[33m",
            Self::Failed => "\x1b[31m",
            Self::Unknown => "\x1b[90m",
        }
    }
}

const RESET: &str = "\x1b[0m";

#[derive(Debug)]
struct Probe {
    health: Health,
    nonce: Option<u64>,
    pubkey_prefix: Option<String>,
}

async fn probe_all(state: &HostState) -> Vec<Probe> {
    let mut set: JoinSet<(usize, Probe)> = JoinSet::new();
    for (i, node) in state.nodes.iter().enumerate() {
        let port = node.api_port;
        set.spawn(async move {
            let probe = probe_one(port).await;
            (i, probe)
        });
    }
    let mut by_index: Vec<Probe> = (0..state.nodes.len())
        .map(|_| Probe {
            health: Health::Unknown,
            nonce: None,
            pubkey_prefix: None,
        })
        .collect();
    while let Some(res) = set.join_next().await {
        match res {
            Ok((i, probe)) => {
                by_index[i] = probe;
            }
            Err(_) => { /* task panicked; leave default Unknown */ }
        }
    }
    by_index
}

async fn probe_one(port: u16) -> Probe {
    let client = match NodeClient::new("127.0.0.1", port) {
        Ok(c) => c,
        Err(_) => {
            return Probe {
                health: Health::Failed,
                nonce: None,
                pubkey_prefix: None,
            };
        }
    };
    let result = tokio::time::timeout(PROBE_TIMEOUT, client.status()).await;
    match result {
        Ok(Ok(status)) => {
            let metrics: NodeMetrics = status.data.metrics;
            let pubkey_prefix = metrics.pubkey_prefix().map(|s| s.to_string());
            let nonce = metrics.erd_nonce;
            // Heuristic: `erd_is_syncing == 0` (or absent) and a non-zero
            // nonce → ok. Anything else is "lagging" — we don't have the
            // network high nonce here yet (proxy probe is Phase 2).
            let health = match metrics.erd_is_syncing {
                Some(0) | None if nonce.is_some_and(|n| n > 0) => Health::Ok,
                _ => Health::Lagging,
            };
            Probe {
                health,
                nonce,
                pubkey_prefix,
            }
        }
        Ok(Err(_)) | Err(_) => Probe {
            health: Health::Failed,
            nonce: None,
            pubkey_prefix: None,
        },
    }
}

fn render_table(state: &HostState, probes: &[Probe], color: bool) {
    let header = state
        .install
        .as_ref()
        .map(|i| {
            let tags = state
                .install
                .as_ref()
                .map(|i| {
                    format!(
                        "tags: config {} binary {}",
                        i.versions
                            .config_tag
                            .as_ref()
                            .map(|t| t.as_str())
                            .unwrap_or("?"),
                        i.versions
                            .binary_tag
                            .as_ref()
                            .map(|t| t.as_str())
                            .unwrap_or("?"),
                    )
                })
                .unwrap_or_default();
            format!(
                "mxnode {} │ {} │ {} nodes │ {tags}",
                env!("CARGO_PKG_VERSION"),
                i.environment,
                i.node_count,
            )
        })
        .unwrap_or_else(|| {
            format!(
                "mxnode {} │ no install recorded — run `mxnode install`",
                env!("CARGO_PKG_VERSION")
            )
        });
    println!("{header}");
    if state.nodes.is_empty() {
        println!("(no nodes)");
        return;
    }
    println!("H │ idx │ name                     │ shard      │ nonce      │ pubkey       │ port");
    println!("──┼─────┼──────────────────────────┼────────────┼────────────┼──────────────┼──────");
    let template = &state
        .install
        .as_ref()
        .map(|_| "")  // status doesn't have a config-side template; lean on persisted name only
        .unwrap_or("");
    let env_str = state
        .install
        .as_ref()
        .map(|i| i.environment.as_str())
        .unwrap_or("");
    for (node, probe) in state.nodes.iter().zip(probes.iter()) {
        let nonce = probe
            .nonce
            .map(format_nonce)
            .unwrap_or_else(|| "-".to_string());
        let pubkey = probe
            .pubkey_prefix
            .clone()
            .or_else(|| {
                if node.last_known_pubkey.is_empty() {
                    None
                } else {
                    Some(node.last_known_pubkey.clone())
                }
            })
            .unwrap_or_else(|| "-".to_string());
        let glyph = probe.health.glyph();
        let glyph_cell = if color {
            format!("{}{}{}", probe.health.color(), glyph, RESET)
        } else {
            glyph.to_string()
        };
        // Show the operator's chosen NodeDisplayName (persisted at
        // install time) rather than the systemd unit filename. The
        // unit name is recoverable via `--json` for tooling that
        // wants it.
        let label = crate::commands::prompts::resolve_display_name(
            &node.display_name,
            template,
            env_str,
            node.index.get(),
        );
        let label = if label.is_empty() {
            node.unit.clone()
        } else {
            label
        };
        println!(
            "{glyph_cell} │ {idx:<3} │ {label:<24} │ {shard:<10} │ {nonce:<10} │ {pubkey:<12} │ {port}",
            idx = node.index.get(),
            label = truncate(&label, 24),
            shard = node.shard.as_str(),
            port = node.api_port,
        );
    }
    let summary = summarize(probes);
    println!("\nhealth: {summary}");
}

fn render_json(state: &HostState, probes: &[Probe]) {
    let payload = JsonReport::from_state_and_probes(state, probes);
    println!("{}", serde_json::to_string(&payload).unwrap_or_default());
}

#[derive(Debug, Serialize)]
struct JsonReport {
    schema_version: u32,
    install: Option<JsonInstall>,
    nodes: Vec<JsonNode>,
}

#[derive(Debug, Serialize)]
struct JsonInstall {
    environment: String,
    kind: String,
    node_count: u16,
    config_tag: Option<String>,
    binary_tag: Option<String>,
}

#[derive(Debug, Serialize)]
struct JsonNode {
    index: u16,
    /// Operator-chosen `NodeDisplayName` persisted at install time.
    /// Empty on legacy installs whose mxnode.toml predates the field.
    display_name: String,
    unit: String,
    shard: String,
    api_port: u16,
    health: &'static str,
    nonce: Option<u64>,
    pubkey_prefix: Option<String>,
}

impl JsonReport {
    fn from_state_and_probes(state: &HostState, probes: &[Probe]) -> Self {
        let install = state.install.as_ref().map(|i| JsonInstall {
            environment: i.environment.to_string(),
            kind: i.kind.to_string(),
            node_count: i.node_count,
            config_tag: i.versions.config_tag.as_ref().map(|t| t.to_string()),
            binary_tag: i.versions.binary_tag.as_ref().map(|t| t.to_string()),
        });
        let nodes = state
            .nodes
            .iter()
            .zip(probes.iter())
            .map(|(node, probe)| JsonNode {
                index: node.index.get(),
                display_name: node.display_name.clone(),
                unit: node.unit.clone(),
                shard: node.shard.as_str().to_string(),
                api_port: node.api_port,
                health: probe.health.label(),
                nonce: probe.nonce,
                pubkey_prefix: probe.pubkey_prefix.clone(),
            })
            .collect();
        Self {
            schema_version: state.schema_version,
            install,
            nodes,
        }
    }
}

fn summarize(probes: &[Probe]) -> String {
    let mut ok = 0usize;
    let mut lag = 0usize;
    let mut fail = 0usize;
    let mut unk = 0usize;
    for p in probes {
        match p.health {
            Health::Ok => ok += 1,
            Health::Lagging => lag += 1,
            Health::Failed => fail += 1,
            Health::Unknown => unk += 1,
        }
    }
    format!("{ok} ok, {lag} lagging, {fail} failed, {unk} unknown")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

fn format_nonce(n: u64) -> String {
    let mut digits = n.to_string();
    let mut formatted = String::new();
    while digits.len() > 3 {
        let split = digits.len() - 3;
        formatted = format!(",{}{}", &digits[split..], formatted);
        digits.truncate(split);
    }
    format!("{digits}{formatted}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_nonce_inserts_commas() {
        assert_eq!(format_nonce(0), "0");
        assert_eq!(format_nonce(42), "42");
        assert_eq!(format_nonce(1_234), "1,234");
        assert_eq!(format_nonce(4_201_331), "4,201,331");
        assert_eq!(format_nonce(1_234_567_890), "1,234,567,890");
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789ab", 10), "012345678…");
    }

    #[test]
    fn health_summary_counts_categories() {
        let probes = vec![
            Probe {
                health: Health::Ok,
                nonce: Some(1),
                pubkey_prefix: None,
            },
            Probe {
                health: Health::Ok,
                nonce: Some(2),
                pubkey_prefix: None,
            },
            Probe {
                health: Health::Lagging,
                nonce: Some(3),
                pubkey_prefix: None,
            },
            Probe {
                health: Health::Failed,
                nonce: None,
                pubkey_prefix: None,
            },
        ];
        assert_eq!(summarize(&probes), "2 ok, 1 lagging, 1 failed, 0 unknown");
    }
}
