//! `mxnode doctor`: runtime checks beyond `config validate`.
//!
//! Validates the host's actual readiness for state-changing ops:
//!   - config + state schema parse cleanly
//!   - state and runtime directories are writable
//!   - inflight.toml is either absent or stale-with-dead-pid
//!   - systemctl + journalctl are on PATH
//!   - the supervisor unit dir is readable
//!
//! Each check produces a `Finding`; the command exits non-zero if any
//! finding has severity `Error`.

use std::path::Path;
use std::process::{Command, Stdio};

use mxnode_core::{Environment, Role};
use mxnode_state::{classify, inflight_path, Inflight, Liveness, StateStore};
use mxnode_systemd::scan_supervisor_dir;
use serde::Serialize;

use crate::cli::{DoctorArgs, DoctorFix, GlobalArgs};
use crate::errors::CliError;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

const DEFAULT_SYSTEMD_DIR: &str = "/etc/systemd/system";
const MIN_CPU_PER_NODE: usize = 4;
const MIN_MEMORY_GB_PER_NODE: u64 = 8;
const MIN_DISK_GB_PER_NODE: u64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Severity {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Serialize)]
pub(crate) struct Finding {
    pub(crate) check: &'static str,
    pub(crate) severity: Severity,
    pub(crate) summary: String,
    /// Operator-actionable next step. Empty when severity is `Ok`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) action: String,
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

pub fn run(args: DoctorArgs, global: &GlobalArgs) -> Result<(), CliError> {
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
                "run any state-changing command to auto-init, or fix the existing config file",
            ));
            None
        }
    };

    // External binaries on PATH — per-platform.
    findings.extend(check_supervisor_tools());

    if let Some(runtime) = runtime.as_ref() {
        findings.extend(check_state(runtime));
        findings.extend(check_system_requirements(runtime));
        findings.extend(check_directories(runtime));
        findings.extend(check_inflight(runtime));
        findings.extend(check_discovery(runtime));
        findings.extend(check_p2p_ports());
        findings.extend(check_journald());
    }

    let any_error = findings.iter().any(|f| f.severity == Severity::Error);
    let error_count = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();

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

    if let Some(fix) = args.fix {
        if any_error {
            return Err(CliError::new(
                "refusing to apply --fix while doctor reports errors",
                format!("{error_count} error(s) above"),
                "fix the reported errors first, then re-run with --fix",
            )
            .silent());
        }
        match fix {
            DoctorFix::Journald => {
                apply_journald_fix(global)?;
                if global.json {
                    let ack = serde_json::json!({
                        "fix": {
                            "applied": true,
                            "kind": "journald",
                            "system_max_use": mxnode_systemd::journald::DEFAULT_SYSTEM_MAX_USE,
                            "system_max_file_size": mxnode_systemd::journald::DEFAULT_SYSTEM_MAX_FILE_SIZE,
                        }
                    });
                    println!("{ack}");
                }
            }
        }
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

pub(crate) fn check_system_requirements(runtime: &Runtime) -> Vec<Finding> {
    let context = system_requirements_context(runtime);
    check_system_requirements_with_context(runtime, context)
}

pub(crate) fn planned_system_requirements_context(
    node_count: usize,
    environment: Environment,
    role: Role,
) -> SystemRequirementsContext {
    SystemRequirementsContext {
        node_count: node_count.max(1),
        environment: Some(environment),
        has_validator_role: matches!(role, Role::Validator | Role::Multikey),
    }
}

pub(crate) fn check_system_requirements_with_context(
    runtime: &Runtime,
    context: SystemRequirementsContext,
) -> Vec<Finding> {
    let nodes = context.node_count.max(1);
    let required_cpu = nodes.saturating_mul(MIN_CPU_PER_NODE);
    let required_memory_gb = (nodes as u64).saturating_mul(MIN_MEMORY_GB_PER_NODE);
    let required_disk_gb = (nodes as u64).saturating_mul(MIN_DISK_GB_PER_NODE);

    let mut findings = Vec::new();
    let available_cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    if available_cpu >= required_cpu {
        findings.push(Finding::ok(
            "requirements.cpu",
            format!(
                "{available_cpu} logical CPU(s) available for {nodes} node(s); docs minimum is {required_cpu}",
            ),
        ));
    } else {
        findings.push(Finding::err(
            "requirements.cpu",
            format!(
                "{available_cpu} logical CPU(s) available for {nodes} node(s); need at least {required_cpu}",
            ),
            "use dedicated CPU cores; shared VPS CPUs can reduce rating and lead to jailing",
        ));
    }

    match total_memory_gb() {
        Some(memory_gb) if memory_gb >= required_memory_gb => findings.push(Finding::ok(
            "requirements.memory",
            format!("{memory_gb} GB RAM detected; docs minimum is {required_memory_gb} GB"),
        )),
        Some(memory_gb) => findings.push(Finding::err(
            "requirements.memory",
            format!("{memory_gb} GB RAM detected; need at least {required_memory_gb} GB"),
            "run fewer nodes on this host or move to a larger machine",
        )),
        None => findings.push(Finding::warn(
            "requirements.memory",
            "could not determine total RAM",
            "verify manually: docs minimum is 8 GB RAM per node",
        )),
    }

    let disk_probe = nearest_existing_path(&runtime.paths.custom_home);
    match free_disk_gb(&disk_probe) {
        Some(free_gb) if free_gb >= required_disk_gb => findings.push(Finding::ok(
            "requirements.disk",
            format!(
                "{free_gb} GB free at {}; docs minimum is {required_disk_gb} GB",
                disk_probe.display()
            ),
        )),
        Some(free_gb) => findings.push(Finding::err(
            "requirements.disk",
            format!(
                "{free_gb} GB free at {}; need at least {required_disk_gb} GB",
                disk_probe.display()
            ),
            "free disk space or move paths.custom_home to a larger SSD-backed volume",
        )),
        None => findings.push(Finding::warn(
            "requirements.disk",
            format!("could not inspect free disk at {}", disk_probe.display()),
            "verify manually: docs minimum is 200 GB SSD per node",
        )),
    }

    findings.extend(check_cpu_features(&context));
    findings.extend(check_os_floor());
    findings
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SystemRequirementsContext {
    pub(crate) node_count: usize,
    pub(crate) environment: Option<Environment>,
    pub(crate) has_validator_role: bool,
}

fn system_requirements_context(runtime: &Runtime) -> SystemRequirementsContext {
    let store = StateStore::new(&runtime.paths.config_dir);
    if let Ok(Some(state)) = store.load() {
        let node_count = state.nodes.len().max(1);
        let has_validator_role = state
            .nodes
            .iter()
            .any(|n| matches!(n.role, Role::Validator | Role::Multikey));
        let environment = state.install.as_ref().map(|i| i.environment).or(runtime
            .loaded
            .file
            .network
            .environment);
        return SystemRequirementsContext {
            node_count,
            environment,
            has_validator_role,
        };
    }
    SystemRequirementsContext {
        node_count: 1,
        environment: runtime.loaded.file.network.environment,
        has_validator_role: false,
    }
}

fn check_cpu_features(context: &SystemRequirementsContext) -> Vec<Finding> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        let sse41 = std::is_x86_feature_detected!("sse4.1");
        let sse42 = std::is_x86_feature_detected!("sse4.2");
        if sse41 && sse42 {
            return vec![Finding::ok(
                "requirements.cpu-flags",
                "SSE4.1 and SSE4.2 detected",
            )];
        }
        return vec![Finding::err(
            "requirements.cpu-flags",
            format!("SSE4.1={sse41}, SSE4.2={sse42}"),
            "use an Intel/AMD CPU with SSE4.1 and SSE4.2; the node cannot sync VM 1.5+ blocks without them",
        )];
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    {
        if matches!(context.environment, Some(Environment::Mainnet)) && context.has_validator_role {
            vec![Finding::warn(
                "requirements.cpu-flags",
                "ARM detected for a mainnet validator/multikey host",
                "official docs do not recommend ARM for mainnet validators; prefer Intel/AMD for production signing",
            )]
        } else {
            vec![Finding::ok(
                "requirements.cpu-flags",
                "ARM detected; docs support ARM with genesis-sync and production-validator caveats",
            )]
        }
    }

    #[cfg(not(any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "arm"
    )))]
    {
        vec![Finding::warn(
            "requirements.cpu-flags",
            format!("unsupported CPU architecture {}", std::env::consts::ARCH),
            "verify manually against the MultiversX system requirements",
        )]
    }
}

