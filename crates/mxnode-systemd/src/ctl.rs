//! Thin wrappers around `systemctl` for the lifecycle commands.
//!
//! v0.1 shells out to `systemctl` rather than talking dbus directly: simpler
//! deps, more debuggable, and `systemctl` is universally on PATH wherever
//! systemd is. The audit flagged `zbus` as a Phase 2+ optimisation; we keep
//! that door open by hiding the implementation behind the [`Ctl`] trait so
//! callers can swap in a different backend later.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum CtlError {
    #[error("failed to spawn systemctl: {0}")]
    Spawn(#[source] std::io::Error),

    #[error("systemctl exited {code}: {stderr}")]
    NonZero { code: i32, stderr: String },

    #[error("systemctl exited via signal")]
    Signaled,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of `systemctl is-active <unit>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveState {
    Active,
    Inactive,
    Failed,
    Activating,
    Deactivating,
    Unknown,
}

impl ActiveState {
    pub fn from_string(s: &str) -> Self {
        match s.trim() {
            "active" => Self::Active,
            "inactive" => Self::Inactive,
            "failed" => Self::Failed,
            "activating" => Self::Activating,
            "deactivating" => Self::Deactivating,
            _ => Self::Unknown,
        }
    }
}

/// Backend abstraction so command modules don't have to know whether we
/// shell out or talk dbus. The trait is async-friendly because the daemon
/// (Phase 2+) will need long-running interactions with systemd.
#[async_trait::async_trait]
pub trait Ctl: Send + Sync {
    async fn start(&self, unit: &str) -> Result<(), CtlError>;
    async fn stop(&self, unit: &str) -> Result<(), CtlError>;
    async fn restart(&self, unit: &str) -> Result<(), CtlError>;
    async fn is_active(&self, unit: &str) -> Result<ActiveState, CtlError>;
    /// Read a single property via `systemctl show -p <prop>`. Returns the
    /// trimmed value (e.g. `ActiveState=active` → `"active"`).
    async fn show_property(&self, unit: &str, property: &str) -> Result<String, CtlError>;
    /// `systemctl daemon-reload`. Must run after any unit file is written
    /// to or removed from `/etc/systemd/system` so the manager doesn't act
    /// on a stale in-memory view.
    async fn daemon_reload(&self) -> Result<(), CtlError>;
    /// `systemctl enable <unit>`.
    async fn enable(&self, unit: &str) -> Result<(), CtlError>;
    /// `systemctl disable <unit>`. Idempotent: a unit that was never
    /// enabled is treated as success, not an error.
    async fn disable(&self, unit: &str) -> Result<(), CtlError>;
    /// `systemctl reset-failed` (all units). Clears lingering failed state
    /// after stopping/removing units.
    async fn reset_failed(&self) -> Result<(), CtlError>;
    /// Place a rendered unit file at `dest` (privileged on Linux:
    /// `/etc/systemd/system` is root-owned; unprivileged on macOS).
    async fn install_unit_file(&self, src: &Path, dest: &Path) -> Result<(), CtlError>;
    /// Remove a file at a privileged path (unit file under
    /// `/etc/systemd/system`). Missing file is success.
    async fn remove_file(&self, path: &Path) -> Result<(), CtlError>;
}

/// Default `Ctl` backed by the host's `systemctl` binary, prefixed with
/// `sudo` because every state-changing op (`start`/`stop`/`restart`)
/// requires it on a default Ubuntu install.
///
/// Read-only ops (`is-active`, `show`) skip sudo so unprivileged status
/// queries don't trigger a password prompt — matches what the bash does.
pub struct SystemctlCtl {
    sudo: bool,
}

impl SystemctlCtl {
    /// Defaults to `sudo systemctl` for state-changing ops. Pass
    /// `with_sudo(false)` for environments where the operator already runs
    /// as root or has a separate privilege-escalation strategy.
    pub fn new() -> Self {
        Self { sudo: true }
    }

    pub fn with_sudo(mut self, sudo: bool) -> Self {
        self.sudo = sudo;
        self
    }

    /// Build a `Command` for `program`, prefixed with `sudo
    /// --non-interactive` when `privileged` and `self.sudo`. The single
    /// place the sudo-prefix decision lives, shared by mutations,
    /// privileged file ops, and (non-privileged) reads.
    fn command(&self, privileged: bool, program: &str) -> Command {
        if privileged && self.sudo {
            let mut c = Command::new("sudo");
            c.arg("--non-interactive").arg(program);
            c
        } else {
            Command::new(program)
        }
    }

    async fn run_mutation(&self, args: &[&str]) -> Result<(), CtlError> {
        let mut cmd = self.command(true, "systemctl");
        for a in args {
            cmd.arg(a);
        }
        classify_output(self.capture(cmd).await?)
    }

    async fn run_read(&self, args: &[&str]) -> Result<String, CtlError> {
        let mut cmd = self.command(false, "systemctl");
        for a in args {
            cmd.arg(a);
        }
        let output = self.capture(cmd).await?;
        // `systemctl is-active` exits non-zero for inactive/failed units;
        // we still want the stdout text so the caller can classify. Treat
        // exit codes as informational here — only spawn errors propagate.
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn run_privileged(&self, program: &str, args: &[&OsStr]) -> Result<(), CtlError> {
        let mut cmd = self.command(true, program);
        for a in args {
            cmd.arg(a);
        }
        classify_output(self.capture(cmd).await?)
    }

    /// Wire up the standard stdio (null stdin, captured stdout/stderr) and
    /// run the command to completion. Spawn failures map to [`CtlError::Spawn`].
    async fn capture(&self, mut cmd: Command) -> Result<std::process::Output, CtlError> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output().await.map_err(CtlError::Spawn)
    }
}

