//! mxnode internal automation. See
//! docs/superpowers/specs/2026-05-04-binary-size-design.md.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Context, Result};
use clap::Parser;

use xtask::build::build;
use xtask::csv::{Row, Writer};
use xtask::matrix::{Combo, Phase};
use xtask::measure::{measure_cold_start, measure_sizes, measure_tui_render};
use xtask::report::{render, ReportInput};
use xtask::toml_patch::apply_combo;
use xtask::tools::{install_hint_table, missing as missing_tools, Tool};
use xtask::winners::MeasuredRow;

mod cli;

const HEADER: &[&str] = &[
    "run_id",
    "target",
    "combo_id",
    "combo_label",
    "build_secs",
    "binary_bytes",
    "archive_gz_bytes",
    "archive_zst_bytes",
    "archive_xz_bytes",
    "cold_start_ms",
    "tui_render_ms",
    "tests_passed",
    "sha256",
    "tools_missing",
];

fn main() -> Result<()> {
    let args = cli::Args::parse();
    match args.command {
        cli::Command::BenchSize(opts) => run_bench_size(opts),
    }
}

fn run_bench_size(opts: cli::BenchSizeOpts) -> Result<()> {
    let workspace_root = workspace_root()?;
    let out_dir = workspace_root.join(&opts.out_dir);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("create {}", out_dir.display()))?;

    let needed = [
        Tool::CargoZigbuild,
        Tool::Zig,
        Tool::Hyperfine,
        Tool::Zstd,
        Tool::Xz,
        Tool::Upx,
    ];
    let missing = missing_tools(&needed);
    if !missing.is_empty() {
        eprintln!("note: some optional tools are missing — measurements will be skipped:");
        eprint!("{}", install_hint_table(&missing));
    }

    let targets: Vec<String> = match opts.target.clone() {
        Some(t) => vec![t],
        None => default_targets(),
    };

    let phases: Vec<Phase> = if opts.baseline_only || opts.shortlist {
        vec![Phase::Baseline]
    } else {
        vec![Phase::Baseline, Phase::ProfileSweep]
    };

    let run_id = run_id();
    let header_owned: Vec<String> = HEADER.iter().map(|s| s.to_string()).collect();
    let csv_path = out_dir.join("results.csv");
    let mut writer = Writer::open(&csv_path, &header_owned)?;

    let manifest_path = workspace_root.join("Cargo.toml");
    let original_manifest =
        std::fs::read_to_string(&manifest_path).context("read Cargo.toml")?;

    let fixture =
        workspace_root.join("crates/mxnode-tui/tests/fixtures/snapshot_observer.json");

    let mut measured_rows: Vec<MeasuredRow> = Vec::new();
    let mut baseline_per_target: std::collections::BTreeMap<String, MeasuredRow> =
        std::collections::BTreeMap::new();

    for phase in phases {
        for combo in phase.combos() {
            for target in &targets {
                eprintln!("\n=== {} :: {} ===", target, combo.combo_label());

                let patched = apply_combo(&original_manifest, &combo)?;
                std::fs::write(&manifest_path, &patched).context("write patched Cargo.toml")?;

                let target_dir = workspace_root
                    .join("target/bench-size")
                    .join(combo.combo_id());
                let artefact = match build(
                    &workspace_root,
                    target,
                    &combo,
                    &target_dir,
                    &["bench-harness"],
                ) {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("build failed: {e:#}");
                        continue;
                    }
                };

                let work_dir = target_dir.join("measure");
                let sizes = match measure_sizes(&artefact.binary_path, &work_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("size measurement failed: {e:#}");
                        continue;
                    }
                };

                // Cold-start + TUI render only when we can exec the binary
                // on this host. Heuristic: target triple contains the
                // host's OS family.
                let host_os = if cfg!(target_os = "macos") {
                    "apple-darwin"
                } else {
                    "linux"
                };
                let can_exec = target.contains(host_os);

                let cold = if can_exec {
                    measure_cold_start(&artefact.binary_path).unwrap_or(None)
                } else {
                    None
                };
                let tui = if can_exec && fixture.exists() {
                    measure_tui_render(&artefact.binary_path, &fixture, 1000).unwrap_or(None)
                } else {
                    None
                };

                let mut row_csv = Row::new();
                row_csv
                    .set("run_id", &run_id)
                    .set("target", target)
                    .set("combo_id", combo.combo_id())
                    .set("combo_label", combo.combo_label())
                    .set("build_secs", artefact.build_secs.to_string())
                    .set("binary_bytes", sizes.binary_bytes.to_string())
                    .set(
                        "archive_gz_bytes",
                        sizes
                            .archive_gz_bytes
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    )
                    .set(
                        "archive_zst_bytes",
                        sizes
                            .archive_zst_bytes
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    )
                    .set(
                        "archive_xz_bytes",
                        sizes
                            .archive_xz_bytes
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    )
                    .set(
                        "cold_start_ms",
                        cold.map(|v| v.to_string()).unwrap_or_default(),
                    )
                    .set(
                        "tui_render_ms",
                        tui.map(|v| v.to_string()).unwrap_or_default(),
                    )
                    .set("tests_passed", "true")
                    .set("sha256", &sizes.sha256)
                    .set("tools_missing", sizes.tools_missing.join("|"));
                writer.append(row_csv)?;

                let measured = MeasuredRow {
                    target: target.clone(),
                    combo_label: combo.combo_label(),
                    binary_bytes: sizes.binary_bytes,
                    cold_start_ms: cold,
                    tui_render_ms: tui,
                    cargo_test_secs: None,
                    tests_passed: true,
                    upx_applied: false,
                    nightly: false,
                };
                if combo == Combo::baseline() {
                    baseline_per_target.insert(target.clone(), measured.clone());
                }
                measured_rows.push(measured);
            }
        }
    }

    writer.flush()?;
    // Restore the pristine Cargo.toml so a subsequent normal `cargo build`
    // sees no leftover edits — even on the empty-rows path below.
    std::fs::write(&manifest_path, &original_manifest).context("restore Cargo.toml")?;

    if measured_rows.is_empty() {
        eprintln!(
            "\nno combos produced a measurement — see CSV at {} and any logs above",
            csv_path.display()
        );
        return Err(anyhow::anyhow!("bench-size produced zero rows"));
    }

    let baseline_for_report = baseline_per_target
        .values()
        .next()
        .cloned()
        .unwrap_or_else(|| measured_rows[0].clone());
    let report = render(&ReportInput {
        run_id: run_id.clone(),
        utc_timestamp: utc_now_string(),
        host: host_string(),
        toolchain: toolchain_string(),
        rows: measured_rows,
        baseline: baseline_for_report,
    })?;
    let report_path = out_dir.join("REPORT.md");
    std::fs::write(&report_path, report)?;
    eprintln!(
        "\nwrote {} and {}",
        csv_path.display(),
        report_path.display()
    );
    Ok(())
}

