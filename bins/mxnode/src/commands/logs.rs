//! `mxnode logs`: shell-out to `journalctl --unit elrond-node-{N}`.
//!
//! Two modes:
//!   - default: stream journalctl output to stdout, optionally `--follow`
//!   - `--save-archive`: replicate the bash `get_logs` flow — for each
//!     selected node, dump the journal to a file named
//!     `mx-chain-node-{INDEX}-{PUBKEY_PREFIX}.log`, then tar.gz the lot
//!     to `$CUSTOM_HOME/mx-chain-logs/mx-chain-node-logs-{TIMESTAMP}.tar.gz`.
//!
//! Picks units from `mxnode.toml` so the operator gets accurate names even on
//! hosts where the units aren't sequentially indexed. If mxnode.toml is
//! missing, falls back to whatever filenames currently live in
//! `/etc/systemd/system/elrond-node-*.service`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use mxnode_core::NodeState;
use mxnode_core::Platform;
use mxnode_rpc::{LogProfile, LogStream, LogStreamEvent, NodeClient};
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
    if args.ws {
        return run_ws_logs(args, global);
    }
    reject_ws_only_args(&args, global)?;
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
        cmd.arg("--since").arg(translate_since(since));
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

fn reject_ws_only_args(args: &LogsArgs, global: &GlobalArgs) -> Result<(), CliError> {
    if args.log_level.is_some()
        || args.log_save
        || args.log_correlation
        || args.log_logger_name
        || args.use_wss
    {
        return Err(CliError::new(
            "logviewer flags require --ws",
            "`--log-level`, `--log-save`, `--log-correlation`, `--log-logger-name`, and `--use-wss` configure the /log WebSocket stream",
            "rerun with `mxnode logs --ws --node N ...`, or drop the logviewer-specific flags",
        )
        .json_if(global.json));
    }
    Ok(())
}

