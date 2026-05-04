# Binary size reduction — design

**Date:** 2026-05-04
**Owner:** Mihai
**Status:** Approved (brainstorm complete, awaiting implementation plan)
**Targets:** `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`, `x86_64-apple-darwin`

---

## 1. Goals & success criteria

Reduce mxnode release-binary size on every target, with Linux-musl as the priority axis. Ship two winners side-by-side:

- **`size-max`** — smallest binary, anything goes (UPX-LZMA, `opt-level=z`, fat LTO, nightly `-Zbuild-std`, `panic_immediate_abort`). No perf budget.
- **`perf-safe`** — smallest binary that holds a perf bar:
  - cold-start ≤ baseline + 50 ms,
  - TUI render wall-time (`--bench-render 1000`) ≤ baseline × 1.05,
  - `cargo test --release` wall-time ≤ baseline × 1.05.

  No UPX, stable channel only, musl-static preserved.

**Hard constraint:** the shipped binary keeps every feature on for every user, including the TUI dashboard live-log streaming (`prost` + `tokio-tungstenite` + `futures-util` stay in). No `--no-default-features` builds in the shipping path.

**Definition of done (this spec covers Stage A + B only):**
- A reproducible bench harness (`cargo xtask bench-size`) that emits a CSV per `(target, combo)` and a markdown summary with both winners highlighted.
- Baseline matrix run committed to `docs/superpowers/specs/<date>-binary-size-results-baseline.md`.
- Stages C/D/E (applying the winners to `release.yml`, optional UPX, optional nightly) are **follow-on plans** written *after* the baseline matrix has been measured — we don't pre-commit to applying changes whose magnitude we haven't yet measured. This keeps each PR justifiable by data.

**Non-goals:**
- Refactoring application code purely for size (only pruning unused dep features and dropping deps that are genuinely unused).
- Switching musl → glibc on Linux (loses old-distro portability).
- Optimising compile time (we already accept slow LTO; `lto=fat` will make it worse).

---

## 2. Architecture

### New components

1. **`xtask` crate** — `crates/xtask/` (workspace member, not published). Single binary `xtask` with subcommands. `cargo xtask bench-size` is the entry point. Type-safe matrix definition in Rust. Reuses `cargo-zigbuild` under the hood — same toolchain as `release.yml`.

2. **`--bench-render` mode in `mxnode-tui`** — hidden CLI flag behind `cfg(feature = "bench-harness")` so it never bloats the shipped binary. Reads a fixture `MetricsSnapshot` JSON, instantiates the dashboard widget tree, calls `terminal.draw()` N times against ratatui's `TestBackend`. Stderr: `elapsed_ms=<n>`.

3. **CI workflow** — `.github/workflows/binary-size-matrix.yml`. `workflow_dispatch` only (never on tag-push). Runs on the same self-hosted Linux runner as `release.yml`. Cross-builds all four targets per combo via `cargo-zigbuild`. Uploads CSV + markdown summary as workflow artefact (90-day retention). Does **not** sign or publish anything — purely measurement.

4. **Bench fixtures** — `crates/mxnode-tui/tests/fixtures/snapshot_observer.json` and `snapshot_validator.json` — captured `MetricsSnapshot` JSONs from a real node, checked in. Used by `--bench-render`.

### Data flow

```
xtask bench-size --target <T> --combo <C>
  ├─ writes a temp Cargo.toml override with profile/dep-feature tweaks
  ├─ cargo-zigbuild --release --target T --bin mxnode (with combo's RUSTFLAGS)
  ├─ optional: upx --best --lzma <bin> (post-process step)
  ├─ measures: binary_bytes, archive_bytes (.tar.gz, .tar.zst, .tar.xz)
  ├─ runs: ./mxnode --version (cold-start, hyperfine, 5 reps, median)
  ├─ runs: ./mxnode --bench-render 1000 (TUI bar; native target only)
  └─ appends one CSV row to dist/bench-size/results.csv
```

### Why a Cargo.toml override and not features

