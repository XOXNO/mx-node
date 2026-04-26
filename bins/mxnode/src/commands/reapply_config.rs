//! `mxnode reapply-config`: walk every node in `state.toml` and re-run
//! the per-node TOML edit pass — display name from the template,
//! observer-shape edits where applicable, and the operator's
//! `[overrides.prefs]` / `[overrides.config]` map.
//!
//! Useful when the operator changes config-side overrides and wants
//! them propagated without a full upgrade. By default the running units
//! are NOT restarted; pass `--restart` to roll the affected nodes after
//! the rewrite. Most node config keys take effect on the next natural
//! restart anyway, so silent in-place edits are the safer default.

use std::sync::Arc;

use mxnode_core::{InstallKind, NodeIndex, NodeState};
use mxnode_state::StateStore;
use mxnode_systemd::Ctl;
use serde::Serialize;

use crate::cli::{GlobalArgs, ReapplyConfigArgs};
use crate::errors::CliError;
use crate::events::global_op;
use crate::orchestrator::install::{apply_node_tomledit, ConfigEdits};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};
use crate::orchestrator::supervisor::build_supervisor;

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: ReapplyConfigArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.state);
    let state = store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read state.toml",
                e.to_string(),
                "run `mxnode install` first",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no state.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode install` first, or `mxnode install` for a fresh setup",
            )
            .json_if(global.json)
        })?;

    let install = state.install.clone().ok_or_else(|| {
        CliError::new(
            "state.toml has no [install] section",
            "expected an existing install",
            "run `mxnode install` first",
        )
        .json_if(global.json)
    })?;

    let edits = match install.kind {
        InstallKind::Validators | InstallKind::Mixed => ConfigEdits::Validator,
        InstallKind::ObserversSquad | InstallKind::MultikeySquad => ConfigEdits::Observer,
    };

    // Filter the node list. `args.node` empty → all nodes.
    let selected: Vec<&NodeState> = if args.node.is_empty() {
        state.nodes.iter().collect()
    } else {
        let wanted: std::collections::BTreeSet<u16> = args.node.iter().copied().collect();
        state
            .nodes
            .iter()
            .filter(|n| wanted.contains(&n.index.get()))
            .collect()
    };

    if selected.is_empty() {
        return Err(CliError::new(
            "no nodes selected",
            "the supplied --node list matched zero nodes in state.toml",
            "run `mxnode status` to see available indices",
        )
        .json_if(global.json));
    }

    let prefs_overrides = &runtime.loaded.config.overrides.prefs;
    let config_overrides = &runtime.loaded.config.overrides.config;
    let template = &runtime.loaded.config.node.name_template;

    global_op(
        "reapply-config",
        &format!(
            "{} node(s); {} prefs / {} config override(s)",
            selected.len(),
            prefs_overrides.len(),
            config_overrides.len(),
        ),
    );

    let mut report = Report {
        nodes: Vec::new(),
        prefs_overrides: prefs_overrides.len(),
        config_overrides: config_overrides.len(),
        restarted: false,
    };

    for node in &selected {
        // Recompute the display name the same way `install` does.
        let display_name = if let Some(o) = state
            .install
            .as_ref()
            .and_then(|_| Some(node)) // borrow shape, no-op
            .filter(|_| !template.is_empty())
        {
            template
                .replace("{env}", install.environment.as_str())
                .replace("{index}", &o.index.get().to_string())
        } else {
            String::new()
        };
        // reapply-config preserves the operator's `RedundancyLevel`
        // by passing `None`: only install-time stamps the value, and
        // re-applying overrides should never silently reset it.
        // Operators who need to flip primary↔backup edit prefs.toml
        // directly or via `[overrides.prefs]` in config.toml.
        apply_node_tomledit(
            &node.workdir,
            &display_name,
            node.shard,
            edits,
            node.role,
            None,
            prefs_overrides,
            config_overrides,
        )
        .map_err(|e| {
            CliError::new(
                format!("re-apply failed on node {}", node.index),
                e.to_string(),
                "ensure the node's config/ directory is writable and contains valid TOML",
            )
            .json_if(global.json)
        })?;
        report.nodes.push(NodeReport {
            index: node.index,
            workdir: node.workdir.display().to_string(),
            unit: node.unit.clone(),
        });
    }

    if args.restart {
        let ctl: Arc<dyn Ctl> = build_supervisor();
        for node in &selected {
            if let Err(e) = ctl.restart(&node.unit).await {
                eprintln!("warn: restart {} failed: {e}", node.unit);
            }
        }
        report.restarted = true;
    }

    if global.json {
        println!("{}", serde_json::to_string(&report).unwrap_or_default());
    } else {
        println!(
            "✓ re-applied config on {} node(s) ({} prefs / {} config override key(s))",
            report.nodes.len(),
            report.prefs_overrides,
            report.config_overrides,
        );
        for n in &report.nodes {
            println!("  node {} → {}", n.index, n.workdir);
        }
        if report.restarted {
            println!("  units restarted");
        } else {
            println!("  units left untouched (pass --restart to roll them)");
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct Report {
    nodes: Vec<NodeReport>,
    prefs_overrides: usize,
    config_overrides: usize,
    restarted: bool,
}

#[derive(Debug, Serialize)]
struct NodeReport {
    index: NodeIndex,
    workdir: String,
    unit: String,
}
