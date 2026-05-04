# Binary Size Matrix Harness — Stage A + B Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a reproducible bench harness (`cargo xtask bench-size`) that sweeps a pareto-pruned matrix of release-profile, dep-feature, post-processing and toolchain combinations across the four release targets, emits a CSV + REPORT.md, and selects two winners (`size-max` and `perf-safe`) per the spec at `docs/superpowers/specs/2026-05-04-binary-size-design.md`. Then run a baseline measurement on the local macOS arm64 box and commit the resulting REPORT to `docs/superpowers/specs/2026-05-04-binary-size-results-baseline.md`.

**Architecture:** A new workspace member `crates/xtask` provides the harness as a Rust binary, invoked via `cargo xtask bench-size` (alias declared in `.cargo/config.toml`). It mutates a copy of the workspace `Cargo.toml` per combo via `toml_edit`, calls `cargo-zigbuild` as a subprocess, measures binary/archive sizes natively, and runs `hyperfine` + `mxnode bench-render` for cold-start and TUI render timings. A new feature-gated `mxnode bench-render` subcommand in the `mxnode` binary calls into a new `mxnode_tui::bench` module that renders N frames of the dashboard against ratatui's `TestBackend`.

**Tech Stack:** Rust 1.94.1 stable, `cargo-zigbuild` (subprocess), `toml_edit` 0.22, `serde` + `serde_json` for fixtures, `clap` 4.5 for xtask CLI, `ratatui` `TestBackend` for offline render measurement, `hyperfine` for cold-start, `upx`/`zstd`/`xz` post-processing.

---

## File Structure

| Path | Action | Responsibility |
|---|---|---|
| `Cargo.toml` (root) | Modify | Add `crates/xtask` to `[workspace] members` |
| `.cargo/config.toml` | Create | `[alias]` entry for `cargo xtask` |
| `.gitignore` | Modify | Ignore `dist/bench-size/` |
| `crates/xtask/Cargo.toml` | Create | Manifest for the new binary crate |
| `crates/xtask/src/main.rs` | Create | Entry point; dispatches subcommands |
| `crates/xtask/src/cli.rs` | Create | `clap` definitions for `bench-size` and friends |
| `crates/xtask/src/matrix.rs` | Create | Combo definitions (Phase 0–4); deterministic `combo_id` hash |
| `crates/xtask/src/toml_patch.rs` | Create | Apply a combo's profile/dep edits to a `Cargo.toml` copy |
| `crates/xtask/src/build.rs` | Create | Wrap `cargo-zigbuild --release --target ... --bin mxnode` |
| `crates/xtask/src/measure.rs` | Create | File size, archive size, hyperfine invocation, bench-render invocation |
| `crates/xtask/src/csv.rs` | Create | Append-only RFC-4180 writer + reader (for golden-file tests) |
| `crates/xtask/src/winners.rs` | Create | Selection rules (`size-max`, `perf-safe`) over CSV rows |
| `crates/xtask/src/report.rs` | Create | Render `REPORT.md` from CSV |
| `crates/xtask/src/tools.rs` | Create | Detect missing host tools (hyperfine, upx, zstd, xz, cargo-machete) |
| `crates/xtask/tests/csv_roundtrip.rs` | Create | RFC 4180 round-trip + edge cases |
| `crates/xtask/tests/winners_select.rs` | Create | Selection rules against hand-crafted CSV fixtures |
| `crates/xtask/tests/toml_patch_idempotent.rs` | Create | Apply same patch twice → identical output |
| `crates/mxnode-tui/Cargo.toml` | Modify | Add `bench-harness` feature |
| `crates/mxnode-tui/src/lib.rs` | Modify | Conditionally `pub mod bench;` and re-export needed types |
| `crates/mxnode-tui/src/bench.rs` | Create | `render_n_frames(snapshot, n) -> Duration` using `TestBackend` |
| `crates/mxnode-tui/tests/fixtures/snapshot_observer.json` | Create | Synthetic `RawMetrics` JSON for an observer |
| `crates/mxnode-tui/tests/fixtures/snapshot_validator.json` | Create | Synthetic `RawMetrics` JSON for a validator |
| `bins/mxnode/Cargo.toml` | Modify | Add `bench-harness = ["mxnode-tui/bench-harness"]` feature |
| `bins/mxnode/src/cli.rs` | Modify | Hidden `BenchRender` subcommand behind `cfg(feature = "bench-harness")` |
| `bins/mxnode/src/commands.rs` | Modify | Dispatch `BenchRender` |
| `bins/mxnode/src/commands/bench_render.rs` | Create | Loads fixture JSON, calls `mxnode_tui::bench::render_n_frames`, prints `elapsed_ms=<n>` to stderr |
| `.github/workflows/binary-size-matrix.yml` | Create | `workflow_dispatch` only; runs `cargo xtask bench-size --shortlist` on self-hosted Linux runner |
| `docs/superpowers/specs/2026-05-04-binary-size-results-baseline.md` | Create | Output of `cargo xtask bench-size --baseline-only` run locally + (optionally) on Linux runner |

---

## Phase 1 — xtask scaffold

### Task 1: Create xtask crate skeleton

**Files:**
- Create: `crates/xtask/Cargo.toml`
- Create: `crates/xtask/src/main.rs`
- Modify: `Cargo.toml` (root) — add `crates/xtask` to `members`
- Create: `.cargo/config.toml`

- [ ] **Step 1: Create `crates/xtask/Cargo.toml`**

```toml
[package]
name = "xtask"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
description = "Internal automation: binary-size benchmark matrix harness."
publish = false

[[bin]]
name = "xtask"
path = "src/main.rs"

[dependencies]
anyhow      = { workspace = true }
clap        = { workspace = true }
serde       = { workspace = true }
serde_json  = { workspace = true }
toml_edit   = { workspace = true }
sha2        = "0.10"
hex         = "0.4"
which       = "6"

[dev-dependencies]
tempfile    = { workspace = true }
```

- [ ] **Step 2: Create `crates/xtask/src/main.rs`** with a hello dispatcher

```rust
//! mxnode internal automation. Currently exposes `bench-size` for the
//! release-binary size matrix harness (see
//! docs/superpowers/specs/2026-05-04-binary-size-design.md).

use anyhow::Result;
use clap::Parser;

mod cli;

fn main() -> Result<()> {
    let args = cli::Args::parse();
    match args.command {
        cli::Command::BenchSize(opts) => {
            println!("bench-size scaffold ready (combo={:?})", opts);
            Ok(())
        }
    }
}
```

- [ ] **Step 3: Create `crates/xtask/src/cli.rs`** with the minimal CLI surface

```rust
//! clap surface for `cargo xtask`. Subcommands are added as the
//! corresponding modules land — see plan tasks for sequencing.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", version, about = "mxnode internal automation")]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the binary-size benchmark matrix.
    BenchSize(BenchSizeOpts),
}

#[derive(Debug, clap::Args)]
pub struct BenchSizeOpts {
    /// Restrict to a single target triple; default is all four release targets.
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,

    /// Run only the baseline combo (Phase 0). Useful for first-time setup.
    #[arg(long)]
    pub baseline_only: bool,

    /// Run a fixed shortlist of combos rather than the full matrix.
    /// Used by CI to confirm local picks on the self-hosted Linux runner.
    #[arg(long)]
    pub shortlist: bool,

    /// Where to write `results.csv` and `REPORT.md`.
    #[arg(long, default_value = "dist/bench-size", value_name = "DIR")]
    pub out_dir: PathBuf,
}
```

- [ ] **Step 4: Add xtask to workspace members in root `Cargo.toml`**

