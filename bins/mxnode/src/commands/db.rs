//! `mxnode db remove|prune`: destructive ops that wipe a node's working
//! data. Both require `--yes`; on a TTY both also require an interactive
//! "type the index" confirm before deleting anything.
//!
//! Refuses if the targeted unit is currently active — mxnode does not
//! pause the node for you because pausing without restarting can cause
//! validator rating loss; the operator must `mxnode stop --node N` first.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use mxnode_core::{NodeIndex, NodeState, Shard, HostState};
use mxnode_state::StateStore;
use mxnode_systemd::ActiveState;
use serde::Serialize;

use crate::cli::{DbCommand, GlobalArgs};
use crate::errors::CliError;
use crate::events::{node_op_end, node_op_start, Outcome};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

/// Top-level dispatcher for `mxnode db <subcmd>`.
pub fn run(cmd: DbCommand, global: &GlobalArgs) -> Result<(), CliError> {
    match cmd {
        DbCommand::Remove { node, yes } => run_op(DbVerb::Remove, &node, None, yes, global),
        DbCommand::Prune { node, epochs } => run_op(DbVerb::Prune, &node, epochs, true, global),
        DbCommand::Reseed { node, yes } => run_op(DbVerb::Reseed, &node, None, yes, global),
        DbCommand::Import {
            node,
            source,
            no_sig_check,
            replace,
            yes,
            dry_run,
        } => run_import(
            ImportRequest {
                node,
                source,
                no_sig_check,
                replace,
                yes,
                dry_run,
            },
            global,
        ),
        DbCommand::ImportPlan {
            source_root,
            no_sig_check,
            replace,
            require_elasticsearch,
            yes,
            output,
        } => run_import_plan(
            ImportPlanRequest {
                source_root,
                no_sig_check,
                replace,
                require_elasticsearch,
                yes,
                output,
            },
            global,
        ),
    }
}

struct ImportRequest {
    node: u16,
    source: PathBuf,
    no_sig_check: bool,
    replace: bool,
    yes: bool,
    dry_run: bool,
}

struct ImportPlanRequest {
    source_root: PathBuf,
    no_sig_check: bool,
    replace: bool,
    require_elasticsearch: bool,
    yes: bool,
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum DbVerb {
    Remove,
    Prune,
    Reseed,
}

impl DbVerb {
    fn op_label(self) -> &'static str {
        match self {
            Self::Remove => "db.remove",
            Self::Prune => "db.prune",
            Self::Reseed => "db.reseed",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Remove => "remove db/, logs/, stats/, health-records/",
            Self::Prune => "trim db/Epoch_N directories",
            Self::Reseed => "remove db then start the node (resync from genesis)",
        }
    }
}

/// Default number of recent epochs `db prune` keeps when `--epochs` is
/// not supplied. The mainnet config rotates an epoch every ~24 hours,
/// so 4 keeps roughly the last four days of block data.
const DEFAULT_PRUNE_KEEP: u32 = 4;