/// Map a finished process's exit status onto the success/[`CtlError`]
/// contract shared by every mutating command (`systemctl` verbs and the
/// privileged `mv`/`rm` file ops).
fn classify_output(output: std::process::Output) -> Result<(), CtlError> {
    if output.status.success() {
        return Ok(());
    }
    match output.status.code() {
        Some(code) => Err(CtlError::NonZero {
            code,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }),
        None => Err(CtlError::Signaled),
    }
}

impl Default for SystemctlCtl {
    fn default() -> Self {
        Self::new()
    }
}

/// True when a `systemctl disable` failure means "there was nothing to
/// disable" (the unit was never enabled, or its file is already gone) —
/// which uninstall treats as success.
fn is_not_enabled(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("not enabled") || s.contains("does not exist") || s.contains("no such file")
}

#[async_trait::async_trait]
impl Ctl for SystemctlCtl {
    async fn start(&self, unit: &str) -> Result<(), CtlError> {
        self.run_mutation(&["start", unit]).await
    }
    async fn stop(&self, unit: &str) -> Result<(), CtlError> {
        self.run_mutation(&["stop", unit]).await
    }
    async fn restart(&self, unit: &str) -> Result<(), CtlError> {
        self.run_mutation(&["restart", unit]).await
    }
    async fn is_active(&self, unit: &str) -> Result<ActiveState, CtlError> {
        let raw = self.run_read(&["is-active", unit]).await?;
        Ok(ActiveState::from_string(&raw))
    }
    async fn show_property(&self, unit: &str, property: &str) -> Result<String, CtlError> {
        let arg = format!("--property={property}");
        let raw = self.run_read(&["show", &arg, unit]).await?;
        // Output shape: `Property=Value` — return just the value.
        match raw.split_once('=') {
            Some((_, v)) => Ok(v.trim().to_string()),
            None => Ok(raw),
        }
    }
    async fn daemon_reload(&self) -> Result<(), CtlError> {
        self.run_mutation(&["daemon-reload"]).await
    }
    async fn enable(&self, unit: &str) -> Result<(), CtlError> {
        self.run_mutation(&["enable", unit]).await
    }
    async fn disable(&self, unit: &str) -> Result<(), CtlError> {
        match self.run_mutation(&["disable", unit]).await {
            Err(CtlError::NonZero { stderr, .. }) if is_not_enabled(&stderr) => Ok(()),
            other => other,
        }
    }
    async fn reset_failed(&self) -> Result<(), CtlError> {
        self.run_mutation(&["reset-failed"]).await
    }
    async fn install_unit_file(&self, src: &Path, dest: &Path) -> Result<(), CtlError> {
        self.run_privileged("mv", &[src.as_os_str(), dest.as_os_str()])
            .await
    }
    async fn remove_file(&self, path: &Path) -> Result<(), CtlError> {
        self.run_privileged("rm", &[OsStr::new("-f"), path.as_os_str()])
            .await
    }
}

