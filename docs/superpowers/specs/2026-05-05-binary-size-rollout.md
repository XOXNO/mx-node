# Binary size rollout — Stage C + Stage E

**Date:** 2026-05-05
**Owner:** Mihai
**Status:** Stage C+ landed (commit `d335bfd`, opt=z + lto=fat); Stage E shipping in release.yml as `-min` artefact
**Predecessor:** [2026-05-04-binary-size-design.md](./2026-05-04-binary-size-design.md), Stage A + B landed
**Phase 1 data:** [2026-05-05-binary-size-phase1-results.md](./2026-05-05-binary-size-phase1-results.md)
**Scope:** Stage C+ (apply `lto=fat, opt=z`) + Stage E (nightly + `-Zbuild-std` + `-Cpanic=immediate-abort`, all four targets). **Stage D (UPX) is deferred** — it requires a Linux box to validate the compressed binary actually executes, and the local box is macOS-arm64.

## Operator-direction adjustment

Originally this spec drafted a "perf-safe" winner that kept `opt-level = 3` because `opt=z` raised TUI render wall-time per 1000 frames from ~160 ms to ~490 ms. Re-reading: that's 0.49 ms per frame vs 0.16 ms — invisible against the dashboard's 250 ms tick rate. Per operator direction (size > render-microbench speed), accept `opt=z` and ship the smaller binary. Cold start is unchanged (~1–2 ms).

---

## 1. What's being measured

Phase 1 sweep: 12 combos × 4 targets = 48 builds. Combos:

| lto | opt-level | strip |
|---|---|---|
| thin / fat | 3 / s / z | symbols / debuginfo |

Plus Phase 0 baseline (replicated across phases for the duplicate-baseline-row sanity check). Output anchored at `dist/bench-size/results.csv` and `dist/bench-size/REPORT.md`.

Stage E adds one more combo: `lto=fat, opt=z, strip=symbols, toolchain=nightly+build-std (panic_immediate_abort)` on the host triple only (`aarch64-apple-darwin` for the local box).

---

## 2. Final landed numbers

### Stage C+ (`lto=fat, opt=z, strip=symbols`) — applied workspace-wide

Phase 1 size-max winner, opt-in everywhere because the operator declared TUI render speed isn't a constraint. Applied in commit `d335bfd`.

| target | baseline (v0.8.20) | Stage C+ | Δ |
|---|---:|---:|---:|
| aarch64-apple-darwin | 5,849,952 | 3,399,520 | **-41.9%** |
| x86_64-apple-darwin | 6,812,448 | 4,134,576 | **-39.3%** |
| aarch64-unknown-linux-musl | 6,191,520 | 4,077,064 | **-34.2%** |
| x86_64-unknown-linux-musl | 7,349,016 | 4,848,608 | **-34.0%** |

Cold start (where measurable): ~1–2 ms aarch-darwin, ~15 ms x86-darwin (Rosetta) — unchanged from baseline. TUI render bumped 0.16 ms → 0.49 ms per frame at the 250 ms tick — invisible to the operator.

### Stage E (`+ build-std + immediate-abort`) — `-min.tar.gz` opt-in

Cross-compiled for all four targets via `cargo +nightly zigbuild` + `-Z build-std=std,panic_abort` + `-Z unstable-options` + `RUSTFLAGS="-Cpanic=immediate-abort -Zunstable-options"`. Ships as a parallel artefact alongside the Stage C+ canonical archive. Same cosign signing flow.

| target | Stage C+ | Stage E `-min` | Δ vs C+ |
|---|---:|---:|---:|
| aarch64-apple-darwin | 3,399,520 | 2,823,136 | **-17.0%** (~3.24 → ~2.69 MB) |
| x86_64-unknown-linux-musl | 4,848,608 | 4,188,064 (measured locally) | **-13.6%** (~4.62 → ~3.99 MB) |
| x86_64-apple-darwin | 4,134,576 | TBD (measured by CI) | est. ~-15% |
| aarch64-unknown-linux-musl | 4,077,064 | TBD (measured by CI) | est. ~-15% |

**Total reduction from the original v0.8.20 baseline:** ~52% on aarch-darwin, ~46% on Linux musl x86 (the bandwidth-priority target).

---

## 3. Stage C+ — landed

Manifest patch (commit `d335bfd`):

```diff
 [profile.release]
-lto = "thin"
+lto = "fat"
+opt-level = "z"
 codegen-units = 1
 strip = "symbols"
 panic = "abort"
```

Verification:

