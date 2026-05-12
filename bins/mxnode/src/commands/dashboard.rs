//! `mxnode status --watch`: multi-node ratatui live dashboard.
//!
//! Reads mxnode.toml + the operator's config to build a per-node spec
//! (label / unit / api port / workdir) then hands off to mxnode-tui.

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use mxnode_core::InstallKind;
use mxnode_github::{Client as GithubClient, ClientConfig as GithubClientConfig};
use mxnode_state::StateStore;
use mxnode_tui::{DashboardOpts, NodeSpec, VersionInfo};

use crate::cli::{DashboardArgs, GlobalArgs};
use crate::errors::CliError;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
pub async fn run(args: DashboardArgs, global: &GlobalArgs) -> Result<(), CliError> {
    // The TUI grabs the terminal in raw mode and renders an alternate
    // screen. Without a TTY (piped, ssh -T, systemd-run) the ratatui
    // backend surfaces a raw `os error 6` from the first termios call.
    // Catch that case up front with a focused error.
    if !std::io::stdout().is_terminal() {
        return Err(CliError::new(
            "status --watch requires a terminal for the live TUI",
            "stdout is not a TTY (piped, redirected, or non-interactive shell)",
            "run `mxnode status --watch` directly in a terminal, or use `mxnode status` (one-shot) for piped output",
        )
        .json_if(global.json));
    }
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.config_dir);
    let state = store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read mxnode.toml",
                e.to_string(),
                "run `mxnode install` first",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no mxnode.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode install` to set up nodes",
            )
            .json_if(global.json)
        })?;

    let api_port_base = runtime.loaded.file.node.api_port_base;
    let template = &runtime.loaded.file.node.name_template;
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
            // Honour the name persisted on `NodeState` (operator's
            // wizard / `mxnode keys rename` choice) before falling back to
            // re-templating from config. Re-templating without that
            // check is the bug the dashboard was previously hitting:
            // the operator typed a custom name, it landed in
            // mxnode.toml + prefs.toml, and the dashboard ignored both.
            let display_name = crate::commands::prompts::resolve_display_name(
                &n.display_name,
                template,
                env_str,
                n.role.as_str(),
                n.index.get(),
            );
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
            "mxnode.toml is empty or the --node filter matched nothing",
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
                .file
                .network
                .environment
                .map(|e| e.to_string())
        });

    // Multikey squads share `allValidatorsKeys.pem` across every
    // observer, so the header should not multiply the per-node count
    // by the squad size. Validator and observer-squad installs each
    // own their own keys; sum is correct there.
    let shares_keys = state
        .install
        .as_ref()
        .map(|i| matches!(i.kind, InstallKind::MultikeySquad))
        .unwrap_or(false);

    // Seed the version info with what mxnode.toml says is installed,
    // then spawn a background task to resolve "latest" from GitHub
    // Releases. The poller writes into the shared mutex; the TUI
    // renderer reads a clone every frame and colour-codes accordingly.
    let installed_binary = state
        .install
        .as_ref()
        .and_then(|i| i.versions.binary_tag.as_ref().map(|t| t.to_string()));
    let installed_config = state
        .install
        .as_ref()
        .and_then(|i| i.versions.config_tag.as_ref().map(|t| t.to_string()));
    let version_info = Arc::new(std::sync::Mutex::new(VersionInfo {
        installed_binary_tag: installed_binary,
        installed_config_tag: installed_config,
        latest_binary_tag: None,
        latest_config_tag: None,
    }));
    spawn_version_poller(
        Arc::clone(&version_info),
        runtime.loaded.file.network.github_org.clone(),
        state
            .install
            .as_ref()
            .map(|i| i.environment.config_repo()),
        runtime.github_token(),
    );

    let opts = DashboardOpts {
        nodes,
        interval: Duration::from_millis(args.interval.max(100)),
        gateway: runtime.loaded.file.network.gateway.clone(),
        ws_logs: args.ws_logs,
        environment,
        title: runtime.loaded.file.branding.title.clone(),
        shares_keys,
        version_info,
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

/// Background loop that resolves the latest `mx-chain-go` and
/// `mx-chain-{env}-config` release tags from GitHub and writes them
/// into the shared `VersionInfo`. The TUI renderer reads a clone every
/// frame to colour-code the BinaryVer / ConfigVer rows.
///
/// Refresh cadence is **30 minutes** — fast enough to surface a fresh
/// release during a long dashboard session, slow enough to stay well
/// under the 60 req/h anonymous GitHub API limit even for operators
/// who run multiple dashboards in parallel. A `MXNODE_GITHUB_TOKEN`
/// dodges the limit entirely if the operator has one configured.
///
/// Failures are silent. The TUI already shows "unknown" colour state
/// when `latest_*` is `None`, so a flaky GitHub API just keeps the
/// instance panel neutral instead of surfacing transient errors mid
/// frame.
fn spawn_version_poller(
    version_info: Arc<std::sync::Mutex<VersionInfo>>,
    github_org: String,
    config_repo: Option<String>,
    token: Option<String>,
) {
    tokio::spawn(async move {
        // Build the client once. `unwrap_or_return` on the first
        // failure is fine — a client build error means reqwest itself
        // is broken and retrying won't help.
        let client = match GithubClient::new(GithubClientConfig {
            token,
            ..GithubClientConfig::default()
        }) {
            Ok(c) => c,
            Err(_) => return,
        };
        loop {
            // Binary: always queried. mx-chain-go is org-wide.
            if let Ok(release) = client.latest_release(&github_org, "mx-chain-go").await {
                if let Ok(mut guard) = version_info.lock() {
                    guard.latest_binary_tag = Some(release.tag_name.clone());
                }
            }
            // Config: only queried when the operator's environment is
            // recorded (i.e. `mxnode install` has run). No env → no
            // config repo to compare against.
            if let Some(repo) = config_repo.as_deref() {
                if let Ok(release) = client.latest_release(&github_org, repo).await {
                    if let Ok(mut guard) = version_info.lock() {
                        guard.latest_config_tag = Some(release.tag_name.clone());
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        }
    });
}