The matrix touches workspace-level profile values (`lto`, `opt-level`) that can't be feature-gated. xtask does an in-place `toml_edit` mutation on a copy of the workspace, builds in `target/bench-size/<combo>/`, then restores the original. Atomic per combo, no leftovers.

### Where measurements run

- **Local macOS box (current dev env, arm64)** — canonical iteration loop. xtask runs natively. Cold-start + bench-render measured on macOS arm64 (and x86_64 cross-build via zig — size-only, since we can't `exec` x86_64 Mach-O on arm64 without Rosetta).
- **Self-hosted Linux runner (x86_64)** — confirmation loop. Same xtask, runs `binary-size-matrix.yml` with the *shortlist* (not the full matrix) once we've picked winners locally. Cold-start + bench-render measured natively for `x86_64-unknown-linux-musl`. `aarch64-unknown-linux-musl` is size-only on that host. UPX-LZMA gets measured here for the first time (UPX targets ELF, not Mach-O).

### Working assumption

Profile/dep wins are platform-agnostic in *direction* (a 12 % win on macOS arm64 ≈ a 10–14 % win on Linux musl); only post-processing (UPX) and link-time bits (musl static libc bloat) differ. We pick winners on macOS, *confirm* on Linux, and only chase Linux-specific outliers if the confirmation row diverges materially (> 30 % delta in win magnitude).

---

## 3. Matrix dimensions & pruning rules

### Phase 0 — baseline (1 build per target)

Current `release.yml` profile, no changes. Establishes the reference row for every other combo.

### Phase 1 — profile sweep (12 combos × 4 targets = 48 builds)

Cartesian product:
- `lto`: `thin` | `fat`
- `opt-level`: `3` | `"s"` | `"z"`
- `strip`: `symbols` | `debuginfo`

Held constant: `codegen-units=1`, `panic="abort"`, `incremental=false`. Pick the **best profile per axis (size-max and perf-safe)** before moving on. Pruning rule: if `lto=thin` dominates `lto=fat` on every metric for a given `(opt-level, strip)`, drop fat for the next phase.

### Phase 2 — dep surgery (cumulative, in order)

Prunes are applied **cumulatively in the listed order**: each is a single Cargo edit, then `cargo check --all-targets --all-features`, then bench. If a prune regresses size or fails `cargo check`/tests, it is **reverted before applying the next**. The Phase 2 final winner = Phase 1 winner + every prune that survived. Order = expected impact, biggest first.

| # | Change | Expected | Risk |
|---|---|---|---|
| 1 | `tracing-subscriber`: drop `env-filter`, hand-roll a `LevelFilter` from `RUST_LOG` | ~400 KB (kills regex+aho-corasick) | medium — module-level filtering must be preserved |
| 2 | Add `tracing` `release_max_level_info` feature | ~50–150 KB (compiles out `debug!`/`trace!` formatters) | low — dev builds keep all levels |
| 3 | `tokio` features: drop unused (verify with grep + `cargo machete`; `cargo udeps` requires nightly so we prefer `machete` on stable) | varies | low if grep is honest |
| 4 | `time` features: drop `macros` if no `format_description!` callsites | ~30–80 KB | low |
| 5 | `clap`: drop `wrap_help` | ~20–60 KB | cosmetic — long help wraps differently |
| 6 | `reqwest`: confirm `gzip` is used; drop if not | ~50–100 KB | low — github API does send gzip, probably keep |
| 7 | `figment`: confirm both `toml` + `env` are used | low | low |
| 8 | `toml` + `toml_edit` overlap — both pulled. Check if we can drop plain `toml` and use `toml_edit` only | ~80–150 KB | medium — `toml_edit`'s deserialize path is slower |
| 9 | **(measurement only, NOT shipped)** disable `prost` + `tokio-tungstenite` + live-log subsystem to record its weight | informational | n/a |
| 10 | `prost` features: drop `prost-derive` if only consumed for decode (not encode) | ~30–80 KB | low — codegen-only feature |
| 11 | `tokio-tungstenite` features: confirm `connect` is the only one needed; drop `native-tls`/`rustls-tls-native-roots` if pulled transitively | ~50–100 KB | low — we use rustls already via reqwest |

Pruning rule: any change that *grows* the binary or fails `cargo check`/tests gets reverted immediately. Change #9 is informational only and never ships.

### Phase 3 — post-processing (Linux only, applied on top of Phase 2 winner)

- UPX: `none` | `--best` | `--best --lzma`
- Archive: `tar.gz` | `tar.zst -19` | `tar.xz -9`

9 combos × 2 Linux targets = 18 measurements (no rebuilds — same binary, different post-process).

UPX caveat: UPX-compressed binary still gets cosign-signed, but the install script's `file mxnode` introspection breaks (it reports "UPX compressed"). Acceptable; install.sh tolerates it.

### Phase 4 — nightly `-Zbuild-std` (size-max only, applied on top of Phase 3 winner)

Single experiment per target: nightly toolchain + `-Zbuild-std=std,panic_abort -Zbuild-std-features=panic_immediate_abort`. 4 builds. If the win is < 5 % we drop it (not worth maintaining a nightly job).

### Total budget

Phase 0 (4) + Phase 1 (48) + Phase 2 (~36, mostly cumulative-incremental) + Phase 3 (18, no rebuilds) + Phase 4 (4) ≈ **~110 builds**. On macOS arm64 with warm sccache, a release build is ~60 s, so the full matrix is roughly **2 hours wall-clock locally**. CI confirmation pass is a shortlist (~10 builds × 4 targets = 40, ~30 min on the self-hosted box).

### Two final winners selected

- `size-max` = best `(profile, deps, post, toolchain)` combo by `binary_bytes` ascending.
- `perf-safe` = best `(profile, deps)` combo where `cold_start_ms ≤ baseline+50` AND `tui_render_ms ≤ baseline×1.05` AND `cargo test wall-clock ≤ baseline×1.05`. No UPX, no nightly.

---

## 4. Measurement methodology

### CSV schema (one row per `(target, combo)`)

| Column | How measured | Notes |
|---|---|---|
| `run_id` | UUIDv4 per xtask invocation | append-only history |
| `target` | matrix label | e.g. `aarch64-apple-darwin` |
| `combo_id` | hash of (profile + deps + post + toolchain) | reproducible label |
| `combo_label` | human-readable | e.g. `lto=fat,opt=z,strip=sym,no-env-filter,upx-lzma` |
| `build_secs` | `time cargo zigbuild` (cold sccache) | informational |
| `binary_bytes` | `stat -c %s` (Linux) / `stat -f %z` (macOS) on stripped+post-processed bin | primary size metric |
| `archive_gz_bytes` | `tar -czf` then `stat` | download metric |
| `archive_zst_bytes` | `tar -I 'zstd -19' -cf` | download metric |
| `archive_xz_bytes` | `tar -cJf` (xz -9) | download metric |
| `cold_start_ms` | `hyperfine --warmup 1 --runs 5 './mxnode --version'` median | only on natively-runnable target |
| `tui_render_ms` | `./mxnode --bench-render 1000` median of 5 | only on natively-runnable target |
| `cargo_test_secs` | `time cargo test --release --no-fail-fast` | only on macOS arm64 (single platform); proxy for "did anything regress functionally" |
| `tests_passed` | exit code of above | gate — combo with failing tests is dropped |
| `live_logs_kb` | informational, populated only by Phase 2 #9 | size of WS+protobuf subsystem |
| `sha256` | `sha256sum bin` | for cache-key dedup |

### Native-runnable matrix

- macOS arm64 host: cold-start + bench-render natively for the macOS-arm64 build. macOS-x86, Linux-x86, Linux-arm: size-only locally.
- Linux x86_64 self-hosted runner: cold-start + bench-render natively for the Linux-x86_64-musl build. Linux-arm64-musl: size-only on that host.

### Tooling

- `hyperfine` for cold-start (median, not mean — robust to first-run noise; `--warmup 1` strips the first cold-page-fault measurement so subsequent runs reflect steady-state cold-start, not file-cache miss).
- `--bench-render` mode shipped behind `cfg(feature = "bench-harness")`. Reads fixture JSON, instantiates the dashboard widget tree, calls `terminal.draw()` N times against `ratatui::backend::TestBackend`. No real terminal, no ANSI emission.
- `cargo test --release --no-fail-fast` runs the existing workspace test suite; wall-clock as proxy for hot-path perf.

If `hyperfine`, `upx`, `zstd`, `xz`, or `cargo-machete` are missing on the host, xtask skips the affected measurement with a CSV row marker `tool_missing=<name>` (e.g. `tool_missing=hyperfine`) rather than failing the whole run. End-of-run hint prints the install commands for whatever was missing (`brew install hyperfine upx zstd xz`, `cargo install cargo-machete`).

### Baseline freezing

Baseline (Phase 0) captured *once* at the start of each xtask run, on the same machine, in the same session. We never compare across machines or across days — only relative to that session's baseline. CSV records the baseline row's `combo_id` so downstream comparisons are anchored.

### Repeatability gate

Each cold-start / bench-render measurement repeated 5×, median reported. Variance > 10 % triggers a "noisy combo" flag in the CSV — perf numbers marked unreliable, user prompted to re-run on a quiet system before trusting the perf-safe winner.

### Out of scope

- Memory footprint at runtime (RSS).
- Network throughput, RPC latency end-to-end.
- Compile-time RAM usage (informational only, not in CSV).

---

## 5. Output format & winners selection

### Artefacts

1. **`dist/bench-size/results.csv`** — append-only, one row per `(target, combo)` per `run_id`.
2. **`dist/bench-size/REPORT.md`** — human-readable, regenerated from CSV on every run.

### REPORT.md structure

```
# Binary size matrix — run <run_id> (<utc_timestamp>)

Host: <uname -a>
Toolchain: <rustc -V>

## Baseline (Phase 0)
<table: target | binary_bytes | archive_gz_bytes | cold_start_ms | tui_render_ms>

## Pareto frontier per target
<table per target: combo_label | binary_bytes | Δ vs baseline | cold_start_ms | tui_render_ms | tests_passed>

## Winners
### size-max
- target=aarch64-apple-darwin: <combo_label>, <binary_bytes> (-XX%)
- (one line per target)
Configuration to apply:
```toml
[profile.release]
lto = "..."
opt-level = "..."
```
Post-process: `upx --best --lzma` (Linux only)
Toolchain: nightly + `-Zbuild-std=std,panic_abort -Zbuild-std-features=panic_immediate_abort`

### perf-safe
<same shape, no UPX, no nightly>

## Informational
- live-logs subsystem weight: <kb>
- regex/aho-corasick (env-filter) weight: <kb>
- <other one-shot prunes that didn't ship>

## Failed combos
<table: combo_label | reason>
```

### Winners selection rules (deterministic, scriptable)

```
size-max winner per target =
  argmin(binary_bytes) over rows where
    tests_passed = true
    AND build_succeeded = true

perf-safe winner per target =
  argmin(binary_bytes) over rows where
    tests_passed = true
    AND build_succeeded = true
    AND combo.upx = none
    AND combo.toolchain = stable
    AND cold_start_ms <= baseline.cold_start_ms + 50
    AND tui_render_ms <= baseline.tui_render_ms * 1.05
    AND cargo_test_secs <= baseline.cargo_test_secs * 1.05
    (the last three checks only apply on natively-runnable targets;
     non-native targets inherit the perf-safe profile/dep choice from
     the natively-runnable target of the same OS family)
```

### Inheritance rule

macOS-x86 inherits the macOS-arm64 perf-safe winner's config; Linux-arm64 inherits the Linux-x86 perf-safe winner's config. Size-only confirmation row still recorded for the inherited target.

### Tie-breaking

If two combos hit identical `binary_bytes`, prefer the one with fewer non-default knobs — simpler config wins.

### Output consumed by

- A human (you) reads `REPORT.md` and approves the winners.
- A follow-up `cargo xtask apply-winner --profile size-max | perf-safe` *patches* `Cargo.toml` + `release.yml` with the selected config. Patch reviewed in a normal PR — never auto-merged.

---

## 6. Phased rollout

**This spec covers Stages A and B.** Stages C–E are *not* implementation-planned here — they require the baseline data Stage B produces. After Stage B lands, a follow-on `docs/superpowers/specs/<date>-binary-size-rollout.md` will be written with concrete Stage C/D/E plans grounded in the measured wins.

- **Stage A — land the harness only (PR #1) [in scope]:** new `crates/xtask`, new `bench-harness` feature in `mxnode-tui`, new `--bench-render` hidden CLI, fixture JSONs, `binary-size-matrix.yml` workflow. No production behaviour change.

- **Stage B — run matrix locally + commit baseline REPORT [in scope]:** generate `dist/bench-size/REPORT-baseline.md` and check it in to `docs/superpowers/specs/<date>-binary-size-results-baseline.md`. Anchors all future comparisons. Optionally trigger `binary-size-matrix.yml` on the self-hosted Linux runner to capture musl numbers in the same commit.

- **Stage C — apply perf-safe winner (PR #2) [deferred to follow-on spec]:** patch `Cargo.toml` profile + dep features per `cargo xtask apply-winner --profile perf-safe`. Re-run matrix in CI to confirm. Commit updated REPORT. Conservative win, ships in next normal release.

- **Stage D — apply size-max post-processing (PR #3, optional) [deferred]:** add UPX-LZMA step to `release.yml` for Linux targets only, signed *after* compression so cosign verifies the compressed blob. macOS targets keep the perf-safe binary (no UPX on Mach-O). Update install.sh's `file mxnode` introspection to tolerate "UPX compressed". Ships only if the size win > 30 % archive reduction.

- **Stage E — nightly `-Zbuild-std` (PR #4, optional) [deferred]:** add a parallel nightly build job to `release.yml` producing `mxnode-<tag>-<target>-min.tar.gz` alongside the stable artefact. Skip if the win < 5 % vs Stage D.

**Auto-rollback:** any deferred stage that triggers a CI red on `integration.yml` or causes install.sh smoke tests to fail gets reverted in the same session. The harness PR (Stage A) is the only stage that lands without conditional gating, because it cannot affect production builds.

---

## 7. Risk & rollback

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `lto=fat` blows up the self-hosted runner's RAM | low | high — release blocked | measure peak RSS during Phase 1; if > 12 GB, cap at `lto=thin` for self-hosted runner, keep `lto=fat` only for local |
| `opt-level="z"` regresses TUI render perf | medium | medium | perf bar enforced in selection rules; if no `z`/`s` combo passes the bar, perf-safe winner = baseline profile |
| Dropping `tracing-subscriber`'s `env-filter` breaks `RUST_LOG=mxnode_rpc=debug` | high | medium — silent UX regression | hand-rolled filter MUST support module-level filtering; xtask test-suite exercises this on the resulting binary; if too complex, revert (it's optional) |
| UPX triggers Linux AV (ClamAV, Hetzner WAF) | low–medium | medium | Stage D ships UPX as a *secondary* artefact `mxnode-<tag>-<target>-upx.tar.gz`, not replacing the primary; install.sh defaults to non-UPX |
| UPX breaks cosign signature workflow | low | high if missed | sign post-UPX; CI smoke step downloads the signed UPX archive and runs `cosign verify-blob` before publishing release |
| Nightly toolchain churn breaks Stage E builds randomly | medium | low — Stage E is optional and parallel; failure doesn't block stable release | nightly job is `continue-on-error: true` in `release.yml`; missing artefact just means no `-min` variant for that release |
| `cargo xtask apply-winner` patches `Cargo.toml` incorrectly | low | medium | `apply-winner` writes to a new branch, opens a PR, never commits to `main`; human reviews diff |
| Bench-render fixture goes stale (mxnode-tui dashboard refactored) | medium | low | fixture JSONs versioned alongside `MetricsSnapshot` schema; CI step compiles `mxnode --features bench-harness` on every PR (cheap) |
| macOS-arm64 wins don't translate to Linux-musl as predicted | medium | medium | Phase 0–4 all re-run on Linux self-hosted runner before Stage C lands; if divergence > 30 %, re-pick winners from Linux data |

### Rollback paths

- Stage A: `git revert` the PR. No production impact.
- Stage C: `git revert` the profile/dep edits in `Cargo.toml`. Next release uses old profile.
- Stage D: remove the UPX step from `release.yml`. Next release ships uncompressed.
- Stage E: drop the nightly job from `release.yml`. Next release ships only the stable artefact.

Each rollback is a single PR, no data migration, no user-facing breakage (install.sh stays compatible across all stages because the primary artefact name doesn't change).

---

## 8. Testing plan

### Per-PR testing

- **Stage A:** `cargo build --release --features bench-harness` succeeds on macOS arm64 + Linux musl x86. `mxnode --bench-render 10` exits 0 and prints `elapsed_ms=<n>` to stderr. `cargo xtask bench-size --target aarch64-apple-darwin --combo baseline-only` produces a CSV row.
- **Stage C:** full `integration.yml` green. Smoke install on a clean Hetzner test box: `curl install.sh | bash`, `mxnode --version`, `mxnode dashboard` opens within 2 s.
- **Stage D:** same as C, plus `cosign verify-blob` succeeds against the UPX archive, plus `mxnode --version` cold-start measured on the Hetzner box ≤ baseline + 200 ms (UPX adds decompression).
- **Stage E:** same as D, plus the `-min` archive smoke-installs on three distros (Ubuntu 24.04, Debian 12, Alpine 3.20).

### Bench harness self-tests (in `crates/xtask/tests/`)

- CSV writer produces parseable RFC 4180 output.
- Winner-selection rules produce the expected combo on a hand-crafted CSV fixture.
- `apply-winner` produces an idempotent diff against a fixture `Cargo.toml` (running it twice = same result).

### Out of scope

End-to-end validator-network tests with the optimised binary on real chain (operator's responsibility post-release; install.sh smoke + integration.yml is our gate).

---

## Appendix A — current dep landscape (snapshot 2026-05-04)

Workspace: 9 internal crates + 1 binary. Lockfile: 287 unique crates.

Heavy direct deps with feature exposure:
- `tokio = ["rt-multi-thread", "macros", "process", "fs", "io-util", "time"]`
- `reqwest = ["rustls-tls", "json", "gzip", "stream"]` (`default-features = false`)
- `ratatui` (`default-features = false`, `["crossterm"]`)
- `crossterm` (default features)
- `tokio-tungstenite = ["connect"]` (`default-features = false`)
- `prost`
- `clap = ["derive", "env", "wrap_help"]`
- `figment = ["toml", "env"]`
- `tracing-subscriber = ["env-filter", "fmt"]`
- `time = ["serde", "formatting", "parsing", "macros"]`
- `toml` AND `toml_edit` (potential overlap)

Already-applied profile (`[profile.release]`):
```toml
lto = "thin"
codegen-units = 1
strip = "symbols"
panic = "abort"
```

Already-applied build env (in `release.yml`):
```yaml
RUSTFLAGS: '-C strip=symbols'
```

Toolchain: `1.94.1` stable, pinned via `rust-toolchain.toml`, with cross-targets pre-installed.

Build linker: `cargo-zigbuild` (zig as universal C linker, hermetic, no per-target sysroot needed).

---

## Appendix B — open questions deferred to implementation

1. Should `--bench-render` be a top-level `bench-render` subcommand or a `--bench-render` flag on the existing root command? (Implementation detail; pick whichever doesn't conflict with clap's existing argument tree.)
2. Should `xtask` use `cargo-zigbuild` directly via subprocess, or import it as a library? (Subprocess is simpler and matches CI; import couples us to cargo-zigbuild's internal API.)
3. Storage location for `dist/bench-size/results.csv` — gitignored locally, or checked in for trend-tracking? (Recommendation: gitignore the CSV; commit only the curated `REPORT-baseline.md` + per-stage REPORTs.)

These are deferred to the writing-plans phase, not blockers for the design.
