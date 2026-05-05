//! `mxnode completions <shell>`: generate shell completion scripts from the
//! actual clap parser. This replaces the legacy Bash script's hardcoded
//! top-level completion list without mutating `/etc/bash_completion.d`.

use clap::CommandFactory;

use crate::cli::{Cli, CompletionsArgs, GlobalArgs};
use crate::errors::CliError;

pub fn run(args: CompletionsArgs, global: &GlobalArgs) -> Result<(), CliError> {
    if global.json {
        return Err(CliError::new(
            "completions cannot emit JSON",
            "shell completion scripts must be written as shell code",
            "rerun without --json and redirect stdout into your shell completion path",
        )
        .json());
    }

    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(args.shell, &mut cmd, bin_name, &mut std::io::stdout());
    Ok(())
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
        no_update_check: true,
        }
    }

    #[test]
    fn bash_completion_mentions_nested_commands() {
        let mut cmd = Cli::command();
        let mut out = Vec::new();
        clap_complete::generate(clap_complete::Shell::Bash, &mut cmd, "mxnode", &mut out);
        let body = String::from_utf8(out).unwrap();
        assert!(body.contains("mxnode"));
        assert!(body.contains("migrate-bash"));
        assert!(body.contains("completions"));
    }

    #[test]
    fn json_mode_is_rejected() {
        let err = run(
            CompletionsArgs {
                shell: clap_complete::Shell::Bash,
            },
            &global(true),
        )
        .unwrap_err();
        assert!(err.summary.contains("cannot emit JSON"));
        assert!(err.json);
    }
}
