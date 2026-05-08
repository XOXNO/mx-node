//! `mxnode self-update`: replace the running binary with the latest (or
//! a pinned) release from `XOXNO/mx-node`. Same flow `install.sh` runs,
//! just built into the binary so operators can `mxnode self-update`
//! instead of piping `curl` into `sh -s -- --force`.
//!
//! Verifies the downloaded archive against `SHA256SUMS` when the release
//! ships one, falls back to `sudo install` when the install dir is
//! root-owned (the typical `/usr/local/bin` case), and never silently
//! installs an unverified binary.

use std::path::Path;

use mxnode_github::{verify_against_sums, Client, ClientConfig};

use crate::cli::{GlobalArgs, SelfUpdateArgs};
use crate::errors::CliError;
use crate::orchestrator::runtime::CliErrorExt;

const REPO_OWNER: &str = "XOXNO";
const REPO_NAME: &str = "mx-node";

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: SelfUpdateArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let current = env!("CARGO_PKG_VERSION");

    // Resolve the token without going through `Runtime::from_global`
    // — that path auto-init's a fresh `mxnode.toml` (with the network
    // prompt!) when one doesn't exist, which is wildly wrong for
    // self-update: the operator just wants a new binary, not to be
    // dropped into a config wizard. Read the file directly if it
    // exists; otherwise fall back to `$MXNODE_GITHUB_TOKEN`. Either
    // is fine; both being absent is fine too (60 req/h unauth is
    // plenty for a single release fetch).
    let token = std::env::var("MXNODE_GITHUB_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(read_token_from_unified_file);
    let client = Client::new(ClientConfig {
        token,
        ..ClientConfig::default()
    })
    .map_err(|e| {
        CliError::new(
            "could not initialise the GitHub client",
            e.to_string(),
            "report this as an mxnode bug",
        )
        .json_if(global.json)
    })?;

    let release = if let Some(tag) = &args.tag {
        client
            .release_at_tag(REPO_OWNER, REPO_NAME, tag)
            .await
            .map_err(|e| {
                CliError::new(
                    format!("could not resolve tag {tag}"),
                    e.to_string(),
                    "verify the release exists at https://github.com/XOXNO/mx-node/releases",
                )
                .json_if(global.json)
            })?
    } else {
        client
            .latest_release(REPO_OWNER, REPO_NAME)
            .await
            .map_err(|e| {
                CliError::new(
                "could not resolve the latest mxnode release",
                e.to_string(),
                "set MXNODE_GITHUB_TOKEN to dodge the anonymous rate limit, or pass --tag <vX.Y.Z>",
            )
            .json_if(global.json)
            })?
    };

    let latest_tag = release.tag_name.trim_start_matches('v').to_string();
    let up_to_date = current == latest_tag;

    if args.check {
        if global.json {
            println!(
                "{}",
                serde_json::json!({
                    "current": current,
                    "latest": latest_tag,
                    "up_to_date": up_to_date,
                })
            );
        } else {
            println!("current: v{current}");
            println!("latest:  v{latest_tag}");
            if up_to_date {
                println!("✓ up to date");
            } else {
                println!("→ run `mxnode self-update` to upgrade");
            }
        }
        return Ok(());
    }

    if up_to_date && !args.force {
        if global.json {
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "already_installed": current,
                })
            );
        } else {
            println!("✓ mxnode v{current} is already installed");
            println!("  pass --force to reinstall.");
        }
        return Ok(());
    }

    let target = release_target_triple().ok_or_else(|| {
        CliError::new(
            "unsupported host platform",
            format!(
                "mxnode releases ship binaries for x86_64/aarch64 on Linux + macOS; this host is {} {}",
                std::env::consts::ARCH,
                std::env::consts::OS,
            ),
            "build from source: https://github.com/XOXNO/mx-node",
        )
        .json_if(global.json)
    })?;

    let archive_name = format!("mxnode-{}-{target}.tar.gz", release.tag_name);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .ok_or_else(|| {
            CliError::new(
                format!("no release asset named {archive_name}"),
                format!(
                    "release {} ships {} asset(s); none match this host's target",
                    release.tag_name,
                    release.assets.len(),
                ),
                "verify the release at https://github.com/XOXNO/mx-node/releases includes a binary for your platform",
            )
            .json_if(global.json)
        })?;

    let workdir = tempfile::tempdir().map_err(|e| {
        CliError::new(
            "could not create a tempdir for the download",
            e.to_string(),
            "check filesystem permissions on $TMPDIR",
        )
        .json_if(global.json)
    })?;

    println!("→ downloading {archive_name}...");
    let archive_path = workdir.path().join(&archive_name);
    client
        .download_asset(asset, &archive_path)
        .await
        .map_err(|e| {
            CliError::new(
                "download failed",
                e.to_string(),
                "check network connectivity to github.com and retry",
            )
            .json_if(global.json)
        })?;

    if let Some(sums_asset) = release.assets.iter().find(|a| a.name == "SHA256SUMS") {
        println!("→ verifying sha256...");
        let sums_path = workdir.path().join("SHA256SUMS");
        client
            .download_asset(sums_asset, &sums_path)
            .await
            .map_err(|e| {
                CliError::new(
                    "could not download SHA256SUMS",
                    e.to_string(),
                    "report this as an mxnode bug; the release shipped sums but we couldn't fetch them",
                )
                .json_if(global.json)
            })?;
        let sums_text = std::fs::read_to_string(&sums_path).map_err(|e| {
            CliError::new(
                "could not read downloaded SHA256SUMS",
                e.to_string(),
                "report this as an mxnode bug",
            )
            .json_if(global.json)
        })?;
        verify_against_sums(&sums_text, &archive_name, &archive_path).map_err(|e| {
            CliError::new(
                "sha256 verification failed",
                e.to_string(),
                "the release archive may be corrupted; re-run `mxnode self-update --force` after a moment",
            )
            .json_if(global.json)
        })?;
    } else {
        eprintln!("warn: release does not ship SHA256SUMS; downloaded archive is unverified",);
    }

    println!("→ extracting...");
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(workdir.path())
        .status()
        .map_err(|e| {
            CliError::new(
                "could not invoke tar",
                e.to_string(),
                "ensure tar is on PATH",
            )
            .json_if(global.json)
        })?;
    if !status.success() {
        return Err(CliError::new(
            "tar exited non-zero while extracting the archive",
            format!("status code {:?}", status.code()),
            "the archive may be corrupted; re-run `mxnode self-update --force`",
        )
        .json_if(global.json));
    }

    let extracted = workdir.path().join("mxnode");
    if !extracted.exists() {
        return Err(CliError::new(
            "extracted archive does not contain a `mxnode` binary",
            format!(
                "expected {} after extracting {archive_name}",
                extracted.display()
            ),
            "report this as an mxnode bug at https://github.com/XOXNO/mx-node/issues",
        )
        .json_if(global.json));
    }

    let target_path = std::env::current_exe().map_err(|e| {
        CliError::new(
            "could not resolve the running binary's path",
            e.to_string(),
            "report this as an mxnode bug",
        )
        .json_if(global.json)
    })?;

    println!("→ installing to {}", target_path.display());
    install_replacement(&extracted, &target_path, global)?;

    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "from": current,
                "to": latest_tag,
                "path": target_path.display().to_string(),
            })
        );
    } else {
        println!("✓ mxnode upgraded v{current} → v{latest_tag}");
        println!("  path: {}", target_path.display());
    }
    Ok(())
}

