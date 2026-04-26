//! `mxnode logs`: shell-out to `journalctl --unit elrond-node-{N}`.
//!
//! Two modes:
//!   - default: stream journalctl output to stdout, optionally `--follow`
//!   - `--save-archive`: replicate the bash `get_logs` flow — for each
//!     selected node, dump the journal to a file named
//!     `mx-chain-node-{INDEX}-{PUBKEY_PREFIX}.log`, then tar.gz the lot
//!     to `$CUSTOM_HOME/mx-chain-logs/mx-chain-node-logs-{TIMESTAMP}.tar.gz`.
//!
//! Picks units from `state.toml` so the operator gets accurate names even on
//! hosts where the units aren't sequentially indexed. If state.toml is
//! missing, falls back to whatever filenames currently live in
//! `/etc/systemd/system/elrond-node-*.service`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use mxnode_core::NodeState;
use mxnode_rpc::NodeClient;
use mxnode_core::Platform;
use mxnode_state::StateStore;
use mxnode_systemd::{scan_supervisor_dir, DiscoveredKind};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::cli::{GlobalArgs, LogsArgs};
use crate::errors::CliError;
use crate::events::global_op;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

const DEFAULT_SYSTEMD_DIR: &str = "/etc/systemd/system";

pub fn run(args: LogsArgs, global: &GlobalArgs) -> Result<(), CliError> {
    if args.save_archive {
        return run_save_archive(args, global);
    }

    let runtime = Runtime::from_global(global)?;

    // macOS has no journald. The node already file-logs to
    // `$WORKDIR/logs/`; tail those instead. The user-visible behaviour
    // matches the Linux `journalctl --unit` flow: per-node selection,
    // optional `--follow`, optional `--since` (we filter by mtime when
    // possible, otherwise pass through to nothing).
    if !Platform::current().has_journal() {
        return tail_node_log_files(&runtime, &args, global);
    }

    let units = pick_units(&runtime, &args.node, global)?;

    let mut cmd = Command::new("journalctl");
    for unit in &units {
        cmd.arg("--unit").arg(unit);
    }
    if let Some(since) = &args.since {
        cmd.arg("--since").arg(since);
    }
    if args.follow {
        cmd.arg("--follow");
    }
    cmd.stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::null());

    let status: ExitStatus = cmd.status().map_err(|e| {
        // Surface the binary name explicitly — the OS error alone usually
        // says "No such file or directory" without naming what's missing,
        // which is confusing on hosts where it's a PATH issue rather than
        // a missing package.
        CliError::new(
            "failed to invoke `journalctl`",
            format!("could not exec journalctl: {e}"),
            "ensure `journalctl` is on PATH (Linux: install systemd; macOS: not supported, mxnode is Linux-only for state ops)",
        )
        .json_if(global.json)
    })?;

    if !status.success() {
        // journalctl already printed its own error to stderr; surface the
        // exit code in our 3-line shape for consistency. Common cause:
        // permission denied (operator not in `systemd-journal` group).
        return Err(CliError::new(
            "journalctl exited non-zero",
            format!("status code {:?}", status.code()),
            "if you saw a permission error, add the user to the `systemd-journal` group, or rerun with sudo",
        )
        .json_if(global.json));
    }
    Ok(())
}