#[tokio::main(flavor = "current_thread")]
async fn run_op(
    verb: DbVerb,
    requested: &[u16],
    epochs_to_keep: Option<u32>,
    yes: bool,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    if !yes {
        return Err(CliError::new(
            "refusing without --yes",
            format!(
                "{} permanently changes the node's data; pass --yes to confirm",
                verb.description(),
            ),
            format!("rerun with `mxnode db {} --yes --node N`", phase_name(verb)),
        )
        .json_if(global.json));
    }

    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.config_dir);
    let state = load_state_or_err(&store, global)?;

    let targets = pick_targets(&state, requested, global)?;

    let ctl = crate::orchestrator::supervisor::build_supervisor();
    // Refuse when any target is currently active. We could stop them
    // automatically but doing so to a validator without operator intent
    // is exactly the rating-loss failure mode the audit warned about.
    for node in &targets {
        let active = match ctl.is_active(&node.unit).await {
            Ok(s) => s,
            Err(_) => ActiveState::Unknown,
        };
        if matches!(active, ActiveState::Active | ActiveState::Activating) {
            return Err(CliError::new(
                format!("refusing: node {} is {}", node.index.get(), label(active)),
                format!("{} is still running", node.unit),
                "run `mxnode stop --node N` first; mxnode never auto-stops a live unit for db ops",
            )
            .json_if(global.json));
        }
    }

    if std::io::stdin().is_terminal() && !global.json {
        prompt_confirm(verb, &targets, global)?;
    }

    let keep = epochs_to_keep.unwrap_or(DEFAULT_PRUNE_KEEP);
    let mut results: Vec<NodeResult> = Vec::with_capacity(targets.len());
    for node in &targets {
        node_op_start(verb.op_label(), node.index, &node.unit);
        let outcome: std::io::Result<Option<String>> = match verb {
            DbVerb::Remove => wipe_node_data(&node.workdir).map(|_| None),
            DbVerb::Prune => prune_old_epochs(&node.workdir, keep).map(Some),
            DbVerb::Reseed => match wipe_node_data(&node.workdir) {
                Err(e) => Err(e),
                Ok(()) => match ctl.start(&node.unit).await {
                    Ok(()) => Ok(Some(format!("started {}", node.unit))),
                    Err(e) => Err(std::io::Error::other(format!(
                        "wiped db but failed to start {}: {e}",
                        node.unit
                    ))),
                },
            },
        };
        let (ok, error, note) = match outcome {
            Ok(note) => (true, None, note),
            Err(e) => (false, Some(e.to_string()), None),
        };
        let event_outcome = match &error {
            None => Outcome::Ok,
            Some(e) => Outcome::Fail { cause: e.as_str() },
        };
        node_op_end(verb.op_label(), node.index, &node.unit, event_outcome);
        results.push(NodeResult {
            index: node.index.get(),
            unit: node.unit.clone(),
            ok,
            error,
            note,
        });
    }

    let any_failed = results.iter().any(|r| !r.ok);
    if global.json {
        let mut payload = serde_json::to_value(&DbReport {
            op: verb.op_label(),
            description: verb.description(),
            nodes: results.clone(),
        })
        .unwrap_or(serde_json::Value::Null);
        if any_failed {
            payload["error"] = serde_json::json!({
                "summary": format!("{} reported failures", verb.op_label()),
                "cause": format!(
                    "{} of {} node(s) failed",
                    results.iter().filter(|r| !r.ok).count(),
                    results.len(),
                ),
                "try": "inspect the working dirs manually",
            });
        }
        println!("{payload}");
    } else {
        for r in &results {
            let glyph = if r.ok { "✓" } else { "✗" };
            print!("{glyph} {} node-{}", verb.op_label(), r.index);
            if let Some(note) = &r.note {
                print!(" — {note}");
            }
            if let Some(err) = &r.error {
                print!(" — {err}");
            }
            println!();
        }
    }

    if any_failed {
        return Err(CliError::new(
            format!("{} reported failures", verb.op_label()),
            "see per-node errors above",
            "fix the listed problems and rerun against the failing indices",
        )
        .silent());
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn run_import(req: ImportRequest, global: &GlobalArgs) -> Result<(), CliError> {
    if req.replace && !req.yes {
        return Err(CliError::new(
            "refusing --replace without --yes",
            "--replace removes the target node's db/ before starting import-db",
            "rerun with `mxnode db import --replace --yes ...` after verifying the source DB",
        )
        .json_if(global.json));
    }
    if global.json && !req.dry_run {
        return Err(CliError::new(
            "db import does not stream JSON while executing",
            "the node import-db process can emit a long log stream",
            "use `--dry-run --json` for a machine-readable plan, or run without --json to execute",
        )
        .json_if(global.json));
    }

    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.config_dir);
    let state = load_state_or_err(&store, global)?;
    let node = state
        .nodes
        .iter()
        .find(|n| n.index.get() == req.node)
        .ok_or_else(|| {
            CliError::new(
                "no such node",
                format!("mxnode.toml has no node at index {}", req.node),
                "run `mxnode status` to list valid indices",
            )
            .json_if(global.json)
        })?;

    let ctl = crate::orchestrator::supervisor::build_supervisor();
    let active = match ctl.is_active(&node.unit).await {
        Ok(s) => s,
        Err(_) => ActiveState::Unknown,
    };
    if matches!(active, ActiveState::Active | ActiveState::Activating) {
        return Err(CliError::new(
            format!("refusing: node {} is {}", node.index.get(), label(active)),
            format!("{} is still running", node.unit),
            "run `mxnode stop --node N` first; import-db needs the node workdir exclusively",
        )
        .json_if(global.json));
    }

    let source = validate_import_source(&req.source, global)?;
    let node_binary = node.workdir.join("node");
    if !node_binary.exists() {
        return Err(CliError::new(
            "node binary is missing",
            format!("expected {}", node_binary.display()),
            "run `mxnode reapply-config`, reinstall, or repair the node symlink before import-db",
        )
        .json_if(global.json));
    }
    validate_node_config_for_import(&node.workdir, global)?;
    prepare_import_target(&node.workdir, &source, req.replace, req.dry_run, global)?;

    let args = import_db_args(
        &runtime.loaded.file.node.log_level,
        &source,
        req.no_sig_check,
    );
    if req.dry_run {
        return emit_import_dry_run(node, &node_binary, &args, &source, global);
    }

    node_op_start("db.import", node.index, &node.unit);
    let status = std::process::Command::new(&node_binary)
        .current_dir(&node.workdir)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            CliError::new(
                "failed to start import-db process",
                e.to_string(),
                "check the node binary symlink and file permissions",
            )
            .json_if(global.json)
        })?;

    if status.success() {
        node_op_end("db.import", node.index, &node.unit, Outcome::Ok);
        println!("✓ db.import node-{} completed", node.index.get());
        Ok(())
    } else {
        let cause = format!("node exited with status {status}");
        node_op_end(
            "db.import",
            node.index,
            &node.unit,
            Outcome::Fail { cause: &cause },
        );
        Err(CliError::new(
            "import-db process failed",
            cause,
            "inspect the log output above and verify the source db matches the node config and shard",
        )
        .json_if(global.json))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn run_import_plan(req: ImportPlanRequest, global: &GlobalArgs) -> Result<(), CliError> {
    if req.replace && !req.yes {
        return Err(CliError::new(
            "refusing --replace without --yes",
            "--replace would be included in generated destructive import commands",
            "rerun with `mxnode db import-plan --replace --yes ...` after verifying the source DBs",
        )
        .json_if(global.json));
    }

    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.config_dir);
    let state = load_state_or_err(&store, global)?;
    let sources = discover_import_sources(&req.source_root, global)?;
    let ctl = crate::orchestrator::supervisor::build_supervisor();
    let mut report = build_import_plan_report(&state, &sources, &req, global, |unit| {
        let ctl = ctl.clone();
        async move {
            match ctl.is_active(&unit).await {
                Ok(s) => s,
                Err(_) => ActiveState::Unknown,
            }
        }
    })
    .await;

    if let Some(output) = &req.output {
        let body = serde_json::to_string_pretty(&report).map_err(|e| {
            CliError::new(
                "failed to encode import plan",
                e.to_string(),
                "report this as a bug",
            )
            .json_if(global.json)
        })?;
        std::fs::write(output, format!("{body}\n")).map_err(|e| {
            CliError::new(
                "failed to write import plan",
                format!("{}: {e}", output.display()),
                "choose a writable --output path",
            )
            .json_if(global.json)
        })?;
        report.output = Some(output.display().to_string());
    }

    if global.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("report serializes")
        );
    } else {
        print_import_plan_report(&report);
    }

    if !report.ok {
        return Err(CliError::new(
            "import plan is incomplete",
            "one or more required shard mappings or safety checks failed",
            "fix the warnings above, then rerun import-plan before executing the generated commands",
        )
        .silent());
    }
    Ok(())
}

