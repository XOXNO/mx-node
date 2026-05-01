//! Thin wrappers around `systemctl` for the lifecycle commands.
//!
//! v0.1 shells out to `systemctl` rather than talking dbus directly: simpler
//! deps, more debuggable, and `systemctl` is universally on PATH wherever
//! systemd is. The audit flagged `zbus` as a Phase 2+ optimisation; we keep
//! that door open by hiding the implementation behind the [`Ctl`] trait so
//! callers can swap in a different backend later.

use std::path::PathBuf;
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

    fn build_command(&self, mutating: bool, args: &[&str]) -> Command {
        let cmd = if mutating && self.sudo {
            let mut c = Command::new("sudo");
            c.arg("--non-interactive").arg("systemctl");
            for a in args {
                c.arg(a);
            }
            c
        } else {
            let mut c = Command::new("systemctl");
            for a in args {
                c.arg(a);
            }
            c
        };
        cmd
    }

    async fn run_mutation(&self, args: &[&str]) -> Result<(), CtlError> {
        let mut cmd = self.build_command(true, args);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd.output().await.map_err(CtlError::Spawn)?;
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

    async fn run_read(&self, args: &[&str]) -> Result<String, CtlError> {
        let mut cmd = self.build_command(false, args);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd.output().await.map_err(CtlError::Spawn)?;
        // `systemctl is-active` exits non-zero for inactive/failed units;
        // we still want the stdout text so the caller can classify. Treat
        // exit codes as informational here — only spawn errors propagate.
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

impl Default for SystemctlCtl {
    fn default() -> Self {
        Self::new()
    }
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
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeCtl {
        pub calls: Mutex<Vec<(String, String)>>,
        active_states: Mutex<HashMap<String, ActiveState>>,
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
