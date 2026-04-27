//! `mxnode dashboard` — multi-node ratatui TUI.
//!
//! Renders the same metrics as the upstream Go `termui`, plus:
//!   - **multi-node tabs** (Go shows one node only),
//!   - **sparklines** for nonce / peers / CPU / memory / network in/out
//!     (Go renders gauges only — no time-series),
//!   - **mouse-clickable tabs** + keyboard nav (q/tab/1-9/l/p/?),
//!   - **color-coded sync state** with a glyph per tab (✓ ↻ ↯ … ✗),
//!   - **help overlay** (`?`) and **pause polling** (`p` / space),
//!   - **native Rust** — no Go runtime needed.
//!
//! Architecture: one tokio task per node polls `/node/status` +
//! `/node/bootstrapstatus` every `interval`. Each task writes a
//! [`NodeSnapshot`] under a per-node `Mutex`; the renderer reads the
//! cloned snapshot under a brief lock. The terminal event loop runs on
//! the main task, draining keyboard + mouse events and redrawing at a
//! steady cadence (~16 fps target, capped to redraw-on-event for
//! efficiency).

mod app;
mod journal_tail;
mod log_tail;
mod metrics;
mod poller;
mod theme;
mod view;
mod ws_log;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use mxnode_core::{NodeIndex, Platform};
use mxnode_rpc::{GatewayClient, NodeClient};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use thiserror::Error;
use tokio::sync::Mutex;

pub use crate::app::NodeHandle;
use crate::app::App;
use crate::metrics::NodeSnapshot;
use crate::poller::Poller;
use crate::view::{draw, DrawContext};

#[derive(Debug, Error)]
pub enum DashboardError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("rpc init: {0}")]
    Rpc(#[from] mxnode_rpc::RpcError),
}

pub struct DashboardOpts {
    /// Nodes to display. One tab per entry.
    pub nodes: Vec<NodeSpec>,
    /// REST poll cadence per node.
    pub interval: Duration,
    /// Public gateway base URL. Empty string disables trie-stats lookups.
    pub gateway: String,
    /// Stream logs over the node's `/log` WebSocket (true) instead of
    /// the OS-default source (false). On Linux the OS default is
    /// `journalctl --unit <unit> --follow`; on macOS it's a tail of
    /// `<workdir>/logs/*.log` (launchd's stdout redirect). Both deliver
    /// the same content; WS gives structured field highlighting plus
    /// level detection straight from the node's logger.
    pub ws_logs: bool,
    /// Network environment label rendered as a badge in the header
    /// (`mainnet` / `testnet` / `devnet`). `None` = no badge.
    pub environment: Option<String>,
    /// Brand string shown leftmost in the header bar. Defaults to
    /// `"mxnode"` upstream; operators running under their own banner
    /// pass something like `"By XOXNO ✦ TrustStaking"`.
    pub title: String,
}

#[derive(Clone)]
pub struct NodeSpec {
    pub index: NodeIndex,
    pub label: String,
    pub unit: String,
    pub host: String,
    pub api_port: u16,
    pub workdir: PathBuf,
}