fn check_os_floor() -> Vec<Finding> {
    use mxnode_core::Platform;
    match Platform::current() {
        Platform::Macos => vec![Finding::ok("requirements.os", "macOS supported")],
        Platform::Unsupported => vec![Finding::err(
            "requirements.os",
            format!("{} is not supported", std::env::consts::OS),
            "use Linux (Ubuntu 22.04/Debian 12 minimum) or macOS",
        )],
        Platform::Linux => match linux_os_release() {
            Some(info) if linux_release_meets_floor(&info) => vec![Finding::ok(
                "requirements.os",
                format!(
                    "{} {} meets Ubuntu 22.04/Debian 12 floor",
                    info.id,
                    info.version_id.as_deref().unwrap_or("unknown")
                ),
            )],
            Some(info) if info.id == "ubuntu" || info.id == "debian" => vec![Finding::err(
                "requirements.os",
                format!(
                    "{} {} is below the documented floor",
                    info.id,
                    info.version_id.as_deref().unwrap_or("unknown")
                ),
                "upgrade to Ubuntu 22.04+ or Debian 12+ before running production nodes",
            )],
            Some(info) => vec![Finding::warn(
                "requirements.os",
                format!(
                    "{} {} is not one of the documented baseline distros",
                    info.id,
                    info.version_id.as_deref().unwrap_or("unknown")
                ),
                "verify manually that the host is equivalent to Ubuntu 22.04/Debian 12 or newer",
            )],
            None => vec![Finding::warn(
                "requirements.os",
                "could not read /etc/os-release",
                "verify manually: Linux floor is Ubuntu 22.04 or Debian 12",
            )],
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinuxOsRelease {
    id: String,
    version_id: Option<String>,
}

fn linux_os_release() -> Option<LinuxOsRelease> {
    let body = std::fs::read_to_string("/etc/os-release").ok()?;
    parse_linux_os_release(&body)
}

fn parse_linux_os_release(body: &str) -> Option<LinuxOsRelease> {
    let mut id = None;
    let mut version_id = None;
    for line in body.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_ascii_lowercase();
        match key {
            "ID" => id = Some(value),
            "VERSION_ID" => version_id = Some(value),
            _ => {}
        }
    }
    Some(LinuxOsRelease {
        id: id?,
        version_id,
    })
}

fn linux_release_meets_floor(info: &LinuxOsRelease) -> bool {
    let major = info
        .version_id
        .as_deref()
        .and_then(|v| v.split('.').next())
        .and_then(|v| v.parse::<u32>().ok());
    match (info.id.as_str(), major) {
        ("ubuntu", Some(v)) => v >= 22,
        ("debian", Some(v)) => v >= 12,
        _ => false,
    }
}

fn total_memory_gb() -> Option<u64> {
    // SAFETY: sysconf has no memory-safety preconditions. We only read
    // positive return values and convert to bytes with saturating math.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
    if pages <= 0 || page_size <= 0 {
        return None;
    }
    Some((pages as u64).saturating_mul(page_size as u64) / 1024 / 1024 / 1024)
}

fn nearest_existing_path(path: &Path) -> std::path::PathBuf {
    let mut candidate = path;
    loop {
        if candidate.exists() {
            return candidate.to_path_buf();
        }
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => return Path::new("/").to_path_buf(),
        }
    }
}

fn free_disk_gb(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let path = path.to_string_lossy();
    let cpath = CString::new(path.as_bytes()).ok()?;
    // SAFETY: statvfs is a libc syscall that takes a CStr path and a
    // pointer to a writable struct. Path comes from a CString we own;
    // the struct is MaybeUninit::zeroed() and only read on success.
    let mut stat: MaybeUninit<libc::statvfs> = MaybeUninit::zeroed();
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize) / 1024 / 1024 / 1024)
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
                Platform::Unsupported => {
                    "configure your firewall to allow inbound 37373..38383/tcp"
                }
            };
            vec![Finding::warn(
                "p2p ports",
                format!("could not bind tcp 37373: {e}"),
                action,
            )]
        }
    }
}

