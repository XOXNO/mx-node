//! `mxnode migrate-bash`: import an existing `mx-chain-scripts` (bash) install
//! into mxnode's cache-derived `state.toml`. Pure inference here — no
//! filesystem writes, no systemctl probing.

use std::path::Path;

use clap::Args;
use mxnode_core::{
    state::InstallSection, DEFAULT_API_PORT_BASE, DEFAULT_PROXY_PORT, Environment, InstallKind,
    NodeIndex, NodeState, Paths, ProxyState, Role, Shard, State,
};
use thiserror::Error;

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

#[derive(Debug, Error)]
pub enum MigrateError {
    #[error("could not parse {field}: {detail}")]
    Parse { field: &'static str, detail: String },
    #[error("not a bash install — missing {0}")]
    NotBash(&'static str),
}

/// Inspect a `$CUSTOM_HOME` looking for the bash sentinels (`.installedenv`,
/// `.numberofnodes`, optionally `.squad_install`) and return the
/// cache-derived [`State`] without touching disk. Tags inside
/// `InstallVersions` are intentionally left at their default `None` —
/// bash does not record them as data, and `state.toml` must not lie about
/// what's installed. A subsequent `mxnode upgrade` resolves them from GitHub.
pub fn infer_state_from_bash(custom_home: &Path) -> Result<State, MigrateError> {
    let env_raw = std::fs::read_to_string(custom_home.join(".installedenv"))
        .map_err(|_| MigrateError::NotBash(".installedenv"))?;
    let environment = match env_raw.trim() {
        "mainnet" => Environment::Mainnet,
        "testnet" => Environment::Testnet,
        "devnet" => Environment::Devnet,
        other => {
            return Err(MigrateError::Parse {
                field: ".installedenv",
                detail: format!("unknown environment {other:?}"),
            });
        }
    };

    let count: u16 = std::fs::read_to_string(custom_home.join(".numberofnodes"))
        .map_err(|_| MigrateError::NotBash(".numberofnodes"))?
        .trim()
        .parse()
        .map_err(|e: std::num::ParseIntError| MigrateError::Parse {
            field: ".numberofnodes",
            detail: format!("{e}"),
        })?;

    let kind = match std::fs::read_to_string(custom_home.join(".squad_install"))
        .as_deref()
        .map(str::trim)
    {
        Ok("Observers Squad") => InstallKind::ObserversSquad,
        Ok("Multikey Squad") => InstallKind::MultikeySquad,
        // Either the file is absent, unreadable, or holds an unknown
        // marker. Bash treats anything other than the two recognised
        // strings as "validators install"; mirror that.
        _ => InstallKind::Validators,
    };

    let role = match kind {
        InstallKind::Validators => Role::Validator,
        InstallKind::ObserversSquad => Role::Observer,
        InstallKind::MultikeySquad => Role::Multikey,
        // Unreachable: bash has no `Mixed` install concept; the inference
        // above can only return Validators/ObserversSquad/MultikeySquad.
        // If a future contributor adds Mixed to that match, this arm forces
        // them to update the role mapping in the same change.
        InstallKind::Mixed => unreachable!("bash sentinels do not produce InstallKind::Mixed"),
    };

    // Squad layout: indices 0/1/2 → shards 0/1/2, index 3 → metachain.
    // Validator layout: shard is operator-driven, leave Auto.
    let shard_for = |i: u16| -> Shard {
        match (kind, i) {
            (InstallKind::Validators, _) => Shard::Auto,
            (_, 0) => Shard::Zero,
            (_, 1) => Shard::One,
            (_, 2) => Shard::Two,
            (_, 3) => Shard::Metachain,
            // bash squads are always 4 nodes; surface anything beyond that as Auto
            // so we don't fabricate a shard assignment we cannot justify.
            _ => Shard::Auto,
        }
    };

    // Build a Paths for the bash layout. Only `custom_home` is read by the
    // helpers we call; the other fields are placeholders set to safe defaults
    // (Paths::default() for the bash convention) and never observed here.
    let paths = Paths {
        custom_home: custom_home.to_path_buf(),
        ..Paths::default()
    };

    let nodes: Vec<NodeState> = (0..count)
        .map(|i| {
            let index = NodeIndex::new(i);
            NodeState {
                index,
                role,
                shard: shard_for(i),
                display_name: String::new(),
                api_port: DEFAULT_API_PORT_BASE + i,
                unit: Paths::node_unit_name(index),
                unit_override: String::new(),
                workdir: paths.node_workdir(index),
                last_known_pubkey: String::new(),
                last_action: String::new(),
                last_action_at: None,
            }
        })
        .collect();

    let proxy = if kind == InstallKind::ObserversSquad {
        Some(ProxyState {
            present: true,
            unit: Paths::proxy_unit_name().to_string(),
            workdir: paths.elrond_proxy_root(),
            server_port: DEFAULT_PROXY_PORT,
        })
    } else {
        None
    };

    let mut state = State::empty("mxnode/migrate-bash");
    state.discovered = true;
    state.install = Some(InstallSection::observed(kind, environment, "multiversx", count));
    state.nodes = nodes;
    state.proxy = proxy;
    Ok(state)
}

#[derive(Debug, Args)]
pub struct MigrateBashArgs {
    /// Path to the bash `$CUSTOM_HOME` to import. Defaults to
    /// `paths.custom_home` from the resolved mxnode config (mirrors the bash
    /// `CUSTOM_HOME` variable).
    #[arg(long, value_name = "PATH")]
    pub from: Option<std::path::PathBuf>,

    /// Apply the migration. Without this flag, the inferred state is
    /// printed and `state.toml` is NOT modified.
    #[arg(long)]
    pub execute: bool,
}

/// Import an existing bash install into mxnode's `state.toml`. Dry-run by
/// default; pass `--execute` to actually persist. The bash files on disk
/// are never modified.
pub fn run(args: MigrateBashArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let custom_home = args
        .from
        .clone()
        .unwrap_or_else(|| runtime.paths.custom_home.clone());

    let state = infer_state_from_bash(&custom_home).map_err(|e| {
        CliError::new(
            "could not infer mxnode state from bash install",
            e.to_string(),
            "verify .installedenv and .numberofnodes exist under the path passed via --from",
        )
        .json_if(global.json)
    })?;

    if !args.execute {
        print_dry_run(&state, global);
        return Ok(());
    }

    let store = mxnode_state::StateStore::new(&runtime.paths.state);
    if store.exists() {
        return Err(CliError::new(
            "refusing to overwrite existing state.toml",
            format!("{} already exists", store.state_path().display()),
            "delete or move the existing state.toml first, or run on a fresh host",
        )
        .json_if(global.json));
    }
    let guard = store.lock().map_err(|e| {
        CliError::new(
            "failed to acquire state.toml lock",
            e.to_string(),
            "ensure no other mxnode invocation is running, then retry",
        )
        .json_if(global.json)
    })?;
    store.save(&state, &guard).map_err(|e| {
        CliError::new(
            "failed to write state.toml",
            e.to_string(),
            "ensure mxnode has write access to the state directory",
        )
        .json_if(global.json)
    })?;
    if global.json {
        let body = serde_json::json!({
            "ok": true,
            "wrote": store.state_path().display().to_string(),
            "node_count": state.nodes.len(),
        });
        println!("{body}");
    } else {
        println!("wrote {}", store.state_path().display());
    }
    Ok(())
}

fn print_dry_run(state: &State, global: &GlobalArgs) {
    if global.json {
        let body = serde_json::json!({
            "mode": "dry-run",
            "node_count": state.nodes.len(),
            "kind": state.install.as_ref().map(|i| i.kind.as_str()),
            "environment": state.install.as_ref().map(|i| i.environment.as_str()),
            "proxy": state.proxy.is_some(),
            "nodes": state.nodes.iter().map(|n| serde_json::json!({
                "index": n.index.get(),
                "role": n.role.as_str(),
                "shard": n.shard.as_str(),
                "unit": n.unit,
                "api_port": n.api_port,
            })).collect::<Vec<_>>(),
        });
        println!("{body}");
    } else {
        println!("dry-run — pass --execute to write state.toml");
        if let Some(install) = state.install.as_ref() {
            println!(
                "  inferred {} nodes, kind={}, env={}",
                install.node_count, install.kind, install.environment,
            );
        }
        for n in &state.nodes {
            println!(
                "  node-{}: {} on {} ({})",
                n.index.get(),
                n.role,
                n.shard,
                n.unit,
            );
        }
        if state.proxy.is_some() {
            println!("  + proxy");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_fixture(kind: &str, count: u16, env: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".installedenv"), env).unwrap();
        std::fs::write(dir.path().join(".numberofnodes"), count.to_string()).unwrap();
        if !kind.is_empty() {
            std::fs::write(dir.path().join(".squad_install"), kind).unwrap();
        }
        dir
    }

    #[test]
    fn infers_observers_squad_with_proxy() {
        let dir = bash_fixture("Observers Squad", 4, "mainnet");
        let state = infer_state_from_bash(dir.path()).unwrap();
        assert_eq!(state.nodes.len(), 4);
        let install = state.install.as_ref().expect("install present");
        assert_eq!(install.environment, Environment::Mainnet);
        assert_eq!(install.kind, InstallKind::ObserversSquad);
        assert_eq!(install.node_count, 4);
        assert!(state.proxy.is_some());
        assert_eq!(state.nodes[3].shard, Shard::Metachain);
        assert_eq!(state.nodes[0].shard, Shard::Zero);
        assert_eq!(state.nodes[0].role, Role::Observer);
        assert_eq!(state.nodes[0].api_port, 8080);
        assert_eq!(state.nodes[0].unit, "elrond-node-0.service");
        assert!(state.discovered);
    }

    #[test]
    fn infers_multikey_squad_without_proxy() {
        let dir = bash_fixture("Multikey Squad", 4, "testnet");
        let state = infer_state_from_bash(dir.path()).unwrap();
        let install = state.install.as_ref().expect("install present");
        assert_eq!(install.kind, InstallKind::MultikeySquad);
        assert!(state.proxy.is_none());
        assert_eq!(state.nodes[0].role, Role::Multikey);
    }

    #[test]
    fn infers_validators_without_squad_file() {
        let dir = bash_fixture("", 2, "devnet");
        let state = infer_state_from_bash(dir.path()).unwrap();
        let install = state.install.as_ref().expect("install present");
        assert_eq!(install.kind, InstallKind::Validators);
        assert_eq!(install.environment, Environment::Devnet);
        assert!(state.proxy.is_none());
        assert_eq!(state.nodes[0].role, Role::Validator);
        assert_eq!(state.nodes[0].shard, Shard::Auto);
    }

    #[test]
    fn errors_when_not_a_bash_install() {
        let dir = tempfile::tempdir().unwrap();
        let err = infer_state_from_bash(dir.path()).unwrap_err();
        assert!(matches!(err, MigrateError::NotBash(".installedenv")));
    }

    #[test]
    fn errors_on_unknown_environment() {
        let dir = bash_fixture("", 1, "mythicnet");
        let err = infer_state_from_bash(dir.path()).unwrap_err();
        assert!(matches!(err, MigrateError::Parse { field: ".installedenv", .. }));
    }

    #[test]
    fn install_versions_are_left_unset_on_import() {
        // Cache-derived model: bash does not record tags, so the import
        // must not fabricate them. Subsequent `mxnode upgrade` will fill
        // them in from observed reality.
        let dir = bash_fixture("Observers Squad", 4, "mainnet");
        let state = infer_state_from_bash(dir.path()).unwrap();
        let install = state.install.as_ref().unwrap();
        assert!(install.versions.config_tag.is_none());
        assert!(install.versions.binary_tag.is_none());
        assert!(install.versions.proxy_tag.is_none());
        assert!(install.versions.go_version.is_empty());
    }
}
