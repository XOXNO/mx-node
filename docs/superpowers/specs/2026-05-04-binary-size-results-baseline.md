# Binary size matrix — Stage B baseline

> Captured by Stage B of `docs/superpowers/specs/2026-05-04-binary-size-design.md`.
> This is the **Phase 0 baseline only** — current `release.yml` profile, no
> changes. Winners == baseline because there are no other combos to compare
> against yet. Stage C/D/E follow-on specs will land subsequent matrix runs
> (profile sweep, dep surgery, post-processing, nightly) and supersede the
> winners columns below with real picks.
>
> Cold-start + TUI render measured natively for `aarch64-apple-darwin` (the
> host) and via Rosetta for `x86_64-apple-darwin`. Linux targets are
> size-only on this host — the self-hosted Linux runner will populate those
> columns via `binary-size-matrix.yml`.

# Binary size matrix — run 29f85a1eb4a43156
_2026-05-04T11:40:14Z_

Host: `Darwin xoxno.local 25.4.0 Darwin Kernel Version 25.4.0: Thu Mar 19 19:26:07 PDT 2026; root:xnu-12377.101.15~1/RELEASE_ARM64_T6031 arm64`
Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`

## Baseline (Phase 0)

| target | binary_bytes | cold_start_ms | tui_render_ms |
|---|---:|---:|---:|
| aarch64-apple-darwin | 5849952 | 4 | 217 |
| aarch64-unknown-linux-musl | 6191520 | - | - |
| x86_64-apple-darwin | 6812448 | 27 | 259 |
| x86_64-unknown-linux-musl | 7349016 | - | - |

## Pareto frontier per target

### aarch64-apple-darwin

| combo | bytes | Δ vs baseline | cold_ms | tui_ms | tests |
|---|---:|---:|---:|---:|:-:|
| lto=thin,opt=3,strip=sym | 5849952 | +0.0% | 4 | 217 | ✓ |

### aarch64-unknown-linux-musl

| combo | bytes | Δ vs baseline | cold_ms | tui_ms | tests |
|---|---:|---:|---:|---:|:-:|
| lto=thin,opt=3,strip=sym | 6191520 | +0.0% | - | - | ✓ |

### x86_64-apple-darwin

| combo | bytes | Δ vs baseline | cold_ms | tui_ms | tests |
|---|---:|---:|---:|---:|:-:|
| lto=thin,opt=3,strip=sym | 6812448 | +0.0% | 27 | 259 | ✓ |

### x86_64-unknown-linux-musl

| combo | bytes | Δ vs baseline | cold_ms | tui_ms | tests |
|---|---:|---:|---:|---:|:-:|
| lto=thin,opt=3,strip=sym | 7349016 | +0.0% | - | - | ✓ |

## Winners

### aarch64-apple-darwin
- **size-max**: `lto=thin,opt=3,strip=sym` — 5849952 bytes
- **perf-safe**: `lto=thin,opt=3,strip=sym` — 5849952 bytes

### aarch64-unknown-linux-musl
- **size-max**: `lto=thin,opt=3,strip=sym` — 6191520 bytes
- **perf-safe**: `lto=thin,opt=3,strip=sym` — 6191520 bytes

### x86_64-apple-darwin
- **size-max**: `lto=thin,opt=3,strip=sym` — 6812448 bytes
- **perf-safe**: `lto=thin,opt=3,strip=sym` — 6812448 bytes

### x86_64-unknown-linux-musl
- **size-max**: `lto=thin,opt=3,strip=sym` — 7349016 bytes
- **perf-safe**: `lto=thin,opt=3,strip=sym` — 7349016 bytes

