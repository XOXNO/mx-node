//! Detect + auto-install the Go toolchain that `mxnode-build` shells out
//! to. Auto-install matches the bash flow byte-for-byte:
//!
//!   - apt deps (`build-essential`, `git`, `rsync`, `curl`, `zip`, `unzip`,
//!     `jq`, `gcc`, `wget`) on Debian-likes via `sudo apt-get install`.
//!   - Go tarball downloaded from `dl.google.com/go/...` and extracted to
//!     `/usr/local/go` via `sudo tar -C /usr/local -xzf`.
//!   - `~/.profile` updated with `PATH=$PATH:/usr/local/go/bin:$GOPATH/bin`
//!     and `GOPATH=$HOME/go` if not already present.
//!
//! Hosts where Go was installed by some other channel (homebrew, asdf,
//! mise, system packages elsewhere) are honoured: `detect_go` finds them
//! first via `which`. Auto-install only fires when no Go is on PATH.
//! Version-mismatch on a non-bash-managed install surfaces a typed error
//! instead of silently nuking the operator's tooling.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use thiserror::Error;

/// Process-wide latch — bootstrap is heavy (apt + tar + sudo), so we
/// run the install side at most once per process. Subsequent
/// `bootstrap()` calls fall through to the fast `ensure_go` detect.
static BOOTSTRAP_INSTALLED: OnceLock<()> = OnceLock::new();

/// Default Go version installed by [`bootstrap`] when no Go is on PATH.
/// Matches the bash `GO_LATEST_TESTED` value at the time of the v0.1
/// release. Operators on a fork that pins a different version override
/// via `[install].go_version` in `config.toml` or by passing
/// `--go-version` to `install` / `upgrade`.
pub const DEFAULT_GO_VERSION: &str = "1.20.7";

/// apt packages auto-installed before the first source build. Lifted
/// directly from the bash `assert_required_packages` recipe.
pub const APT_BUILD_DEPS: &[&str] = &[
    "build-essential",
    "git",
    "rsync",
    "curl",
    "zip",
    "unzip",
    "jq",
    "gcc",
    "wget",
];

#[derive(Debug, Error)]
pub enum ToolchainError {
    #[error("could not exec `{cmd}`: {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`{cmd}` exited {status}: {stderr}")]
    NonZero {
        cmd: String,
        status: i32,
        stderr: String,
    },

    #[error("`go version` output was not parseable: {0:?}")]
    ParseVersion(String),

    #[error(
        "go is not on PATH. Install it (https://go.dev/dl/) or run `mxnode upgrade --skip-build`"
    )]
    NotInstalled,

    #[error(
        "go {found} is installed but mxnode requires {required}. Install the matching version: \
         https://go.dev/dl/"
    )]
    VersionMismatch { found: String, required: String },

    #[error("auto-install is unsupported on this host: {0}")]
    AutoInstallUnsupported(String),

    #[error("io error during toolchain install: {0}")]
    Io(#[source] std::io::Error),
}

/// Snapshot of an installed Go toolchain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoInstall {
    /// Path to the `go` binary used to invoke the compiler.
    pub bin: PathBuf,
    /// Version reported by `go version`, normalised without the leading `go`.
    pub version: String,
}

/// Locate `go` on PATH and read its version. Returns
/// `ToolchainError::NotInstalled` when the binary is missing.
pub fn detect_go() -> Result<GoInstall, ToolchainError> {
    let bin = which_go()?;
    let version = read_go_version(&bin)?;
    Ok(GoInstall { bin, version })
}

/// Detect Go and verify it satisfies a required version. We do not enforce
/// strict equality — only that the major.minor on disk is at least
/// `required`. The bash always grabs the latest known-good version, but
/// operators on managed hosts often have newer Go which compiles
/// mx-chain-go fine.
pub fn ensure_go(required: &str) -> Result<GoInstall, ToolchainError> {
    let install = detect_go()?;
    if !satisfies(&install.version, required) {
        return Err(ToolchainError::VersionMismatch {
            found: install.version,
            required: required.to_string(),
        });
    }
    Ok(install)
}

