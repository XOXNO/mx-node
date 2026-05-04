//! REPORT.md generator. Reads the measured rows produced by the
//! harness and emits a human-readable markdown summary per spec §5.

use std::collections::BTreeMap;
use std::fmt::Write;

use anyhow::Result;

use crate::winners::{select, MeasuredRow, PerfBar};

pub struct ReportInput {
    pub run_id: String,
    pub utc_timestamp: String,
    pub host: String,
    pub toolchain: String,
    pub rows: Vec<MeasuredRow>,
    pub baseline: MeasuredRow,
}

pub fn render(input: &ReportInput) -> Result<String> {
    let mut out = String::new();
    writeln!(out, "# Binary size matrix — run {}", input.run_id)?;
    writeln!(out, "_{}_", input.utc_timestamp)?;
    writeln!(out)?;
    writeln!(out, "Host: `{}`", input.host)?;
    writeln!(out, "Toolchain: `{}`", input.toolchain)?;
    writeln!(out)?;

    writeln!(out, "## Baseline (Phase 0)")?;
    writeln!(out)?;
    writeln!(out, "| target | binary_bytes | cold_start_ms | tui_render_ms |")?;
    writeln!(out, "|---|---:|---:|---:|")?;
    let by_target = group_by_target(&input.rows);
    for (target, rows) in &by_target {
        if let Some(b) = baseline_row(rows) {
            writeln!(
                out,
                "| {} | {} | {} | {} |",
                target,
                b.binary_bytes,
                fmt_opt(b.cold_start_ms),
                fmt_opt(b.tui_render_ms),
            )?;
        }
    }
    writeln!(out)?;

    writeln!(out, "## Pareto frontier per target")?;
    for (target, rows) in &by_target {
        writeln!(out)?;
        writeln!(out, "### {}", target)?;
        writeln!(out)?;
        writeln!(
            out,
            "| combo | bytes | Δ vs baseline | cold_ms | tui_ms | tests |"
        )?;
        writeln!(out, "|---|---:|---:|---:|---:|:-:|")?;
        let baseline_bytes = baseline_row(rows).map(|r| r.binary_bytes).unwrap_or(0);
        let mut sorted = rows.clone();
        sorted.sort_by_key(|r| r.binary_bytes);
        for r in &sorted {
            let delta = if baseline_bytes > 0 {
                let pct = 100.0 * (r.binary_bytes as f64 - baseline_bytes as f64)
                    / baseline_bytes as f64;
                format!("{pct:+.1}%")
            } else {
                "-".to_string()
            };
            writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                r.combo_label,
                r.binary_bytes,
                delta,
                fmt_opt(r.cold_start_ms),
                fmt_opt(r.tui_render_ms),
                if r.tests_passed { "✓" } else { "✗" },
            )?;
        }
    }
    writeln!(out)?;

    writeln!(out, "## Winners")?;
    writeln!(out)?;
    for (target, rows) in &by_target {
        let baseline_for_target = baseline_row(rows)
            .cloned()
            .unwrap_or_else(|| input.baseline.clone());
        let picks = select(rows, &baseline_for_target, PerfBar::default());
        writeln!(out, "### {}", target)?;
        writeln!(
            out,
            "- **size-max**: `{}` — {} bytes",
            picks.size_max.combo_label, picks.size_max.binary_bytes
        )?;
        writeln!(
            out,
            "- **perf-safe**: `{}` — {} bytes",
            picks.perf_safe.combo_label, picks.perf_safe.binary_bytes
        )?;
        writeln!(out)?;
    }

    Ok(out)
}

fn group_by_target(rows: &[MeasuredRow]) -> BTreeMap<String, Vec<MeasuredRow>> {
    let mut out: BTreeMap<String, Vec<MeasuredRow>> = BTreeMap::new();
    for r in rows {
        out.entry(r.target.clone()).or_default().push(r.clone());
    }
    out
}

/// The baseline combo's label is `lto=thin,opt=3,strip=sym` — distinct
/// from every Phase-1 sweep combo, which differ in at least one of
/// (lto, opt, strip). We match on the full canonical label.
fn baseline_row(rows: &[MeasuredRow]) -> Option<&MeasuredRow> {
    rows.iter()
        .find(|r| r.combo_label == "lto=thin,opt=3,strip=sym")
}

fn fmt_opt(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "-".to_string(),
    }
}
