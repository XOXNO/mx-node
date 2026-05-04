//! Winner selection rules per spec §5.

#[derive(Debug, Clone)]
pub struct MeasuredRow {
    pub target: String,
    pub combo_label: String,
    pub binary_bytes: u64,
    pub cold_start_ms: Option<u64>,
    pub tui_render_ms: Option<u64>,
    pub cargo_test_secs: Option<u64>,
    pub tests_passed: bool,
    pub upx_applied: bool,
    pub nightly: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PerfBar {
    pub cold_start_extra_ms: u64,
    pub tui_render_pct: f64,
    pub cargo_test_pct: f64,
}

impl Default for PerfBar {
    fn default() -> Self {
        Self {
            cold_start_extra_ms: 50,
            tui_render_pct: 1.05,
            cargo_test_pct: 1.05,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Picks {
    pub size_max: MeasuredRow,
    pub perf_safe: MeasuredRow,
}

pub fn select(rows: &[MeasuredRow], baseline: &MeasuredRow, bar: PerfBar) -> Picks {
    let size_max = pick_size_max(rows).expect("at least one passing row");
    let perf_safe = pick_perf_safe(rows, baseline, bar).unwrap_or_else(|| baseline.clone());
    Picks {
        size_max,
        perf_safe,
    }
}

fn pick_size_max(rows: &[MeasuredRow]) -> Option<MeasuredRow> {
    rows.iter()
        .filter(|r| r.tests_passed)
        .min_by(|a, b| {
            a.binary_bytes
                .cmp(&b.binary_bytes)
                .then_with(|| a.combo_label.len().cmp(&b.combo_label.len()))
        })
        .cloned()
}

fn pick_perf_safe(
    rows: &[MeasuredRow],
    baseline: &MeasuredRow,
    bar: PerfBar,
) -> Option<MeasuredRow> {
    rows.iter()
        .filter(|r| r.tests_passed)
        .filter(|r| !r.upx_applied)
        .filter(|r| !r.nightly)
        .filter(|r| meets_perf_bar(r, baseline, bar))
        .min_by(|a, b| {
            a.binary_bytes
                .cmp(&b.binary_bytes)
                .then_with(|| a.combo_label.len().cmp(&b.combo_label.len()))
        })
        .cloned()
}

fn meets_perf_bar(r: &MeasuredRow, baseline: &MeasuredRow, bar: PerfBar) -> bool {
    let cold_ok = match (r.cold_start_ms, baseline.cold_start_ms) {
        (Some(c), Some(b)) => c <= b + bar.cold_start_extra_ms,
        _ => true,
    };
    let tui_ok = match (r.tui_render_ms, baseline.tui_render_ms) {
        (Some(c), Some(b)) => c as f64 <= b as f64 * bar.tui_render_pct,
        _ => true,
    };
    let test_ok = match (r.cargo_test_secs, baseline.cargo_test_secs) {
        (Some(c), Some(b)) => c as f64 <= b as f64 * bar.cargo_test_pct,
        _ => true,
    };
    cold_ok && tui_ok && test_ok
}