/// Run the dashboard until the operator quits. Acquires the alternate
/// screen + raw mode + mouse capture; restores everything on Drop or
/// panic via [`guard`](Restore).
pub async fn run(opts: DashboardOpts) -> Result<(), DashboardError> {
    let environment = opts.environment.clone();
    let title = opts.title.clone();
    let mut handles = Vec::with_capacity(opts.nodes.len());
    let mut poll_tasks = Vec::with_capacity(opts.nodes.len() * 2);
    for spec in &opts.nodes {
        let snap = Arc::new(Mutex::new(NodeSnapshot::default()));
        let client = NodeClient::new(&spec.host, spec.api_port)?;
        let gateway = if opts.gateway.trim().is_empty() {
            None
        } else {
            GatewayClient::new(opts.gateway.trim()).ok()
        };
        let poller = Poller {
            client,
            gateway,
            snapshot: Arc::clone(&snap),
            interval: opts.interval,
        };
        poll_tasks.push(poller.spawn());
        // Per-node log streamer. Three sources, mutually exclusive
        // (any pair into the same ring would duplicate lines):
        //
        //   1. WS  — `--ws-logs`. Structured, level-tagged lines straight
        //            from the node's `/log` socket. Works on any OS.
        //   2. journalctl — Linux default. The systemd unit pipes stdout
        //            to the journal (no `-log-save` on the node), so
        //            `<workdir>/logs/*.log` is empty there. `mxnode logs`
        //            already shells out to journalctl; the dashboard
        //            now matches.
        //   3. file tail — macOS / non-systemd hosts. launchd writes
        //            stdout/stderr to `<workdir>/logs/{stdout,stderr}.log`,
        //            and the node binary itself writes
        //            `mx-chain-{pubkey}-{round}.log` next to those.
        if opts.ws_logs {
            poll_tasks.push(ws_log::spawn(
                spec.host.clone(),
                spec.api_port,
                Arc::clone(&snap),
            ));
        } else if Platform::current().has_journal() && !spec.unit.trim().is_empty() {
            poll_tasks.push(journal_tail::spawn(spec.unit.clone(), Arc::clone(&snap)));
        } else {
            poll_tasks.push(log_tail::spawn(spec.workdir.clone(), Arc::clone(&snap)));
        }
        handles.push(NodeHandle {
            index: spec.index,
            label: spec.label.clone(),
            unit: spec.unit.clone(),
            api_port: spec.api_port,
            workdir: spec.workdir.clone(),
            snapshot: snap,
        });
    }

    let _restore = Restore::install()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(handles);
    app.environment = environment;
    app.title = title;
    let mut ctx = DrawContext { tab_columns: Vec::new() };

    // Event-loop cadence: redraw every 250ms (so sparklines + clock
    // tick smoothly even when the operator isn't typing). Within each
    // 250ms window we drain any pending crossterm events without
    // blocking the runtime — `crossterm::event::poll` with a 0ms
    // timeout is non-blocking, but we wrap it in `spawn_blocking` so
    // the I/O reactor isn't paused while waiting.
    let mut redraw_tick = tokio::time::interval(Duration::from_millis(250));

    'outer: loop {
        // Pause-freeze handling. When the operator just pressed `p` to
        // pause, snapshot every node once so the renderer reads from
        // those clones while paused — that freezes both the panel
        // metrics and the log buffer. When unpaused, drop the freeze
        // so subsequent draws read live.
        if app.paused && app.frozen.is_none() {
            let mut frozen = Vec::with_capacity(app.nodes.len());
            for h in &app.nodes {
                frozen.push(h.snapshot.lock().await.clone());
            }
            app.frozen = Some(frozen);
        } else if !app.paused && app.frozen.is_some() {
            app.frozen = None;
        }

        // Acquire the current node's snapshot under a brief lock —
        // unless paused, in which case we read from the freeze. Both
        // produce an owned `NodeSnapshot` we can hand to the (sync)
        // renderer.
        let current_owned: Option<(String, NodeSnapshot)> = match (
            app.paused,
            app.frozen.as_ref(),
            app.current(),
        ) {
            (true, Some(frozen), Some(handle)) => frozen
                .get(app.selected)
                .cloned()
                .map(|s| (handle.label.clone(), s)),
            (_, _, Some(handle)) => {
                let snap = handle.snapshot.lock().await.clone();
                Some((handle.label.clone(), snap))
            }
            _ => None,
        };

        terminal.draw(|f| {
            let view_arg = current_owned.as_ref().map(|(l, s)| (l.as_str(), s));
            draw(f, &app, &mut ctx, view_arg);
        })?;

        // Drain any keyboard / mouse events queued since the last draw.
        loop {
            let ready = tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(0)))
                .await
                .unwrap_or(Ok(false))
                .unwrap_or(false);
            if !ready {
                break;
            }
            let evt = tokio::task::spawn_blocking(event::read)
                .await
                .unwrap_or(Err(io::Error::new(io::ErrorKind::Other, "event read")))?;
            match evt {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if app.on_key(k) {
                        break 'outer;
                    }
                }
                Event::Mouse(m) => app.on_mouse(m, &ctx.tab_columns),
                Event::Resize(_, _) => { /* next draw catches it */ }
                _ => {}
            }
        }

        // Wait for the next redraw tick.
        redraw_tick.tick().await;
    }

    for t in poll_tasks {
        t.abort();
    }
    Ok(())
}