/// `Ctl` implementation backed by macOS `launchctl`. Maps the systemd
/// verbs onto the launchd domain model:
///
///   - `start` → `launchctl bootstrap gui/<uid> <plist>` (idempotent
///     load) followed by `launchctl kickstart -k gui/<uid>/<label>` to
///     ensure the agent is actually running. The bootstrap is only
///     needed on first install; subsequent starts go through `kickstart`.
///   - `stop` → `launchctl bootout gui/<uid>/<label>`
///   - `restart` → `launchctl kickstart -k gui/<uid>/<label>`
///   - `is_active` → `launchctl print gui/<uid>/<label>` parsed for
///     `state = running`. The plist must already be loaded; absent =
///     inactive.
///
/// No `sudo`. LaunchAgents are per-user and writable by the operator.
pub struct LaunchdCtl {
    /// Resolved at construction time. Cached because launchctl errors
    /// are clearer when we hand it the explicit `gui/<uid>/<label>`
    /// service target rather than relying on `--user` / current-context.
    uid: u32,
    /// Per-node plist directory; defaults to `~/Library/LaunchAgents`.
    /// Configurable so tests can drop a fake location.
    agent_dir: PathBuf,
}

impl LaunchdCtl {
    pub fn new() -> Self {
        let uid = current_uid();
        let agent_dir = crate::plist::user_launch_agents_dir()
            .unwrap_or_else(|| std::env::temp_dir().join("LaunchAgents"));
        Self { uid, agent_dir }
    }

    pub fn with_agent_dir(mut self, dir: PathBuf) -> Self {
        self.agent_dir = dir;
        self
    }

    /// Convert a systemd-style unit name (`elrond-node-0.service`) into
    /// the launchd label (`com.multiversx.elrond-node-0`). Used so the
    /// orchestrator can pass the same identifier to both backends.
    fn label_from_unit(unit: &str) -> String {
        let stem = unit.strip_suffix(".service").unwrap_or(unit);
        format!("{}.{stem}", crate::plist::LAUNCH_AGENT_PREFIX)
    }

    fn service_target(&self, unit: &str) -> String {
        format!("gui/{}/{}", self.uid, Self::label_from_unit(unit))
    }

    fn plist_path(&self, unit: &str) -> PathBuf {
        let label = Self::label_from_unit(unit);
        self.agent_dir.join(format!("{label}.plist"))
    }

    async fn run(&self, args: &[&str]) -> Result<std::process::Output, CtlError> {
        let mut cmd = Command::new("launchctl");
        for a in args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output().await.map_err(CtlError::Spawn)
    }
}