fn phase_name(v: DbVerb) -> &'static str {
    match v {
        DbVerb::Prune => "prune",
        DbVerb::Reseed => "reseed",
        DbVerb::Remove => "remove",
    }
}

fn load_state_or_err(store: &StateStore, global: &GlobalArgs) -> Result<HostState, CliError> {
    store
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
        })
}

fn pick_targets<'a>(
    state: &'a HostState,
    requested: &[u16],
    global: &GlobalArgs,
) -> Result<Vec<&'a NodeState>, CliError> {
    if requested.is_empty() {
        return Err(CliError::new(
            "no --node specified",
            "db ops always require explicit indices to prevent accidental fleet-wide wipes",
            "rerun with `--node N` (repeat for more) or list available indices via `mxnode status`",
        )
        .json_if(global.json));
    }
    let known: Vec<u16> = state.nodes.iter().map(|n| n.index.get()).collect();
    let missing: Vec<u16> = requested
        .iter()
        .copied()
        .filter(|i| !known.contains(i))
        .collect();
    if !missing.is_empty() {
        return Err(CliError::new(
            "no such node",
            format!("mxnode.toml has no node(s) at index {missing:?}"),
            "run `mxnode status` to list valid indices",
        )
        .json_if(global.json));
    }
    let _ = NodeIndex::new(0); // silence unused-import lint when refactor moves
    Ok(state
        .nodes
        .iter()
        .filter(|n| requested.contains(&n.index.get()))
        .collect())
}