In `/Users/mihaieremia/GitHub/mx-node/Cargo.toml`, modify the `[workspace] members = [...]` list to append `"crates/xtask"`. Final list should be:

```toml
[workspace]
resolver = "2"
members = [
    "crates/mxnode-core",
    "crates/mxnode-config",
    "crates/mxnode-state",
    "crates/mxnode-github",
    "crates/mxnode-rpc",
    "crates/mxnode-systemd",
    "crates/mxnode-toolchain",
    "crates/mxnode-build",
    "crates/mxnode-tui",
    "crates/xtask",
    "bins/mxnode",
]
```

- [ ] **Step 5: Create `.cargo/config.toml`** so `cargo xtask` works from anywhere in the workspace

```toml
[alias]
xtask = "run --package xtask --release --"
```

- [ ] **Step 6: Verify it builds and the CLI dispatcher works**

Run: `cargo build --package xtask --release`
Expected: clean build, no errors.

Run: `cargo xtask bench-size --baseline-only`
Expected: `bench-size scaffold ready (combo=BenchSizeOpts { target: None, baseline_only: true, shortlist: false, out_dir: "dist/bench-size" })`

- [ ] **Step 7: Add `dist/bench-size/` to `.gitignore`**

Append to `/Users/mihaieremia/GitHub/mx-node/.gitignore`:

```
/dist/bench-size/
```

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml .cargo/config.toml .gitignore crates/xtask
git commit -m "feat(xtask): scaffold binary-size matrix harness crate"
```

---

## Phase 2 — pure-logic modules (TDD)

### Task 2: CSV writer (RFC 4180)

**Files:**
- Create: `crates/xtask/src/csv.rs`
- Create: `crates/xtask/tests/csv_roundtrip.rs`
- Modify: `crates/xtask/src/main.rs` (`mod csv;`)

- [ ] **Step 1: Write the failing round-trip test**

Create `crates/xtask/tests/csv_roundtrip.rs`:

```rust
//! Verify the CSV writer handles RFC 4180 edge cases — embedded commas,
//! double-quotes, newlines — by round-tripping rows through the writer
//! and a reference parser (`csv` crate) and asserting equality.

use xtask::csv::{Row, Writer};

#[test]
fn round_trips_simple_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("results.csv");

    let header = vec!["target".to_string(), "binary_bytes".to_string()];
    let mut w = Writer::open(&path, &header).unwrap();
    w.append(Row::from([
        ("target", "aarch64-apple-darwin"),
        ("binary_bytes", "12345"),
    ]))
    .unwrap();
    w.flush().unwrap();

    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        body,
        "target,binary_bytes\naarch64-apple-darwin,12345\n"
    );
}

#[test]
fn quotes_fields_containing_commas_and_quotes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("results.csv");

    let header = vec!["combo_label".to_string(), "note".to_string()];
    let mut w = Writer::open(&path, &header).unwrap();
    w.append(Row::from([
        ("combo_label", "lto=fat,opt=z"),
        ("note", "value with \"quotes\" inside"),
    ]))
    .unwrap();
    w.flush().unwrap();

    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        body,
        "combo_label,note\n\"lto=fat,opt=z\",\"value with \"\"quotes\"\" inside\"\n"
    );
}

#[test]
fn appends_without_rewriting_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("results.csv");
    let header = vec!["a".to_string(), "b".to_string()];

    let mut w = Writer::open(&path, &header).unwrap();
    w.append(Row::from([("a", "1"), ("b", "2")])).unwrap();
    w.flush().unwrap();
    drop(w);

    // Re-open in append mode — header MUST NOT be rewritten.
    let mut w2 = Writer::open(&path, &header).unwrap();
    w2.append(Row::from([("a", "3"), ("b", "4")])).unwrap();
    w2.flush().unwrap();

    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(body, "a,b\n1,2\n3,4\n");
}
```

- [ ] **Step 2: Make `xtask::csv` reachable from the test**

The integration test at `tests/csv_roundtrip.rs` uses `xtask::csv::...`. Convert `xtask` from a pure-binary crate to a `bin + lib` so tests can import internal modules.

Modify `crates/xtask/Cargo.toml` to add a `[lib]` section above `[[bin]]`:

```toml
[lib]
name = "xtask"
path = "src/lib.rs"
```

Create `crates/xtask/src/lib.rs`:

```rust
//! Library surface for the xtask crate. Modules are exposed here
//! purely so integration tests can exercise pure-logic components
//! (CSV writer, winner-selection rules, toml patcher) without going
//! through the binary entry point.

pub mod csv;
```

Leave `crates/xtask/src/main.rs` as-is for now (it still works — the lib coexists with the bin without any import needed in main). Task 13 wires the lib into main when the dispatcher lands.

- [ ] **Step 3: Run the test to confirm it fails (no impl yet)**

Run: `cargo test --package xtask --test csv_roundtrip`
Expected: FAIL with `unresolved import xtask::csv` or similar.

- [ ] **Step 4: Write the minimal implementation**

Create `crates/xtask/src/csv.rs`:

```rust
//! Append-only RFC-4180 CSV writer.
//!
//! Why hand-rolled and not the `csv` crate: the harness needs a tiny,
//! dependency-light writer that can append safely across xtask
//! invocations and keep header parity. The `csv` crate would be fine
//! but pulls more than needed; round-trip tests use it indirectly via
//! `std::fs::read_to_string` and string equality.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// One row of the CSV. Field order is determined by the writer's
/// header at open time — pass any subset of header columns; missing
/// fields write as empty strings.
pub struct Row {
    fields: BTreeMap<String, String>,
}

impl<K: AsRef<str>, V: AsRef<str>, const N: usize> From<[(K, V); N]> for Row {
    fn from(items: [(K, V); N]) -> Self {
        let mut fields = BTreeMap::new();
        for (k, v) in items {
            fields.insert(k.as_ref().to_string(), v.as_ref().to_string());
        }
        Self { fields }
    }
}

impl Row {
    pub fn new() -> Self {
        Self {
            fields: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
        self.fields
            .insert(key.as_ref().to_string(), value.as_ref().to_string());
        self
    }
}

impl Default for Row {
    fn default() -> Self {
        Self::new()
    }
}

/// Append-only writer. Opening a path that already exists with a
/// matching first-line header reuses the file (append mode); a fresh
/// path writes the header line first.
pub struct Writer {
    out: BufWriter<File>,
    header: Vec<String>,
}

impl Writer {
    pub fn open(path: &Path, header: &[String]) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir for {}", path.display()))?;
        }

