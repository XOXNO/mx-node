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
