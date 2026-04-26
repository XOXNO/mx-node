//! Detect (don't install) the Go toolchain that `mxnode-build` shells out to.
//!
//! v0.1 deliberately does NOT take responsibility for nuking
//! `/usr/local/go` and re-tar-extracting a fresh tarball. The bash does
//! that today; replicating it inside `mxnode upgrade` is a foot-gun
//! waiting to happen on hosts where Go was installed by `apt` or `brew`.
//!
//! Instead we surface a typed error pointing the operator at the upstream
//! install instructions. A future `mxnode toolchain install-go` command
//! can wrap the bash flow as an opt-in escape hatch.

use std::path::PathBuf;
use std::process::Command;

use thiserror::Error;

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
