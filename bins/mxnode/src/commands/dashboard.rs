//! `mxnode dashboard`: multi-node ratatui live dashboard.
//!
//! Reads state.toml + the operator's config to build a per-node spec
//! (label / unit / api port / workdir) then hands off to mxnode-tui.

use std::time::Duration;

use mxnode_state::StateStore;
use mxnode_tui::{DashboardOpts, NodeSpec};

use crate::cli::{DashboardArgs, GlobalArgs};
use crate::errors::CliError;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
pub async fn run(args: DashboardArgs, global: &GlobalArgs) -> Result<(), CliError> {
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
                "run `mxnode install` to set up nodes",
            )
            .json_if(global.json)
        })?;

    let api_port_base = runtime.loaded.config.node.api_port_base;
    let template = &runtime.loaded.config.node.name_template;
    let env_str = state
        .install
        .as_ref()
        .map(|i| i.environment.as_str())
        .unwrap_or("");

    let want_idx: Option<std::collections::BTreeSet<u16>> = if args.node.is_empty() {
        None
    } else {
        Some(args.node.iter().copied().collect())
    };

    let mut nodes: Vec<NodeSpec> = state
        .nodes
        .iter()
        .filter(|n| {
            want_idx
                .as_ref()
                .map(|s| s.contains(&n.index.get()))
                .unwrap_or(true)
        })
        .map(|n| {
            let display_name = template
                .replace("{env}", env_str)
                .replace("{index}", &n.index.get().to_string());
            NodeSpec {
                index: n.index,
                label: if display_name.is_empty() {
                    format!("node-{}", n.index.get())
                } else {
                    display_name
                },
                unit: n.unit.clone(),
                host: args.host.clone(),
                api_port: if n.api_port == 0 {
                    api_port_base.saturating_add(n.index.get())
                } else {
                    n.api_port
                },
                workdir: n.workdir.clone(),
            }
        })
        .collect();

    if nodes.is_empty() {
        return Err(CliError::new(
            "no nodes to display",
            "state.toml is empty or the --node filter matched nothing",
            "run `mxnode status` to list available indices",
        )
        .json_if(global.json));
    }

    nodes.sort_by_key(|n| n.index);

    let environment = state
        .install
        .as_ref()
        .map(|i| i.environment.to_string())
        .or_else(|| {
            runtime
                .loaded
                .config
                .network
                .environment
                .map(|e| e.to_string())
        });

    let opts = DashboardOpts {
        nodes,
        interval: Duration::from_millis(args.interval.max(100)),
        gateway: runtime.loaded.config.network.gateway.clone(),
        ws_logs: args.ws_logs,
        environment,
        title: runtime.loaded.config.branding.title.clone(),
    };

    mxnode_tui::run(opts).await.map_err(|e| {
        CliError::new(
            "dashboard exited with error",
            e.to_string(),
            "rerun with `--verbose` for details, or open an issue with the trace",
        )
        .json_if(global.json)
    })
}