fn wipe_node_data(workdir: &Path) -> std::io::Result<()> {
    // Recreate the four data subdirs as empty so the next node start
    // finds the expected layout. The operator's CUSTOM_USER owns these
    // dirs; an io error surfaces directly if it doesn't.
    for sub in ["db", "logs", "stats", "health-records"] {
        let path = workdir.join(sub);
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        std::fs::create_dir_all(&path)?;
    }
    Ok(())
}

/// Trim `db/Epoch_N/` directories down to the most recent `keep` of
/// them. Returns a one-line summary the caller prints alongside the
/// per-node `✓`. Missing/empty `db/` directories are a no-op.
fn prune_old_epochs(workdir: &Path, keep: u32) -> std::io::Result<String> {
    let db = workdir.join("db");
    if !db.exists() {
        return Ok("no db/ to prune".to_string());
    }
    let mut epochs: Vec<(u32, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&db)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(rest) = name_str.strip_prefix("Epoch_") {
            if let Ok(n) = rest.parse::<u32>() {
                epochs.push((n, entry.path()));
            }
        }
    }
    if epochs.is_empty() {
        return Ok("no Epoch_N directories found".to_string());
    }
    // Sort descending so the head of the vec is the newest.
    epochs.sort_by(|a, b| b.0.cmp(&a.0));
    let total = epochs.len();
    let to_remove = epochs.split_off(keep.min(total as u32) as usize);
    let removed = to_remove.len();
    for (_, path) in &to_remove {
        std::fs::remove_dir_all(path)?;
    }
    Ok(format!(
        "removed {removed} of {total} Epoch_N directories (kept newest {})",
        keep.min(total as u32),
    ))
}

fn validate_import_source(source: &Path, global: &GlobalArgs) -> Result<PathBuf, CliError> {
    let source = std::fs::canonicalize(source).map_err(|e| {
        CliError::new(
            "import source is not readable",
            format!("{}: {e}", source.display()),
            "pass a directory that contains the source `db/` subdirectory",
        )
        .json_if(global.json)
    })?;
    let source_db = source.join("db");
    if !source_db.is_dir() {
        return Err(CliError::new(
            "import source must contain db/",
            format!("expected {}", source_db.display()),
            "the `-import-db` flag points at the parent directory, not at db/ itself",
        )
        .json_if(global.json));
    }
    Ok(source)
}

fn validate_node_config_for_import(workdir: &Path, global: &GlobalArgs) -> Result<(), CliError> {
    for rel in ["config/mxnode.toml", "config/prefs.toml"] {
        let path = workdir.join(rel);
        if !path.exists() {
            return Err(CliError::new(
                "node config is incomplete",
                format!("expected {}", path.display()),
                "import-db requires the target config/ to match the node that produced the source db",
            )
            .json_if(global.json));
        }
    }
    Ok(())
}

fn prepare_import_target(
    workdir: &Path,
    source: &Path,
    replace: bool,
    dry_run: bool,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let target_db = workdir.join("db");
    let source_db = source.join("db");
    if same_path(&target_db, &source_db) {
        return Err(CliError::new(
            "source db and target db are the same directory",
            format!("both resolve to {}", source_db.display()),
            "place the import-db source outside the target node workdir",
        )
        .json_if(global.json));
    }

    let has_entries = db_has_entries(&target_db).map_err(|e| {
        CliError::new(
            "failed to inspect target db/",
            format!("{}: {e}", target_db.display()),
            "fix permissions or pass a different node index",
        )
        .json_if(global.json)
    })?;
    if has_entries && !replace {
        return Err(CliError::new(
            "target db/ is not empty",
            "import-db must start with an empty target db/ so it reprocesses from genesis",
            "stop the node and rerun with `--replace --yes`, or empty db/ manually",
        )
        .json_if(global.json));
    }

    if replace && !dry_run && target_db.exists() {
        std::fs::remove_dir_all(&target_db).map_err(|e| {
            CliError::new(
                "failed to remove target db/",
                format!("{}: {e}", target_db.display()),
                "fix permissions and rerun",
            )
            .json_if(global.json)
        })?;
    }
    if !dry_run {
        std::fs::create_dir_all(&target_db).map_err(|e| {
            CliError::new(
                "failed to create empty target db/",
                format!("{}: {e}", target_db.display()),
                "fix permissions and rerun",
            )
            .json_if(global.json)
        })?;
    }
    Ok(())
}

