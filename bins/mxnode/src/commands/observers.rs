//! `mxnode observers [--count 4]`: install N observers + a proxy.
//!
//! Wraps the same install orchestrator with `ConfigEdits::Observer` per
//! node and `install_proxy = true`. Default count is 4 to match the
//! bash `observing_squad` flow.

use mxnode_core::{InstallKind, NodeIndex, Role, Shard};
use mxnode_state::StateStore;

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::events::global_op;
use crate::orchestrator::acquirer_factory::build_acquirer;
use crate::orchestrator::install::{
    install_units, persist_state, run_install, ConfigEdits, InstallPlan, NodeSpec,
};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};
use crate::orchestrator::tag_resolver::{
    resolve_binary_tag, resolve_config_tag, resolve_proxy_tag,
};

use super::install::{announce_resolved, emit_dry_run, emit_success, install_err, resolve_err};

#[tokio::main(flavor = "current_thread")]
pub async fn run(count: u16, global: &GlobalArgs) -> Result<(), CliError> {
    drive(count, true, InstallKind::ObserversSquad, "observers", global).await
}

pub(super) async fn drive(
    count: u16,
    install_proxy: bool,
    kind: InstallKind,
    label: &str,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.state);
    if store.exists() {
        return Err(CliError::new(
            "state.toml already exists",
            format!("found {}", store.state_path().display()),
            "use `mxnode add-nodes` to extend, or `mxnode cleanup --yes --execute` to start over",
        )
        .json_if(global.json));
    }

    let environment = runtime
        .loaded
        .config
        .network
        .environment
        .ok_or_else(|| {
            CliError::new(
                "network.environment is not set",
                "install needs the operator's chosen network",
                "run `mxnode init` (or `mxnode config set network.environment <env>`)",
            )
            .json_if(global.json)
        })?;

    // observers / multikey have no per-call --tag flags today; the
    // resolver handles `[overrides]` and falls back to GitHub latest
    // for anything still unset.
    let binary = resolve_binary_tag(&runtime, None).await.map_err(|e| resolve_err(e, global))?;
    let config = resolve_config_tag(&runtime, environment, None)
        .await
        .map_err(|e| resolve_err(e, global))?;
    let proxy = if install_proxy {
        Some(
            resolve_proxy_tag(&runtime, None)
                .await
                .map_err(|e| resolve_err(e, global))?,
        )
    } else {
        None
    };
    announce_resolved(global, "binary", &binary);
    announce_resolved(global, "config", &config);
    if let Some(p) = &proxy {
        announce_resolved(global, "proxy", p);
    }
    let binary_tag = binary.tag;
    let config_tag = config.tag;
    let proxy_tag = proxy.map(|p| p.tag);

    let count = count.max(1);
    let nodes: Vec<NodeSpec> = (0..count)
        .map(|i| NodeSpec {
            index: NodeIndex::new(i),
            role: match kind {
                InstallKind::MultikeySquad => Role::Multikey,
                _ => Role::Observer,
            },
            shard: shard_for_index(i, count),
            display_name: String::new(),
        })
        .collect();

    if global.json && std::env::var("MXNODE_DRY_RUN").is_ok() {
        // Defensive: not currently surfaced via clap, but keeps the path
        // available if a test wants to bypass the real acquirer.
    }

    // Reuse the install dry-run shape — observers/multikey present the
    // same data as `install` plus the kind label.
    if let Ok(_) = std::env::var("MXNODE_INSTALL_DRY_RUN") {
        return emit_dry_run(global, environment, &binary_tag, &config_tag, count, label);
    }

    let plan = InstallPlan {
        paths: &runtime.paths,
        environment,
        github_org: &runtime.loaded.config.network.github_org,
        binary_tag: binary_tag.clone(),
        config_tag: config_tag.clone(),
        proxy_tag: proxy_tag.clone(),
        node_count: count,
        kind,
        nodes,
        api_port_base: runtime.loaded.config.node.api_port_base,
        log_level: &runtime.loaded.config.node.log_level,
        limit_nofile: runtime.loaded.config.node.limit_nofile,
        restart_sec: runtime.loaded.config.node.restart_sec,
        custom_user: &runtime.loaded.config.paths.custom_user,
        extra_flags: &runtime.loaded.config.node.extra_flags,
        name_template: &runtime.loaded.config.node.name_template,
        config_edits: ConfigEdits::Observer,
        install_proxy,
        prefs_overrides: &runtime.loaded.config.overrides.prefs,
        config_overrides: &runtime.loaded.config.overrides.config,
    };

    let acquirer = build_acquirer(&runtime);
    // global_op expects a 'static op label, so we pick one based on the
    // observer-vs-multikey shape rather than passing the dynamic `label`.
    let op_label: &'static str = if install_proxy { "observers" } else { "multikey" };
    global_op(op_label, &format!("{count} {label} on {environment}"));
    let outcome = run_install(plan, acquirer)
        .await
        .map_err(|e| install_err(e, global))?;

    install_units(&outcome.unit_files, true)
        .await
        .map_err(|e| install_err(e, global))?;

    let state_path = persist_state(&runtime.paths, &outcome.state)
        .map_err(|e| install_err(e, global))?;

    emit_success(global, &outcome, &state_path)?;
    let _ = (binary_tag, config_tag, proxy_tag); // borrow-checker placeholder
    Ok(())
}

/// Default shard mapping for an observer squad: 4 nodes → shards
/// 0,1,2,metachain (the bash convention). Other counts: shard = index.
fn shard_for_index(index: u16, count: u16) -> Shard {
    if count == 4 && index == 3 {
        Shard::Metachain
    } else {
        match index {
            0 => Shard::Zero,
            1 => Shard::One,
            2 => Shard::Two,
            _ => Shard::Auto,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_mapping_for_4node_squad_pins_metachain() {
        assert_eq!(shard_for_index(0, 4), Shard::Zero);
        assert_eq!(shard_for_index(1, 4), Shard::One);
        assert_eq!(shard_for_index(2, 4), Shard::Two);
        assert_eq!(shard_for_index(3, 4), Shard::Metachain);
    }

    #[test]
    fn shard_mapping_for_other_counts_uses_index() {
        assert_eq!(shard_for_index(0, 2), Shard::Zero);
        assert_eq!(shard_for_index(5, 7), Shard::Auto);
    }
}