/// Restore terminal state on drop. Used so a panic mid-render still
/// returns the terminal to a usable state.
struct Restore;

impl Restore {
    fn install() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(DisableMouseCapture);
    }
}

// (No async-bridging helper any more — the renderer is sync. Snapshot
// acquisition happens above, in the event loop, where we already have
// `await` available.)

#[cfg(test)]
mod tests {
    use super::*;
    use mxnode_core::NodeIndex;
    use mxnode_rpc::RawMetrics;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    /// Render one frame against a TestBackend and assert the buffer
    /// contains the headline widgets — this catches layout panics and
    /// spec drift without needing a real TTY.
    #[test]
    fn renders_one_frame_with_synthetic_node() {
        let mut snap = NodeSnapshot {
            metrics: synthetic_metrics(),
            ..NodeSnapshot::default()
        };
        snap.recompute_state();
        let label = "test-node".to_string();

        let app = App::new(vec![NodeHandle {
            index: NodeIndex::new(0),
            label: label.clone(),
            unit: "elrond-node-0.service".to_string(),
            api_port: 8080,
            workdir: PathBuf::from("/tmp/test-workdir"),
            snapshot: std::sync::Arc::new(Mutex::new(snap.clone())),
        }]);

        // 60 rows so the bordered header (3) + tabs (2) + body (Min 20)
        // + logs (Min 15) + status (1) all get their full allocations
        // without ratatui's solver compressing the fixed-Length cells.
        let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 60)).unwrap();
        let mut ctx = DrawContext { tab_columns: Vec::new() };
        terminal
            .draw(|f| view::draw(f, &app, &mut ctx, Some((label.as_str(), &snap))))
            .unwrap();

        let buf = terminal.backend().buffer();
        // Render the buffer to a flat string for substring assertions.
        let mut rendered = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                rendered.push_str(buf[(x, y)].symbol());
            }
            rendered.push('\n');
        }
        // Default brand string set by `App::new()` and
        // `BrandingSection::default()`. Asserting a stable substring
        // — operators forking and changing the default just need to
        // update both lock-step.
        assert!(
            rendered.contains("XOXNO"),
            "missing brand banner. frame:\n{rendered}"
        );
        assert!(rendered.contains("test-node"), "missing tab label");
        assert!(
            rendered.contains("MultiversX instance"),
            "missing instance panel title"
        );
        assert!(rendered.contains("Chain"), "missing chain panel title");
        assert!(rendered.contains("CPU"), "missing CPU gauge");
        assert!(rendered.contains("Memory"), "missing Memory gauge");
        assert!(rendered.contains("Block"), "missing Block panel");
        // PascalCase row labels.
        assert!(rendered.contains("PubKey"), "missing PubKey row");
        assert!(rendered.contains("Validator"), "missing Validator row");
        assert!(rendered.contains("TxPool"), "missing TxPool row");
        assert!(rendered.contains("MiniBlocks"), "missing MiniBlocks row");
        assert!(rendered.contains("KnownVal"), "missing KnownVal row");
        // Tab column ranges should be populated for the mouse handler.
        assert_eq!(ctx.tab_columns.len(), 1);
    }

    fn synthetic_metrics() -> RawMetrics {
        let mut m: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        m.insert("erd_nonce".to_string(), serde_json::json!(13768651));
        m.insert(
            "erd_probable_highest_nonce".to_string(),
            serde_json::json!(13768651),
        );
        m.insert("erd_shard_id".to_string(), serde_json::json!(4_294_967_295u64));
        m.insert("erd_app_version".to_string(), serde_json::json!("v1.11.5"));
        m.insert(
            "erd_public_key_block_sign".to_string(),
            serde_json::json!("8a9f1234567890abcdef1234567890abcdef"),
        );
        m.insert("erd_chain_id".to_string(), serde_json::json!("D"));
        m.insert("erd_node_type".to_string(), serde_json::json!("observer"));
        m.insert("erd_count_consensus".to_string(), serde_json::json!(1234));
        m.insert("erd_cpu_load_percent".to_string(), serde_json::json!(42));
        m.insert("erd_mem_load_percent".to_string(), serde_json::json!(62));
        m.insert("erd_num_connected_peers".to_string(), serde_json::json!(64));
        RawMetrics(m)
    }
}