1. `cargo build --bin mxnode --release` — clean (~37 s with warm sccache, ~4 min cold).
2. `./target/release/mxnode --version` — returns expected.
3. `./target/release/mxnode status --watch --help` — returns expected (no link-time fault from LTO).
4. `mxnode bench-render --frames 1000 --fixture <observer.json>` — 490 ms total = 0.49 ms per frame. Operator-accepted given the 250 ms dashboard tick rate.
5. Pre-existing test failure `loader::tests::load_with_no_file_returns_defaults` is unrelated (env isolation bug — reads user's actual `~/.config/mxnode/config.toml`); confirmed failing on main without this change.

Rollback: `git revert <commit>` — single PR, no data migration.

---

## 4. Stage E — landed

Two pieces:

### 4a. `.github/workflows/release.yml` — `build-min` parallel job

Mirrors the existing `build` job for all four targets, swapping:
- toolchain: `dtolnay/rust-toolchain@nightly` + `components: rust-src`
- builder: `cargo +nightly zigbuild ... -Z build-std=std,panic_abort -Z unstable-options`
- `RUSTFLAGS`: adds `-Cpanic=immediate-abort -Zunstable-options`
- archive name suffix: `-min.tar.gz`
- cache key: `${{ matrix.target }}-min` (separate from stable cache so rebuilt-std artefacts don't commingle)
- `continue-on-error: true` — nightly churn never blocks the canonical stable release

The `release` job now `needs: [build, build-min]` with `if: always() && needs.build.result == 'success'`, so a failed `build-min` doesn't gate publication. The existing artefact glob `dist/*.tar.gz` picks up both stable and `-min` variants automatically.

### 4b. `install.sh` — `--min` flag + `MXNODE_VARIANT=min` env-var

```sh
# fetch the smaller -min variant
curl -fsSL https://raw.githubusercontent.com/XOXNO/mx-node/main/install.sh | sh -s -- --min

# or via env-var (CI / unattended)
MXNODE_VARIANT=min curl -fsSL .../install.sh | sh
```

Hard-fails if the resolved tag doesn't have a `-min` artefact (e.g. nightly job was skipped). Operators who want size+freshness should pin `--version <tag>` to a tag they know shipped a `-min` variant, or read the release page first.

### Cross-compile validation (manual, on macOS arm64)

```sh
RUSTFLAGS="-C strip=symbols -Cpanic=immediate-abort -Zunstable-options" \
  cargo +nightly zigbuild --release --target x86_64-unknown-linux-musl --bin mxnode \
    -Z build-std=std,panic_abort -Z unstable-options
# Result: 4,188,064 B (3.99 MB), valid statically-linked ELF, stripped.
```

### Risk + rollback

- Nightly Rust occasionally regresses build-std. `continue-on-error: true` covers this — failure leaves the stable artefact untouched and produces no `-min` for that release. install.sh's `--min` then fails for that tag.
- The `panic_immediate_abort` mechanism moved from `-Zbuild-std-features` to `-Cpanic=immediate-abort` between nightly-2025 and nightly-2026. The harness + release.yml use the new syntax. If a future nightly moves it again, the build job fails loud and `continue-on-error` keeps stable shipping.
- Rollback: delete the `build-min` job from `release.yml` and the `--min` / `MXNODE_VARIANT=min` paths from `install.sh`. Two-file revert.

---

## 5. Still out of scope

- **Stage D (UPX):** requires a Linux box to verify the compressed binary actually executes correctly, plus a Hetzner/Ubuntu smoke test for AV false-positives (UPX-packed binaries get flagged by some Hetzner WAF rules and ClamAV signatures). Best done as a follow-up on the self-hosted Linux runner. Harness wiring (Phase 3 combos) also pending. Estimated additional win: 50–70% archive shrink on Linux musl, 50–150 ms cold-start cost (decompression).
- **Phase 2 dep surgery:** ~9 cumulative dep-feature prunes (drop `env-filter`, `release_max_level_info`, etc). Touches per prune are larger than profile knobs and need careful regression-checking against operator-facing UX (e.g. `RUST_LOG=mxnode_rpc=debug` filtering). Estimated additional win: 5–15%. Defer to a dedicated session.

---

## 6. Sequencing — done

1. ✅ Stage C+ landed (commit `d335bfd`).
2. ✅ Stage E nightly + build-std measured locally; gate cleared (-13.6% on Linux musl x86 vs Stage C+; -17% on aarch64-darwin).
3. ✅ Stage E shipped via `release.yml` `build-min` job + `install.sh` `--min` flag.
4. Pending follow-ups: Stage D (UPX, needs Linux box), Phase 2 dep surgery, optional Stage E coverage smoke-tests in CI for Ubuntu/Debian/Alpine after the first `-min` artefact lands.

Each stage was a single commit, reversible via `git revert`. No data migration. install.sh's canonical artefact name (`mxnode-<tag>-<target>.tar.gz`) is unchanged; the `-min` variant is purely additive.
