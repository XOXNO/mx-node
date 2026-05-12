# mxnode

[![Release](https://img.shields.io/github/v/release/XOXNO/mx-node?label=release)](https://github.com/XOXNO/mx-node/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/XOXNO/mx-node/integration.yml?branch=main&label=ci)](https://github.com/XOXNO/mx-node/actions/workflows/integration.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)

A single static Rust binary for installing, upgrading, and operating **MultiversX** nodes — validators, observer squads, multikey nodes, and the proxy — on Linux hosts via `systemd`.

A modern, typed replacement for [`multiversx/mx-chain-scripts`](https://github.com/multiversx/mx-chain-scripts).

> `mxnode` does **not** phone home. The only outbound network requests it makes are to GitHub Releases (for upgrades) and the local node REST API.

---

## Install

One-liner — works on Linux x86_64/aarch64 and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/XOXNO/mx-node/main/install.sh | sh
```

What the installer does:

1. Detects OS + CPU and resolves the latest release tag from GitHub.
2. Downloads the matching tarball and `SHA256SUMS`.
3. Verifies `sha256`.
4. Verifies the keyless **cosign** signature against the canonical GitHub Actions OIDC identity for `release.yml` (skipped if `cosign` isn't installed — set `MXNODE_REQUIRE_COSIGN=1` to make it mandatory).
5. Installs to `/usr/local/bin/mxnode` (or `--dir <PATH>`).
6. Runs `mxnode --version` to confirm the install succeeded.

### Installer options

| Flag / env                       | Default            | Purpose                                                                |
| -------------------------------- | ------------------ | ---------------------------------------------------------------------- |
| `--version <TAG>` / `MXNODE_VERSION` | `latest`       | Pin a specific release (e.g. `v0.9.1`).                                |
| `--dir <PATH>` / `MXNODE_INSTALL_DIR` | `/usr/local/bin` | Install destination. Use `$HOME/.local/bin` for a user-only install. |
| `--force` / `MXNODE_FORCE=1`      | off                | Reinstall even when the requested version is already present.          |
| `--min` / `MXNODE_VARIANT=min`    | off                | Bandwidth-optimised artefact (~10–18% smaller, same functionality).    |
| `MXNODE_REQUIRE_COSIGN=1`         | off                | Hard-fail when `cosign` is missing or the release isn't signed.        |
| `MXNODE_GITHUB_TOKEN`             | unset              | Avoids the 60 req/h anonymous GitHub API limit on `latest` lookups.    |

```sh
# Pin a release
curl -fsSL https://raw.githubusercontent.com/XOXNO/mx-node/main/install.sh | sh -s -- --version v0.9.1

# User-only install (no sudo)
curl -fsSL https://raw.githubusercontent.com/XOXNO/mx-node/main/install.sh | sh -s -- --dir "$HOME/.local/bin"

# Strict mode: refuse anything that isn't cosign-verified
MXNODE_REQUIRE_COSIGN=1 curl -fsSL https://raw.githubusercontent.com/XOXNO/mx-node/main/install.sh | sh
```

### Self-update later

```sh
mxnode self-update         # latest
mxnode self-update --tag v0.9.1
mxnode self-update --check # current vs latest, no download
```

---

## Quick start

Install one observer squad with the proxy on mainnet, then watch it sync:

```sh
mxnode install --role observer --squad --with-proxy   # phase-by-phase install
mxnode start --all                                    # start units
mxnode status --watch                                 # live TUI dashboard
```

Or a single validator (the binary expects `node-{i}.zip` in `$HOME/VALIDATOR_KEYS/`):

```sh
mxnode install --role validator
mxnode keys check       # confirms key archives are present
mxnode start --all
mxnode status           # one-shot table
```

Switch network before installing (default is mainnet):

```sh
mxnode config set network.environment testnet
```

---

## What you get

- **One binary, one config file.** Everything lives in `~/.config/mxnode/mxnode.toml` (mode `0600`) — operator settings, host inventory, secrets, and update cache.
- **All node shapes.** Single validator, validator squad, observer, 4-node observer squad, 4-shard multikey, plus the MultiversX proxy. Backup hosts via `--backup [N]`.
- **First-class `mx-chain-go` operation modes.** `--operation-mode full-archive | db-lookup-extension | historical-balances | snapshotless-observer` plumbed straight into the supervisor command line.
- **Live multi-node dashboard.** `mxnode status --watch` opens a ratatui TUI with sparklines, per-shard tabs, color-coded sync state, and an embedded WS log stream — replaces upstream `termui` + `logviewer`.
- **Safe rolling upgrades.** `mxnode upgrade --binary-tag X --config-tag Y` preserves `prefs.toml`, recomputes only changed nodes, and supports `--strategy rolling` with `--max-parallel`.
- **Import-DB workflows.** `mxnode db import` for single-node, `mxnode db import-plan` for shard + metachain validation with `--require-elasticsearch` for Elasticsearch backfill setups.
- **Migrate from bash in place.** `mxnode import-bash` reads your existing `mx-chain-scripts` install (markers, `variables.cfg`, units, proxy) and lifts it into `mxnode.toml` without touching the bash files.
- **Typed config + safety gates.** `mxnode config validate` and `mxnode doctor` (with `--benchmark` for host capability) gate every state-changing command — bypass with `--force` when you know what you're doing.
- **Signed releases.** Every tarball ships SHA256 + keyless cosign signatures via Sigstore. Verify with `MXNODE_REQUIRE_COSIGN=1` or manually:

  ```sh
  cosign verify-blob \
      --signature mxnode-v0.9.1-x86_64-unknown-linux-musl.tar.gz.sig \
      --certificate mxnode-v0.9.1-x86_64-unknown-linux-musl.tar.gz.pem \
      --certificate-identity https://github.com/XOXNO/mx-node/.github/workflows/release.yml@refs/tags/v0.9.1 \
      --certificate-oidc-issuer https://token.actions.githubusercontent.com \
      mxnode-v0.9.1-x86_64-unknown-linux-musl.tar.gz
  ```

---

## Command surface

| Command          | Purpose                                                                   |
| ---------------- | ------------------------------------------------------------------------- |
| `install`        | Fresh install. `--add N` extends an existing install.                     |
| `upgrade`        | Rolling upgrade (or downgrade) for nodes, the proxy, or both.             |
| `uninstall`      | Remove units, binaries, and `mxnode.toml`. Dry-run by default.            |
| `start` / `stop` / `restart` | Lifecycle by `--all`, `--node N`, `--shard N`, or `--select expr`.    |
| `status`         | Health snapshot. `--watch` on a TTY launches the live TUI.                |
| `logs`           | Tail (`-f`), archive (`--save-archive`), or stream `/log` WebSocket.      |
| `metrics`        | Prometheus endpoint on `--port`.                                          |
| `config`         | `show` / `get` / `set` / `edit` / `validate` / `apply` (re-apply edits).  |
| `keys`           | `check` (verify zips), `generate` (keygenerator), `rename`.               |
| `db`             | `prune`, `remove`, `reseed`, `import`, `import-plan`.                     |
| `doctor`         | Full host diagnostic; `--benchmark` adds CPU/memory/disk-IO benchmarks.   |
| `import-bash`    | Lift an existing bash install into `mxnode.toml`.                         |
| `self-update`    | Verify + replace the running binary.                                      |
| `completions`    | Print shell completion script (`bash`/`zsh`/`fish`/`elvish`/`powershell`).|

Every command honours `--json`, `--verbose`/`--quiet`, and `--force`. Run `mxnode <cmd> --help` for the full surface.

---

## Common operations

```sh
# Four pinned observers + proxy in snapshotless mode
mxnode install --role observer --squad --with-proxy --operation-mode snapshotless-observer

# Archive observer (single node)
mxnode install --role observer --count 1 --operation-mode full-archive

# Multikey signer host with a custom keys bundle
mxnode install --role multikey --keys-file /srv/keys/allValidatorsKeys.pem

# Bash-style upgrade: resolves the latest config release, reads its
# `binaryVersion` file, swaps binary + config on every node, and leaves
# the units stopped so you can verify before `mxnode start --all`.
mxnode upgrade

# Pin the config tag (binary follows from the repo's binaryVersion)
mxnode upgrade --config-tag T1.7.99.0

# Pin both for full control (expert mode — config X with binary Y)
mxnode upgrade --config-tag T1.7.99.0 --binary-tag v1.7.99

# Auto-start every node after the swap (rolling restart + readiness probe)
mxnode upgrade --start

# Import an existing database (dry-run first)
mxnode db import --node 0 --source /srv/import-db --dry-run
mxnode db import --node 0 --source /srv/import-db --replace --yes

# Validate a full-shard import plan, optionally enforcing Elasticsearch wiring
mxnode db import-plan --source-root /srv/imports --require-elasticsearch --output /tmp/plan.json

# Re-apply per-node config overrides without touching binaries or restarting units
mxnode config apply

# Stream the node /log WebSocket with a runtime log profile + local save
mxnode logs --ws --node 0 --log-level '*:DEBUG,api:INFO' --log-save

# Generate shell completions
mxnode completions bash > ~/.local/share/bash-completion/completions/mxnode

# Migrate an existing mx-chain-scripts install (read-only against bash files)
mxnode import-bash --from "$HOME" --execute

# Tear it all down (dry-run by default; --execute is the second gate)
mxnode uninstall --yes --execute
```

---

## Configuration

Everything lives in **one file**: `~/.config/mxnode/mxnode.toml` (mode `0600`).

```sh
mxnode config show              # merged view
mxnode config show --origin     # where each value came from
mxnode config get network.environment
mxnode config set network.environment testnet --scope user
mxnode config edit              # open in $EDITOR
mxnode config validate --strict # also check network reachability + token
```

The file is split into typed sections — `[network]`, `[paths]`, `[node]`, `[proxy]`, `[install]`, `[overrides]`, `[metrics]`, `[branding]`, `[[nodes]]`, `[host]`, `[secrets]`, `[update_cache]`. Run `mxnode config show --origin` to see which layer (defaults / file / flags) supplied each value.

---

## Architecture

```
crates/
  mxnode-core        Typed primitives: Environment, NodeIndex, Shard, Role, Tag, Paths, Config, State
  mxnode-config      3-layer resolver (defaults → file → flags) + validate()
  mxnode-state       mxnode.toml read/write, atomic-rename, flock, transaction log
  mxnode-github      ETag-cached release client; rate-limit aware
  mxnode-rpc         Typed REST client for /node/status, /node/bootstrapstatus, etc.
  mxnode-systemd     Unit rendering (golden-tested) + systemctl wrappers
  mxnode-toolchain   Go toolchain bootstrap for source builds
  mxnode-build       git clone + cargo-style build cache
  mxnode-tui         ratatui multi-node dashboard
  mxnode-update      Once-per-day update-check gate with sticky 24h cache
bins/
  mxnode             The CLI binary
```

The same orchestrator powers `install`, `install --add N`, and `upgrade`. Every state-changing command flock-guards `mxnode.toml`, writes atomically (write-temp + rename), and appends a structured event to the migration log.

---

## Build from source

Prereqs: stable Rust 1.94+ (`rust-toolchain.toml` pins it for you), `git`, `cmake`, a working C toolchain.

```sh
git clone https://github.com/XOXNO/mx-node
cd mx-node
cargo build --release
./target/release/mxnode --version
```

Run the test suite:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

End-to-end sweep against the compiled binary (idempotent; cleans up after itself):

```sh
./tests/integration/sweep.sh ./target/release/mxnode
```

---

## Status

Active development. CLI surface and config schema are stable for the `v0.9.x` line — see [release notes](https://github.com/XOXNO/mx-node/releases) for breaking changes between minor versions.

Report bugs or request features at <https://github.com/XOXNO/mx-node/issues>.

---

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
