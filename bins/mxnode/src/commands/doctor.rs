//! `mxnode doctor`: runtime checks beyond `config validate`.
//!
//! Validates the host's actual readiness for state-changing ops:
//!   - config + state schema parse cleanly
//!   - state and runtime directories are writable
//!   - inflight.toml is either absent or stale-with-dead-pid
//!   - systemctl + journalctl are on PATH
//!   - discovery sees no drift / drop-ins beyond what state.toml records
//!
//! Each check produces a `Finding`; the command exits non-zero if any
//! finding has severity `Error`.

use std::path::Path;
use std::process::{Command, Stdio};

use mxnode_state::{classify, inflight_path, Inflight, Liveness, StateStore};
use mxnode_systemd::scan_supervisor_dir;
use serde::Serialize;

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::orchestrator::adopt::{analyze, AdoptInputs};
use crate::orchestrator::runtime::Runtime;

const DEFAULT_SYSTEMD_DIR: &str = "/etc/systemd/system";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Serialize)]
struct Finding {
    check: &'static str,
    severity: Severity,
    summary: String,
    /// Operator-actionable next step. Empty when severity is `Ok`.
    #[serde(skip_serializing_if = "String::is_empty")]
    action: String,
}

impl Finding {
    fn ok(check: &'static str, summary: impl Into<String>) -> Self {
        Self {
            check,
            severity: Severity::Ok,
            summary: summary.into(),
            action: String::new(),
        }
    }
    fn warn(check: &'static str, summary: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            check,
            severity: Severity::Warn,
            summary: summary.into(),
            action: action.into(),
        }
    }
    fn err(check: &'static str, summary: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            check,
            severity: Severity::Error,
            summary: summary.into(),
            action: action.into(),
        }
    }
}

pub fn run(global: &GlobalArgs) -> Result<(), CliError> {
    let mut findings: Vec<Finding> = Vec::new();

    // Config + path resolution. We surface the loader error as a finding
    // rather than an early CliError so doctor reports as much as it can in
    // one pass, even when config is missing.
    let runtime_result = Runtime::from_global(global);
    let runtime = match runtime_result {
        Ok(r) => {
            findings.push(Finding::ok("config", "loaded successfully"));
            Some(r)
        }
        Err(_) => {
            findings.push(Finding::err(
                "config",
                "could not load config",
                "run `mxnode init` to create ~/.config/mxnode/config.toml, or fix the existing file",
            ));
            None
        }
    };

    // External binaries on PATH — per-platform.
    findings.extend(check_supervisor_tools());

    if let Some(runtime) = runtime.as_ref() {
        findings.extend(check_state(runtime));
        findings.extend(check_directories(runtime));
        findings.extend(check_inflight(runtime));
        findings.extend(check_discovery(runtime));
        findings.extend(check_p2p_ports());
    }

    let any_error = findings.iter().any(|f| f.severity == Severity::Error);
    let error_count = findings.iter().filter(|f| f.severity == Severity::Error).count();

    if global.json {
        // Emit the unified JSON payload once. The `error` block is only
        // present when there's something to report; consumers can rely on
        // its presence/absence as a binary success signal.
        let mut payload = serde_json::json!({
            "ok": !any_error,
            "findings": findings,
        });
        if any_error {
            payload["error"] = serde_json::json!({
                "summary": "doctor reported errors",
                "cause": format!("{error_count} error(s)"),
                "try": "address the items marked `error` above",
            });
        }
        println!("{payload}");
    } else {
        print_findings(&findings);
    }

    if any_error {
        // We already emitted the structured output (JSON or human). Mark
        // the error as silent so `report_error` doesn't add a duplicate
        // blob to stdout — only the non-zero exit code matters now.
        return Err(CliError::new(
            "doctor reported errors",
            format!("{error_count} error(s)"),
            "address the items marked `error` above",
        )
        .silent());
    }
    Ok(())
}

