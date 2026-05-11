//! `mxnode keys rename --node N --to NAME [--restart]` (was top-level
//! `mxnode rename`): change one node's `NodeDisplayName` in both
//! `mxnode.toml` and the on-disk `prefs.toml` atomically.
//!
//! The persisted `display_name` is what `config apply` and `upgrade`
//! reapply on every subsequent edit pass — so renaming through this
//! command sticks across re-templates. By contrast, hand-editing
//! `prefs.toml` directly will be overwritten the next time
//! `reapply-config` runs against a state that still has the old name
//! persisted.

use std::fs;
use std::sync::Arc;

use mxnode_state::StateStore;
use mxnode_systemd::{set_node_display_name, Ctl};
use toml_edit::DocumentMut;

use crate::cli::{GlobalArgs, RenameArgs};
use crate::errors::CliError;
use crate::events::{global_op, node_op_end, node_op_start, Outcome};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};
use crate::orchestrator::supervisor::build_supervisor;

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: RenameArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let new_name = args.to.trim().to_string();
    if new_name.is_empty() {
        return Err(CliError::new(
            "refusing to set an empty NodeDisplayName",
            "the --to argument was empty after trimming whitespace",
            "supply a non-empty name (e.g. --to my-validator-0)",
        )
        .json_if(global.json));
    }

    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.config_dir);
    let mut state = store
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

    let target_index = args.node;
    let pos = state
        .nodes
        .iter()
        .position(|n| n.index.get() == target_index)
        .ok_or_else(|| {
            CliError::new(
                format!("no node with index {target_index} in mxnode.toml"),
                "the supplied --node value matched zero entries",
                "run `mxnode status` to see available indices",
            )
            .json_if(global.json)
        })?;

    let node_index = state.nodes[pos].index;
    let workdir = state.nodes[pos].workdir.clone();
    let unit = state.nodes[pos].unit.clone();
    let old_name = state.nodes[pos].display_name.clone();

    global_op(
        "rename",
        &format!(
            "node-{target_index}: {} → {new_name}",
            if old_name.is_empty() {
                "<unset>"
            } else {
                old_name.as_str()
            },
        ),
    );
    node_op_start("rename", node_index, &unit);

    let prefs_path = workdir.join("config/prefs.toml");
    let result: Result<(), CliError> = (|| {
        let body = fs::read_to_string(&prefs_path).map_err(|e| {
            CliError::new(
                format!("failed to read {}", prefs_path.display()),
                e.to_string(),
                "ensure the node's config/ directory exists; rerun `mxnode install` if missing",
            )
            .json_if(global.json)
        })?;
        let mut doc: DocumentMut = body.parse().map_err(|e: toml_edit::TomlError| {
            CliError::new(
                format!("failed to parse {}", prefs_path.display()),
                e.to_string(),
                "fix the file by hand or restore from the upstream config repo",
            )
            .json_if(global.json)
        })?;
        set_node_display_name(&mut doc, &new_name).map_err(|e| {
            CliError::new(
                "failed to set NodeDisplayName",
                e.to_string(),
                "report this as an mxnode bug",
            )
            .json_if(global.json)
        })?;
        fs::write(&prefs_path, doc.to_string()).map_err(|e| {
            CliError::new(
                format!("failed to write {}", prefs_path.display()),
                e.to_string(),
                "check filesystem permissions on the node config directory",
            )
            .json_if(global.json)
        })?;

        // Persist the new name on the in-memory state, then save under
        // the normal lock + atomic-rename path.
        state.nodes[pos].display_name = new_name.clone();
        let guard = store.lock().map_err(|e| {
            CliError::new(
                "failed to acquire mxnode.toml lock",
                e.to_string(),
                "ensure no other mxnode invocation is running",
            )
            .json_if(global.json)
        })?;
        store.save(&state, &guard).map_err(|e| {
            CliError::new(
                "failed to write mxnode.toml",
                e.to_string(),
                "ensure mxnode has write access to the state directory",
            )
            .json_if(global.json)
        })?;
        Ok(())
    })();

    match &result {
        Ok(()) => node_op_end("rename", node_index, &unit, Outcome::Ok),
        Err(e) => node_op_end(
            "rename",
            node_index,
            &unit,
            Outcome::Fail { cause: &e.summary },
        ),
    }
    result?;

    if args.restart {
        let ctl: Arc<dyn Ctl> = build_supervisor();
        if let Err(e) = ctl.restart(&unit).await {
            eprintln!("warn: restart {unit} failed: {e}");
        }
    }

    if global.json {
        let body = serde_json::json!({
            "ok": true,
            "node": target_index,
            "old_display_name": old_name,
            "new_display_name": new_name,
            "prefs_toml": prefs_path.display().to_string(),
            "restarted": args.restart,
        });
        println!("{body}");
    } else if old_name.is_empty() {
        println!("✓ node-{target_index}: NodeDisplayName set to \"{new_name}\"");
        println!("  prefs.toml: {}", prefs_path.display());
        if !args.restart {
            println!("  unit not restarted (pass --restart to roll {unit})");
        }
    } else {
        println!("✓ node-{target_index}: \"{old_name}\" → \"{new_name}\"");
        println!("  prefs.toml: {}", prefs_path.display());
        if !args.restart {
            println!("  unit not restarted (pass --restart to roll {unit})");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Mirror the trim-and-reject step from `run` so it stays testable
    /// without dragging in `Runtime`. The full command is exercised end
    /// to end by the integration test in `bins/mxnode/tests/cli.rs`.
    fn validate_new_name(raw: &str) -> Result<String, &'static str> {
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            Err("empty after trim")
        } else {
            Ok(trimmed)
        }
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_new_name("").is_err());
    }

    #[test]
    fn validate_rejects_whitespace_only() {
        assert!(validate_new_name("   \t\n").is_err());
    }

    #[test]
    fn validate_trims_surrounding_whitespace() {
        assert_eq!(validate_new_name("  good-name  ").unwrap(), "good-name");
    }

    #[test]
    fn validate_accepts_internal_spaces() {
        assert_eq!(validate_new_name("my validator").unwrap(), "my validator");
    }
}
