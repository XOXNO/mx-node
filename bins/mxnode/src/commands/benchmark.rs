//! `mxnode benchmark`: run the bundled assessment binary if no node units
//! are currently active. The bash version checks `systemctl list-units` for
//! anything with `elrond` running and refuses if anything is up; we do the
//! same but go through `mxnode-systemd::Ctl` for testability.

use std::process::{Command, Stdio};

use mxnode_state::StateStore;
use mxnode_systemd::ActiveState;

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

#[tokio::main(flavor = "current_thread")]
pub async fn run(global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.state);
    let state = store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read state.toml",
                e.to_string(),
                "run `mxnode adopt` first",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no state.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode adopt` first",
            )
            .json_if(global.json)
        })?;

    let ctl = crate::orchestrator::supervisor::build_supervisor();
    for node in &state.nodes {
        let active = ctl.is_active(&node.unit).await.unwrap_or(ActiveState::Unknown);
        if matches!(active, ActiveState::Active | ActiveState::Activating) {
            return Err(CliError::new(
                "refusing to run benchmark while nodes are active",
                format!(
                    "{} is {}",
                    node.unit,
                    match active {
                        ActiveState::Active => "active",
                        ActiveState::Activating => "activating",
                        _ => "running",
                    }
                ),
                "run `mxnode stop --all` first; benchmark assesses raw host throughput",
            )
            .json_if(global.json));
        }
    }

    let binary = runtime
        .paths
        .elrond_utils_root()
        .join("assessment")
        .join("assessment");
    if !binary.exists() {
        return Err(CliError::new(
            "assessment binary is not installed",
            format!("expected {}", binary.display()),
            "run `mxnode install` (Phase 3) first; the assessment tool ships with the install_utils flow",
        )
        .json_if(global.json));
    }

    let workdir = runtime.paths.elrond_utils_root().join("assessment");
    let status = Command::new(&binary)
        .current_dir(&workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            CliError::new(
                "failed to invoke assessment",
                format!("{}: {e}", binary.display()),
                "ensure the binary has execute permissions",
            )
            .json_if(global.json)
        })?;
    if !status.success() {
        return Err(CliError::new(
            "assessment exited non-zero",
            format!("status code {:?}", status.code()),
            "see stdout/stderr above for benchmark output",
        )
        .json_if(global.json));
    }
    Ok(())
}