fn check_supervisor_tools() -> Vec<Finding> {
    use mxnode_core::Platform;
    let mut findings = Vec::new();
    findings.push(Finding::ok(
        "platform",
        format!(
            "{} ({})",
            Platform::current().label(),
            Platform::current().supervisor_label(),
        ),
    ));
    match Platform::current() {
        Platform::Linux => {
            findings.push(check_command("systemctl", &["--version"]));
            findings.push(check_command("journalctl", &["--version"]));
        }
        Platform::Macos => {
            findings.push(check_command("launchctl", &["version"]));
        }
        Platform::Unsupported => {
            findings.push(Finding::err(
                "platform",
                "this OS is not supported by mxnode",
                "mxnode currently supports Linux (systemd) and macOS (launchd)",
            ));
        }
    }
    findings
}

/// Best-effort probe of the MultiversX p2p port range (37373–38383/tcp).
/// We don't open a long-running listener; we just try `bind` and release
/// immediately to confirm the kernel allows it. Failure usually means
/// the firewall blocks inbound — emit a platform-specific hint.
fn check_p2p_ports() -> Vec<Finding> {
    use mxnode_core::Platform;
    use std::net::TcpListener;
    // Probe the bottom of the range; the rest follow the same firewall
    // rule on every operator stack we've seen.
    let probe_port = 37373;
    match TcpListener::bind(("0.0.0.0", probe_port)) {
        Ok(listener) => {
            drop(listener);
            vec![Finding::ok(
                "p2p ports",
                format!(
                    "tcp {}..38383 bindable on 0.0.0.0 (firewall + UPnP must still permit inbound)",
                    probe_port,
                ),
            )]
        }
        Err(e) => {
            let action = match Platform::current() {
                Platform::Linux => {
                    "open the range with `sudo ufw allow 37373:38383/tcp` (Ubuntu) \
                     or the firewalld/iptables equivalent for your distro"
                }
                Platform::Macos => {
                    "System Settings → Network → Firewall → allow incoming for the node binary, \
                     OR `sudo pfctl` rules; macOS port-binding errors usually mean another \
                     process holds the port, not a firewall block"
                }
                Platform::Unsupported => "configure your firewall to allow inbound 37373..38383/tcp",
            };
            vec![Finding::warn(
                "p2p ports",
                format!("could not bind tcp 37373: {e}"),
                action,
            )]
        }
    }
}

fn check_command(bin: &'static str, args: &[&str]) -> Finding {
    let probe = Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status();
    match probe {
        Ok(status) if status.success() => Finding::ok(bin, format!("{bin} is on PATH")),
        Ok(status) => Finding::warn(
            bin,
            format!("{bin} exited {:?}", status.code()),
            format!("ensure {bin} works: try `{bin} {}`", args.join(" ")),
        ),
        Err(e) => Finding::err(
            bin,
            format!("could not run {bin}: {e}"),
            format!("install {bin}; mxnode shells out to it for state-changing ops"),
        ),
    }
}

fn check_state(runtime: &Runtime) -> Vec<Finding> {
    let mut out = Vec::new();
    let store = StateStore::new(&runtime.paths.state);
    match store.load() {
        Ok(Some(state)) => {
            out.push(Finding::ok(
                "state",
                format!("state.toml schema_version={}", state.schema_version),
            ));
        }
        Ok(None) => {
            out.push(Finding::warn(
                "state",
                "no state.toml on this host",
                "run `mxnode adopt` to populate state from existing units",
            ));
        }
        Err(e) => {
            out.push(Finding::err(
                "state",
                format!("could not parse state.toml: {e}"),
                "either fix the file manually or remove it and run `mxnode rebuild-state`",
            ));
        }
    }
    out
}

fn check_directories(runtime: &Runtime) -> Vec<Finding> {
    let mut out = Vec::new();
    for (label, dir) in [
        ("paths.state", &runtime.paths.state),
        ("paths.runtime", &runtime.paths.runtime),
        ("paths.binaries", &runtime.paths.binaries),
    ] {
        if dir.exists() {
            if dir_is_writable(dir) {
                out.push(Finding::ok(label, format!("{} is writable", dir.display())));
            } else {
                out.push(Finding::warn(
                    label,
                    format!("{} exists but is not writable", dir.display()),
                    "fix permissions; mxnode writes state and binaries here",
                ));
            }
        } else {
            // Non-existence is fine — the orchestrator creates dirs on demand.
            out.push(Finding::ok(label, format!("{} (will be created on demand)", dir.display())));
        }
    }
    out
}