/// Linux-only: detect whether mxnode's managed journald block has been
/// applied to `/etc/systemd/journald.conf`. macOS hosts don't have
/// systemd, so we short-circuit with an empty vec to keep the macOS
/// doctor pass quiet.
fn check_journald() -> Vec<Finding> {
    use mxnode_core::Platform;
    if Platform::current() != Platform::Linux {
        return Vec::new();
    }
    let path = "/etc/systemd/journald.conf";
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains("# >>> mxnode journald managed block >>>") {
        vec![Finding::ok("journald", "managed retention block present")]
    } else {
        vec![Finding::warn(
            "journald",
            "journal disk usage is uncapped — long-running nodes can fill /var/log/journal",
            format!(
                "run `mxnode doctor --fix journald` to apply SystemMaxUse={} caps",
                mxnode_systemd::journald::DEFAULT_SYSTEM_MAX_USE,
            ),
        )]
    }
}

#[cfg(target_os = "linux")]
fn apply_journald_fix(global: &GlobalArgs) -> Result<(), CliError> {
    use mxnode_systemd::journald::{
        apply_managed_block, DEFAULT_SYSTEM_MAX_FILE_SIZE, DEFAULT_SYSTEM_MAX_USE,
    };
    use std::io::Write;

    let path = "/etc/systemd/journald.conf";
    use std::io::ErrorKind;
    let existing = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(CliError::new(
                "could not read /etc/systemd/journald.conf",
                format!("{e}"),
                "check that the file is readable (re-run with sudo if needed)",
            )
            .json_if(global.json));
        }
    };
    let new_body = apply_managed_block(
        &existing,
        DEFAULT_SYSTEM_MAX_USE,
        DEFAULT_SYSTEM_MAX_FILE_SIZE,
    );
    if new_body == existing {
        // Stderr — stdout is reserved for the structured doctor output
        // (findings table or --json payload). Fix-step status messages go
        // to stderr so a `--json` consumer's parser is not corrupted.
        eprintln!("✓ journald.conf already up to date");
        return Ok(());
    }

    let mut child = Command::new("sudo")
        .args(["tee", path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| {
            CliError::new(
                "could not spawn `sudo tee`",
                format!("{e}"),
                "ensure sudo is available and the operator has write access via sudo",
            )
            .json_if(global.json)
        })?;
    let stdin = child.stdin.as_mut().ok_or_else(|| {
        CliError::new(
            "sudo tee child has no stdin",
            "Stdio::piped() did not produce a writable handle",
            "report this as an mxnode bug",
        )
        .json_if(global.json)
    })?;
    stdin.write_all(new_body.as_bytes()).map_err(|e| {
        CliError::new(
            "failed to write journald.conf via sudo tee",
            format!("{e}"),
            "check disk space and sudo permissions",
        )
        .json_if(global.json)
    })?;
    let status = child.wait().map_err(|e| {
        CliError::new(
            "failed to wait on `sudo tee`",
            format!("{e}"),
            "investigate why the child process did not exit",
        )
        .json_if(global.json)
    })?;
    if !status.success() {
        return Err(CliError::new(
            "`sudo tee` exited non-zero",
            format!("status code {:?}", status.code()),
            "verify sudo permissions and that /etc/systemd/journald.conf is writable",
        )
        .json_if(global.json));
    }

    let restart = Command::new("sudo")
        .args(["systemctl", "restart", "systemd-journald"])
        .status()
        .map_err(|e| {
            CliError::new(
                "failed to invoke `sudo systemctl`",
                format!("{e}"),
                "ensure systemctl is on PATH",
            )
            .json_if(global.json)
        })?;
    if !restart.success() {
        return Err(CliError::new(
            "`sudo systemctl restart systemd-journald` exited non-zero",
            format!("status code {:?}", restart.code()),
            "check systemctl status systemd-journald",
        )
        .json_if(global.json));
    }

    eprintln!(
        "✓ journald capped (SystemMaxUse={}, SystemMaxFileSize={}); journald restarted",
        DEFAULT_SYSTEM_MAX_USE, DEFAULT_SYSTEM_MAX_FILE_SIZE,
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_journald_fix(global: &GlobalArgs) -> Result<(), CliError> {
    Err(CliError::new(
        "--fix journald is Linux-only",
        "journald is part of systemd, which is not present on this OS",
        "no action needed; this platform does not need journald capping",
    )
    .json_if(global.json))
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
    let store = StateStore::new(&runtime.paths.config_dir);
    match store.load() {
        Ok(Some(state)) => {
            out.push(Finding::ok(
                "state",
                format!("mxnode.toml schema_version={}", state.schema_version),
            ));
        }
        Ok(None) => {
            out.push(Finding::warn(
                "state",
                "no mxnode.toml on this host",
                "run `mxnode install` to set up nodes",
            ));
        }
        Err(e) => {
            out.push(Finding::err(
                "state",
                format!("could not parse mxnode.toml: {e}"),
                "either fix the file manually or remove it and run hand-edit and re-run",
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
            out.push(Finding::ok(
                label,
                format!("{} (will be created on demand)", dir.display()),
            ));
        }
    }
    out
}

fn dir_is_writable(dir: &Path) -> bool {
    // Try to create a tempfile inside; remove it on drop. We don't rely on
    // metadata-mode bits because they're not authoritative on macOS APFS.
    tempfile::Builder::new()
        .prefix(".mxnode-doctor-write-probe.")
        .tempfile_in(dir)
        .is_ok()
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
                "remove the file and rerun the failed op",
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
            "delete inflight.toml manually to clear",
        )],
        Liveness::Unknown => vec![Finding::warn(
            "inflight",
            "could not determine liveness of recorded pid",
            "be conservative: only delete inflight.toml after confirming no mxnode process is alive",
        )],
    }
}

fn check_discovery(_runtime: &Runtime) -> Vec<Finding> {
    use crate::orchestrator::supervisor::unit_dir_for_platform;
    use mxnode_core::Platform;
    let supervisor_dir = unit_dir_for_platform(Platform::current())
        .unwrap_or_else(|| Path::new(DEFAULT_SYSTEMD_DIR).to_path_buf());
    match scan_supervisor_dir(&supervisor_dir) {
        Ok(_) => vec![Finding::ok(
            "supervisor-dir",
            format!("readable: {}", supervisor_dir.display()),
        )],
        Err(_) => vec![Finding::warn(
            "supervisor-dir",
            format!("could not read {}", supervisor_dir.display()),
            match Platform::current() {
                Platform::Linux => "run as root or with read access on /etc/systemd/system",
                Platform::Macos => "ensure ~/Library/LaunchAgents is readable by the current user",
                Platform::Unsupported => "this platform is not yet supported",
            },
        )],
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journald_managed_block_round_trip_is_noop() {
        // The C1 helper is itself idempotent; this pins that the doctor's
        // sentinel substring matches what `apply_managed_block` actually
        // emits, so the WARN-vs-OK branch in `check_journald` keys off
        // the same string the fix writes.
        let configured = mxnode_systemd::journald::apply_managed_block("", "4000M", "800M");
        assert!(configured.contains("# >>> mxnode journald managed block >>>"));
    }

    #[test]
    fn parses_linux_os_release_values_with_quotes() {
        let parsed = parse_linux_os_release(
            r#"
NAME="Ubuntu"
ID=ubuntu
VERSION_ID="22.04"
"#,
        )
        .unwrap();
        assert_eq!(
            parsed,
            LinuxOsRelease {
                id: "ubuntu".to_string(),
                version_id: Some("22.04".to_string()),
            }
        );
        assert!(linux_release_meets_floor(&parsed));
    }

    #[test]
    fn linux_release_floor_rejects_old_ubuntu_and_accepts_debian_12() {
        assert!(!linux_release_meets_floor(&LinuxOsRelease {
            id: "ubuntu".to_string(),
            version_id: Some("20.04".to_string()),
        }));
        assert!(linux_release_meets_floor(&LinuxOsRelease {
            id: "debian".to_string(),
            version_id: Some("12".to_string()),
        }));
    }

    #[test]
    fn nearest_existing_path_walks_up_to_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing/deep/path");
        assert_eq!(nearest_existing_path(&missing), tmp.path());
    }
}
