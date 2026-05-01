//! Git clone + Go build wrappers used by the source-build acquirer.
//!
//! Both functions shell out to the operator's `git` and `go` binaries.
//! mxnode does NOT bundle either; `mxnode_toolchain::ensure_go` validates
//! the Go install before any caller invokes [`build_artifact`].
//!
//! Output paths are returned to the caller, which is responsible for
//! moving the binary into the versioned `BinStore` layout.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use mxnode_core::Tag;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("io error spawning {cmd}: {source}")]
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

    #[error("expected output binary at {0} after build, but it does not exist")]
    OutputMissing(PathBuf),
}

/// Shallow-clone `repo` at `tag` into `dest`. Equivalent to the bash:
/// `git clone --branch=<tag> --single-branch --depth=1 <repo> <dest>`.
pub async fn clone_shallow(repo: &str, tag: &Tag, dest: &Path) -> Result<(), BuildError> {
    if dest.exists() {
        // Operator already has a clone — refuse to overwrite. The
        // orchestrator passes a fresh tempdir per build, so this only
        // fires on programmer error.
        return Err(BuildError::Spawn {
            cmd: "git clone".to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("destination {} already exists", dest.display()),
            ),
        });
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BuildError::Spawn {
            cmd: "git clone".to_string(),
            source: e,
        })?;
    }

    let status = Command::new("git")
        .arg("clone")
        .arg("--branch")
        .arg(tag.as_str())
        .arg("--single-branch")
        .arg("--depth=1")
        .arg(repo)
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| BuildError::Spawn {
            cmd: "git".to_string(),
            source: e,
        })?;
    if !status.status.success() {
        return Err(BuildError::NonZero {
            cmd: format!("git clone {repo} @ {tag}"),
            status: status.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&status.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// Run `cd <repo_dir>/<sub_path> && go build <ldflags>` and return the
/// compiled binary path.
///
/// `binary_name` is the basename Go writes (matches the directory name in
/// the mx-chain-go repo: `node`, `proxy`, `keygenerator`).
///
/// `ldflags` is appended verbatim. Caller assembles the full
/// `-X main.appVersion=...` string.
pub async fn build_artifact(
    repo_dir: &Path,
    sub_path: &str,
    binary_name: &str,
    ldflags: &str,
) -> Result<PathBuf, BuildError> {
    let cmd_dir = repo_dir.join(sub_path);
    if !cmd_dir.exists() {
        return Err(BuildError::OutputMissing(cmd_dir));
    }

    let mut cmd = Command::new("go");
    cmd.arg("build");
    if !ldflags.trim().is_empty() {
        cmd.arg("-ldflags").arg(ldflags);
    }
    cmd.current_dir(&cmd_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = cmd.output().await.map_err(|e| BuildError::Spawn {
        cmd: "go".to_string(),
        source: e,
    })?;
    if !output.status.success() {
        return Err(BuildError::NonZero {
            cmd: format!("go build in {}", cmd_dir.display()),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let bin = cmd_dir.join(binary_name);
    if !bin.exists() {
        return Err(BuildError::OutputMissing(bin));
    }
    Ok(bin)
}

/// Construct the bash-style `appVersion` ldflag. Matches:
/// `-X main.appVersion=$SHOWVER-0-$(git describe --tags --long --always | tail -c 11)`.
///
/// We emit a deterministic version when `git describe` is unavailable so
/// builds in tempdirs without `.git` (mocking tests) still produce a
/// usable binary.
pub fn build_ldflags(version_tag: &Tag, git_commit_suffix: Option<&str>) -> String {
    let suffix = git_commit_suffix.unwrap_or("0-mxnode");
    format!(
        "-X main.appVersion={tag}-0-{suffix}",
        tag = version_tag.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn build_ldflags_includes_tag_and_suffix() {
        let tag = Tag::from_str("v1.7.13").unwrap();
        let flags = build_ldflags(&tag, Some("abcd1234"));
        assert!(flags.contains("v1.7.13"));
        assert!(flags.contains("abcd1234"));
        assert!(flags.starts_with("-X main.appVersion="));
    }

    #[test]
    fn build_ldflags_falls_back_when_no_git_metadata() {
        let tag = Tag::from_str("v1.0.0").unwrap();
        let flags = build_ldflags(&tag, None);
        assert!(flags.contains("v1.0.0"));
        assert!(flags.contains("mxnode"));
    }

    /// Failing-clone smoke test: cloning a definitely-bogus URL into a
    /// tempdir surfaces a `BuildError::NonZero`, not a panic. The test
    /// runs without network because the fake URL never resolves; git
    /// returns a non-zero status almost immediately.
    #[tokio::test]
    async fn clone_shallow_surfaces_git_failure() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("clone");
        let tag = Tag::from_str("v0.0.0").unwrap();
        let err = clone_shallow("https://example.invalid/does-not-exist.git", &tag, &dest).await;
        // Could be NonZero (git ran and failed) or Spawn (git missing).
        // Both are acceptable failure shapes; we only assert it's not Ok.
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn clone_shallow_refuses_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("existing");
        std::fs::create_dir_all(&dest).unwrap();
        let tag = Tag::from_str("v1.0.0").unwrap();
        let err = clone_shallow("https://example.invalid/x.git", &tag, &dest)
            .await
            .unwrap_err();
        match err {
            BuildError::Spawn { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }
}