fn pick_units(
    runtime: &Runtime,
    requested_indices: &[u16],
    global: &GlobalArgs,
) -> Result<Vec<String>, CliError> {
    let store = StateStore::new(&runtime.paths.config_dir);

    // Source 1: mxnode.toml (preferred — exact unit names).
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
                format!("mxnode.toml has no node(s) at index {missing:?}"),
                "run `mxnode status` to list available indices, or hand-edit and re-run if mxnode.toml is stale",
            )
            .json_if(global.json));
        }
        if units.is_empty() {
            return Err(CliError::new(
                "no nodes recorded in mxnode.toml",
                "mxnode.toml is empty",
                "run `mxnode install` to set up nodes",
            )
            .json_if(global.json));
        }
        units.sort();
        return Ok(units);
    }

    // Source 2: discovery (fallback when mxnode.toml is missing).
    let discovered = scan_supervisor_dir(Path::new(DEFAULT_SYSTEMD_DIR)).map_err(|e| {
        CliError::new(
            "no mxnode.toml; failed to scan systemd dir",
            e.to_string(),
            "run `mxnode install` first",
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
                "run `mxnode install` first",
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

const WS_RETRY_BACKOFF_SECS: u64 = 10;

#[tokio::main(flavor = "current_thread")]
async fn run_ws_logs(args: LogsArgs, global: &GlobalArgs) -> Result<(), CliError> {
    if global.json {
        return Err(CliError::new(
            "logs --ws cannot emit JSON",
            "the /log WebSocket is an unbounded log stream",
            "rerun without --json, or use `mxnode logs --save-archive` for a finite artifact",
        )
        .json());
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
                "run `mxnode install` first",
            )
            .json_if(global.json)
        })?;
    let node = select_ws_log_node(&state.nodes, &args, global)?;

    let mut save_file = if args.log_save {
        Some(open_ws_log_file(&runtime, node, global)?)
    } else {
        None
    };
    let profile = ws_log_profile(&args);
    loop {
        match LogStream::connect(&args.host, node.api_port, args.use_wss, profile.clone()).await {
            Ok(mut stream) => {
                eprintln!(
                    "connected to {}://{}:{}/log for node-{}",
                    if args.use_wss { "wss" } else { "ws" },
                    args.host,
                    node.api_port,
                    node.index.get()
                );
                loop {
                    match stream.next_event().await {
                        Ok(LogStreamEvent::Line(line)) => {
                            let text = line.format_plain();
                            println!("{text}");
                            if let Some(file) = save_file.as_mut() {
                                writeln!(file, "{text}").map_err(|e| {
                                    CliError::new(
                                        "failed to write log file",
                                        e.to_string(),
                                        "ensure $CUSTOM_HOME/mx-chain-logs is writable",
                                    )
                                    .json_if(global.json)
                                })?;
                            }
                        }
                        Ok(LogStreamEvent::Text(text)) => {
                            println!("{text}");
                            if let Some(file) = save_file.as_mut() {
                                writeln!(file, "{text}").map_err(|e| {
                                    CliError::new(
                                        "failed to write log file",
                                        e.to_string(),
                                        "ensure $CUSTOM_HOME/mx-chain-logs is writable",
                                    )
                                    .json_if(global.json)
                                })?;
                            }
                        }
                        Ok(LogStreamEvent::Closed) => break,
                        Err(e) => {
                            eprintln!(
                                "log websocket error: {e}; retrying in {WS_RETRY_BACKOFF_SECS}s"
                            );
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("log websocket error: {e}; retrying in {WS_RETRY_BACKOFF_SECS}s");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(WS_RETRY_BACKOFF_SECS)).await;
    }
}

fn select_ws_log_node<'a>(
    nodes: &'a [NodeState],
    args: &LogsArgs,
    global: &GlobalArgs,
) -> Result<&'a NodeState, CliError> {
    match args.node.as_slice() {
        [idx] => nodes.iter().find(|n| n.index.get() == *idx).ok_or_else(|| {
            CliError::new(
                "no such node",
                format!("mxnode.toml has no node at index {idx}"),
                "run `mxnode status` to list available indices",
            )
            .json_if(global.json)
        }),
        [] if nodes.len() == 1 => Ok(&nodes[0]),
        [] => Err(CliError::new(
            "logs --ws needs a single node",
            format!("mxnode.toml has {} nodes", nodes.len()),
            "pass `--node N`; the upstream logviewer connects to one node API socket at a time",
        )
        .json_if(global.json)),
        many => Err(CliError::new(
            "logs --ws accepts one node",
            format!("got node selection {many:?}"),
            "run one `mxnode logs --ws --node N` session per node, or use `mxnode dashboard --ws-logs` for multi-node viewing",
        )
        .json_if(global.json)),
    }
}

fn ws_log_profile(args: &LogsArgs) -> Option<LogProfile> {
    let custom = args.log_level.is_some() || args.log_correlation || args.log_logger_name;
    custom.then(|| {
        LogProfile::new(
            args.log_level.as_deref().unwrap_or("*:INFO"),
            args.log_correlation,
            args.log_logger_name,
        )
    })
}

fn open_ws_log_file(
    runtime: &Runtime,
    node: &NodeState,
    global: &GlobalArgs,
) -> Result<std::io::BufWriter<fs::File>, CliError> {
    let logs_dir = runtime.paths.custom_home.join("mx-chain-logs");
    fs::create_dir_all(&logs_dir).map_err(|e| {
        CliError::new(
            "failed to create logs directory",
            format!("{}: {e}", logs_dir.display()),
            "ensure $CUSTOM_HOME is writable by the current user",
        )
        .json_if(global.json)
    })?;
    let stamp = timestamp_for_filename(global)?;
    let path = logs_dir.join(format!("logviewer-node-{}-{stamp}.log", node.index.get()));
    let file = fs::File::create(&path).map_err(|e| {
        CliError::new(
            "failed to open log file",
            format!("{}: {e}", path.display()),
            "ensure $CUSTOM_HOME/mx-chain-logs is writable",
        )
        .json_if(global.json)
    })?;
    eprintln!("saving websocket logs to {}", path.display());
    Ok(std::io::BufWriter::new(file))
}

#[tokio::main(flavor = "current_thread")]
async fn run_save_archive(args: LogsArgs, global: &GlobalArgs) -> Result<(), CliError> {
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
                "run `mxnode install` first",
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
            "mxnode.toml has no nodes (or none matched --node)",
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
            cmd.arg("--since").arg(translate_since(since));
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
    let stamp = timestamp_for_filename(global)?;
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

fn timestamp_for_filename(global: &GlobalArgs) -> Result<String, CliError> {
    let stamp = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| {
            CliError::new(
                "failed to format timestamp",
                e.to_string(),
                "report this as a bug",
            )
            .json_if(global.json)
        })?
        .replace(':', "");
    Ok(stamp)
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

/// Translate a duration shorthand like `1h`, `30min`, `5s`, `2d` into the
/// `"N units ago"` form journalctl's `--since` accepts. Operators reach
/// for the short form; journalctl insists on the long. Anything that
/// doesn't match the shorthand pattern (absolute timestamps, words like
/// `yesterday`, the long form itself) is passed through unchanged.
fn translate_since(raw: &str) -> String {
    let trimmed = raw.trim();
    let (num_part, unit_part) = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| trimmed.split_at(i))
        .unwrap_or((trimmed, ""));
    if num_part.is_empty() || num_part.parse::<u64>().is_err() {
        return raw.to_string();
    }
    let unit_word = match unit_part.trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => "seconds",
        "m" | "min" | "mins" | "minute" | "minutes" => "minutes",
        "h" | "hr" | "hrs" | "hour" | "hours" => "hours",
        "d" | "day" | "days" => "days",
        "w" | "week" | "weeks" => "weeks",
        _ => return raw.to_string(),
    };
    format!("{num_part} {unit_word} ago")
}

#[cfg(test)]
mod tests {
    use super::{
        reject_ws_only_args, select_ws_log_node, translate_since, ws_log_profile, LogProfile,
    };
    use crate::cli::{GlobalArgs, LogsArgs};
    use mxnode_core::{NodeIndex, NodeState, Role, Shard};
    use std::path::PathBuf;

    fn global(json: bool) -> GlobalArgs {
        GlobalArgs {
            config: None,
            force: false,
            json,
            verbose: false,
            quiet: false,
        no_update_check: true,
        }
    }

    fn logs_args() -> LogsArgs {
        LogsArgs {
            node: Vec::new(),
            since: None,
            follow: false,
            save_archive: false,
            ws: false,
            host: "127.0.0.1".to_string(),
            log_level: None,
            log_save: false,
            log_correlation: false,
            log_logger_name: false,
            use_wss: false,
        }
    }

    fn node(index: u16) -> NodeState {
        NodeState {
            index: NodeIndex::new(index),
            role: Role::Observer,
            shard: Shard::Auto,
            display_name: format!("node-{index}"),
            api_port: 8080 + index,
            unit: format!("elrond-node-{index}.service"),
            unit_override: String::new(),
            workdir: PathBuf::from(format!("/tmp/node-{index}")),
            last_known_pubkey: String::new(),
            last_action: String::new(),
            last_action_at: None,
        }
    }

    #[test]
    fn translate_since_handles_shorthand() {
        assert_eq!(translate_since("5s"), "5 seconds ago");
        assert_eq!(translate_since("30min"), "30 minutes ago");
        assert_eq!(translate_since("1h"), "1 hours ago");
        assert_eq!(translate_since("2d"), "2 days ago");
        assert_eq!(translate_since("3 weeks"), "3 weeks ago");
    }

    #[test]
    fn translate_since_passes_through_absolute_and_words() {
        assert_eq!(translate_since("2024-01-01"), "2024-01-01");
        assert_eq!(translate_since("yesterday"), "yesterday");
        assert_eq!(translate_since("1 hour ago"), "1 hour ago");
        assert_eq!(translate_since(""), "");
    }

    #[test]
    fn ws_log_profile_is_default_unless_runtime_profile_flags_are_set() {
        let mut args = logs_args();
        args.log_save = true;
        assert!(
            ws_log_profile(&args).is_none(),
            "--log-save must not change the node's runtime log profile"
        );

        args.log_level = Some("*:DEBUG,api:INFO".to_string());
        let profile: LogProfile = ws_log_profile(&args).expect("custom profile expected");
        assert_eq!(profile.log_level_patterns, "*:DEBUG,api:INFO");
        assert!(!profile.with_correlation);
        assert!(!profile.with_logger_name);
    }

    #[test]
    fn reject_ws_only_args_requires_ws() {
        let mut args = logs_args();
        args.log_level = Some("*:DEBUG".to_string());
        assert!(reject_ws_only_args(&args, &global(false)).is_err());

        args.log_level = None;
        assert!(reject_ws_only_args(&args, &global(false)).is_ok());
    }

    #[test]
    fn select_ws_log_node_requires_single_target() {
        let nodes = vec![node(0), node(1)];
        let mut args = logs_args();
        assert!(select_ws_log_node(&nodes, &args, &global(false)).is_err());

        args.node = vec![1];
        let picked = select_ws_log_node(&nodes, &args, &global(false)).unwrap();
        assert_eq!(picked.index.get(), 1);

        args.node = vec![0, 1];
        assert!(select_ws_log_node(&nodes, &args, &global(false)).is_err());
    }

    #[test]
    fn select_ws_log_node_uses_only_node_by_default() {
        let nodes = vec![node(7)];
        let args = logs_args();
        let picked = select_ws_log_node(&nodes, &args, &global(false)).unwrap();
        assert_eq!(picked.index.get(), 7);
    }
}