/// One-shot bootstrap for a fresh build host: install apt deps if
/// missing, install Go if missing, return the resolved [`GoInstall`].
/// Mirrors the bash `assert_required_packages` + `go_lang` flow.
///
/// Operations performed (each idempotent):
///   1. On Debian-likes, `sudo apt-get install` the bash dep list.
///   2. If `go` isn't on PATH, download Go `version` from `dl.google.com`,
///      extract to `/usr/local/go`, append `~/.profile` exports.
///   3. Re-detect Go; bubble up a typed error if it's still missing or
///      version-mismatched.
///
/// Skipped silently on non-Linux hosts (macOS dev boxes already have
/// Go via brew or Xcode + the operator never asked us to nuke their
/// system).
pub fn bootstrap(version: &str) -> Result<GoInstall, ToolchainError> {
    // Fast path: already bootstrapped this process — just verify Go.
    if BOOTSTRAP_INSTALLED.get().is_some() {
        return ensure_go(version);
    }
    if !cfg!(target_os = "linux") {
        // macOS / freebsd: detect-only; the operator owns the toolchain.
        BOOTSTRAP_INSTALLED.set(()).ok();
        return ensure_go(version);
    }
    if is_debian_like() {
        if let Err(e) = install_apt_deps() {
            // Don't fail the bootstrap on apt errors — the operator may
            // have these packages from a non-apt source. Surface as a
            // warning to stderr and continue to the Go check.
            eprintln!("warn: apt-get install of build deps failed: {e}");
        }
    }
    let result = match ensure_go(version) {
        Ok(install) => Ok(install),
        Err(ToolchainError::NotInstalled) => {
            install_go(version)?;
            ensure_go(version)
        }
        Err(other) => Err(other),
    };
    if result.is_ok() {
        BOOTSTRAP_INSTALLED.set(()).ok();
    }
    result
}

/// `sudo apt-get install` the bash build-deps list. Idempotent — apt
/// is happy to re-confirm already-installed packages.
pub fn install_apt_deps() -> Result<(), ToolchainError> {
    eprintln!("→ installing apt build deps ({})", APT_BUILD_DEPS.join(" "));
    let mut cmd = Command::new("sudo");
    cmd.env("DEBIAN_FRONTEND", "noninteractive")
        .arg("DEBIAN_FRONTEND=noninteractive")
        .args(["apt-get", "-qy", "install"])
        .args(APT_BUILD_DEPS);
    let status = cmd.status().map_err(|e| ToolchainError::Spawn {
        cmd: "sudo apt-get".to_string(),
        source: e,
    })?;
    if !status.success() {
        return Err(ToolchainError::NonZero {
            cmd: "sudo apt-get install".to_string(),
            status: status.code().unwrap_or(-1),
            stderr: "see apt output above".to_string(),
        });
    }
    Ok(())
}

/// Download Go `version` and extract to `/usr/local/go`. Mirrors the
/// bash `wget … && sudo tar -C /usr/local -xzf …` exactly.
pub fn install_go(version: &str) -> Result<GoInstall, ToolchainError> {
    if !cfg!(target_os = "linux") {
        return Err(ToolchainError::AutoInstallUnsupported(
            "automatic Go install is Linux-only — install via brew/dl on macOS".to_string(),
        ));
    }
    let arch = detect_arch()?;
    let tarball_name = format!("go{version}.linux-{arch}.tar.gz");
    let url = format!("https://dl.google.com/go/{tarball_name}");
    let tarball = std::env::temp_dir().join(&tarball_name);

    eprintln!("→ downloading {url}");
    run_visible("curl", &["-fsSL", "-o", tarball.to_string_lossy().as_ref(), &url])?;

    // If a previous bash- or scripts-managed Go install lives at
    // /usr/local/go, blow it away before we extract — bash does the
    // same (`sudo rm -rf /usr/local/go`). Operators with a non-bash
    // Go install never reach this branch (detect_go found their
    // installation first).
    if Path::new("/usr/local/go").exists() {
        eprintln!("→ removing previous /usr/local/go (sudo)");
        run_visible("sudo", &["rm", "-rf", "/usr/local/go"])?;
    }

    eprintln!("→ extracting to /usr/local (sudo)");
    run_visible(
        "sudo",
        &["tar", "-C", "/usr/local", "-xzf", tarball.to_string_lossy().as_ref()],
    )?;
    let _ = std::fs::remove_file(&tarball);

    update_profile_for_go()?;

    eprintln!("✓ go {version} installed at /usr/local/go");
    eprintln!(
        "  ~/.profile updated. New shells get go on PATH automatically;\n  \
         current shell needs `source ~/.profile` if you want `go version` to work there."
    );

    detect_go()
}

/// Append the bash `~/.profile` exports if not already present. We
/// match bash's lines exactly so re-running mxnode (or alternating
/// with the bash flow) doesn't produce duplicate exports.
fn update_profile_for_go() -> Result<(), ToolchainError> {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return Ok(()), // headless / no HOME — silently skip
    };
    let profile = home.join(".profile");
    let body = std::fs::read_to_string(&profile).unwrap_or_default();
    if body.contains("/usr/local/go/bin") {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&profile)
        .map_err(ToolchainError::Io)?;
    writeln!(f, "\n# Added by mxnode bootstrap").map_err(ToolchainError::Io)?;
    writeln!(f, "export PATH=$PATH:/usr/local/go/bin:$GOPATH/bin")
        .map_err(ToolchainError::Io)?;
    writeln!(f, "export GOPATH=$HOME/go").map_err(ToolchainError::Io)?;
    Ok(())
}

