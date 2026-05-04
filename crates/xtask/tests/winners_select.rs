//! Selection rules per the spec §5.

use xtask::winners::{select, MeasuredRow, PerfBar};

fn row(
    label: &str,
    bytes: u64,
    cold: u64,
    tui: u64,
    tests: u64,
    ok: bool,
    upx: bool,
    nightly: bool,
) -> MeasuredRow {
    MeasuredRow {
        target: "aarch64-apple-darwin".to_string(),
        combo_label: label.to_string(),
        binary_bytes: bytes,
        cold_start_ms: Some(cold),
        tui_render_ms: Some(tui),
        cargo_test_secs: Some(tests),
        tests_passed: ok,
        upx_applied: upx,
        nightly,
    }
}

#[test]
fn size_max_picks_smallest_passing() {
    let rows = vec![
        row("a", 5_000_000, 12, 100, 30, true, false, false),
        row("b", 3_000_000, 15, 110, 31, true, true, true),
        row("c", 4_000_000, 11, 95, 29, false, false, false), // failed tests
    ];
    let baseline = rows[0].clone();
    let picks = select(&rows, &baseline, PerfBar::default());
    assert_eq!(picks.size_max.combo_label, "b");
    assert_eq!(picks.size_max.binary_bytes, 3_000_000);
}

#[test]
fn perf_safe_excludes_upx_and_nightly() {
    let rows = vec![
        row("baseline", 5_000_000, 12, 100, 30, true, false, false),
        row("smaller-upx", 3_000_000, 15, 110, 31, true, true, false),
        row("smaller-nightly", 3_500_000, 12, 100, 30, true, false, true),
        row("smaller-stable", 4_500_000, 13, 102, 30, true, false, false),
    ];
    let baseline = rows[0].clone();
    let picks = select(&rows, &baseline, PerfBar::default());
    assert_eq!(picks.perf_safe.combo_label, "smaller-stable");
}

#[test]
fn perf_safe_enforces_cold_start_bar() {
    let baseline_row = row("baseline", 5_000_000, 100, 200, 30, true, false, false);
    let rows = vec![
        baseline_row.clone(),
        // cold +100ms — over the +50ms bar
        row("too-slow", 3_000_000, 200, 200, 30, true, false, false),
        // cold +40ms — under the bar
        row("ok", 4_000_000, 140, 200, 30, true, false, false),
    ];
    let picks = select(&rows, &baseline_row, PerfBar::default());
    assert_eq!(picks.perf_safe.combo_label, "ok");
}

#[test]
fn tie_breaks_by_simpler_config() {
    // Two rows with identical bytes; the one with the shorter combo_label wins.
    let rows = vec![
        row("baseline", 5_000_000, 12, 100, 30, true, false, false),
        row(
            "lto=fat,opt=3,strip=sym,no-env-filter,no-time-macros",
            3_000_000,
            12,
            100,
            30,
            true,
            false,
            false,
        ),
        row(
            "lto=fat,opt=3,strip=sym",
            3_000_000,
            12,
            100,
            30,
            true,
            false,
            false,
        ),
    ];
    let baseline = rows[0].clone();
    let picks = select(&rows, &baseline, PerfBar::default());
    assert_eq!(picks.size_max.combo_label, "lto=fat,opt=3,strip=sym");
}