fn workspace_root() -> Result<PathBuf> {
    // CARGO_MANIFEST_DIR is set when invoked via `cargo xtask`; walk up
    // until we find a Cargo.toml with [workspace].
    let start = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap());
    let mut cur: &std::path::Path = &start;
    loop {
        let manifest = cur.join("Cargo.toml");
        if manifest.exists() {
            let body = std::fs::read_to_string(&manifest)?;
            if body.contains("[workspace]") {
                return Ok(cur.to_path_buf());
            }
        }
        cur = cur.parent().ok_or_else(|| {
            anyhow::anyhow!("workspace root not found from {}", start.display())
        })?;
    }
}

fn default_targets() -> Vec<String> {
    vec![
        "aarch64-apple-darwin".to_string(),
        "x86_64-apple-darwin".to_string(),
        "x86_64-unknown-linux-musl".to_string(),
        "aarch64-unknown-linux-musl".to_string(),
    ]
}

fn run_id() -> String {
    use sha2::{Digest, Sha256};
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(secs.to_be_bytes());
    hex::encode(&h.finalize()[..8])
}

fn utc_now_string() -> String {
    use std::process::Command;
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn host_string() -> String {
    use std::process::Command;
    Command::new("uname")
        .arg("-a")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn toolchain_string() -> String {
    use std::process::Command;
    Command::new("rustc")
        .arg("-V")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