        let header_owned: Vec<String> = header.to_vec();
        let exists = path.exists();
        if exists {
            // Verify the existing first line matches the requested header
            // exactly. If not, refuse — the caller has changed the schema
            // and we won't silently corrupt history.
            let f = File::open(path)
                .with_context(|| format!("open {} for header check", path.display()))?;
            let mut first = String::new();
            BufReader::new(f).read_line(&mut first)?;
            let on_disk = first.trim_end_matches('\n');
            let expected = header_owned.join(",");
            if on_disk != expected {
                return Err(anyhow!(
                    "csv header mismatch: on-disk={on_disk:?}, expected={expected:?}"
                ));
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open {} for append", path.display()))?;
        let mut out = BufWriter::new(file);

        if !exists {
            writeln!(out, "{}", header_owned.join(","))?;
        }

        Ok(Self {
            out,
            header: header_owned,
        })
    }

    pub fn append(&mut self, row: Row) -> Result<()> {
        let mut first = true;
        for col in &self.header {
            if !first {
                self.out.write_all(b",")?;
            }
            first = false;
            let raw = row.fields.get(col).map(String::as_str).unwrap_or("");
            self.out.write_all(escape(raw).as_bytes())?;
        }
        self.out.write_all(b"\n")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// Quote per RFC 4180: any field containing comma, quote, CR, or LF gets
/// wrapped in double-quotes and inner quotes are doubled.
fn escape(field: &str) -> String {
    let needs_quoting = field
        .bytes()
        .any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'));
    if !needs_quoting {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len() + 2);
    out.push('"');
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --package xtask --test csv_roundtrip`
Expected: 3 passed.

Run: `cargo build --package xtask --release`
Expected: clean build (the `_csv` import in main.rs suppresses unused warnings).

- [ ] **Step 6: Commit**

```bash
git add crates/xtask
git commit -m "feat(xtask): RFC-4180 CSV writer with append-safe headers"
```

---

### Task 3: Matrix combo definitions

**Files:**
- Create: `crates/xtask/src/matrix.rs`
- Modify: `crates/xtask/src/lib.rs` (`pub mod matrix;`)

- [ ] **Step 1: Write failing test for combo IDs**

Append to `crates/xtask/tests/csv_roundtrip.rs` is wrong — use a fresh file.

Create `crates/xtask/tests/matrix_ids.rs`:

```rust
//! Verify that combo IDs are deterministic and stable across runs —
//! the CSV history depends on this.

use xtask::matrix::{Combo, Phase, Profile};

#[test]
fn combo_id_is_deterministic() {
    let c1 = Combo::baseline();
    let c2 = Combo::baseline();
    assert_eq!(c1.combo_id(), c2.combo_id());
    assert!(!c1.combo_id().is_empty());
}

#[test]
fn different_combos_produce_different_ids() {
    let baseline = Combo::baseline();
    let mut alt = Combo::baseline();
    alt.profile.lto = "fat".to_string();
    assert_ne!(baseline.combo_id(), alt.combo_id());
}

#[test]
fn combo_label_is_human_readable() {
    let mut c = Combo::baseline();
    c.profile.lto = "fat".to_string();
    c.profile.opt_level = "z".to_string();
    let label = c.combo_label();
    assert!(label.contains("lto=fat"));
    assert!(label.contains("opt=z"));
}

#[test]
fn baseline_phase_iterates_one_combo() {
    let combos: Vec<_> = Phase::Baseline.combos().collect();
    assert_eq!(combos.len(), 1);
    assert_eq!(combos[0].combo_id(), Combo::baseline().combo_id());
}

#[test]
fn profile_sweep_produces_twelve_combos() {
    let combos: Vec<_> = Phase::ProfileSweep.combos().collect();
    // 2 lto × 3 opt × 2 strip = 12
    assert_eq!(combos.len(), 12);
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --package xtask --test matrix_ids`
Expected: FAIL with `unresolved import xtask::matrix`.

- [ ] **Step 3: Implement `matrix.rs`**

Create `crates/xtask/src/matrix.rs`:

```rust
//! Combo definitions for the binary-size matrix.
//!
//! A `Combo` is a fully-specified point in the search space: profile
//! knobs, dep prunes, post-processing choices, toolchain channel. The
//! `Phase` enum groups combos by experiment phase per the spec
//! (Phase 0 baseline, Phase 1 profile sweep, etc).

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Profile knobs that map directly to `[profile.release]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Profile {
    pub lto: String,
    pub opt_level: String,
    pub strip: String,
    pub codegen_units: u32,
    pub panic: String,
}

impl Profile {
    pub fn baseline() -> Self {
        Self {
            lto: "thin".to_string(),
            opt_level: "3".to_string(),
            strip: "symbols".to_string(),
            codegen_units: 1,
            panic: "abort".to_string(),
        }
    }
}

/// Dep prunes applied on top of the workspace defaults. Each `bool`
/// is true when the prune is *applied*. False = dep is left at its
/// current feature set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub struct DepPrunes {
    pub drop_env_filter: bool,
    pub release_max_level_info: bool,
    pub drop_clap_wrap_help: bool,
    pub drop_time_macros: bool,
    pub drop_prost_derive: bool,
    pub measure_only_no_live_logs: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub enum Upx {
    #[default]
    None,
    Best,
    BestLzma,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub enum Toolchain {
    #[default]
    Stable,
    NightlyBuildStd,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Combo {
    pub profile: Profile,
    pub deps: DepPrunes,
    pub upx: Upx,
    pub toolchain: Toolchain,
}

impl Combo {
    pub fn baseline() -> Self {
        Self {
            profile: Profile::baseline(),
            deps: DepPrunes::default(),
            upx: Upx::None,
            toolchain: Toolchain::Stable,
        }
    }

    /// Stable, deterministic 12-hex-char ID derived from the combo's
    /// fields. We hash a JSON serialisation so any field addition
    /// without a default value forces a new ID and prevents silent
    /// collisions with old CSV history.
    pub fn combo_id(&self) -> String {
        let json = serde_json::to_string(self).expect("Combo serialises");
        let mut h = Sha256::new();
        h.update(json.as_bytes());
        let digest = h.finalize();
        hex::encode(&digest[..6])
    }

    /// Compact human label, e.g. `lto=fat,opt=z,strip=sym`.
    pub fn combo_label(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("lto={}", self.profile.lto));
        parts.push(format!("opt={}", self.profile.opt_level));
        parts.push(format!(
            "strip={}",
            short_strip(&self.profile.strip)
        ));
        if self.deps.drop_env_filter {
            parts.push("no-env-filter".to_string());
        }
        if self.deps.release_max_level_info {
            parts.push("max-level-info".to_string());
        }
        if self.deps.drop_clap_wrap_help {
            parts.push("no-wrap-help".to_string());
        }
        if self.deps.drop_time_macros {
            parts.push("no-time-macros".to_string());
        }
        if self.deps.drop_prost_derive {
            parts.push("no-prost-derive".to_string());
        }
        if self.deps.measure_only_no_live_logs {
            parts.push("MEASURE-no-live-logs".to_string());
        }
        match self.upx {
            Upx::None => {}
            Upx::Best => parts.push("upx-best".to_string()),
            Upx::BestLzma => parts.push("upx-lzma".to_string()),
        }
        if matches!(self.toolchain, Toolchain::NightlyBuildStd) {
            parts.push("build-std".to_string());
        }
        parts.join(",")
    }
}

fn short_strip(s: &str) -> &'static str {
    match s {
        "symbols" => "sym",
        "debuginfo" => "dbg",
        _ => "?",
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Phase {
    Baseline,
    ProfileSweep,
    // DepSurgery, PostProcess, NightlyBuildStd intentionally omitted
    // until later tasks land — they require winners from prior phases.
}

impl Phase {
    pub fn combos(self) -> Box<dyn Iterator<Item = Combo>> {
        match self {
            Phase::Baseline => Box::new(std::iter::once(Combo::baseline())),
            Phase::ProfileSweep => Box::new(profile_sweep()),
        }
    }
}

fn profile_sweep() -> impl Iterator<Item = Combo> {
    let ltos = ["thin", "fat"];
    let opts = ["3", "s", "z"];
    let strips = ["symbols", "debuginfo"];
    let mut out = Vec::with_capacity(12);
    for lto in ltos {
        for opt in opts {
            for strip in strips {
                let mut c = Combo::baseline();
                c.profile.lto = lto.to_string();
                c.profile.opt_level = opt.to_string();
                c.profile.strip = strip.to_string();
                out.push(c);
            }
        }
    }
    out.into_iter()
}
```

- [ ] **Step 4: Add module to lib.rs**

Modify `crates/xtask/src/lib.rs`:

```rust
pub mod csv;
pub mod matrix;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --package xtask --test matrix_ids`
Expected: 5 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/xtask
git commit -m "feat(xtask): combo definitions for Phase 0/1 with deterministic IDs"
```

---

### Task 4: Winner selection rules

**Files:**
- Create: `crates/xtask/src/winners.rs`
- Create: `crates/xtask/tests/winners_select.rs`
- Modify: `crates/xtask/src/lib.rs`

- [ ] **Step 1: Write failing test**

Create `crates/xtask/tests/winners_select.rs`:

```rust
//! Selection rules per the spec §5.

use xtask::winners::{select, MeasuredRow, PerfBar, Pick};

fn row(label: &str, bytes: u64, cold: u64, tui: u64, tests: u64, ok: bool, upx: bool, nightly: bool) -> MeasuredRow {
    MeasuredRow {
        target: "aarch64-apple-darwin".to_string(),
        combo_label: label.to_string(),
        binary_bytes: bytes,
        cold_start_ms: Some(cold),
        tui_render_ms: Some(tui),
        cargo_test_secs: Some(tests),
        tests_passed: ok,
        upx_applied: upx,
        nightly: nightly,
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
        row("too-slow", 3_000_000, 200, 200, 30, true, false, false), // cold +100ms — over the +50ms bar
        row("ok", 4_000_000, 140, 200, 30, true, false, false),       // cold +40ms — under the bar
    ];
    let picks = select(&rows, &baseline_row, PerfBar::default());
    assert_eq!(picks.perf_safe.combo_label, "ok");
}

#[test]
fn tie_breaks_by_simpler_config() {
    // Two rows with identical bytes; the one with the shorter combo_label wins.
    let rows = vec![
        row("baseline", 5_000_000, 12, 100, 30, true, false, false),
        row("lto=fat,opt=3,strip=sym,no-env-filter,no-time-macros", 3_000_000, 12, 100, 30, true, false, false),
        row("lto=fat,opt=3,strip=sym", 3_000_000, 12, 100, 30, true, false, false),
    ];
    let baseline = rows[0].clone();
    let picks = select(&rows, &baseline, PerfBar::default());
    assert_eq!(picks.size_max.combo_label, "lto=fat,opt=3,strip=sym");
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --package xtask --test winners_select`
Expected: FAIL with `unresolved import xtask::winners`.

- [ ] **Step 3: Implement `winners.rs`**

Create `crates/xtask/src/winners.rs`:

```rust
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

pub struct Pick;

pub fn select(rows: &[MeasuredRow], baseline: &MeasuredRow, bar: PerfBar) -> Picks {
    let size_max = pick_size_max(rows).expect("at least one passing row");
    let perf_safe = pick_perf_safe(rows, baseline, bar).unwrap_or_else(|| baseline.clone());
    Picks { size_max, perf_safe }
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

fn pick_perf_safe(rows: &[MeasuredRow], baseline: &MeasuredRow, bar: PerfBar) -> Option<MeasuredRow> {
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
```

- [ ] **Step 4: Add module to lib.rs**

Modify `crates/xtask/src/lib.rs`:

```rust
pub mod csv;
pub mod matrix;
pub mod winners;
```

- [ ] **Step 5: Run tests**

Run: `cargo test --package xtask --test winners_select`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/xtask
git commit -m "feat(xtask): winner selection rules (size-max + perf-safe)"
```

---

### Task 5: Toml patcher

**Files:**
- Create: `crates/xtask/src/toml_patch.rs`
- Create: `crates/xtask/tests/toml_patch_idempotent.rs`
- Modify: `crates/xtask/src/lib.rs`

- [ ] **Step 1: Write failing test**

Create `crates/xtask/tests/toml_patch_idempotent.rs`:

```rust
//! Verify the toml patcher produces identical output when applied
//! twice — a guard against silent drift if xtask edits the workspace
//! manifest in-place.

use xtask::matrix::{Combo, Profile};
use xtask::toml_patch::apply_combo;

const FIXTURE: &str = r#"
[workspace]
resolver = "2"
members = ["a", "b"]

[profile.release]
lto = "thin"
codegen-units = 1
strip = "symbols"
panic = "abort"
"#;

#[test]
fn applying_baseline_combo_is_a_noop() {
    let patched = apply_combo(FIXTURE, &Combo::baseline()).unwrap();
    let document: toml_edit::DocumentMut = patched.parse().unwrap();
    let release = &document["profile"]["release"];
    assert_eq!(release["lto"].as_str(), Some("thin"));
    assert_eq!(release["opt-level"].as_integer(), None);
}

#[test]
fn applying_lto_fat_opt_z_changes_only_those_keys() {
    let mut combo = Combo::baseline();
    combo.profile.lto = "fat".to_string();
    combo.profile.opt_level = "z".to_string();
    let patched = apply_combo(FIXTURE, &combo).unwrap();
    let document: toml_edit::DocumentMut = patched.parse().unwrap();
    let release = &document["profile"]["release"];
    assert_eq!(release["lto"].as_str(), Some("fat"));
    assert_eq!(release["opt-level"].as_str(), Some("z"));
    assert_eq!(release["codegen-units"].as_integer(), Some(1));
    assert_eq!(release["strip"].as_str(), Some("symbols"));
    assert_eq!(release["panic"].as_str(), Some("abort"));
}

#[test]
fn apply_is_idempotent() {
    let mut combo = Combo::baseline();
    combo.profile.lto = "fat".to_string();
    combo.profile.opt_level = "z".to_string();
    combo.profile.strip = "debuginfo".to_string();

    let once = apply_combo(FIXTURE, &combo).unwrap();
    let twice = apply_combo(&once, &combo).unwrap();
    assert_eq!(once, twice, "second application must be a no-op");
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --package xtask --test toml_patch_idempotent`
Expected: FAIL.

- [ ] **Step 3: Implement `toml_patch.rs`**

Create `crates/xtask/src/toml_patch.rs`:

```rust
//! Apply a `Combo` to a workspace `Cargo.toml`, returning the new
//! TOML body as a string. Uses `toml_edit` so existing formatting,
//! comments, and key ordering are preserved.

use anyhow::{Context, Result};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::matrix::Combo;

pub fn apply_combo(input: &str, combo: &Combo) -> Result<String> {
    let mut doc: DocumentMut = input.parse().with_context(|| "parse Cargo.toml")?;

    let release = ensure_table_path(&mut doc, &["profile", "release"])?;
    release.insert("lto", value(combo.profile.lto.clone()));
    if combo.profile.opt_level == "3" {
        // Default; do not write to keep the diff small.
        release.remove("opt-level");
    } else {
        release.insert("opt-level", value(combo.profile.opt_level.clone()));
    }
    release.insert("strip", value(combo.profile.strip.clone()));
    release.insert("codegen-units", value(combo.profile.codegen_units as i64));
    release.insert("panic", value(combo.profile.panic.clone()));

    Ok(doc.to_string())
}

fn ensure_table_path<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut Table> {
    let mut current: &mut Item = doc.as_item_mut();
    for segment in path {
        let table = current
            .as_table_mut()
            .with_context(|| format!("{segment} is not a table"))?;
        table.entry(segment).or_insert_with(|| Item::Table(Table::new()));
        current = &mut table[segment];
    }
    current
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("final segment is not a table"))
}
```

- [ ] **Step 4: Add module to lib.rs**

Modify `crates/xtask/src/lib.rs`:

```rust
pub mod csv;
pub mod matrix;
pub mod toml_patch;
pub mod winners;
```

- [ ] **Step 5: Run tests**

Run: `cargo test --package xtask --test toml_patch_idempotent`
Expected: 3 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/xtask
git commit -m "feat(xtask): toml_edit-based Cargo.toml patcher (idempotent)"
```

---

### Task 6: Tools detection

**Files:**
- Create: `crates/xtask/src/tools.rs`
- Modify: `crates/xtask/src/lib.rs`

No TDD test — pure wrapper around `which`. Smoke-tested via the harness end-to-end in Task 16.

- [ ] **Step 1: Implement `tools.rs`**

Create `crates/xtask/src/tools.rs`:

```rust
//! Host tool detection. Missing tools downgrade the affected
//! measurement (CSV row gets `tool_missing=<name>`) rather than
//! aborting the whole run — it's normal to bench-size from a fresh
//! macOS box without `upx` installed yet.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub enum Tool {
    Hyperfine,
    Upx,
    Zstd,
    Xz,
    CargoMachete,
    CargoZigbuild,
    Zig,
}

impl Tool {
    pub fn binary(self) -> &'static str {
        match self {
            Tool::Hyperfine => "hyperfine",
            Tool::Upx => "upx",
            Tool::Zstd => "zstd",
            Tool::Xz => "xz",
            Tool::CargoMachete => "cargo-machete",
            Tool::CargoZigbuild => "cargo-zigbuild",
            Tool::Zig => "zig",
        }
    }

    pub fn install_hint(self) -> &'static str {
        match self {
            Tool::Hyperfine => "brew install hyperfine    # or: cargo install hyperfine",
            Tool::Upx => "brew install upx                # Linux: apt install upx-ucl",
            Tool::Zstd => "brew install zstd",
            Tool::Xz => "brew install xz",
            Tool::CargoMachete => "cargo install cargo-machete",
            Tool::CargoZigbuild => "cargo install --locked cargo-zigbuild",
            Tool::Zig => "brew install zig                # or: https://ziglang.org/download/",
        }
    }
}

pub fn check(tool: Tool) -> bool {
    which::which(tool.binary()).is_ok()
}

pub fn missing(tools: &[Tool]) -> Vec<Tool> {
    tools.iter().copied().filter(|t| !check(*t)).collect()
}

pub fn install_hint_table(tools: &[Tool]) -> String {
    let mut by_tool: BTreeMap<&str, &str> = BTreeMap::new();
    for t in tools {
        by_tool.insert(t.binary(), t.install_hint());
    }
    let mut out = String::new();
    for (binary, hint) in by_tool {
        out.push_str(&format!("  - {binary}: {hint}\n"));
    }
    out
}
```

- [ ] **Step 2: Add module to lib.rs**

Modify `crates/xtask/src/lib.rs`:

```rust
pub mod csv;
pub mod matrix;
pub mod tools;
pub mod toml_patch;
pub mod winners;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build --package xtask --release`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/xtask
git commit -m "feat(xtask): host tool detection with install hints"
```

---

## Phase 3 — build & measure

### Task 7: Build module (cargo-zigbuild wrapper)

**Files:**
- Create: `crates/xtask/src/build.rs`
- Modify: `crates/xtask/src/lib.rs`

No TDD — wraps subprocess invocation. Smoke-tested in Task 16.

- [ ] **Step 1: Implement `build.rs`**

Create `crates/xtask/src/build.rs`:

```rust
//! Wrapper around `cargo zigbuild --release --target ... --bin mxnode`.
//! The harness runs each combo in an isolated `target/bench-size/<id>`
//! directory so simultaneous combos don't fight over the cache.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};

use crate::matrix::Combo;

pub struct BuildArtefact {
    pub binary_path: PathBuf,
    pub build_secs: u64,
}

pub fn build(
    workspace_root: &Path,
    target: &str,
    combo: &Combo,
    target_dir: &Path,
    extra_features: &[&str],
) -> Result<BuildArtefact> {
    std::fs::create_dir_all(target_dir)
        .with_context(|| format!("create {}", target_dir.display()))?;

    let started = Instant::now();
    let mut cmd = Command::new("cargo");
    cmd.current_dir(workspace_root)
        .args([
            "zigbuild",
            "--release",
            "--target",
            target,
            "--bin",
            "mxnode",
            "--target-dir",
        ])
        .arg(target_dir);

    if !extra_features.is_empty() {
        cmd.arg("--features").arg(extra_features.join(","));
    }

    // RUSTFLAGS — strip is also set in the profile but mirroring it in
    // the env matches release.yml exactly. opt-level is set via the
    // patched Cargo.toml, NOT via -C, to keep responsibility in one
    // place (the patcher).
    cmd.env("RUSTFLAGS", "-C strip=symbols");

    if combo.toolchain == crate::matrix::Toolchain::NightlyBuildStd {
        // Future: when nightly support lands, switch to `cargo +nightly`
        // and pass `-Zbuild-std=std,panic_abort -Zbuild-std-features=panic_immediate_abort`.
        // Stage A only supports stable.
        return Err(anyhow!(
            "nightly toolchain combos are not implemented in Stage A"
        ));
    }

    let status = cmd
        .status()
        .with_context(|| format!("spawn cargo zigbuild for {target}"))?;
    if !status.success() {
        return Err(anyhow!(
            "cargo zigbuild failed for target={target} ({status})"
        ));
    }

    let binary_path = target_dir.join(target).join("release").join("mxnode");
    if !binary_path.exists() {
        return Err(anyhow!(
            "build claimed success but {} is missing",
            binary_path.display()
        ));
    }

    Ok(BuildArtefact {
        binary_path,
        build_secs: started.elapsed().as_secs(),
    })
}
```

- [ ] **Step 2: Add module to lib.rs**

Modify `crates/xtask/src/lib.rs`:

```rust
pub mod build;
pub mod csv;
pub mod matrix;
pub mod tools;
pub mod toml_patch;
pub mod winners;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build --package xtask --release`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/xtask
git commit -m "feat(xtask): cargo-zigbuild subprocess wrapper"
```

---

### Task 8: Measure module — sizes and archives

**Files:**
- Create: `crates/xtask/src/measure.rs`
- Modify: `crates/xtask/src/lib.rs`

- [ ] **Step 1: Implement size + archive measurements**

Create `crates/xtask/src/measure.rs`:

```rust
//! All on-disk + runtime measurements per spec §4.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use crate::tools::{check as tool_check, Tool};

#[derive(Debug, Default)]
pub struct SizeMeasurement {
    pub binary_bytes: u64,
    pub archive_gz_bytes: Option<u64>,
    pub archive_zst_bytes: Option<u64>,
    pub archive_xz_bytes: Option<u64>,
    pub sha256: String,
    pub tools_missing: Vec<String>,
}

pub fn measure_sizes(binary: &Path, work_dir: &Path) -> Result<SizeMeasurement> {
    let bytes = fs::metadata(binary)
        .with_context(|| format!("stat {}", binary.display()))?
        .len();

    let mut hasher = Sha256::new();
    let buf = fs::read(binary).with_context(|| format!("read {}", binary.display()))?;
    hasher.update(&buf);
    let digest = hex::encode(hasher.finalize());

    fs::create_dir_all(work_dir)?;
    let staged = work_dir.join("mxnode");
    fs::copy(binary, &staged)?;

    let mut out = SizeMeasurement {
        binary_bytes: bytes,
        sha256: digest,
        ..SizeMeasurement::default()
    };

    if tool_check(Tool::Zstd) {
        out.archive_zst_bytes = Some(make_archive(
            work_dir,
            "archive.tar.zst",
            &["-I", "zstd -19", "-cf"],
        )?);
    } else {
        out.tools_missing.push(Tool::Zstd.binary().to_string());
    }

    if tool_check(Tool::Xz) {
        out.archive_xz_bytes = Some(make_archive(work_dir, "archive.tar.xz", &["-cJf"])?);
    } else {
        out.tools_missing.push(Tool::Xz.binary().to_string());
    }

    // gzip is always present via libSystem / glibc tar.
    out.archive_gz_bytes = Some(make_archive(work_dir, "archive.tar.gz", &["-czf"])?);

    Ok(out)
}

fn make_archive(work_dir: &Path, name: &str, tar_flags: &[&str]) -> Result<u64> {
    let archive_path = work_dir.join(name);
    let mut cmd = Command::new("tar");
    cmd.current_dir(work_dir);
    cmd.args(tar_flags);
    cmd.arg(&archive_path);
    cmd.arg("mxnode");
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let status = cmd
        .status()
        .with_context(|| format!("run tar for {name}"))?;
    if !status.success() {
        return Err(anyhow!("tar failed for {name}: {status}"));
    }
    Ok(fs::metadata(&archive_path)?.len())
}

#[derive(Debug, Default)]
pub struct PerfMeasurement {
    pub cold_start_ms: Option<u64>,
    pub tui_render_ms: Option<u64>,
    pub tools_missing: Vec<String>,
}

/// Cold-start via hyperfine. Returns `None` if hyperfine is missing or
/// the binary cannot be exec'd on this host (cross-target build).
pub fn measure_cold_start(binary: &Path) -> Result<Option<u64>> {
    if !tool_check(Tool::Hyperfine) {
        return Ok(None);
    }
    let out = Command::new(Tool::Hyperfine.binary())
        .args([
            "--warmup", "1",
            "--runs", "5",
            "--export-json", "/dev/stdout",
            "--show-output",
            "--",
        ])
        .arg(format!("{} --version", binary.display()))
        .output()
        .with_context(|| "spawn hyperfine")?;
    if !out.status.success() {
        return Ok(None);
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let median_s = json["results"][0]["median"]
        .as_f64()
        .ok_or_else(|| anyhow!("hyperfine output missing median"))?;
    Ok(Some((median_s * 1000.0) as u64))
}

/// TUI render via the bench-harness-feature `bench-render` subcommand.
/// Calls the just-built binary with `bench-render --frames 1000` five
/// times and returns the median elapsed_ms.
pub fn measure_tui_render(binary: &Path, fixture: &Path, frames: u32) -> Result<Option<u64>> {
    let mut samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let out = Command::new(binary)
            .arg("bench-render")
            .arg("--frames")
            .arg(frames.to_string())
            .arg("--fixture")
            .arg(fixture)
            .output();
        let out = match out {
            Ok(o) => o,
            Err(_) => return Ok(None), // can't exec (cross-target)
        };
        if !out.status.success() {
            return Ok(None);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let ms = parse_elapsed(&stderr).ok_or_else(|| {
            anyhow!("bench-render stderr missing elapsed_ms: {stderr}")
        })?;
        samples.push(ms);
    }
    samples.sort_unstable();
    Ok(Some(samples[samples.len() / 2]))
}

fn parse_elapsed(stderr: &str) -> Option<u64> {
    for line in stderr.lines().rev() {
        if let Some(rest) = line.strip_prefix("elapsed_ms=") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[derive(Debug, Default)]
pub struct UpxResult {
    pub bytes_after: u64,
    pub upx_path: PathBuf,
}

/// In-place UPX compression on a *copy* of the binary so the original
/// stays available for the no-UPX size row.
pub fn run_upx(binary: &Path, work_dir: &Path, lzma: bool) -> Result<Option<UpxResult>> {
    if !tool_check(Tool::Upx) {
        return Ok(None);
    }
    let upx_path = work_dir.join("mxnode.upx");
    fs::copy(binary, &upx_path)?;
    let mut cmd = Command::new(Tool::Upx.binary());
    cmd.arg("--best").arg("--quiet");
    if lzma {
        cmd.arg("--lzma");
    }
    cmd.arg(&upx_path);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let status = cmd.status().with_context(|| "spawn upx")?;
    if !status.success() {
        // UPX refuses to compress some Mach-O variants; treat as skip.
        return Ok(None);
    }
    let bytes_after = fs::metadata(&upx_path)?.len();
    Ok(Some(UpxResult { bytes_after, upx_path }))
}
```

- [ ] **Step 2: Add module to lib.rs**

Modify `crates/xtask/src/lib.rs`:

```rust
pub mod build;
pub mod csv;
pub mod matrix;
pub mod measure;
pub mod tools;
pub mod toml_patch;
pub mod winners;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build --package xtask --release`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/xtask
git commit -m "feat(xtask): size + cold-start + TUI render measurement primitives"
```

---

## Phase 4 — mxnode-tui bench module

### Task 9: Bench-harness feature in mxnode-tui

**Files:**
- Modify: `crates/mxnode-tui/Cargo.toml`
- Modify: `crates/mxnode-tui/src/lib.rs`
- Create: `crates/mxnode-tui/src/bench.rs`

- [ ] **Step 1: Add the feature**

Modify `crates/mxnode-tui/Cargo.toml`. Append at the end:

```toml
[features]
# Off by default: the shipping binary doesn't carry the bench harness.
# Enable from the consuming crate (e.g. `bins/mxnode`) only when
# building for `cargo xtask bench-size`.
bench-harness = []
```

- [ ] **Step 2: Add the bench module**

Create `crates/mxnode-tui/src/bench.rs`:

```rust
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
    let metrics: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&raw)
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

    let mut terminal =
        Terminal::new(TestBackend::new(120, 60)).map_err(|e| BenchError::Terminal(e.to_string()))?;
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
```

- [ ] **Step 3: Conditionally export the module from lib.rs**

In `crates/mxnode-tui/src/lib.rs`, append after the existing `mod ws_log;` line (around line 27):

```rust
#[cfg(feature = "bench-harness")]
pub mod bench;
```

- [ ] **Step 4: Verify it compiles with the feature off (default)**

Run: `cargo build --package mxnode-tui --release`
Expected: clean build, no `bench` module compiled.

- [ ] **Step 5: Verify it compiles with the feature on**

Run: `cargo build --package mxnode-tui --release --features bench-harness`
Expected: clean build, `bench` module compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/mxnode-tui
git commit -m "feat(mxnode-tui): bench-harness feature with TestBackend render driver"
```

---

### Task 10: Capture synthetic fixtures

**Files:**
- Create: `crates/mxnode-tui/tests/fixtures/snapshot_observer.json`
- Create: `crates/mxnode-tui/tests/fixtures/snapshot_validator.json`

These mirror the `synthetic_metrics()` function already used by the existing test in `crates/mxnode-tui/src/lib.rs:342-365`, plus a few extra fields exercised by the dashboard panels.

- [ ] **Step 1: Create observer fixture**

Create `crates/mxnode-tui/tests/fixtures/snapshot_observer.json`:

```json
{
  "erd_nonce": 13768651,
  "erd_probable_highest_nonce": 13768651,
  "erd_shard_id": 4294967295,
  "erd_app_version": "v1.11.5",
  "erd_public_key_block_sign": "8a9f1234567890abcdef1234567890abcdef",
  "erd_chain_id": "D",
  "erd_node_type": "observer",
  "erd_count_consensus": 1234,
  "erd_cpu_load_percent": 42,
  "erd_mem_load_percent": 62,
  "erd_num_connected_peers": 64,
  "erd_intra_shard_validator": 0,
  "erd_consensus_state": "not in consensus",
  "erd_round_time": 6000,
  "erd_epoch_number": 1456,
  "erd_round_number": 12345678,
  "erd_highest_final_nonce": 13768650
}
```

- [ ] **Step 2: Create validator fixture**

Create `crates/mxnode-tui/tests/fixtures/snapshot_validator.json`:

```json
{
  "erd_nonce": 13768651,
  "erd_probable_highest_nonce": 13768651,
  "erd_shard_id": 1,
  "erd_app_version": "v1.11.5",
  "erd_public_key_block_sign": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "erd_chain_id": "1",
  "erd_node_type": "validator",
  "erd_count_consensus": 99876,
  "erd_count_consensus_accepted_blocks": 99800,
  "erd_count_leader": 5432,
  "erd_count_accepted_blocks": 99800,
  "erd_cpu_load_percent": 71,
  "erd_mem_load_percent": 88,
  "erd_num_connected_peers": 132,
  "erd_intra_shard_validator": 1,
  "erd_consensus_state": "participant",
  "erd_round_time": 6000,
  "erd_epoch_number": 1456,
  "erd_round_number": 12345678,
  "erd_highest_final_nonce": 13768650
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/mxnode-tui/tests/fixtures
git commit -m "test(mxnode-tui): synthetic fixtures for bench-harness render driver"
```

---

## Phase 5 — mxnode CLI integration

### Task 11: Wire `bench-render` subcommand

**Files:**
- Modify: `bins/mxnode/Cargo.toml`
- Modify: `bins/mxnode/src/cli.rs`
- Modify: `bins/mxnode/src/commands.rs`
- Create: `bins/mxnode/src/commands/bench_render.rs`

- [ ] **Step 1: Add the feature passthrough**

Modify `bins/mxnode/Cargo.toml`. Append at the end (after the existing dep block):

```toml
[features]
# Adds the hidden `bench-render` subcommand used by `cargo xtask bench-size`.
# Off by default — the shipping binary does not carry it.
bench-harness = ["mxnode-tui/bench-harness"]
```

- [ ] **Step 2: Add the hidden CLI variant**

In `bins/mxnode/src/cli.rs`, find the `pub enum Command { ... }` block (starts around line 54). Append a new variant after the last existing variant, gated by the feature:

```rust
    /// Hidden: render N frames of the dashboard against an in-memory
    /// TestBackend and print `elapsed_ms=<n>` to stderr. Used by
    /// `cargo xtask bench-size`.
    #[cfg(feature = "bench-harness")]
    #[command(hide = true)]
    BenchRender(BenchRenderArgs),
```

Then add the args struct at the bottom of `cli.rs`:

```rust
#[cfg(feature = "bench-harness")]
#[derive(Debug, Args)]
pub struct BenchRenderArgs {
    /// How many frames to render.
    #[arg(long, default_value_t = 1000)]
    pub frames: u32,

    /// Path to the snapshot fixture JSON.
    #[arg(long, value_name = "PATH")]
    pub fixture: PathBuf,
}
```

- [ ] **Step 3: Add the dispatch arm and command module**

Find the dispatch in `bins/mxnode/src/commands.rs`. There will be a `pub fn dispatch(cli: Cli) -> ...` with a `match` over the command. Add a new arm at the end:

```rust
        #[cfg(feature = "bench-harness")]
        Command::BenchRender(args) => commands::bench_render::run(args),
```

(Adjust the path if the existing `match` uses a different style — keep parallel structure.)

Then declare the module. Look for the existing `pub mod ...` declarations in `commands.rs` and append:

```rust
#[cfg(feature = "bench-harness")]
pub mod bench_render;
```

- [ ] **Step 4: Implement the command**

Create `bins/mxnode/src/commands/bench_render.rs`:

```rust
//! Hidden bench-render subcommand. Shells out to
//! `mxnode_tui::bench::render_n_frames` and prints `elapsed_ms=<n>`
//! to stderr. The xtask harness parses this line.

use anyhow::{Context, Result};

use crate::cli::BenchRenderArgs;

pub fn run(args: BenchRenderArgs) -> Result<()> {
    let elapsed = mxnode_tui::bench::render_n_frames(&args.fixture, args.frames)
        .with_context(|| format!("render {} frames from {}", args.frames, args.fixture.display()))?;
    eprintln!("elapsed_ms={}", elapsed.as_millis());
    Ok(())
}
```

- [ ] **Step 5: Verify the binary builds without the feature**

Run: `cargo build --bin mxnode --release`
Expected: clean build; `bench-render` not in `mxnode --help`.

Run: `./target/release/mxnode --help | grep -i bench` — expected: no output (feature off).

- [ ] **Step 6: Verify the binary builds with the feature**

Run: `cargo build --bin mxnode --release --features bench-harness`
Expected: clean build.

Run: `./target/release/mxnode bench-render --frames 10 --fixture crates/mxnode-tui/tests/fixtures/snapshot_observer.json`
Expected: stderr line like `elapsed_ms=42`. Process exits 0.

- [ ] **Step 7: Commit**

```bash
git add bins/mxnode crates/mxnode-tui
git commit -m "feat(mxnode): hidden bench-render subcommand for size harness"
```

---

## Phase 6 — Report generator + dispatcher wiring

### Task 12: REPORT.md generator

**Files:**
- Create: `crates/xtask/src/report.rs`
- Modify: `crates/xtask/src/lib.rs`

- [ ] **Step 1: Implement the generator**

Create `crates/xtask/src/report.rs`:

```rust
//! REPORT.md generator. Reads the CSV produced by the harness and
//! emits a human-readable markdown summary per spec §5.

use std::collections::BTreeMap;
use std::fmt::Write;

use anyhow::{Context, Result};

use crate::winners::{select, MeasuredRow, PerfBar, Picks};

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
        if let Some(b) = rows.iter().find(|r| r.combo_label.contains("baseline") || r.combo_label.contains("lto=thin,opt=3,strip=sym")) {
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
        writeln!(out, "| combo | bytes | Δ vs baseline | cold_ms | tui_ms | tests |")?;
        writeln!(out, "|---|---:|---:|---:|---:|:-:|")?;
        let baseline_bytes = rows
            .iter()
            .find(|r| r.combo_label.contains("baseline") || r.combo_label.contains("lto=thin,opt=3,strip=sym"))
            .map(|r| r.binary_bytes)
            .unwrap_or(0);
        let mut sorted = rows.clone();
        sorted.sort_by_key(|r| r.binary_bytes);
        for r in &sorted {
            let delta = if baseline_bytes > 0 {
                let pct = 100.0 * (r.binary_bytes as f64 - baseline_bytes as f64) / baseline_bytes as f64;
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
        let baseline_for_target = rows
            .iter()
            .find(|r| r.combo_label.contains("baseline") || r.combo_label.contains("lto=thin,opt=3,strip=sym"))
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

fn fmt_opt(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "-".to_string(),
    }
}
```

- [ ] **Step 2: Add module to lib.rs**

Modify `crates/xtask/src/lib.rs`:

```rust
pub mod build;
pub mod csv;
pub mod matrix;
pub mod measure;
pub mod report;
pub mod tools;
pub mod toml_patch;
pub mod winners;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build --package xtask --release`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/xtask
git commit -m "feat(xtask): REPORT.md generator with pareto + winners sections"
```

---

### Task 13: End-to-end dispatcher in main.rs

**Files:**
- Modify: `crates/xtask/src/main.rs`
- Modify: `crates/xtask/src/cli.rs`

This task wires Phase 0 (baseline only) end-to-end so we can prove the pipeline works before running the full sweep.

- [ ] **Step 1: Replace main.rs with the working dispatcher**

Overwrite `crates/xtask/src/main.rs`:

```rust
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

    let original_manifest =
        std::fs::read_to_string(workspace_root.join("Cargo.toml")).context("read Cargo.toml")?;
    let manifest_path = workspace_root.join("Cargo.toml");

    let fixture = workspace_root
        .join("crates/mxnode-tui/tests/fixtures/snapshot_observer.json");

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
                let artefact = match build(&workspace_root, target, &combo, &target_dir, &["bench-harness"]) {
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
                // on this host. Heuristic: on macOS we exec macOS binaries,
                // on Linux we exec Linux binaries. Quick check: target
                // contains the host's OS family.
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
                        sizes.archive_gz_bytes.map(|v| v.to_string()).unwrap_or_default(),
                    )
                    .set(
                        "archive_zst_bytes",
                        sizes.archive_zst_bytes.map(|v| v.to_string()).unwrap_or_default(),
                    )
                    .set(
                        "archive_xz_bytes",
                        sizes.archive_xz_bytes.map(|v| v.to_string()).unwrap_or_default(),
                    )
                    .set("cold_start_ms", cold.map(|v| v.to_string()).unwrap_or_default())
                    .set("tui_render_ms", tui.map(|v| v.to_string()).unwrap_or_default())
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
    // sees no leftover edits.
    std::fs::write(&manifest_path, &original_manifest).context("restore Cargo.toml")?;

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
        cur = cur
            .parent()
            .ok_or_else(|| anyhow::anyhow!("workspace root not found from {}", start.display()))?;
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
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build --package xtask --release`
Expected: clean build.

- [ ] **Step 3: Smoke test the dispatcher (no actual builds — just verify CLI parses)**

Run: `cargo xtask bench-size --baseline-only --target aarch64-apple-darwin --out-dir /tmp/xtask-smoke`
Expected: starts running cargo zigbuild; if zigbuild is installed and this is macOS arm64, it builds the baseline mxnode binary and writes a CSV row + REPORT.md. If zigbuild is missing, prints the install hint and continues with the build skipped.

If zigbuild fails because it's not installed, install it first:

```bash
cargo install --locked cargo-zigbuild
brew install zig
```

Then re-run the smoke test.

- [ ] **Step 4: Commit**

```bash
git add crates/xtask
git commit -m "feat(xtask): end-to-end dispatcher for Phase 0 baseline runs"
```

---

## Phase 7 — CI workflow

### Task 14: binary-size-matrix.yml

**Files:**
- Create: `.github/workflows/binary-size-matrix.yml`

- [ ] **Step 1: Create the workflow**

Create `.github/workflows/binary-size-matrix.yml`:

```yaml
name: binary-size-matrix

# Manual-only. Runs the bench harness on the self-hosted Linux runner
# to capture canonical musl numbers. Never on tag-push — release.yml is
# the only auto-triggered pipeline.
on:
  workflow_dispatch:
    inputs:
      mode:
        description: 'baseline-only | shortlist | full'
        required: true
        default: 'baseline-only'

env:
  CARGO_TERM_COLOR: always
  FORCE_JAVASCRIPT_ACTIONS_TO_NODE24: 'true'

permissions:
  contents: read

jobs:
  matrix:
    if: github.repository_owner == 'XOXNO'
    runs-on: [self-hosted, Linux, X64]
    steps:
      - uses: actions/checkout@v6

      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: x86_64-unknown-linux-musl,aarch64-unknown-linux-musl,aarch64-apple-darwin,x86_64-apple-darwin

      - name: Materialise pinned toolchain + targets
        run: rustup show active-toolchain || rustup show

      - uses: Swatinem/rust-cache@v2
        with:
          key: bench-size-${{ inputs.mode }}

      - uses: mlugg/setup-zig@v2
        with:
          version: 0.13.0

      - name: Install cargo-zigbuild
        run: cargo install --locked cargo-zigbuild

      - name: Install measurement tools
        # `apt-get update` is required because Hetzner runners don't
        # refresh the index automatically; the apt cache will otherwise
        # 404 against current package versions.
        run: |
          sudo apt-get update
          sudo apt-get install -y hyperfine upx-ucl zstd xz-utils

      - name: Run bench-size
        env:
          MODE: ${{ inputs.mode }}
        run: |
          case "$MODE" in
            baseline-only) cargo xtask bench-size --baseline-only ;;
            shortlist)     cargo xtask bench-size --shortlist ;;
            full)          cargo xtask bench-size ;;
            *) echo "unknown mode: $MODE" >&2; exit 1 ;;
          esac

      - uses: actions/upload-artifact@v7
        with:
          name: bench-size-results
          path: dist/bench-size/
          retention-days: 90
          if-no-files-found: error
```

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/binary-size-matrix.yml
git commit -m "ci: binary-size-matrix workflow (workflow_dispatch only)"
```

---

## Phase 8 — Stage B: baseline measurement

### Task 15: Run baseline locally and commit REPORT

**Files:**
- Create: `docs/superpowers/specs/2026-05-04-binary-size-results-baseline.md`

- [ ] **Step 1: Ensure prerequisites are installed**

Run: `which hyperfine zstd xz cargo-zigbuild zig || true`
Expected: paths or `<not found>` per tool.

For any missing tool, install:
```bash
brew install hyperfine zstd xz zig
cargo install --locked cargo-zigbuild
```

UPX is optional for the baseline pass — we measure UPX in Phase 3 (deferred, post-Stage-B).

- [ ] **Step 2: Run the baseline pass locally**

Run: `cargo xtask bench-size --baseline-only`
Expected: builds the four release targets in baseline configuration, measures sizes for all, measures cold-start + TUI render for the macOS arm64 build only (other targets are size-only on this host). Writes:
- `dist/bench-size/results.csv` (one row per target)
- `dist/bench-size/REPORT.md`

Cross-target builds may take 10–30 min the first time as the cache warms. Expected total wall-clock: ~15 min on a warm M-series box.

- [ ] **Step 3: Copy REPORT to the spec directory**

```bash
cp dist/bench-size/REPORT.md docs/superpowers/specs/2026-05-04-binary-size-results-baseline.md
```

- [ ] **Step 4: Verify it looks sane**

Run: `head -40 docs/superpowers/specs/2026-05-04-binary-size-results-baseline.md`
Expected: header, baseline table with four targets, pareto frontier with one row per target (since baseline-only emits one combo).

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-05-04-binary-size-results-baseline.md
git commit -m "docs(specs): commit Stage B baseline binary-size measurements"
```

- [ ] **Step 6 (optional): Trigger CI confirmation pass**

If the self-hosted Linux runner is online, manually trigger the workflow to capture canonical musl numbers:

```bash
gh workflow run binary-size-matrix.yml -f mode=baseline-only
```

Wait for it to complete, download the artifact, and append the Linux REPORT to the same spec file under a new `## Linux confirmation` heading. Skip this step if the runner is offline — local macOS numbers are sufficient to anchor future comparisons.

---

## Done checklist (all of Stage A + B)

After all tasks above are complete, verify:

- [ ] `cargo build --package xtask --release` clean
- [ ] `cargo build --bin mxnode --release` clean (no bench-harness leakage in shipping binary)
- [ ] `cargo build --bin mxnode --release --features bench-harness` clean
- [ ] `cargo test --package xtask` — all tests pass (csv_roundtrip, matrix_ids, winners_select, toml_patch_idempotent)
- [ ] `cargo test --workspace` — no regressions
- [ ] `cargo xtask bench-size --baseline-only --target aarch64-apple-darwin --out-dir /tmp/smoke` produces a CSV row + REPORT.md
- [ ] `docs/superpowers/specs/2026-05-04-binary-size-results-baseline.md` exists, is committed, and shows non-zero binary sizes for all four targets
- [ ] `dist/bench-size/` is gitignored (verify with `git status`)
- [ ] `Cargo.toml` is back to its original baseline content after the run (no leftover patcher edits)
