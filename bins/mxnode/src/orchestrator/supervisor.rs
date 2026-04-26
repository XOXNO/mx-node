//! Cross-platform `Ctl` selection + per-platform unit/plist install paths.
//!
//! Every command that talks to the supervisor (start/stop/restart, db,
//! upgrade, rollback, cleanup, install) goes through these helpers so
//! the Linux/macOS branch lives in exactly one place.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mxnode_core::Platform;
use mxnode_systemd::{user_launch_agents_dir, Ctl, LaunchdCtl, SystemctlCtl};

/// Pick the right supervisor backend for the current platform.
///
/// Linux → `SystemctlCtl` (sudo-prefixed for state-changing ops).
/// macOS → `LaunchdCtl` (per-user, no sudo).
/// Anything else → SystemctlCtl as a best-effort default; the actual
/// install path will refuse cleanly because `unit_dir_for(platform)`
/// returns `None` for unsupported platforms.
pub fn build_supervisor() -> Arc<dyn Ctl> {
    match Platform::current() {
        Platform::Macos => Arc::new(LaunchdCtl::new()),
        Platform::Linux | Platform::Unsupported => Arc::new(SystemctlCtl::new()),
    }
}

/// Where rendered unit/plist files belong on this platform.
///
/// Returns an absolute directory path. Linux: `/etc/systemd/system`
/// (root-owned, install needs sudo). macOS: `~/Library/LaunchAgents`
/// (operator-owned, no sudo).
pub fn unit_dir_for_platform(platform: Platform) -> Option<PathBuf> {
    match platform {
        Platform::Linux => Some(PathBuf::from("/etc/systemd/system")),
        Platform::Macos => user_launch_agents_dir(),
        Platform::Unsupported => None,
    }
}

/// Translate a systemd-style unit name (`elrond-node-0.service`) to the
/// filename the current platform expects (`elrond-node-0.service` on
/// Linux, `com.multiversx.elrond-node-0.plist` on macOS). The
/// orchestrator always speaks in systemd-style names; only this layer
/// knows about the platform-specific file naming.
pub fn unit_filename(platform: Platform, unit: &str) -> String {
    match platform {
        Platform::Linux | Platform::Unsupported => unit.to_string(),
        Platform::Macos => {
            let stem = unit.strip_suffix(".service").unwrap_or(unit);
            format!("com.multiversx.{stem}.plist")
        }
    }
}

/// Install one rendered unit file into the platform's supervisor dir.
///
/// Linux uses `sudo --non-interactive mv` (matches the bash) and then
/// `sudo systemctl enable`. macOS just `cp`s the plist into the per-user
/// LaunchAgents dir and calls `launchctl bootstrap` — both run as the
/// operator, no privilege escalation.
pub async fn install_one_unit(
    platform: Platform,
    unit_name: &str,
    contents: &str,
    enable: bool,
) -> Result<PathBuf, InstallUnitError> {
    let dir = unit_dir_for_platform(platform).ok_or(InstallUnitError::UnsupportedPlatform)?;
    fs::create_dir_all(&dir).map_err(|e| InstallUnitError::Io {
        path: dir.display().to_string(),
        source: e,
    })?;
    let dest = dir.join(unit_filename(platform, unit_name));

    match platform {
        Platform::Linux => install_unit_linux(&dest, contents, unit_name, enable).await,
        Platform::Macos => install_unit_macos(&dest, contents, unit_name, enable).await,
        Platform::Unsupported => Err(InstallUnitError::UnsupportedPlatform),
    }
    .map(|_| dest)
}

async fn install_unit_linux(
    dest: &Path,
    contents: &str,
    unit_name: &str,
    enable: bool,
) -> Result<(), InstallUnitError> {
    use std::process::Stdio;
    let tmp = std::env::temp_dir().join(unit_name);
    fs::write(&tmp, contents).map_err(|e| InstallUnitError::Io {
        path: tmp.display().to_string(),
        source: e,
    })?;
    let status = std::process::Command::new("sudo")
        .arg("--non-interactive")
        .arg("mv")
        .arg(&tmp)
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .map_err(|e| InstallUnitError::Io {
            path: dest.display().to_string(),
            source: e,
        })?;
    if !status.success() {
        return Err(InstallUnitError::Io {
            path: dest.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("sudo mv exited {:?}", status.code()),
            ),
        });
    }
    if enable {
        let _ = std::process::Command::new("sudo")
            .arg("--non-interactive")
            .arg("systemctl")
            .arg("enable")
            .arg(unit_name)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status();
    }
    Ok(())
}

async fn install_unit_macos(
    dest: &Path,
    contents: &str,
    _unit_name: &str,
    enable: bool,
) -> Result<(), InstallUnitError> {
    fs::write(dest, contents).map_err(|e| InstallUnitError::Io {
        path: dest.display().to_string(),
        source: e,
    })?;
    if enable {
        // launchd doesn't have a separate "enable" verb; bootstrap is
        // the equivalent of "load this plist into the operator's gui
        // domain". `launchctl bootstrap` is idempotent for our purposes
        // because mxnode never installs the same plist twice without a
        // `cleanup` step in between.
        use std::process::Stdio;
        let uid = current_uid();
        let _ = std::process::Command::new("launchctl")
            .arg("bootstrap")
            .arg(format!("gui/{uid}"))
            .arg(dest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status();
    }
    Ok(())
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    #[cfg(unix)]
    unsafe {
        extern "C" {
            #[link_name = "getuid"]
            fn libc_getuid() -> u32;
        }
        libc_getuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Errors install paths surface to the operator.
#[derive(Debug, thiserror::Error)]
pub enum InstallUnitError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("this platform is not yet supported by mxnode")]
    UnsupportedPlatform,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_filename_linux_passthrough() {
        assert_eq!(
            unit_filename(Platform::Linux, "elrond-node-0.service"),
            "elrond-node-0.service",
        );
        assert_eq!(
            unit_filename(Platform::Linux, "elrond-proxy.service"),
            "elrond-proxy.service",
        );
    }

    #[test]
    fn unit_filename_macos_translates_to_plist() {
        assert_eq!(
            unit_filename(Platform::Macos, "elrond-node-0.service"),
            "com.multiversx.elrond-node-0.plist",
        );
        assert_eq!(
            unit_filename(Platform::Macos, "elrond-proxy.service"),
            "com.multiversx.elrond-proxy.plist",
        );
    }

    #[test]
    fn unit_dir_linux_is_etc_systemd() {
        assert_eq!(
            unit_dir_for_platform(Platform::Linux),
            Some(PathBuf::from("/etc/systemd/system")),
        );
    }

    #[test]
    fn unit_dir_macos_is_user_library() {
        let dir = unit_dir_for_platform(Platform::Macos);
        assert!(dir.is_some());
        let p = dir.unwrap();
        assert!(p.ends_with("Library/LaunchAgents"));
    }

    #[test]
    fn unit_dir_unsupported_returns_none() {
        assert!(unit_dir_for_platform(Platform::Unsupported).is_none());
    }
}