fn pick_units(runtime: &Runtime, requested_indices: &[u16], global: &GlobalArgs) -> Result<Vec<String>, CliError> {
    let store = StateStore::new(&runtime.paths.state);

    // Source 1: state.toml (preferred — exact unit names).
    if let Ok(Some(state)) = store.load() {
        let mut units: Vec<String> = if requested_indices.is_empty() {
            state.nodes.iter().map(|n| n.unit.clone()).collect()
        } else {
            state
                .nodes
                .iter()
                .filter(|n| requested_indices.contains(&n.index.get()))
                .map(|n| n.unit.clone())
                .collect()
        };
        if !requested_indices.is_empty() && units.len() != requested_indices.len() {
            // At least one operator-supplied index does not exist in state.
            let missing: Vec<u16> = requested_indices
                .iter()
                .copied()
                .filter(|idx| !state.nodes.iter().any(|n| n.index.get() == *idx))
                .collect();
            return Err(CliError::new(
                "no such node",
                format!("state.toml has no node(s) at index {missing:?}"),
                "run `mxnode status` to list available indices, or `mxnode rebuild-state` if state.toml is stale",
            )
            .json_if(global.json));
        }
        if units.is_empty() {
            return Err(CliError::new(
                "no nodes recorded in state.toml",
                "state.toml is empty",
                "run `mxnode adopt` after installing nodes to populate state.toml",
            )
            .json_if(global.json));
        }
        units.sort();
        return Ok(units);
    }

    // Source 2: discovery (fallback when state.toml is missing).
    let discovered = scan_supervisor_dir(Path::new(DEFAULT_SYSTEMD_DIR)).map_err(|e| {
        CliError::new(
            "no state.toml; failed to scan systemd dir",
            e.to_string(),
            "run `mxnode adopt` first",
        )
        .json_if(global.json)
    })?;
    let mut fallback_units: Vec<String> = discovered
        .iter()
        .filter_map(|d| match &d.kind {
            DiscoveredKind::Node(idx) => {
                if requested_indices.is_empty() || requested_indices.contains(&idx.get()) {
                    Some(d.unit.clone())
                } else {
                    None
                }
            }
            DiscoveredKind::Proxy => None,
        })
        .collect();
    if fallback_units.is_empty() {
        return Err(CliError::new(
            "no node units found",
            format!("nothing under {DEFAULT_SYSTEMD_DIR}/elrond-node-*.service"),
            "install nodes first, or pass --node <i> to target a specific unit",
        )
        .json_if(global.json));
    }
    fallback_units.sort();
    Ok(fallback_units)
}