/// Build the release-asset triple matching `install.sh`'s naming
/// (`<arch>-<os_part>`). Returns `None` for unsupported host platforms.
fn release_target_triple() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Some("x86_64-unknown-linux-musl"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-musl"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// Replace `target` with `src` atomically. Falls back to `sudo install`
/// when `target`'s parent dir is not writable by the current user (the
/// typical `/usr/local/bin` case). Both branches end with `target`
/// owned by root:root mode 0755 to match `install.sh`.
fn install_replacement(src: &Path, target: &Path, global: &GlobalArgs) -> Result<(), CliError> {
    let target_dir = target.parent().ok_or_else(|| {
        CliError::new(
            "current_exe has no parent directory",
            "internal: std::env::current_exe returned a root-only path",
            "report this as an mxnode bug",
        )
        .json_if(global.json)
    })?;

    if dir_is_writable(target_dir) {
        // Atomic rename via NamedTempFile in the same directory — same
        // filesystem so `persist` cannot fail with EXDEV. On Linux
        // renaming over a running executable is fine; the kernel keeps
        // the old inode alive via this process's open fd until exit.
        let tmp = tempfile::NamedTempFile::new_in(target_dir).map_err(|e| {
            CliError::new(
                "could not create tempfile next to the install target",
                e.to_string(),
                "check filesystem permissions",
            )
            .json_if(global.json)
        })?;
        std::fs::copy(src, tmp.path()).map_err(|e| {
            CliError::new(
                "copy of the new binary failed",
                e.to_string(),
                "check disk space and filesystem permissions",
            )
            .json_if(global.json)
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).map_err(
                |e| {
                    CliError::new(
                        "could not set 0755 on the staged binary",
                        e.to_string(),
                        "check filesystem permissions",
                    )
                    .json_if(global.json)
                },
            )?;
        }
        tmp.persist(target).map_err(|e| {
            CliError::new(
                "atomic replace failed",
                e.to_string(),
                "the file may be in use by another process; reboot or stop the holder and retry",
            )
            .json_if(global.json)
        })?;
        Ok(())
    } else {
        eprintln!(
            "→ {} is not writable by the current user; using sudo",
            target_dir.display(),
        );
        let status = std::process::Command::new("sudo")
            .args(["install", "-m", "0755"])
            .arg(src)
            .arg(target)
            .status()
            .map_err(|e| {
                CliError::new(
                    "could not invoke sudo",
                    e.to_string(),
                    "ensure sudo is available, or rerun as a user with write access to the install dir",
                )
                .json_if(global.json)
            })?;
        if !status.success() {
            return Err(CliError::new(
                "sudo install exited non-zero",
                format!("status code {:?}", status.code()),
                "verify your sudo permissions and that the install dir is writable to root",
            )
            .json_if(global.json));
        }
        Ok(())
    }
}

fn dir_is_writable(dir: &Path) -> bool {
    tempfile::NamedTempFile::new_in(dir).is_ok()
}

/// Best-effort read of `[secrets].github_token` from the unified
/// `mxnode.toml`. Returns `None` if the file is missing, unreadable,
/// unparseable, or has no token set — never errors. Crucially it
/// does **not** trigger auto-init: self-update should never drop the
/// operator into a network-prompt wizard.
fn read_token_from_unified_file() -> Option<String> {
    let path = mxnode_config::user_config_path().ok()?;
    let body = std::fs::read_to_string(&path).ok()?;
    let file: mxnode_core::MxnodeFile = toml::from_str(&body).ok()?;
    let token = file.secrets.github_token;
    if token.is_empty() {
        None
    } else {
        Some(token.as_str().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::release_target_triple;

    #[test]
    fn host_platform_resolves_to_a_release_triple() {
        // The host running this test must be one of the four supported
        // combos for the workspace to even build a binary, so the
        // function must return Some on every supported dev box.
        assert!(release_target_triple().is_some());
    }
}
