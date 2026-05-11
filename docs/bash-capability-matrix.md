# mx-chain-scripts Capability Matrix

This matrix tracks the Bash surface from `/Users/mihaieremia/GitHub/mx-chain-scripts`
against the Rust `mxnode` CLI, plus the validator docs paths that drove the
current pass.

## Sources

- Bash entrypoint: `/Users/mihaieremia/GitHub/mx-chain-scripts/script.sh`
- Bash implementation: `/Users/mihaieremia/GitHub/mx-chain-scripts/config/functions.cfg`
- Bash menu implementation: `/Users/mihaieremia/GitHub/mx-chain-scripts/config/menu_functions.cfg`
- Official docs:
  - https://docs.multiversx.com/validators/node-operation-modes/
  - https://docs.multiversx.com/validators/import-db/
  - https://docs.multiversx.com/validators/redundancy/
  - https://docs.multiversx.com/validators/nodes-scripts/install-update/
  - https://docs.multiversx.com/validators/nodes-scripts/config-scripts/

## Coverage

| Bash capability | Rust CLI coverage | Evidence | Notes / caveats |
| --- | --- | --- | --- |
| `install` validators | Covered | `mxnode install --role validator`; `bins/mxnode/src/commands/install.rs` | Validator key zip convention is preserved. |
| `add_nodes` | Covered | `mxnode install --add N`; `bins/mxnode/src/commands/add_nodes.rs` | Rejected for squad installs, matching Bash's safety rule. |
| `observing_squad` | Covered | `mxnode install --role observer --squad --with-proxy`; `ConfigEdits::Observer` | Proxy is explicit instead of always installed. |
| `multikey_group` | Covered | `mxnode install --role multikey --keys-file ...`; redundancy stamping | Rejects unsafe proxy colocation. |
| Host prerequisites / dependency checks | Covered differently | `mxnode doctor`; release-binary acquisition in `mxnode install` and `mxnode upgrade` | Rust does not run blanket `apt-get dist-upgrade`; it diagnoses host readiness and uses cached/downloaded artifacts. |
| Redundancy / hot standby | Covered | `--backup [N]`; `set_redundancy_level` | Mirrors docs `RedundancyLevel`; validators only stamp non-zero. |
| Node operation modes | Covered in this pass | `--operation-mode <full-archive|db-lookup-extension|historical-balances|snapshotless-observer>` | First-class flag now avoids raw `node.extra_flags` duplication. |
| `upgrade`, `upgrade_squad`, `upgrade_proxy` | Covered and extended | `mxnode upgrade`, `mxnode upgrade squad`, `mxnode upgrade proxy` | Node upgrades refresh node/keygenerator/seednode binaries, apply target config repos when `--config-tag` is used, preserve `prefs.toml`, and record retained utility tags in state. |
| `start`, `start_all`, `stop`, `stop_all` | Covered | `mxnode start|stop --all|--node|--select|--shard` | More selector shapes than Bash. |
| `remove_db` | Covered and extended | `mxnode db remove`, `mxnode db prune`, `mxnode db reseed` | Requires explicit node indices and stopped units. |
| `cleanup` | Covered and safer by default | `mxnode uninstall --yes --execute`; dry-run default | Removes managed units/files while preserving the Bash command's explicit confirmation intent. |
| `import-db` docs workflow | Covered in this pass | `mxnode db import --node N --source DIR [--replace --yes]`; `mxnode db import-plan --source-root DIR` | Executes single-node import and validates full shard/metachain plans for Elasticsearch/backfill setups. |
| `get_logs` | Covered and extended | `mxnode logs --save-archive`; live logs via `mxnode logs -f`, `mxnode logs --ws`, and `mxnode status --watch` | Archive destination follows managed paths; `--ws --log-save` covers logviewer-style live saves. |
| `benchmark` | Covered | `mxnode doctor --benchmark` | Still depends on bundled assessment tooling availability. |
| `github_pull` scripts update | Covered differently | `mxnode self-update`; config/binary upgrade commands | Self-update verifies release checksums. |
| TermUI | Covered and extended | `mxnode status --watch` | Rust TUI replaces upstream termui and adds multi-node view. |
| Logviewer | Covered and extended | `mxnode logs --ws --node N --log-level '*:DEBUG' --log-save`; `mxnode status --watch --ws-logs` | Reuses the node `/log` WebSocket protocol and custom profile JSON fields from the upstream Go logviewer; the dashboard keeps the multi-node view. |
| Keygenerator utility | Covered | `mxnode keys generate --output DIR`; install/upgrade refresh keygenerator artifacts | Preserves the utility wrapper without requiring users to run binaries out of `elrond-utils` manually. |
| Bash config variables | Covered and extended | `mxnode config show|get|set|edit|validate`; `[overrides]` | Rust has typed config and origin reporting. |
| Migrate existing Bash install | Covered | `mxnode import-bash` | Imports marker files, variables, units, and proxy shape. |
| Script completion | Covered in this pass | `mxnode completions <shell>` | Explicit stdout generation replaces Bash's implicit `/etc/bash_completion.d` mutation. |
| Seednode utility | Covered in this pass | `mxnode install` installs `elrond-utils/seednode/{seednode,config/*}`; `mxnode upgrade` refreshes it; operators invoke the binary directly | Copies `config.toml` and intended `p2p.toml` from the config repo when present. |
| Import DB with Elasticsearch presets | Covered in this pass | `mxnode db import-plan --require-elasticsearch` enforces one source/node per shard plus metachain and validates `external.toml` connector basics | Future: optional richer checks for indexer topology outside node config. |
| System requirements | Covered with caveats | `mxnode doctor` checks CPU/RAM/disk scaled by node count, CPU flags/ARM caveats, OS floor, p2p ports; `mxnode install` gates planned installs before tag resolution unless `--force` is passed | Dedicated physical CPU and NAT/UPnP status still require operator/network verification. |

## Future Hardening

1. Promote this matrix into a CI-checked manifest.
2. Add richer Elasticsearch topology checks outside node config, e.g. probing
   the target cluster or validating a companion `elasticindexer` setup.
