//! Pre-dispatch update-check gate.
//!
//! Runs once per invocation, *before* the subcommand executes. On a
//! cache hit (TTL window) it's sub-millisecond. On a cache miss it
//! queries `releases/latest` with a 2s timeout and either prints
//! `→ mxnode vX.Y.Z available` (with a Y/N prompt when stdin is a
//! TTY) or silently skips on network failure.
//!
//! Skip rules — the gate is a no-op when ANY of these hold:
//!   - `--no-update-check` (or `MXNODE_NO_UPDATE_CHECK=1`)
//!   - `--json` (machine consumer; never prompt)
//!   - stdin or stderr isn't a TTY (CI, piped scripts, systemd-run)
//!   - `CI=true` / `GITHUB_ACTIONS=true` env (belt-and-braces)
//!   - the command is self-referential or long-running:
//!     `version`, `self-update`, `completions`, `dashboard`,
//!     `metrics`, plus `status --watch` / `logs --follow`.
//!
//! The gate writes to `[update_cache]` via `StateStore.save_file` —
//! same single-file world the rest of the binary uses.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use mxnode_config::user_config_path;
use mxnode_state::StateStore;
use mxnode_update::{check_for_update, record_decline, Decision, Policy, RemoteVersion};
use time::OffsetDateTime;

use crate::cli::{Cli, Command, GlobalArgs};

/// Owner-defined repo coordinates. Hardcoded because the binary itself
/// knows where it ships from — operators can't redirect this.
const RELEASE_ORG: &str = "XOXNO";
const RELEASE_REPO: &str = "mx-node";

/// Outcome of the gate. Only `RanSelfUpdate` short-circuits the rest
/// of dispatch; the other cases let the original subcommand proceed.
pub enum GateOutcome {
    /// Gate was skipped (flag, env, non-TTY, etc.) or the operator
    /// declined / the binary is up to date / fetch failed.
    Continue,
    /// Operator answered Y; we ran `mxnode self-update` and the rest
    /// of dispatch should not run on the old binary.
    RanSelfUpdate,
}

/// Run the gate. Honour every skip rule before doing any IO.
pub fn maybe_prompt(cli: &Cli) -> GateOutcome {
    if should_skip(&cli.global, &cli.command) {
        return GateOutcome::Continue;
    }

    let decision = match run_check(&cli.global) {
        Ok(d) => d,
        Err(_) => return GateOutcome::Continue, // best-effort
    };

    match decision {
        Decision::Skip(_) => GateOutcome::Continue,
        Decision::Prompt(remote) => handle_prompt(remote, &cli.global),
    }
}

fn should_skip(global: &GlobalArgs, command: &Command) -> bool {
    if global.no_update_check || global.json {
        return true;
    }
    if std::env::var("CI").is_ok() || std::env::var("GITHUB_ACTIONS").is_ok() {
        return true;
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return true;
    }
    matches!(
        command,
        Command::Version
            | Command::SelfUpdate(_)
            | Command::Completions(_)
            | Command::Dashboard(_)
            | Command::Metrics(_)
    ) || is_long_running_status_or_logs(command)
}

/// `status --watch` and `logs --follow` are long-running; treat them
/// like the dashboard.
fn is_long_running_status_or_logs(command: &Command) -> bool {
    match command {
        Command::Status(args) => args.watch,
        Command::Logs(args) => args.follow,
        _ => false,
    }
}

fn run_check(global: &GlobalArgs) -> Result<Decision, Box<dyn std::error::Error>> {
    let path = user_config_path()?;
    let parent = path.parent().ok_or("user_config_path has no parent")?;
    let store = StateStore::new(parent);

    // Token resolution mirrors `Runtime::github_token()` but we don't
    // build a full Runtime here — the gate runs before dispatch and a
    // load failure must never block the operator's command.
    let token = std::env::var("MXNODE_GITHUB_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            store
                .load_file()
                .ok()
                .flatten()
                .map(|f| f.secrets.github_token.as_str().to_owned())
                .filter(|s| !s.is_empty())
        });

    let policy = Policy {
        repo_org: RELEASE_ORG.to_string(),
        repo_name: RELEASE_REPO.to_string(),
        ..Policy::default()
    };
    let local = env!("CARGO_PKG_VERSION");

    // Build a tokio runtime for this single async call. We can't
    // assume an existing runtime — dispatch hasn't run yet.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _ = global; // reserved for future flag-aware tweaks
    let decision = rt.block_on(check_for_update(&store, &policy, local, token))?;
    Ok(decision)
}

fn handle_prompt(remote: RemoteVersion, _global: &GlobalArgs) -> GateOutcome {
    let local = env!("CARGO_PKG_VERSION");
    eprintln!();
    eprintln!(
        "→ mxnode {} is available (current: v{}).",
        remote.tag, local,
    );
    eprint!("  run `mxnode self-update` now? [y/N]: ");
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    let read = std::io::stdin().read_line(&mut input);
    let answer = match read {
        Ok(_) => input.trim().to_ascii_lowercase(),
        // Operator hit ctrl-D, treat as decline without persisting
        // (might just be an automation that didn't expect a prompt).
        Err(_) => return GateOutcome::Continue,
    };

    if answer == "y" || answer == "yes" {
        eprintln!("  running self-update…");
        let status = std::process::Command::new(std::env::current_exe().unwrap_or("mxnode".into()))
            .arg("self-update")
            .status();
        match status {
            Ok(s) if s.success() => GateOutcome::RanSelfUpdate,
            _ => {
                eprintln!("  self-update failed; continuing with the existing binary.");
                GateOutcome::Continue
            }
        }
    } else {
        // Persist decline so we don't re-prompt for this same tag for
        // 24h. Best-effort — failure here just means we'll prompt
        // again next time.
        let path = user_config_path().ok();
        if let Some(path) = path {
            if let Some(parent) = path.parent() {
                let store = StateStore::new(parent);
                let _ = record_decline(&store, &remote.tag, OffsetDateTime::now_utc());
            }
        }
        GateOutcome::Continue
    }
}

/// Best-effort wrapper used by the binary when CARGO_PKG_VERSION isn't
/// stable enough (e.g. dev builds with `-dirty` suffix). Currently a
/// no-op shim so callers don't pin to internal types.
#[allow(dead_code)]
pub fn fetch_timeout_default() -> Duration {
    Policy::default().fetch_timeout
}
