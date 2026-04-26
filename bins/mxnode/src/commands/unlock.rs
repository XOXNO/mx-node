//! `mxnode unlock --force`: break a stale `inflight.toml` left behind by a
//! crashed previous run. Refuses without `--force`. Refuses if the recorded
//! process is still alive (we don't want operators stomping on a healthy
//! mxnode invocation).

use mxnode_state::{classify, inflight_path, Inflight, Liveness};

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

pub fn run(force: bool, global: &GlobalArgs) -> Result<(), CliError> {
    if !force {
        return Err(CliError::new(
            "refusing to unlock without --force",
            "unlock removes inflight.toml; that's a destructive op, so we want explicit intent",
            "rerun with `mxnode unlock --force` after confirming no other mxnode invocation is running",
        )
        .json_if(global.json));
    }

    let runtime = Runtime::from_global(global)?;
    let path = inflight_path(&runtime.paths.state);

    let Some(inflight) = Inflight::load(&path).map_err(|e| {
        CliError::new(
            "failed to read inflight.toml",
            e.to_string(),
            "ensure the state directory is readable",
        )
        .json_if(global.json)
    })?
    else {
        if global.json {
            println!(
                "{}",
                serde_json::json!({"ok": true, "removed": false, "reason": "no inflight.toml"})
            );
        } else {
            println!("nothing to unlock: no inflight.toml at {}", path.display());
        }
        return Ok(());
    };

    let liveness = classify(&inflight.identity);
    if liveness == Liveness::Live {
        return Err(CliError::new(
            format!(
                "refusing to unlock: pid {} is still running",
                inflight.identity.pid
            ),
            format!(
                "inflight.toml records op={:?} started_at={}, owner pid is alive",
                inflight.op, inflight.started_at
            ),
            "wait for the running mxnode invocation to finish, or kill the recorded pid first",
        )
        .json_if(global.json));
    }

    Inflight::clear(&path).map_err(|e| {
        CliError::new(
            "failed to remove inflight.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;

    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "removed": true,
                "previous_pid": inflight.identity.pid,
                "previous_liveness": format!("{liveness:?}"),
            })
        );
    } else {
        println!(
            "removed {} (previous pid {} was {:?})",
            path.display(),
            inflight.identity.pid,
            liveness,
        );
    }
    Ok(())
}