fn dir_is_writable(dir: &Path) -> bool {
    // Try to create a tempfile inside; remove it on drop. We don't rely on
    // metadata-mode bits because they're not authoritative on macOS APFS.
    match tempfile::Builder::new()
        .prefix(".mxnode-doctor-write-probe.")
        .tempfile_in(dir)
    {
        Ok(_) => true,
        Err(_) => false,
    }
}

fn check_inflight(runtime: &Runtime) -> Vec<Finding> {
    let path = inflight_path(&runtime.paths.state);
    let inflight = match Inflight::load(&path) {
        Ok(Some(i)) => i,
        Ok(None) => return vec![Finding::ok("inflight", "no inflight.toml")],
        Err(e) => {
            return vec![Finding::err(
                "inflight",
                format!("could not parse inflight.toml: {e}"),
                "remove the file and rerun the failed op, or `mxnode unlock --force`",
            )]
        }
    };
    match classify(&inflight.identity) {
        Liveness::Live => vec![Finding::warn(
            "inflight",
            format!("pid {} is still running an op", inflight.identity.pid),
            "wait for it to complete; do not start another mxnode invocation",
        )],
        Liveness::Stale => vec![Finding::warn(
            "inflight",
            "inflight.toml left over from a crashed run",
            "run `mxnode unlock --force` to clear, then `--resume` or `--abandon` the op",
        )],
        Liveness::Unknown => vec![Finding::warn(
            "inflight",
            "could not determine liveness of recorded pid",
            "be conservative: run `mxnode unlock --force` only after confirming no mxnode process is alive",
        )],
    }
}

fn check_discovery(runtime: &Runtime) -> Vec<Finding> {
    use crate::orchestrator::supervisor::unit_dir_for_platform;
    use mxnode_core::Platform;
    let supervisor_dir = unit_dir_for_platform(Platform::current())
        .unwrap_or_else(|| Path::new(DEFAULT_SYSTEMD_DIR).to_path_buf());
    if scan_supervisor_dir(&supervisor_dir).is_err() {
        return vec![Finding::warn(
            "discovery",
            format!("could not read {}", supervisor_dir.display()),
            match Platform::current() {
                Platform::Linux => "run as root or with read access on /etc/systemd/system",
                Platform::Macos => "ensure ~/Library/LaunchAgents is readable by the current user",
                Platform::Unsupported => "this platform is not yet supported",
            },
        )];
    }
    let environment = match runtime.loaded.config.network.environment {
        Some(e) => e,
        None => {
            return vec![Finding::warn(
                "discovery",
                "skipping drift check (network.environment is unset)",
                "run `mxnode init` to set the network",
            )];
        }
    };
    let outcome = match analyze(
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
        "mxnode/doctor",
    ) {
        Ok(o) => o,
        Err(e) => {
            return vec![Finding::warn(
                "discovery",
                format!("could not analyze units: {e}"),
                "ensure /etc/systemd/system is readable",
            )]
        }
    };
    if outcome.is_clean() {
        return vec![Finding::ok("discovery", "no drift across discovered units")];
    }
    let drift = outcome.drift_reports().count();
    let drop_ins = outcome.has_drop_ins();
    let mut out = Vec::new();
    if drop_ins {
        out.push(Finding::warn(
            "drop-ins",
            "drop-in `.conf` files detected alongside elrond-* units",
            "mxnode does not author or merge drop-ins; review them manually before adopting",
        ));
    }
    if drift > 0 {
        out.push(Finding::warn(
            "drift",
            format!("{drift} unit(s) differ from what mxnode would render"),
            "run `mxnode adopt --force-adopt` to preserve them, or `mxnode rebuild-state` to refresh",
        ));
    }
    out
}

fn print_findings(findings: &[Finding]) {
    for f in findings {
        let glyph = match f.severity {
            Severity::Ok => "✓",
            Severity::Warn => "!",
            Severity::Error => "✗",
        };
        println!("{glyph} [{}] {}", f.check, f.summary);
        if !f.action.is_empty() {
            println!("    → {}", f.action);
        }
    }
}