fn db_has_entries(db: &Path) -> std::io::Result<bool> {
    if !db.exists() {
        return Ok(false);
    }
    let mut entries = std::fs::read_dir(db)?;
    Ok(entries.next().transpose()?.is_some())
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn import_db_args(log_level: &str, source: &Path, no_sig_check: bool) -> Vec<String> {
    let mut args = vec![
        "-use-log-view".to_string(),
        "-log-level".to_string(),
        log_level.to_string(),
        "-import-db".to_string(),
        source.display().to_string(),
    ];
    if no_sig_check {
        args.push("-import-db-no-sig-check".to_string());
    }
    args
}

fn emit_import_dry_run(
    node: &NodeState,
    binary: &Path,
    args: &[String],
    source: &Path,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    if global.json {
        let payload = serde_json::json!({
            "op": "db.import",
            "dry_run": true,
            "node": node.index.get(),
            "unit": node.unit.as_str(),
            "workdir": node.workdir.display().to_string(),
            "source": source.display().to_string(),
            "command": import_command(binary, args),
        });
        println!("{payload}");
    } else {
        println!("dry-run db.import node-{}", node.index.get());
        println!("  cd {}", node.workdir.display());
        println!("  {}", shell_join(&import_command(binary, args)));
    }
    Ok(())
}

fn import_command(binary: &Path, args: &[String]) -> Vec<String> {
    let mut command = Vec::with_capacity(args.len() + 1);
    command.push(binary.display().to_string());
    command.extend(args.iter().cloned());
    command
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            if p.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '*')
            }) {
                p.clone()
            } else {
                format!("'{}'", p.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone)]
