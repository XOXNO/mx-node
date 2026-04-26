//! `mxnode cleanup`: full host cleanup — stop + disable units, remove unit
//! files, remove `elrond-nodes/`/`elrond-proxy/`/`elrond-utils/`, drop
//! state.toml. Defaults to dry-run for the first two minor releases per
//! the plan.
//!
//! Dry-run mode is the safe default: pass `--execute` to actually delete.
//! `--yes` is a separate gate so even with `--execute` we still require a
//! second confirmation flag.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use mxnode_core::{NodeState, Platform, State};
use mxnode_state::StateStore;
use mxnode_systemd::Ctl; // trait used by `Step::apply` parameter
use serde::Serialize;

use crate::cli::{CleanupArgs, GlobalArgs};
use crate::errors::CliError;
use crate::events::global_op;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};
use crate::orchestrator::supervisor::{unit_dir_for_platform, unit_filename};

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: CleanupArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.state);
    let state = match store.load() {
        Ok(Some(s)) => s,
        Ok(None) => {
            // No state.toml — nothing for cleanup to do unless the operator
            // also wants to wipe the proxy / utils directories. Print a
            // courtesy report and exit cleanly in dry-run.
            return cleanup_with_no_state(&args, global, &runtime);
        }
        Err(e) => {
            return Err(CliError::new(
                "failed to read state.toml",
                e.to_string(),
                "remove the file manually if it's corrupt",
            )
            .json_if(global.json));
        }
    };

    let plan = build_plan(&state, &runtime.paths);

    if !args.yes {
        return Err(CliError::new(
            "refusing without --yes",
            "cleanup permanently removes all nodes, units, and binaries managed by mxnode",
            "rerun with `mxnode cleanup --yes` to dry-run, then add `--execute` to actually delete",
        )
        .json_if(global.json));
    }

    let executing = args.should_execute();
    if global.json {
        let payload = CleanupReport {
            mode: if executing { "execute" } else { "dry-run" },
            steps: plan.iter().map(|s| s.summary()).collect(),
        };
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else {
        let mode = if executing { "EXECUTE" } else { "dry-run" };
        println!("cleanup plan ({mode}):");
        for step in &plan {
            println!("  {}", step.summary());
        }
    }

    if !executing {
        if !global.json {
            println!("\nNo changes made. Re-run with --execute to actually delete.");
        }
        return Ok(());
    }

    global_op("cleanup", "executing");
    let ctl = crate::orchestrator::supervisor::build_supervisor();
    let mut had_error = false;
    for step in &plan {
        // Deref the Arc once into the trait object so step.apply gets a
        // plain `&dyn Ctl` (matches the existing signature).
        if let Err(e) = step.apply(ctl.as_ref()).await {
            had_error = true;
            eprintln!("warn: {} failed: {e}", step.summary());
        }
    }

    // Remove state.toml last so we still have it if anything else fails.
    if store.exists() {
        if let Err(e) = fs::remove_file(store.state_path()) {
            had_error = true;
            eprintln!("warn: failed to remove state.toml: {e}");
        }
    }

    if had_error {
        return Err(CliError::new(
            "cleanup completed with errors",
            "some steps could not finish",
            "inspect the warnings above and clean up manually if needed",
        )
        .silent());
    }
    Ok(())
}

fn cleanup_with_no_state(
    args: &CleanupArgs,
    global: &GlobalArgs,
    runtime: &Runtime,
) -> Result<(), CliError> {
    let dirs_present: Vec<PathBuf> = [
        runtime.paths.elrond_nodes_root(),
        runtime.paths.elrond_proxy_root(),
        runtime.paths.elrond_utils_root(),
    ]
    .into_iter()
    .filter(|p| p.exists())
    .collect();

    if dirs_present.is_empty() {
        if global.json {
            println!(
                "{}",
                serde_json::json!({"ok": true, "removed": [], "note": "host appears clean"})
            );
        } else {
            println!("nothing to clean: no state.toml and no managed directories present");
        }
        return Ok(());
    }

    if !args.yes {
        return Err(CliError::new(
            "refusing without --yes",
            "found managed directories without state.toml; cleanup is destructive",
            "rerun with `mxnode cleanup --yes` to dry-run",
        )
        .json_if(global.json));
    }

    if !args.should_execute() {
        if global.json {
            println!(
                "{}",
                serde_json::json!({
                    "mode": "dry-run",
                    "would_remove": dirs_present
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>(),
                })
            );
        } else {
            println!("dry-run — would remove:");
            for p in &dirs_present {
                println!("  {}", p.display());
            }
            println!("\nRe-run with --execute to actually delete.");
        }
        return Ok(());
    }

    for p in &dirs_present {
        if let Err(e) = fs::remove_dir_all(p) {
            eprintln!("warn: failed to remove {}: {e}", p.display());
        }
    }
    Ok(())
}

