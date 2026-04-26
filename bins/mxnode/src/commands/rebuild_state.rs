//! `mxnode rebuild-state`: discovery-only, no drift checks. Always
//! overwrites `state.toml` with what's currently on disk. Operator escape
//! hatch when adopt refuses.

use mxnode_state::StateStore;

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::orchestrator::adopt::{analyze, AdoptInputs};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

/// See `commands::adopt` — picks the right supervisor dir per platform.
fn default_supervisor_dir() -> std::path::PathBuf {
    use crate::orchestrator::supervisor::unit_dir_for_platform;
    use mxnode_core::Platform;
    unit_dir_for_platform(Platform::current())
        .unwrap_or_else(|| std::path::PathBuf::from("/etc/systemd/system"))
}

pub fn run(global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;

    let environment = runtime
        .loaded
        .config
        .network
        .environment
        .ok_or_else(|| {
            CliError::new(
                "network.environment is not set",
                "rebuild-state requires the configured environment to label state.toml",
                "run `mxnode init` first",
            )
            .json_if(global.json)
        })?;

    let supervisor_dir = default_supervisor_dir();
    let outcome = analyze(
        &supervisor_dir,
        &AdoptInputs {
            paths: &runtime.paths,
            environment,
            github_org: &runtime.loaded.config.network.github_org,
            log_level: &runtime.loaded.config.node.log_level,
            limit_nofile: runtime.loaded.config.node.limit_nofile,
            restart_sec: runtime.loaded.config.node.restart_sec,
            api_port_base: runtime.loaded.config.node.api_port_base,
            extra_flags: &runtime.loaded.config.node.extra_flags,
        },
        &format!("mxnode/{}", env!("CARGO_PKG_VERSION")),
    )
    .map_err(|e| {
        CliError::new(
            "failed to scan systemd directory",
            e.to_string(),
            "ensure /etc/systemd/system is readable",
        )
        .json_if(global.json)
    })?;

    let store = StateStore::new(&runtime.paths.state);
    let guard = store.lock().map_err(|e| {
        CliError::new(
            "failed to lock state",
            e.to_string(),
            "another mxnode operation may be in progress; wait or run `mxnode unlock --force`",
        )
        .json_if(global.json)
    })?;
    if store.exists() {
        let _ = store.backup().map_err(|e| {
            CliError::new(
                "failed to back up existing state.toml",
                e.to_string(),
                "ensure the state directory is writable",
            )
            .json_if(global.json)
        })?;
    }
    store.save(&outcome.state, &guard).map_err(|e| {
        CliError::new(
            "failed to write state.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;
    drop(guard);

    if global.json {
        let install = outcome.state.install.as_ref().expect("populated");
        let payload = serde_json::json!({
            "ok": true,
            "state_path": store.state_path().display().to_string(),
            "node_count": install.node_count,
            "drift_count": outcome.drift_reports().count(),
            "drop_ins_present": outcome.has_drop_ins(),
        });
        println!("{payload}");
    } else {
        let install = outcome.state.install.as_ref().expect("populated");
        println!("rebuilt: {install}");
        println!("  state.toml: {}", store.state_path().display());
        if !outcome.is_clean() {
            println!(
                "  note: {} unit(s) with drift were preserved verbatim in state.nodes[].unit_override",
                outcome.drift_reports().count(),
            );
        }
    }
    Ok(())
}
