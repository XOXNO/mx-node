# mxnode

Rust replacement for [`multiversx/mx-chain-scripts`](https://github.com/multiversx/mx-chain-scripts).

A single static binary for installing, upgrading, and operating MultiversX nodes (validators, observer squads, multikey nodes, the proxy) on Linux hosts via systemd.

Status: **early development (Phase 0)**.

## Goals

- Newcomer flow: `mxnode init && mxnode install && mxnode start --all && mxnode status`.
- Power users: every default overridable via config file or CLI flag.
- Safe defaults that respect MultiversX validator economics (rating, jailing).
- No phone-home. The binary makes outbound network requests only to GitHub Releases (for upgrades) and the local node REST API.

## Plan

See `/Users/mihaieremia/.claude/plans/go-into-research-and-vectorized-hammock.md` (the architecture plan).

## Build

```sh
cargo build --release
```

## Examples

```sh
# Four pinned observers plus proxy, using mx-chain-go snapshotless mode.
mxnode install --role observer --squad --with-proxy --operation-mode snapshotless-observer

# Builder/archive host with first-class operation-mode plumbing.
mxnode install --role observer --count 1 --operation-mode full-archive

# Import an existing database into a stopped node. Dry-run first; pass
# --replace --yes only when the target db/ can be emptied.
mxnode db import --node 0 --source /srv/import-db --dry-run
mxnode db import --node 0 --source /srv/import-db --replace --yes

# Validate a full shard/metachain import plan before Elasticsearch backfill.
mxnode db import-plan --source-root /srv/imports --require-elasticsearch --output /tmp/import-plan.json

# Generate shell completions from the actual command tree.
mxnode completions bash > ~/.local/share/bash-completion/completions/mxnode

# Rolling upgrade with a pinned config repo tag; prefs.toml is preserved.
mxnode upgrade --binary-tag v1.7.99 --config-tag T1.7.99.0

# Check host readiness against MultiversX system requirements and mxnode state.
mxnode doctor

# Stream the node /log WebSocket with a runtime log profile and local save.
mxnode logs --ws --node 0 --log-level '*:DEBUG,api:INFO' --log-save

# Run the installed upstream seednode utility (pass upstream flags after --).
mxnode seednode -- --help
```

## Layout

```
crates/
  mxnode-core      types: Environment, NodeIndex, Shard, Role, Tag, Paths, Config, State
  mxnode-config    3-layer resolver (defaults → file → flags); validate()
  mxnode-state     state.toml read/write, atomic-rename, flock, transaction log
  mxnode-github    ETag-cached release client; rate-limit aware
  mxnode-rpc       typed REST client for /node/status etc.
  mxnode-systemd   unit rendering (golden-tested) + systemctl wrappers
bins/
  mxnode           the CLI binary
```