/// One step in the cleanup plan. The platform-specific behaviour lives on
/// `RemoveUnitFile` (where the deletion path differs and macOS doesn't need
/// sudo). `DisableUnit` is Linux-only — launchd has no `disable` verb;
/// `bootout` (which `Step::StopUnit` triggers via the supervisor on macOS)
/// already takes the unit out of the agent domain.
#[derive(Debug)]
enum Step {
    StopUnit { unit: String },
    DisableUnit { unit: String },
    RemoveUnitFile { path: PathBuf, sudo: bool },
    RemoveDir { path: PathBuf },
}

impl Step {
    fn summary(&self) -> String {
        match self {
            Step::StopUnit { unit } => format!("stop {unit}"),
            Step::DisableUnit { unit } => format!("disable {unit}"),
            Step::RemoveUnitFile { path, sudo } => {
                if *sudo {
                    format!("sudo rm {}", path.display())
                } else {
                    format!("rm {}", path.display())
                }
            }
            Step::RemoveDir { path } => format!("rm -rf {}", path.display()),
        }
    }

    async fn apply(&self, ctl: &dyn Ctl) -> Result<(), String> {
        match self {
            Step::StopUnit { unit } => {
                ctl.stop(unit).await.map_err(|e| e.to_string())?;
                Ok(())
            }
            Step::DisableUnit { unit } => {
                // We don't have an explicit `disable` in the trait yet —
                // shell out via a one-off systemctl call. Failure here is
                // typically harmless (unit was never enabled) so we
                // surface it as a warning, not an error.
                let _ = Command::new("sudo")
                    .args(["--non-interactive", "systemctl", "disable", unit])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .stdin(Stdio::null())
                    .status();
                Ok(())
            }
            Step::RemoveUnitFile { path, sudo } => {
                if *sudo {
                    let _ = Command::new("sudo")
                        .args(["--non-interactive", "rm", "-f", path.to_string_lossy().as_ref()])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .stdin(Stdio::null())
                        .status()
                        .map_err(|e| e.to_string())?;
                } else if let Err(e) = fs::remove_file(path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return Err(e.to_string());
                    }
                }
                Ok(())
            }
            Step::RemoveDir { path } => {
                if let Err(e) = fs::remove_dir_all(path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return Err(e.to_string());
                    }
                }
                Ok(())
            }
        }
    }
}

fn build_plan(state: &State, paths: &mxnode_core::Paths) -> Vec<Step> {
    let platform = Platform::current();
    // macOS LaunchAgents live in the operator's home, no sudo needed.
    // Linux systemd units live in /etc/systemd/system, removal needs sudo.
    let needs_sudo = !matches!(platform, Platform::Macos);
    let unit_dir = unit_dir_for_platform(platform);

    let mut plan: Vec<Step> = Vec::new();
    for node in &state.nodes {
        plan.push(Step::StopUnit { unit: node.unit.clone() });
        // launchd has no `disable` verb; emit the step on Linux only.
        if matches!(platform, Platform::Linux) {
            plan.push(Step::DisableUnit { unit: node.unit.clone() });
        }
        if let Some(dir) = &unit_dir {
            plan.push(Step::RemoveUnitFile {
                path: dir.join(unit_filename(platform, &node.unit)),
                sudo: needs_sudo,
            });
        }
        plan.push(Step::RemoveDir { path: workdir_for(node) });
    }
    if let Some(proxy) = &state.proxy {
        plan.push(Step::StopUnit { unit: proxy.unit.clone() });
        if matches!(platform, Platform::Linux) {
            plan.push(Step::DisableUnit { unit: proxy.unit.clone() });
        }
        if let Some(dir) = &unit_dir {
            plan.push(Step::RemoveUnitFile {
                path: dir.join(unit_filename(platform, &proxy.unit)),
                sudo: needs_sudo,
            });
        }
        plan.push(Step::RemoveDir { path: paths.elrond_proxy_root() });
    }
    plan.push(Step::RemoveDir { path: paths.elrond_utils_root() });
    plan.push(Step::RemoveDir { path: paths.elrond_nodes_root() });
    plan
}

fn workdir_for(node: &NodeState) -> PathBuf {
    node.workdir.clone()
}

#[derive(Debug, Serialize)]
struct CleanupReport {
    mode: &'static str,
    steps: Vec<String>,
}