struct ImportSource {
    root: PathBuf,
    shard: Option<Shard>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ImportPlanReport {
    ok: bool,
    source_root: String,
    full_setup: bool,
    required_shards: Vec<String>,
    missing_shards: Vec<String>,
    entries: Vec<ImportPlanEntry>,
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImportPlanEntry {
    node: u16,
    unit: String,
    shard: String,
    source: String,
    command: Vec<String>,
    warnings: Vec<String>,
}

async fn build_import_plan_report<F, Fut>(
    state: &HostState,
    sources: &[ImportSource],
    req: &ImportPlanRequest,
    global: &GlobalArgs,
    mut active_for: F,
) -> ImportPlanReport
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = ActiveState>,
{
    let required = required_import_shards();
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let mut missing = Vec::new();

    for source in sources {
        if source.shard.is_none() {
            warnings.push(format!(
                "could not infer shard for source {}",
                source.root.display()
            ));
        }
        warnings.extend(source.warnings.iter().cloned());
    }

    for shard in &required {
        let Some(node) = state.nodes.iter().find(|n| n.shard == *shard) else {
            missing.push(shard.to_string());
            continue;
        };
        let Some(source) = sources.iter().find(|s| s.shard == Some(*shard)) else {
            missing.push(shard.to_string());
            continue;
        };

        let mut entry_warnings = Vec::new();
        match active_for(node.unit.clone()).await {
            ActiveState::Active | ActiveState::Activating => entry_warnings.push(format!(
                "{} is running; stop node-{} before import",
                node.unit,
                node.index.get()
            )),
            ActiveState::Unknown => entry_warnings.push(format!(
                "could not determine active state for {}",
                node.unit
            )),
            _ => {}
        }
        for rel in ["config/mxnode.toml", "config/prefs.toml"] {
            let path = node.workdir.join(rel);
            if !path.exists() {
                entry_warnings.push(format!("missing {}", path.display()));
            }
        }
        if let Err(e) =
            prepare_import_target(&node.workdir, &source.root, req.replace, true, global)
        {
            entry_warnings.push(e.summary);
        }
        if req.require_elasticsearch {
            entry_warnings.extend(validate_elasticsearch_external(&node.workdir));
        }

        entries.push(ImportPlanEntry {
            node: node.index.get(),
            unit: node.unit.clone(),
            shard: shard.to_string(),
            source: source.root.display().to_string(),
            command: import_plan_command(node.index.get(), &source.root, req),
            warnings: entry_warnings,
        });
    }

    let full_setup = missing.is_empty() && entries.len() == required.len();
    let ok =
        full_setup && warnings.is_empty() && entries.iter().all(|entry| entry.warnings.is_empty());
    ImportPlanReport {
        ok,
        source_root: req.source_root.display().to_string(),
        full_setup,
        required_shards: required.iter().map(ToString::to_string).collect(),
        missing_shards: missing,
        entries,
        warnings,
        output: None,
    }
}

fn print_import_plan_report(report: &ImportPlanReport) {
    println!("import-db plan:");
    println!("  source_root: {}", report.source_root);
    println!("  full_setup:  {}", report.full_setup);
    if !report.missing_shards.is_empty() {
        println!("  missing:     {}", report.missing_shards.join(", "));
    }
    for warning in &report.warnings {
        println!("  ! {warning}");
    }
    for entry in &report.entries {
        let glyph = if entry.warnings.is_empty() {
            "✓"
        } else {
            "!"
        };
        println!(
            "{glyph} shard {} node-{} <- {}",
            entry.shard, entry.node, entry.source
        );
        for warning in &entry.warnings {
            println!("    ! {warning}");
        }
        println!("    {}", shell_join(&entry.command));
    }
    if let Some(output) = &report.output {
        println!("  wrote: {output}");
    }
}

fn import_plan_command(node: u16, source: &Path, req: &ImportPlanRequest) -> Vec<String> {
    let mut command = vec![
        "mxnode".to_string(),
        "db".to_string(),
        "import".to_string(),
        "--node".to_string(),
        node.to_string(),
        "--source".to_string(),
        source.display().to_string(),
    ];
    if req.no_sig_check {
        command.push("--no-sig-check".to_string());
    }
    if req.replace {
        command.push("--replace".to_string());
        command.push("--yes".to_string());
    }
    command
}

fn validate_elasticsearch_external(workdir: &Path) -> Vec<String> {
    let path = workdir.join("config/external.toml");
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return vec![format!("missing {}", path.display())];
        }
        Err(e) => return vec![format!("read {}: {e}", path.display())],
    };
    let parsed = match body.parse::<toml::Value>() {
        Ok(value) => value,
        Err(e) => return vec![format!("parse {}: {e}", path.display())],
    };
    let connector = parsed.get("ElasticSearchConnector");
    let enabled = connector
        .and_then(|c| c.get("Enabled"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let url = connector
        .and_then(|c| c.get("URL"))
        .and_then(toml::Value::as_str)
        .unwrap_or("")
        .trim();

    let mut warnings = Vec::new();
    if !enabled {
        warnings.push(format!(
            "{}: ElasticSearchConnector.Enabled is not true",
            path.display()
        ));
    }
    if url.is_empty() {
        warnings.push(format!(
            "{}: ElasticSearchConnector.URL is empty",
            path.display()
        ));
    }
    warnings
}

fn discover_import_sources(
    source_root: &Path,
    global: &GlobalArgs,
) -> Result<Vec<ImportSource>, CliError> {
    let root = std::fs::canonicalize(source_root).map_err(|e| {
        CliError::new(
            "import source root is not readable",
            format!("{}: {e}", source_root.display()),
            "pass a directory containing import-db sources",
        )
        .json_if(global.json)
    })?;

    let mut candidates = Vec::new();
    if root.join("db").is_dir() {
        candidates.push(root.clone());
    }
    for entry in std::fs::read_dir(&root).map_err(|e| {
        CliError::new(
            "failed to scan import source root",
            format!("{}: {e}", root.display()),
            "fix permissions or pass a different --source-root",
        )
        .json_if(global.json)
    })? {
        let entry = entry.map_err(|e| {
            CliError::new(
                "failed to scan import source root",
                e.to_string(),
                "fix permissions or pass a different --source-root",
            )
            .json_if(global.json)
        })?;
        let path = entry.path();
        if path.join("db").is_dir() {
            candidates.push(path);
        }
    }
    candidates.sort();
    candidates.dedup();

    if candidates.is_empty() {
        return Err(CliError::new(
            "no import-db sources found",
            format!(
                "{} has no db/ child or immediate children containing db/",
                root.display()
            ),
            "arrange sources as <source-root>/shard-0/db, shard-1/db, shard-2/db, metachain/db",
        )
        .json_if(global.json));
    }

    Ok(candidates
        .into_iter()
        .map(|root| {
            let (shard, mut warnings) = infer_import_source_shard(&root);
            if shard.is_none() {
                warnings.push("source will not be mapped until its shard is explicit".to_string());
            }
            ImportSource {
                root,
                shard,
                warnings,
            }
        })
        .collect())
}

fn infer_import_source_shard(source: &Path) -> (Option<Shard>, Vec<String>) {
    let mut found = Vec::new();
    collect_shards(&source.join("db"), 0, &mut found);
    if found.len() == 1 {
        return (found.first().copied(), Vec::new());
    }
    if found.len() > 1 {
        return (
            None,
            vec![format!(
                "source {} contains multiple shard ids: {}",
                source.display(),
                found
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )],
        );
    }
    (infer_shard_from_name(source), Vec::new())
}

fn collect_shards(path: &Path, depth: usize, found: &mut Vec<Shard>) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(shard) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|name| name.strip_prefix("Shard_"))
            .and_then(parse_shard_dir_name)
        {
            if !found.contains(&shard) {
                found.push(shard);
            }
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            collect_shards(&path, depth + 1, found);
        }
    }
}

