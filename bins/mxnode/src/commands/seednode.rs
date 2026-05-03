//! `mxnode seednode`: wrapper around the upstream seednode utility installed
//! under `$CUSTOM_HOME/elrond-utils/seednode/`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::{GlobalArgs, SeednodeArgs};
use crate::errors::CliError;
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

pub fn run(args: SeednodeArgs, global: &GlobalArgs) -> Result<(), CliError> {
    if global.json && !args.dry_run {
        return Err(CliError::new(
            "seednode cannot stream JSON",
            "the upstream seednode binary writes its own terminal output",
            "rerun without --json, or use --dry-run --json to inspect the command",
        )
        .json());
    }

    let runtime = Runtime::from_global(global)?;
    let seednode_dir = seednode_dir(&runtime);
    let binary = seednode_dir.join("seednode");
    if args.dry_run {
        return emit_dry_run(&seednode_dir, &binary, &args.args, global);
    }
    if !binary.exists() {
        return Err(CliError::new(
            "seednode binary is not installed",
            format!("expected {}", binary.display()),
            "run `mxnode install` first, or place the seednode binary manually under elrond-utils/seednode/",
        )
        .json_if(global.json));
    }

    let status = Command::new(&binary)
        .current_dir(&seednode_dir)
        .args(&args.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            CliError::new(
                "failed to invoke seednode",
                format!("{}: {e}", binary.display()),
                "ensure the seednode binary has execute permissions",
            )
            .json_if(global.json)
        })?;
    if !status.success() {
        return Err(CliError::new(
            "seednode exited non-zero",
            format!("status code {:?}", status.code()),
            "inspect stdout/stderr above; pass `-- --help` to see upstream seednode flags",
        )
        .json_if(global.json));
    }
    Ok(())
}

fn seednode_dir(runtime: &Runtime) -> PathBuf {
    runtime.paths.elrond_utils_root().join("seednode")
}

fn emit_dry_run(
    seednode_dir: &Path,
    binary: &Path,
    args: &[String],
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let command = seednode_command(binary, args);
    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "mode": "dry-run",
                "workdir": seednode_dir.display().to_string(),
                "binary": binary.display().to_string(),
                "binary_exists": binary.exists(),
                "command": command,
            })
        );
    } else {
        println!("dry-run seednode");
        println!("  cd {}", seednode_dir.display());
        println!("  {}", shell_join(&command));
    }
    Ok(())
}

fn seednode_command(binary: &Path, args: &[String]) -> Vec<String> {
    let mut command = Vec::with_capacity(args.len() + 1);
    command.push(binary.display().to_string());
    command.extend(args.iter().cloned());
    command
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            if p.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '*')
            }) {
                p.clone()
            } else {
                format!("'{}'", p.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global(json: bool) -> GlobalArgs {
        GlobalArgs {
            config: None,
            skip_safety_checks: false,
            json,
            no_color: false,
            verbose: false,
            quiet: false,
        }
    }

    #[test]
    fn seednode_command_appends_args() {
        let command = seednode_command(
            Path::new("/home/ubuntu/elrond-utils/seednode/seednode"),
            &["--port".to_string(), "12000".to_string()],
        );
        assert_eq!(
            command,
            vec![
                "/home/ubuntu/elrond-utils/seednode/seednode",
                "--port",
                "12000"
            ]
        );
    }

    #[test]
    fn dry_run_json_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        emit_dry_run(
            tmp.path(),
            &tmp.path().join("seednode"),
            &["--help".to_string()],
            &global(true),
        )
        .unwrap();
    }
}
