# Binary size rollout — Stage C + E

**Date:** 2026-05-05
**Owner:** Mihai
**Status:** Stage C landed (commit `620e71a`); Stage E pending nightly measurement
**Predecessor:** [2026-05-04-binary-size-design.md](./2026-05-04-binary-size-design.md), Stage A + B landed
**Phase 1 data:** [2026-05-05-binary-size-phase1-results.md](./2026-05-05-binary-size-phase1-results.md)
**Scope:** Stage C (apply `perf-safe` Phase 1 winner) + Stage E (nightly `-Zbuild-std` host-only). **Stage D (UPX) is deferred** — it requires a Linux box to validate the compressed binary actually executes, and the local box is macOS-arm64.

---

## 1. What's being measured

Phase 1 sweep: 12 combos × 4 targets = 48 builds. Combos:

| lto | opt-level | strip |
|---|---|---|
| thin / fat | 3 / s / z | symbols / debuginfo |

Plus Phase 0 baseline (replicated across phases for the duplicate-baseline-row sanity check). Output anchored at `dist/bench-size/results.csv` and `dist/bench-size/REPORT.md`.

Stage E adds one more combo: `lto=fat, opt=z, strip=symbols, toolchain=nightly+build-std (panic_immediate_abort)` on the host triple only (`aarch64-apple-darwin` for the local box).

---

## 2. Winners (filled in after sweep completes)

### Per-target Phase 1 perf-safe winner (applied)

> Selection rule: `argmin(binary_bytes)` where `tests_passed=true`, `upx=none`, `toolchain=stable`, AND host-runnable rows pass the perf bar (cold-start ≤ baseline+50 ms, TUI render ≤ baseline×1.05). Linux targets inherit the macOS arm64 choice because exec is unavailable on the macOS arm64 host.

Universal winner: `lto=fat, opt=3, strip=symbols`. Already applied in commit `620e71a`.

| target | binary_bytes | Δ vs baseline | cold_ms | tui_ms |
|---|---:|---:|---:|---:|
| aarch64-apple-darwin | 5,347,696 | -8.6% | 1–2 | 161–163 (vs 159–168 baseline) |
| x86_64-apple-darwin | 6,368,164 | -6.5% | 15–16 | 193–197 (vs 187–202 baseline) |
| aarch64-unknown-linux-musl | 5,770,960 | -6.8% | n/a | n/a |
| x86_64-unknown-linux-musl | 6,930,448 | -5.7% | n/a | n/a |

### Stage E nightly + build-std (host only) — pending measurement

| target | combo | binary_bytes | Δ vs Stage C |
|---|---|---:|---:|
| aarch64-apple-darwin | `lto=fat,opt=z,strip=sym,build-std` | `<TBD>` | `<TBD>` |

---

## 3. Apply the winner (Stage C)

### Decision rule

If the perf-safe winner across all four targets converges on the **same profile** (likely — profile knobs are platform-agnostic), apply globally to `[profile.release]` in the workspace `Cargo.toml`. If targets disagree (unlikely), pick the choice that best balances Linux-musl (the priority axis) without regressing macOS more than 5%.

### Manifest patch (applied)

```diff
 [profile.release]
-lto = "thin"
+lto = "fat"
 codegen-units = 1
 strip = "symbols"
 panic = "abort"
```

opt-level stays at the default (3) — `s` and `z` regress TUI render by 20–200% on the dashboard hot path, breaking the perf bar. They land later, gated, via Stage D / Stage E only.

### Verification gate (per CLAUDE.md §6)

After applying:

1. `cargo build --bin mxnode --release` clean.
2. `cargo test --workspace --release` — no regressions vs main.
3. `cargo xtask bench-size --baseline-only` — confirms the new "baseline" matches the perf-safe winner numbers from the prior sweep within ±2% (sanity).
4. Smoke: `./target/release/mxnode --version` returns expected.
5. Smoke: `./target/release/mxnode dashboard --help` returns expected (no link-time fault).

### Rollback

`git revert <commit>` — single PR, no migration. Next release uses the prior profile.

---

## 4. Apply nightly (Stage E)

### Trigger

Stage E lands **only** if Phase 4 measurement shows ≥ 5% reduction beyond the Stage C winner. Below that, the cost of maintaining a nightly job in `release.yml` outweighs the win.

### Release.yml patch (template, conditional)

```yaml
# .github/workflows/release.yml — new parallel job
build-min:
  if: github.repository_owner == 'XOXNO'
  needs: build  # runs alongside the stable matrix
  strategy:
    matrix:
      target:
        - aarch64-apple-darwin
        - aarch64-unknown-linux-musl
  runs-on: [self-hosted, Linux, X64]
  continue-on-error: true  # nightly churn doesn't block stable release
  steps:
    - uses: actions/checkout@v6
    - uses: dtolnay/rust-toolchain@nightly
      with:
        components: rust-src
        targets: ${{ matrix.target }}
    - name: Build (nightly + build-std)
      run: |
        cargo +nightly zigbuild --release --target "${{ matrix.target }}" --bin mxnode \
          -Z build-std=std,panic_abort \
          -Z build-std-features=panic_immediate_abort
    - name: Package + sign + upload as `mxnode-<tag>-<target>-min.tar.gz`
      # Same packaging shape as the stable build job, suffix `-min`.
```

The stable artefact remains the canonical download; `-min.tar.gz` is opt-in for users on size-constrained hosts.

### Risk

- Nightly Rust occasionally regresses build-std for cross-compile. The `continue-on-error` gate ensures this never blocks a stable release.
- macOS arm64 and Linux musl arm64 cover the most common bandwidth-constrained targets. macOS x86_64 + Linux x86_64 stay on stable (those users are typically on dev workstations / cloud, not bandwidth-constrained).

---

## 5. Out of scope

- **Stage D (UPX):** requires a Linux box to verify the compressed binary actually executes correctly, plus a Hetzner/Ubuntu smoke test for AV false-positives. Defer to a session run on the self-hosted Linux runner. Harness wiring (Phase 3 combos) also pending.
- **Phase 2 dep surgery:** ~9 cumulative dep-feature prunes (drop `env-filter`, `release_max_level_info`, etc). Significant code touches per prune; defer to a dedicated session.
- **Cross-compile nightly + build-std:** would need cargo-zigbuild installed under the nightly toolchain. Doable but skipped for Stage E.

---

## 6. Sequencing

1. Land Stage C as PR #1 — commits the new `[profile.release]` and the updated baseline REPORT.
2. Run `cargo xtask bench-size --nightly` locally to capture Stage E data, append to results.
3. If Stage E gate passes (≥5% win), land Stage E as PR #2 — adds the parallel `build-min` job to `release.yml`.
4. Stage D, Phase 2, and cross-compile nightly remain pending.

Each stage is its own PR, reversible via `git revert`. No data migration. install.sh keeps consuming the primary `mxnode-<tag>-<target>.tar.gz` artefact across all stages.