impl Default for LaunchdCtl {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Ctl for LaunchdCtl {
    async fn start(&self, unit: &str) -> Result<(), CtlError> {
        let target = self.service_target(unit);
        let plist = self.plist_path(unit);
        // Bootstrap is idempotent in practice — if already loaded it
        // returns "service already loaded" with non-zero status. We
        // tolerate that and proceed to kickstart; the kickstart -k
        // form is what actually starts the process.
        let _ = self
            .run(&[
                "bootstrap",
                &format!("gui/{}", self.uid),
                &plist.display().to_string(),
            ])
            .await;
        let out = self.run(&["kickstart", "-k", &target]).await?;
        if !out.status.success() {
            return Err(CtlError::NonZero {
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    async fn stop(&self, unit: &str) -> Result<(), CtlError> {
        let target = self.service_target(unit);
        let out = self.run(&["bootout", &target]).await?;
        if !out.status.success() {
            // bootout returns non-zero when the service isn't loaded.
            // That's the moral equivalent of "stop on an already-stopped
            // unit", which the systemd backend treats as success too.
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("Could not find service") || stderr.contains("No such process") {
                return Ok(());
            }
            return Err(CtlError::NonZero {
                code: out.status.code().unwrap_or(-1),
                stderr: stderr.trim().to_string(),
            });
        }
        Ok(())
    }

    async fn restart(&self, unit: &str) -> Result<(), CtlError> {
        let target = self.service_target(unit);
        let out = self.run(&["kickstart", "-k", &target]).await?;
        if !out.status.success() {
            return Err(CtlError::NonZero {
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    async fn is_active(&self, unit: &str) -> Result<ActiveState, CtlError> {
        let target = self.service_target(unit);
        let out = self.run(&["print", &target]).await?;
        if !out.status.success() {
            // `launchctl print` exits non-zero when the service isn't
            // loaded — that's our "inactive".
            return Ok(ActiveState::Inactive);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        // launchctl print emits a key/value tree. We look for either
        // `state = running` or `state = waiting` (KeepAlive between
        // restarts).
        for line in stdout.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("state = ") {
                return Ok(match rest.trim() {
                    "running" => ActiveState::Active,
                    "waiting" => ActiveState::Activating,
                    "exited" => ActiveState::Inactive,
                    _ => ActiveState::Unknown,
                });
            }
        }
        Ok(ActiveState::Unknown)
    }

    async fn show_property(&self, unit: &str, property: &str) -> Result<String, CtlError> {
        // No direct equivalent to `systemctl show -p <prop>`; we read
        // the printed tree and grep. Adequate for the few properties
        // mxnode actually queries (NRestarts, ActiveState).
        let target = self.service_target(unit);
        let out = self.run(&["print", &target]).await?;
        if !out.status.success() {
            return Ok(String::new());
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case(property) {
                    return Ok(value.trim().to_string());
                }
            }
        }
        Ok(String::new())
    }

    async fn daemon_reload(&self) -> Result<(), CtlError> {
        Ok(())
    }
    async fn enable(&self, _unit: &str) -> Result<(), CtlError> {
        Ok(())
    }
    async fn disable(&self, _unit: &str) -> Result<(), CtlError> {
        Ok(())
    }
    async fn reset_failed(&self) -> Result<(), CtlError> {
        Ok(())
    }
    async fn install_unit_file(&self, src: &Path, dest: &Path) -> Result<(), CtlError> {
        tokio::fs::copy(src, dest).await?;
        Ok(())
    }
    async fn remove_file(&self, path: &Path) -> Result<(), CtlError> {
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: getuid is always safe; no preconditions.
    unsafe { libc_getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(unix)]
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

#[cfg(test)]
pub mod testing {
    //! Public test helper: an in-memory [`Ctl`] that records calls and
    //! lets tests dictate what `is_active` returns. Marked `pub` so
    //! integration tests across the workspace can use it.

    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeCtl {
        pub calls: Mutex<Vec<(String, String)>>,
        active_states: Mutex<HashMap<String, ActiveState>>,
        fail_on: Mutex<HashSet<String>>,
    }

    impl FakeCtl {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn set_active(&self, unit: &str, state: ActiveState) {
            self.active_states
                .lock()
                .unwrap()
                .insert(unit.to_string(), state);
        }

        pub fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }

        pub fn fail_on(&self, verb: &str) {
            self.fail_on.lock().unwrap().insert(verb.to_string());
        }

        fn record_or_fail(&self, verb: &str, target: &str) -> Result<(), CtlError> {
            self.calls
                .lock()
                .unwrap()
                .push((verb.to_string(), target.to_string()));
            if self.fail_on.lock().unwrap().contains(verb) {
                return Err(CtlError::NonZero {
                    code: 1,
                    stderr: format!("fake failure on {verb}"),
                });
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Ctl for FakeCtl {
        async fn start(&self, unit: &str) -> Result<(), CtlError> {
            self.calls
                .lock()
                .unwrap()
                .push(("start".into(), unit.into()));
            self.set_active(unit, ActiveState::Active);
            Ok(())
        }
        async fn stop(&self, unit: &str) -> Result<(), CtlError> {
            self.calls
                .lock()
                .unwrap()
                .push(("stop".into(), unit.into()));
            self.set_active(unit, ActiveState::Inactive);
            Ok(())
        }
        async fn restart(&self, unit: &str) -> Result<(), CtlError> {
            self.calls
                .lock()
                .unwrap()
                .push(("restart".into(), unit.into()));
            self.set_active(unit, ActiveState::Active);
            Ok(())
        }
        async fn is_active(&self, unit: &str) -> Result<ActiveState, CtlError> {
            self.calls
                .lock()
                .unwrap()
                .push(("is-active".into(), unit.into()));
            Ok(self
                .active_states
                .lock()
                .unwrap()
                .get(unit)
                .copied()
                .unwrap_or(ActiveState::Unknown))
        }
        async fn show_property(&self, _unit: &str, _property: &str) -> Result<String, CtlError> {
            Ok(String::new())
        }
        async fn daemon_reload(&self) -> Result<(), CtlError> {
            self.record_or_fail("daemon-reload", "")
        }
        async fn enable(&self, unit: &str) -> Result<(), CtlError> {
            self.record_or_fail("enable", unit)
        }
        async fn disable(&self, unit: &str) -> Result<(), CtlError> {
            self.record_or_fail("disable", unit)
        }
        async fn reset_failed(&self) -> Result<(), CtlError> {
            self.record_or_fail("reset-failed", "")
        }
        async fn install_unit_file(&self, _src: &Path, dest: &Path) -> Result<(), CtlError> {
            self.record_or_fail("install-unit", &dest.display().to_string())
        }
        async fn remove_file(&self, path: &Path) -> Result<(), CtlError> {
            self.record_or_fail("remove-file", &path.display().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_state_parsing() {
        assert_eq!(ActiveState::from_string("active"), ActiveState::Active);
        assert_eq!(
            ActiveState::from_string("inactive\n"),
            ActiveState::Inactive
        );
        assert_eq!(ActiveState::from_string("failed"), ActiveState::Failed);
        assert_eq!(
            ActiveState::from_string("not-a-state"),
            ActiveState::Unknown
        );
    }

    #[tokio::test]
    async fn fake_ctl_records_and_transitions_state() {
        use super::testing::FakeCtl;
        let ctl = FakeCtl::new();
        ctl.start("elrond-node-0.service").await.unwrap();
        let state = ctl.is_active("elrond-node-0.service").await.unwrap();
        assert_eq!(state, ActiveState::Active);
        ctl.stop("elrond-node-0.service").await.unwrap();
        let state = ctl.is_active("elrond-node-0.service").await.unwrap();
        assert_eq!(state, ActiveState::Inactive);

        let calls = ctl.calls();
        // is-active calls land between transitions, so we expect 4 entries.
        assert_eq!(calls.len(), 4);
        assert_eq!(
            calls[0],
            ("start".to_string(), "elrond-node-0.service".to_string())
        );
        assert_eq!(
            calls[2],
            ("stop".to_string(), "elrond-node-0.service".to_string())
        );
    }
}

#[cfg(test)]
mod privileged_trait_tests {
    use super::testing::FakeCtl;
    use super::Ctl;
    use std::path::Path;

    #[tokio::test]
    async fn fake_records_privileged_ops_and_can_fail() {
        let ctl = FakeCtl::new();
        ctl.daemon_reload().await.unwrap();
        ctl.enable("elrond-node-0.service").await.unwrap();
        ctl.disable("elrond-node-0.service").await.unwrap();
        ctl.reset_failed().await.unwrap();
        ctl.install_unit_file(Path::new("/tmp/x"), Path::new("/etc/systemd/system/x"))
            .await
            .unwrap();
        ctl.remove_file(Path::new("/etc/systemd/system/x"))
            .await
            .unwrap();
        let verbs: Vec<String> = ctl.calls().into_iter().map(|(v, _)| v).collect();
        assert_eq!(
            verbs,
            vec![
                "daemon-reload",
                "enable",
                "disable",
                "reset-failed",
                "install-unit",
                "remove-file"
            ]
        );

        let ctl = FakeCtl::new();
        ctl.fail_on("remove-file");
        assert!(ctl
            .remove_file(Path::new("/etc/systemd/system/x"))
            .await
            .is_err());
    }
}

#[cfg(test)]
mod systemctl_helpers {
    use super::is_not_enabled;
    #[test]
    fn not_enabled_is_tolerated() {
        assert!(is_not_enabled(
            "Failed to disable unit: Unit file elrond-node-0.service does not exist."
        ));
        assert!(is_not_enabled(
            "The unit files have no installation config (WantedBy=...) and are not enabled."
        ));
        assert!(!is_not_enabled("Interactive authentication required."));
        assert!(!is_not_enabled(""));
    }
}

#[cfg(test)]
mod launchd_privileged {
    use super::{Ctl, LaunchdCtl};
    #[tokio::test]
    async fn install_and_remove_unit_file_are_fs_ops() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.plist");
        let dest = dir.path().join("dest.plist");
        std::fs::write(&src, b"<plist/>").unwrap();
        let ctl = LaunchdCtl::new();
        ctl.daemon_reload().await.unwrap();
        ctl.enable("elrond-node-0.service").await.unwrap();
        ctl.disable("elrond-node-0.service").await.unwrap();
        ctl.reset_failed().await.unwrap();
        ctl.install_unit_file(&src, &dest).await.unwrap();
        assert!(dest.exists());
        ctl.remove_file(&dest).await.unwrap();
        assert!(!dest.exists());
        ctl.remove_file(&dest).await.unwrap();
    }
}