/// `uname -m` → Go-style arch token (`amd64`, `arm64`).
fn detect_arch() -> Result<&'static str, ToolchainError> {
    let out = Command::new("uname").arg("-m").output().map_err(|e| {
        ToolchainError::Spawn {
            cmd: "uname".to_string(),
            source: e,
        }
    })?;
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(match raw.as_str() {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        other => {
            return Err(ToolchainError::AutoInstallUnsupported(format!(
                "no Go binary for arch '{other}' — install manually from https://go.dev/dl/"
            )))
        }
    })
}

fn is_debian_like() -> bool {
    Path::new("/etc/debian_version").exists()
}

/// Spawn a child whose stdout/stderr inherit our terminal, so the
/// operator sees apt + curl + tar progress in real time.
fn run_visible(cmd: &str, args: &[&str]) -> Result<(), ToolchainError> {
    let status = Command::new(cmd).args(args).status().map_err(|e| {
        ToolchainError::Spawn {
            cmd: cmd.to_string(),
            source: e,
        }
    })?;
    if !status.success() {
        return Err(ToolchainError::NonZero {
            cmd: format!("{cmd} {}", args.join(" ")),
            status: status.code().unwrap_or(-1),
            stderr: "see output above".to_string(),
        });
    }
    Ok(())
}

fn which_go() -> Result<PathBuf, ToolchainError> {
    // Prefer `/usr/local/go/bin/go` (the bash install location) when it
    // exists, because `which go` may return shim wrappers (asdf, mise)
    // whose actual binary path moves under us.
    let well_known = PathBuf::from("/usr/local/go/bin/go");
    if well_known.exists() {
        return Ok(well_known);
    }
    let output = Command::new("which").arg("go").output().map_err(|e| {
        ToolchainError::Spawn {
            cmd: "which".to_string(),
            source: e,
        }
    })?;
    if !output.status.success() {
        return Err(ToolchainError::NotInstalled);
    }
    let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path_str.is_empty() {
        return Err(ToolchainError::NotInstalled);
    }
    Ok(PathBuf::from(path_str))
}

fn read_go_version(bin: &std::path::Path) -> Result<String, ToolchainError> {
    let output = Command::new(bin).arg("version").output().map_err(|e| {
        ToolchainError::Spawn {
            cmd: bin.display().to_string(),
            source: e,
        }
    })?;
    if !output.status.success() {
        return Err(ToolchainError::NonZero {
            cmd: bin.display().to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    parse_go_version(&raw).ok_or(ToolchainError::ParseVersion(raw))
}

/// Extract the version from `go version` output, e.g.
/// `go version go1.21.5 darwin/arm64` → `1.21.5`.
fn parse_go_version(s: &str) -> Option<String> {
    // Tokens: ["go", "version", "go1.21.5", "darwin/arm64"]
    s.split_whitespace()
        .find(|t| t.starts_with("go") && t.len() > 2 && t.as_bytes()[2].is_ascii_digit())
        .map(|t| t.trim_start_matches("go").to_string())
}

/// True when `installed >= required` on a (major, minor, patch) basis.
/// Patch/pre-release strings beyond the first three components are
/// compared lexicographically as a tiebreaker.
fn satisfies(installed: &str, required: &str) -> bool {
    use semver::Version;
    let parse = |s: &str| -> Option<Version> {
        // Pad missing components ("1.21" → "1.21.0").
        let mut parts: Vec<&str> = s.split('.').collect();
        while parts.len() < 3 {
            parts.push("0");
        }
        Version::parse(&parts[..3].join(".")).ok()
    };
    match (parse(installed), parse(required)) {
        (Some(a), Some(b)) => a >= b,
        // Fallback: byte-equal counts as "satisfies".
        _ => installed == required,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_go_version_extracts_three_part() {
        assert_eq!(
            parse_go_version("go version go1.21.5 darwin/arm64").as_deref(),
            Some("1.21.5"),
        );
        assert_eq!(
            parse_go_version("go version go1.20 linux/amd64").as_deref(),
            Some("1.20"),
        );
    }

    #[test]
    fn parse_go_version_handles_unexpected_input() {
        assert!(parse_go_version("not what we expected").is_none());
        assert!(parse_go_version("").is_none());
    }

    #[test]
    fn satisfies_treats_higher_as_compatible() {
        assert!(satisfies("1.21.5", "1.20.7"));
        assert!(satisfies("1.21.5", "1.21.5"));
        assert!(satisfies("1.22.0", "1.21.0"));
    }

    #[test]
    fn satisfies_rejects_older() {
        assert!(!satisfies("1.20.0", "1.21.0"));
        assert!(!satisfies("1.20.7", "1.20.8"));
    }

    #[test]
    fn satisfies_pads_missing_components() {
        assert!(satisfies("1.21", "1.21.0"));
        assert!(!satisfies("1.21", "1.21.1"));
    }
}