/// macOS log-tail. Uses the system `tail` binary (always present on
/// macOS) so we don't have to reimplement file-tailing semantics. Works
/// for both `--follow` and one-shot reads; `--since` is approximated by
/// running `tail -n <n>` with a generous default since we can't filter
/// by mtime cleanly without scanning lines.
fn tail_node_log_files(
    runtime: &Runtime,
    args: &LogsArgs,
    global: &GlobalArgs,
) -> Result<(), CliError> {
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

    let target_indices: Vec<u16> = if args.node.is_empty() {
        state.nodes.iter().map(|n| n.index.get()).collect()
    } else {
        args.node.clone()
    };

    let mut log_paths: Vec<PathBuf> = Vec::new();
    for node in &state.nodes {
        if !target_indices.contains(&node.index.get()) {
            continue;
        }
        let logs_dir = node.workdir.join("logs");
        if !logs_dir.exists() {
            continue;
        }
        let entries = std::fs::read_dir(&logs_dir).map_err(|e| {
            CliError::new(
                "failed to read logs directory",
                format!("{}: {e}", logs_dir.display()),
                "ensure $WORKDIR/logs/ is readable by the current user",
            )
            .json_if(global.json)
        })?;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("log") {
                log_paths.push(p);
            }
        }
    }
    if log_paths.is_empty() {
        return Err(CliError::new(
            "no log files to tail",
            "no `*.log` files under any selected node's $WORKDIR/logs/",
            "let the node run for a few seconds first; mxnode does not generate logs itself",
        )
        .json_if(global.json));
    }

    let mut cmd = Command::new("tail");
    if args.follow {
        cmd.arg("-F"); // -F follows by name + retries on rotation
    } else {
        cmd.arg("-n").arg(args.since.as_deref().unwrap_or("200"));
    }
    for p in &log_paths {
        cmd.arg(p);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd.status().map_err(|e| {
        CliError::new(
            "failed to invoke `tail`",
            format!("could not exec tail: {e}"),
            "ensure `tail` is on PATH (BSD or GNU coreutils — both ship by default)",
        )
        .json_if(global.json)
    })?;
    if !status.success() {
        return Err(CliError::new(
            "tail exited non-zero",
            format!("status code {:?}", status.code()),
            "the log files may have been rotated; rerun without --follow to see the latest",
        )
        .json_if(global.json));
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn run_save_archive(args: LogsArgs, global: &GlobalArgs) -> Result<(), CliError> {
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

    let targets: Vec<&NodeState> = if args.node.is_empty() {
        state.nodes.iter().collect()
    } else {
        state
            .nodes
            .iter()
            .filter(|n| args.node.contains(&n.index.get()))
            .collect()
    };
    if targets.is_empty() {
        return Err(CliError::new(
            "no nodes match",
            "state.toml has no nodes (or none matched --node)",
            "run `mxnode status` to list available indices",
        )
        .json_if(global.json));
    }

    let logs_dir = runtime.paths.custom_home.join("mx-chain-logs");
    fs::create_dir_all(&logs_dir).map_err(|e| {
        CliError::new(
            "failed to create logs directory",
            format!("{}: {e}", logs_dir.display()),
            "ensure $CUSTOM_HOME is writable by the current user",
        )
        .json_if(global.json)
    })?;

    global_op("logs.archive", &format!("{} node(s)", targets.len()));

    let mut log_files: Vec<PathBuf> = Vec::with_capacity(targets.len());
    let mut entries: Vec<ArchiveEntry> = Vec::with_capacity(targets.len());
    for node in &targets {
        let prefix = probe_pubkey_prefix(node).await;
        let suffix = prefix.as_deref().unwrap_or("nopubkey");
        let log_name = format!("mx-chain-node-{}-{}.log", node.index.get(), suffix);
        let log_path = logs_dir.join(&log_name);

        let log_file = fs::File::create(&log_path).map_err(|e| {
            CliError::new(
                "failed to open log file",
                format!("{}: {e}", log_path.display()),
                "ensure $CUSTOM_HOME/mx-chain-logs is writable",
            )
            .json_if(global.json)
        })?;

        let mut cmd = Command::new("journalctl");
        cmd.arg("--unit").arg(&node.unit);
        if let Some(since) = &args.since {
            cmd.arg("--since").arg(since);
        }
        cmd.stdout(Stdio::from(log_file))
            .stderr(Stdio::inherit())
            .stdin(Stdio::null());
        let status = cmd.status().map_err(|e| {
            CliError::new(
                "failed to invoke `journalctl`",
                format!("could not exec journalctl for {}: {e}", node.unit),
                "ensure journalctl is on PATH",
            )
            .json_if(global.json)
        })?;
        if !status.success() {
            // journalctl returns non-zero when no entries match — for an
            // archive we still want the (possibly empty) file. Surface
            // permission errors loudly though.
            if let Some(code) = status.code() {
                if code != 0 {
                    eprintln!(
                        "warning: journalctl for {} exited {code} (log file kept; usually means no entries or permission denied)",
                        node.unit,
                    );
                }
            }
        }
        entries.push(ArchiveEntry {
            index: node.index.get(),
            log_name: log_name.clone(),
            pubkey_prefix: prefix.clone(),
        });
        log_files.push(log_path);
    }

    // Pack with `tar -zcvf` to match the bash output. We run it inside
    // logs_dir so the archive contains relative paths.
    let stamp = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| CliError::new(
            "failed to format timestamp",
            e.to_string(),
            "report this as a bug",
        )
        .json_if(global.json))?
        .replace(':', "");
    let archive_name = format!("mx-chain-node-logs-{stamp}.tar.gz");
    let archive_path = logs_dir.join(&archive_name);
    let mut tar_cmd = Command::new("tar");
    tar_cmd
        .current_dir(&logs_dir)
        .arg("-zcf")
        .arg(&archive_name);
    for f in &log_files {
        if let Some(name) = f.file_name() {
            tar_cmd.arg(name);
        }
    }
    let tar_status = tar_cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            CliError::new(
                "failed to invoke `tar`",
                format!("could not exec tar: {e}"),
                "ensure tar is on PATH",
            )
            .json_if(global.json)
        })?;
    if !tar_status.success() {
        return Err(CliError::new(
            "tar exited non-zero",
            format!("status code {:?}", tar_status.code()),
            "the .log files were not removed; inspect $CUSTOM_HOME/mx-chain-logs/ manually",
        )
        .json_if(global.json));
    }

    // Remove the per-node .log files now that the archive is intact —
    // matches the bash `rm *.log` step.
    for f in &log_files {
        let _ = fs::remove_file(f);
    }

    if global.json {
        let payload = ArchiveReport {
            ok: true,
            archive: archive_path.display().to_string(),
            entries,
        };
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else {
        println!("wrote {}", archive_path.display());
        for e in &entries {
            println!("  node-{}: {}", e.index, e.log_name);
        }
    }
    Ok(())
}

async fn probe_pubkey_prefix(node: &NodeState) -> Option<String> {
    let client = NodeClient::new("127.0.0.1", node.api_port).ok()?;
    let status = tokio::time::timeout(std::time::Duration::from_secs(2), client.status())
        .await
        .ok()?
        .ok()?;
    status.data.metrics.pubkey_prefix().map(|p| p.to_string())
}

#[derive(Debug, Serialize)]
struct ArchiveReport {
    ok: bool,
    archive: String,
    entries: Vec<ArchiveEntry>,
}

#[derive(Debug, Serialize)]
struct ArchiveEntry {
    index: u16,
    log_name: String,
    pubkey_prefix: Option<String>,
}
