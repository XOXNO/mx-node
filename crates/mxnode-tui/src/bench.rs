//! Headless render driver for the binary-size harness.
//!
//! Renders the dashboard against ratatui's `TestBackend` N times
//! against a synthetic snapshot. No real terminal; no ANSI emission.
//! Returns the wall-clock duration of the inner draw loop only —
//! fixture deserialisation and terminal construction are *not*
//! counted, so cross-combo comparisons isolate render perf.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mxnode_core::NodeIndex;
use mxnode_rpc::RawMetrics;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use tokio::sync::Mutex;

use crate::app::App;
use crate::metrics::NodeSnapshot;
use crate::view::{draw, DrawContext};
use crate::NodeHandle;

/// Renders `frames` consecutive frames against an 120×60 TestBackend
/// using the snapshot loaded from `fixture_path`. Returns the wall-clock
/// duration of the inner draw loop.
///
/// The fixture file is a JSON map of `RawMetrics` (string → JSON value),
/// matching what `/node/status` returns. See
/// `crates/mxnode-tui/tests/fixtures/snapshot_observer.json` for the
/// canonical shape.
pub fn render_n_frames(fixture_path: &Path, frames: u32) -> Result<Duration, BenchError> {
    let raw = std::fs::read_to_string(fixture_path)
        .map_err(|e| BenchError::Fixture(format!("read {}: {e}", fixture_path.display())))?;
    let metrics: std::collections::BTreeMap<String, serde_json::Value> = serde_json::from_str(&raw)
        .map_err(|e| BenchError::Fixture(format!("parse {}: {e}", fixture_path.display())))?;

    let mut snap = NodeSnapshot {
        metrics: RawMetrics(metrics),
        ..NodeSnapshot::default()
    };
    snap.recompute_state();

    let label = "bench-node".to_string();
    let app = App::new(vec![NodeHandle {
        index: NodeIndex::new(0),
        label: label.clone(),
        unit: "bench.service".to_string(),
        api_port: 0,
        workdir: PathBuf::from("/tmp/bench-workdir"),
        snapshot: Arc::new(Mutex::new(snap.clone())),
    }]);

    let mut terminal = Terminal::new(TestBackend::new(120, 60))
        .map_err(|e| BenchError::Terminal(e.to_string()))?;
    let mut ctx = DrawContext {
        tab_columns: Vec::new(),
    };

    let started = Instant::now();
    for _ in 0..frames {
        terminal
            .draw(|f| draw(f, &app, &mut ctx, Some((label.as_str(), &snap))))
            .map_err(|e| BenchError::Draw(e.to_string()))?;
    }
    Ok(started.elapsed())
}

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("fixture: {0}")]
    Fixture(String),
    #[error("terminal: {0}")]
    Terminal(String),
    #[error("draw: {0}")]
    Draw(String),
}
