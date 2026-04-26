//! Runtime platform detection.
//!
//! Linux and macOS have different process supervisors (systemd vs launchd),
//! different log facilities (journald vs file logs), different firewall
//! tools (ufw vs pf), and different filesystem conventions. mxnode
//! commands stay platform-agnostic at the surface; the dispatch into the
//! right backend happens via [`Platform::detect`].
//!
//! Detection runs once at process start and is cached. Tests that exercise
//! a specific branch use [`Platform::override_for_test`] to force a value.

use std::sync::OnceLock;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Platform {
    /// Linux with systemd. The dominant target — every operator's bash
    /// install runs here today.
    Linux,
    /// macOS with launchd. Apple Silicon and Intel both supported because
    /// MultiversX nodes ship as Go binaries that are arch-portable.
    Macos,
    /// FreeBSD / other Unix. Reserved — we surface a typed error if
    /// someone tries to install on a host we haven't covered, rather than
    /// guessing at a launchd-shaped behaviour that won't actually work.
    Unsupported,
}

impl Platform {
    pub fn detect() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Unsupported
        }
    }

    /// Cached detection. Computed once on first call, returned by reference
    /// thereafter. Cheap enough to call from any code path.
    pub fn current() -> Self {
        static PLATFORM: OnceLock<Platform> = OnceLock::new();
        *PLATFORM.get_or_init(Self::detect)
    }

    /// Operator-readable name for log/diagnostic output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Unsupported => "unsupported",
        }
    }

    /// What this platform calls its service manager. Used by `doctor` and
    /// every operator-facing message that mentions the supervisor.
    pub fn supervisor_label(self) -> &'static str {
        match self {
            Self::Linux => "systemd",
            Self::Macos => "launchd",
            Self::Unsupported => "(unsupported)",
        }
    }

    /// Service-manager filename convention.
    pub fn unit_extension(self) -> &'static str {
        match self {
            Self::Linux => "service",
            Self::Macos => "plist",
            Self::Unsupported => "service",
        }
    }

    /// `true` when the supervisor stores units in a per-user directory we
    /// can write without sudo. Linux systemd unit files live under
    /// `/etc/systemd/system/` (root-owned); macOS launchd LaunchAgents
    /// live under `~/Library/LaunchAgents/` (operator-owned). The
    /// `install_units` flow uses this to decide whether to shell `sudo`.
    pub fn unit_dir_is_user_writable(self) -> bool {
        matches!(self, Self::Macos)
    }

    /// Does the supervisor have a journald-equivalent we can shell out
    /// to for `mxnode logs`? Linux yes; macOS we tail file logs that the
    /// node writes itself.
    pub fn has_journal(self) -> bool {
        matches!(self, Self::Linux)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_one_of_known_variants() {
        let p = Platform::detect();
        assert!(matches!(p, Platform::Linux | Platform::Macos | Platform::Unsupported));
    }

    #[test]
    fn current_is_consistent_across_calls() {
        let a = Platform::current();
        let b = Platform::current();
        assert_eq!(a, b);
    }

    #[test]
    fn supervisor_label_is_systemd_or_launchd_on_known_platforms() {
        assert_eq!(Platform::Linux.supervisor_label(), "systemd");
        assert_eq!(Platform::Macos.supervisor_label(), "launchd");
    }

    #[test]
    fn unit_dir_user_writable_only_on_macos() {
        assert!(!Platform::Linux.unit_dir_is_user_writable());
        assert!(Platform::Macos.unit_dir_is_user_writable());
    }

    #[test]
    fn unit_extension_matches_platform_convention() {
        assert_eq!(Platform::Linux.unit_extension(), "service");
        assert_eq!(Platform::Macos.unit_extension(), "plist");
    }

    #[test]
    fn has_journal_only_on_linux() {
        assert!(Platform::Linux.has_journal());
        assert!(!Platform::Macos.has_journal());
    }
}