fn parse_shard_dir_name(name: &str) -> Option<Shard> {
    let raw = name.strip_prefix("Shard_").unwrap_or(name);
    match raw {
        "0" => Some(Shard::Zero),
        "1" => Some(Shard::One),
        "2" => Some(Shard::Two),
        "4294967295" | "metachain" | "meta" => Some(Shard::Metachain),
        _ => None,
    }
}

fn infer_shard_from_name(path: &Path) -> Option<Shard> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let normalized = name
        .strip_prefix("shard-")
        .or_else(|| name.strip_prefix("shard_"))
        .unwrap_or(&name);
    parse_shard_dir_name(normalized)
}

fn required_import_shards() -> [Shard; 4] {
    [Shard::Zero, Shard::One, Shard::Two, Shard::Metachain]
}

fn prompt_confirm(
    verb: DbVerb,
    targets: &[&NodeState],
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let indices: Vec<u16> = targets.iter().map(|n| n.index.get()).collect();
    let label = format!("{:?}", indices);
    writeln!(
        handle,
        "About to {} on node(s) {label}.",
        verb.description()
    )
    .map_err(|e| io_err(e, global))?;
    write!(handle, "Type the indices back to confirm (e.g. {label}): ")
        .map_err(|e| io_err(e, global))?;
    handle.flush().map_err(|e| io_err(e, global))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| io_err(e, global))?;
    let typed = line.trim();
    if typed != label {
        return Err(CliError::new(
            "confirmation mismatch",
            format!("expected {label:?}, got {typed:?}"),
            "rerun the command and type the exact index list back when prompted",
        )
        .json_if(global.json));
    }
    Ok(())
}

fn io_err(e: std::io::Error, global: &GlobalArgs) -> CliError {
    CliError::new(
        "io error during confirmation prompt",
        e.to_string(),
        "rerun in a terminal, or pass `--json` to bypass the interactive prompt (still requires --yes)",
    )
    .json_if(global.json)
}

fn label(state: ActiveState) -> &'static str {
    match state {
        ActiveState::Active => "active",
        ActiveState::Inactive => "inactive",
        ActiveState::Failed => "failed",
        ActiveState::Activating => "activating",
        ActiveState::Deactivating => "deactivating",
        ActiveState::Unknown => "unknown",
    }
}

#[derive(Debug, Clone, Serialize)]
struct DbReport {
    op: &'static str,
    description: &'static str,
    nodes: Vec<NodeResult>,
}

#[derive(Debug, Clone, Serialize)]
struct NodeResult {
    index: u16,
    unit: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global() -> GlobalArgs {
        GlobalArgs {
            config: None,
            force: false,
            json: false,
            verbose: false,
            quiet: false,
        no_update_check: true,
        }
    }

    #[test]
    fn import_args_include_no_sig_check_only_when_requested() {
        let source = PathBuf::from("/srv/import-db");
        let plain = import_db_args("*:INFO", &source, false);
        assert_eq!(
            plain,
            vec![
                "-use-log-view",
                "-log-level",
                "*:INFO",
                "-import-db",
                "/srv/import-db"
            ]
        );

        let fast = import_db_args("*:INFO", &source, true);
        assert!(fast.contains(&"-import-db-no-sig-check".to_string()));
    }

    #[test]
    fn prepare_import_target_refuses_non_empty_db_without_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path().join("node-0");
        let target_db = workdir.join("db");
        std::fs::create_dir_all(&target_db).unwrap();
        std::fs::write(target_db.join("LOCK"), "").unwrap();

