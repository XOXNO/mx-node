//! `mxnode keys check` and `mxnode keygen`.
//!
//! `keys check` reports whether `node-{INDEX}.zip` is present for every node
//! in `state.toml`. `keygen` shells out to the keygenerator binary that
//! `mxnode install` (Phase 3) puts under `$CUSTOM_HOME/elrond-utils/`. We
//! refuse cleanly if the binary isn't there yet.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use mxnode_core::Role;
use mxnode_state::StateStore;
use serde::Serialize;

use crate::cli::{GlobalArgs, KeygenArgs, KeysCommand};
use crate::errors::CliError;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

pub fn run_keys(cmd: KeysCommand, global: &GlobalArgs) -> Result<(), CliError> {
    match cmd {
        KeysCommand::Check => check(global),
    }
}

pub fn run_keygen(args: KeygenArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let binary = runtime
        .paths
        .elrond_utils_root()
        .join("keygenerator");
    if !binary.exists() {
        return Err(CliError::new(
            "keygenerator binary is not installed",
            format!("expected {}", binary.display()),
            "run `mxnode install` (Phase 3) first, or place the keygenerator binary manually under elrond-utils/",
        )
        .json_if(global.json));
    }

    let output_dir = args
        .output
        .unwrap_or_else(|| runtime.paths.elrond_utils_root());
    std::fs::create_dir_all(&output_dir).map_err(|e| {
        CliError::new(
            "failed to create output directory",
            format!("{}: {e}", output_dir.display()),
            "ensure the parent is writable",
        )
        .json_if(global.json)
    })?;

    let status = Command::new(&binary)
        .current_dir(&output_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            CliError::new(
                "failed to invoke keygenerator",
                format!("{}: {e}", binary.display()),
                "ensure the keygenerator binary has execute permissions",
            )
            .json_if(global.json)
        })?;
    if !status.success() {
        return Err(CliError::new(
            "keygenerator exited non-zero",
            format!("status code {:?}", status.code()),
            "inspect stdout/stderr above; the .pem files are written to the working directory",
        )
        .json_if(global.json));
    }

    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "binary": binary.display().to_string(),
                "output_dir": output_dir.display().to_string(),
                "for_node": args.r#for,
            })
        );
    } else {
        println!("keygenerator wrote .pem files to {}", output_dir.display());
        if let Some(idx) = args.r#for {
            println!("  use them for node-{idx} (mxnode install will pick them up automatically)");
        }
    }
    Ok(())
}

fn check(global: &GlobalArgs) -> Result<(), CliError> {
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
                "run `mxnode install` first",
            )
            .json_if(global.json)
        })?;

    let key_dir = &runtime.paths.node_keys;
    // Observer and multikey nodes generate their keys via keygenerator
    // at install time (and the .pem files are stamped directly into
    // each node's workdir/config). Validator nodes are the only ones
    // that need an operator-supplied `node-{i}.zip`. Filter the check
    // surface accordingly so observer-only installs don't surface
    // spurious "missing" errors.
    let mut entries: Vec<KeyEntry> = Vec::with_capacity(state.nodes.len());
    let mut skipped: Vec<u16> = Vec::new();
    for node in &state.nodes {
        if !matches!(node.role, Role::Validator) {
            skipped.push(node.index.get());
            continue;
        }
        let zip_name = format!("node-{}.zip", node.index.get());
        let zip_path = key_dir.join(&zip_name);
        let present = zip_path.exists();
        entries.push(KeyEntry {
            index: node.index.get(),
            zip_path: zip_path.clone(),
            present,
        });
    }

    let missing = entries.iter().filter(|e| !e.present).count();
    if global.json {
        let payload = KeyReport {
            key_dir: key_dir.display().to_string(),
            entries: entries.iter().map(KeyEntryView::from).collect(),
            missing,
            skipped_non_validator_nodes: skipped.clone(),
        };
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else if entries.is_empty() {
        println!(
            "no validator nodes in state.toml — observer/multikey installs use auto-generated keys",
        );
    } else {
        println!("key dir: {}", key_dir.display());
        for e in &entries {
            let glyph = if e.present { "✓" } else { "✗" };
            println!(
                "  {glyph} node-{}: {} ({})",
                e.index,
                e.zip_path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                if e.present { "present" } else { "missing" },
            );
        }
        if !skipped.is_empty() {
            println!(
                "  (skipped {n} observer/multikey node(s); keys are auto-generated)",
                n = skipped.len(),
            );
        }
        if missing > 0 {
            println!(
                "\n{missing} node(s) have no zip yet — drop them in {} before running `mxnode start`.",
                key_dir.display(),
            );
        }
    }

    if missing > 0 {
        return Err(CliError::new(
            format!("{missing} node(s) missing key archive"),
            format!("expected node-N.zip in {}", key_dir.display()),
            "place the missing archives, then re-run `mxnode keys check`",
        )
        .silent());
    }
    Ok(())
}

#[derive(Debug)]
struct KeyEntry {
    index: u16,
    zip_path: PathBuf,
    present: bool,
}

#[derive(Debug, Serialize)]
struct KeyEntryView {
    index: u16,
    zip_path: String,
    present: bool,
}

impl From<&KeyEntry> for KeyEntryView {
    fn from(e: &KeyEntry) -> Self {
        Self {
            index: e.index,
            zip_path: e.zip_path.display().to_string(),
            present: e.present,
        }
    }
}

#[derive(Debug, Serialize)]
struct KeyReport {
    key_dir: String,
    entries: Vec<KeyEntryView>,
    missing: usize,
    /// Indices of observer/multikey nodes the check skipped — those
    /// roles don't consume operator-supplied zips.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped_non_validator_nodes: Vec<u16>,
}
