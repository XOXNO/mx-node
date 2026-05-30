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
/// Routes all privileged side-effects through the `Ctl` trait so callers
/// can inject a `FakeCtl` in tests and so failures propagate instead of
/// being swallowed. The dest-path resolution (unit dir + filename
/// translation) is unchanged from the previous implementation.
pub async fn install_one_unit(
    ctl: &dyn mxnode_systemd::Ctl,
    platform: Platform,
    unit_name: &str,
    contents: &str,
    enable: bool,
) -> Result<(), InstallUnitError> {
    let dir = unit_dir_for_platform(platform).ok_or(InstallUnitError::UnsupportedPlatform)?;
    let dest = dir.join(unit_filename(platform, unit_name));

    match platform {
        Platform::Linux => install_unit_linux(ctl, &dest, contents, unit_name, enable).await,
        Platform::Macos => install_unit_macos(ctl, &dest, contents, unit_name, enable).await,
        Platform::Unsupported => Err(InstallUnitError::UnsupportedPlatform),
    }
}

async fn install_unit_linux(
    ctl: &dyn mxnode_systemd::Ctl,
    dest: &Path,
    contents: &str,
    unit_name: &str,
    enable: bool,
) -> Result<(), InstallUnitError> {
    let tmp = std::env::temp_dir().join(unit_name);
    fs::write(&tmp, contents).map_err(|e| InstallUnitError::Io {
        path: tmp.display().to_string(),
        source: e,
    })?;
    ctl.install_unit_file(&tmp, dest)
        .await
        .map_err(|e| InstallUnitError::Privileged(format!("install unit {}: {e}", dest.display())))?;
    // systemd caches unit files; reload so the freshly-installed unit is
    // visible to `enable`/`start` instead of failing with "unit file
    // changed on disk" or operating on a stale view.
    ctl.daemon_reload()
        .await
        .map_err(|e| InstallUnitError::Privileged(format!("daemon-reload: {e}")))?;
    if enable {
        ctl.enable(unit_name)
            .await
            .map_err(|e| InstallUnitError::Privileged(format!("enable {unit_name}: {e}")))?;
    }
    Ok(())
}

async fn install_unit_macos(
    ctl: &dyn mxnode_systemd::Ctl,
    dest: &Path,
    contents: &str,
    unit_name: &str,
    enable: bool,
) -> Result<(), InstallUnitError> {
    let tmp = std::env::temp_dir().join(unit_name);
    fs::write(&tmp, contents).map_err(|e| InstallUnitError::Io {
        path: tmp.display().to_string(),
        source: e,
    })?;
    ctl.install_unit_file(&tmp, dest)
        .await
        .map_err(|e| InstallUnitError::Privileged(format!("install plist {}: {e}", dest.display())))?;
    if enable {
        ctl.enable(unit_name)
            .await
            .map_err(|e| InstallUnitError::Privileged(format!("bootstrap {unit_name}: {e}")))?;
    }
    Ok(())
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
    #[error("privileged op failed: {0}")]
    Privileged(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxnode_systemd::ctl_testing::FakeCtl;

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

    #[tokio::test]
    async fn linux_install_reloads_before_enable_and_propagates_failure() {
        let ctl = FakeCtl::new();
        install_one_unit(&ctl, Platform::Linux, "elrond-node-0.service", "[Unit]\n", true)
            .await
            .expect("install should succeed");
        let verbs: Vec<String> = ctl.calls().into_iter().map(|(v, _)| v).collect();
        let reload = verbs.iter().position(|v| v == "daemon-reload").unwrap();
        let enable = verbs.iter().position(|v| v == "enable").unwrap();
        assert!(verbs.contains(&"install-unit".to_string()));
        assert!(reload < enable, "daemon-reload must precede enable: {verbs:?}");

        let ctl = FakeCtl::new();
        ctl.fail_on("enable");
        let err = install_one_unit(&ctl, Platform::Linux, "elrond-node-0.service", "[Unit]\n", true).await;
        assert!(err.is_err(), "enable failure must propagate");
    }
}