        let source = tmp.path().join("import-db");
        std::fs::create_dir_all(source.join("db")).unwrap();

        let err = prepare_import_target(&workdir, &source, false, false, &global()).unwrap_err();
        assert!(err.to_string().contains("target db/ is not empty"));
    }

    #[test]
    fn prepare_import_target_replace_recreates_db() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path().join("node-0");
        let target_db = workdir.join("db");
        std::fs::create_dir_all(&target_db).unwrap();
        std::fs::write(target_db.join("LOCK"), "").unwrap();

        let source = tmp.path().join("import-db");
        std::fs::create_dir_all(source.join("db")).unwrap();

        prepare_import_target(&workdir, &source, true, false, &global()).unwrap();
        assert!(target_db.exists());
        assert!(!db_has_entries(&target_db).unwrap());
    }

    #[test]
    fn import_sources_infer_shards_from_db_tree_and_names() {
        let tmp = tempfile::tempdir().unwrap();
        let shard_0 = tmp.path().join("custom-a");
        std::fs::create_dir_all(shard_0.join("db/1/Epoch_0/Shard_0")).unwrap();
        let meta = tmp.path().join("metachain");
        std::fs::create_dir_all(meta.join("db")).unwrap();

        let sources = discover_import_sources(tmp.path(), &global()).unwrap();
        let shard_0 = std::fs::canonicalize(shard_0).unwrap();
        let meta = std::fs::canonicalize(meta).unwrap();
        assert!(sources
            .iter()
            .any(|s| s.root == shard_0 && s.shard == Some(Shard::Zero)));
        assert!(sources
            .iter()
            .any(|s| s.root == meta && s.shard == Some(Shard::Metachain)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn import_plan_requires_full_shard_set() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = HostState::empty("test");
        state.nodes = vec![
            node_for_plan(tmp.path(), 0, Shard::Zero),
            node_for_plan(tmp.path(), 1, Shard::One),
            node_for_plan(tmp.path(), 2, Shard::Two),
            node_for_plan(tmp.path(), 3, Shard::Metachain),
        ];
        let root = tmp.path().join("imports");
        for (name, shard_dir) in [
            ("shard-0", "Shard_0"),
            ("shard-1", "Shard_1"),
            ("shard-2", "Shard_2"),
            ("metachain", "Shard_4294967295"),
        ] {
            std::fs::create_dir_all(root.join(name).join("db/1/Epoch_0").join(shard_dir)).unwrap();
        }
        let req = ImportPlanRequest {
            source_root: root.clone(),
            no_sig_check: true,
            replace: false,
            require_elasticsearch: false,
            yes: false,
            output: None,
        };
        let sources = discover_import_sources(&root, &global()).unwrap();
        let report = build_import_plan_report(&state, &sources, &req, &global(), |_| async move {
            ActiveState::Inactive
        })
        .await;

        assert!(report.ok, "{report:?}");
        assert_eq!(report.entries.len(), 4);
        assert!(report
            .entries
            .iter()
            .all(|entry| entry.command.contains(&"--no-sig-check".to_string())));
    }

    #[test]
    fn elasticsearch_validation_requires_enabled_connector_and_url() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path().join("node-0");
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        std::fs::write(
            workdir.join("config/external.toml"),
            r#"
[ElasticSearchConnector]
Enabled = false
URL = ""
"#,
        )
        .unwrap();
        let warnings = validate_elasticsearch_external(&workdir);
        assert_eq!(warnings.len(), 2);

        std::fs::write(
            workdir.join("config/external.toml"),
            r#"
[ElasticSearchConnector]
Enabled = true
URL = "http://localhost:9200"
"#,
        )
        .unwrap();
        assert!(validate_elasticsearch_external(&workdir).is_empty());
    }

    fn node_for_plan(root: &Path, index: u16, shard: Shard) -> NodeState {
        let workdir = root.join(format!("node-{index}"));
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        std::fs::write(workdir.join("config/mxnode.toml"), "[General]\n").unwrap();
        std::fs::write(workdir.join("config/prefs.toml"), "[Preferences]\n").unwrap();
        NodeState {
            index: NodeIndex::new(index),
            role: mxnode_core::Role::Observer,
            shard,
            display_name: format!("node-{index}"),
            api_port: 8080 + index,
            unit: format!("elrond-node-{index}.service"),
            unit_override: String::new(),
            workdir,
            last_known_pubkey: String::new(),
            last_action: String::new(),
            last_action_at: None,
        }
    }
}
