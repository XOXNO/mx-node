//! `mxnode init`: write a sparse `~/.config/mxnode/config.toml` from
//! interactive prompts (or `--no-prompt` flags). Tokens are loaded from
//! `--token-file` only and never logged.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use mxnode_config::{user_config_path, xdg_config_home};
use mxnode_core::{ArtifactSource, Environment};

use crate::cli::{GlobalArgs, InitArgs, NetworkArg};
use crate::errors::CliError;
use crate::orchestrator::runtime::CliErrorExt;

pub fn run(args: InitArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let target = user_config_path().map_err(|e| {
        CliError::new(
            "could not determine where to write config.toml",
            e.to_string(),
            "set $XDG_CONFIG_HOME or $HOME so mxnode can place the file under <home>/.config/mxnode/",
        )
        .json_if(global.json)
    })?;
    if target.exists() {
        return Err(CliError::new(
            "config.toml already exists",
            format!("{} already exists", target.display()),
            "edit the file directly or back it up before re-running init",
        )
        .json_if(global.json));
    }

    let answers = if args.no_prompt {
        AnswersBuilder::from_args(&args)?
    } else if std::io::stdin().is_terminal() {
        AnswersBuilder::interactive(&args)?
    } else {
        // Non-interactive context (CI, systemd-run, etc.) without
        // --no-prompt: refuse rather than hang reading stdin.
        return Err(CliError::new(
            "init refuses to prompt: stdin is not a terminal",
            "non-interactive mode requires --no-prompt + per-field flags",
            "rerun with `mxnode init --no-prompt --network <env> --user <name> ...`",
        )
        .json_if(global.json));
    };

    let token = read_token(args.token_file.as_deref())?;
    let body = render_sparse_config(&answers, token.as_deref());
    write_config(&target, &body, global)?;

    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "wrote": target.display().to_string(),
                "token_loaded": token.is_some(),
            })
        );
    } else {
        println!("wrote {}", target.display());
        if token.is_some() {
            println!("  github token loaded from {:?}", args.token_file.unwrap_or_default());
        }
        println!();
        println!("next: place node-{{0..N-1}}.zip under {}", answers.key_dir.display());
        println!("      then run `mxnode install --binary-tag <T> --config-tag <T>`.");
        println!("      (or `mxnode adopt` if you already have a bash-installed setup)");
    }
    Ok(())
}

#[derive(Debug)]
struct Answers {
    network: Environment,
    custom_user: String,
    custom_home: PathBuf,
    key_dir: PathBuf,
    github_org: String,
    name_template: String,
    artifact_source: ArtifactSource,
}

struct AnswersBuilder;

impl AnswersBuilder {
    fn from_args(args: &InitArgs) -> Result<Answers, CliError> {
        let network = args
            .network
            .map(network_arg_to_env)
            .ok_or_else(|| {
                CliError::new(
                    "--no-prompt requires --network",
                    "non-interactive init must be supplied with the chosen network",
                    "pass --network mainnet|testnet|devnet",
                )
            })?;
        // Default `custom_home` + `custom_user` differ per platform:
        // Linux operators have a service user (`ubuntu` on the bash
        // mainnet AMI); macOS hosts run LaunchAgents as the operator
        // and don't have a service-user concept.
        let (default_home, default_user) = platform_defaults();
        let custom_user = args.user.clone().unwrap_or(default_user);
        let custom_home = args.home.clone().unwrap_or(default_home);
        let key_dir = custom_home.join("VALIDATOR_KEYS");
        Ok(Answers {
            network,
            custom_user,
            custom_home,
            key_dir,
            github_org: "multiversx".to_string(),
            name_template: "mx-chain-{env}-validator-{index}".to_string(),
            artifact_source: ArtifactSource::Source,
        })
    }

    fn interactive(args: &InitArgs) -> Result<Answers, CliError> {
        use dialoguer::{theme::ColorfulTheme, Input, Select};

        println!("mxnode init — let's set up your config.");
        println!("This writes {}.\n", display_target());
        println!("(Use ↑/↓ + Enter on selection prompts; Ctrl+C cancels.)\n");

        let theme = ColorfulTheme::default();
        let (default_home, default_user) = platform_defaults();
        let user_label = match mxnode_core::Platform::current() {
            mxnode_core::Platform::Macos => "Operator user (launchd runs as you)",
            _ => "Custom user (systemd User=)",
        };

        // 1. Network — radio Select instead of free-form text. Network
        //    affects everything downstream so getting it right matters
        //    more than typing speed.
        let network = match args.network {
            Some(n) => network_arg_to_env(n),
            None => {
                let items = ["mainnet", "testnet", "devnet"];
                let default_idx = 0;
                let pick = Select::with_theme(&theme)
                    .with_prompt("Network")
                    .items(&items)
                    .default(default_idx)
                    .interact()
                    .map_err(prompt_err)?;
                match pick {
                    1 => Environment::Testnet,
                    2 => Environment::Devnet,
                    _ => Environment::Mainnet,
                }
            }
        };

        // 2. Operator identity — text inputs with sensible defaults.
        let custom_user: String = Input::with_theme(&theme)
            .with_prompt(user_label)
            .default(args.user.clone().unwrap_or(default_user))
            .interact_text()
            .map_err(prompt_err)?;
        let custom_home_str: String = Input::with_theme(&theme)
            .with_prompt("Custom home directory")
            .default(
                args.home
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| default_home.display().to_string()),
            )
            .interact_text()
            .map_err(prompt_err)?;
        let key_dir_str: String = Input::with_theme(&theme)
            .with_prompt("Validator key location")
            .default(format!("{custom_home_str}/VALIDATOR_KEYS"))
            .interact_text()
            .map_err(prompt_err)?;

        // 3. GitHub org — defaults to upstream; operators on a fork
        //    type their org name here.
        let github_org: String = Input::with_theme(&theme)
            .with_prompt("GitHub org (your fork or `multiversx`)")
            .default("multiversx".to_string())
            .interact_text()
            .map_err(prompt_err)?;

        // 4. Node base name — operators usually want `<brand>-<index>`
        //    (e.g. `truststaking-0`, `truststaking-1`). We ask for the
        //    base, then synthesize the template the orchestrator wants.
        //    Power users who need an `{env}` placeholder can edit
        //    config.toml after — covered in the trailing message.
        let env_label = network.as_str();
        let default_base = format!("{env_label}-validator");
        let base_name: String = Input::with_theme(&theme)
            .with_prompt("Node base name (will become <name>-0, <name>-1, ...)")
            .default(default_base)
            .interact_text()
            .map_err(prompt_err)?;
        let name_template = format!("{}-{{index}}", base_name.trim());

        // 5. Artifact source — radio Select. `source` is the
        //    historical default; release-mode is a power-user pick
        //    where pre-built binaries exist for the chosen tag.
        let sources = ["source (build with go)", "release (download zip)", "auto"];
        let pick = Select::with_theme(&theme)
            .with_prompt("Artifact source")
            .items(&sources)
            .default(0)
            .interact()
            .map_err(prompt_err)?;
        let artifact_source = match pick {
            1 => ArtifactSource::Release,
            2 => ArtifactSource::Auto,
            _ => ArtifactSource::Source,
        };

        Ok(Answers {
            network,
            custom_user,
            custom_home: PathBuf::from(custom_home_str),
            key_dir: PathBuf::from(key_dir_str),
            github_org,
            name_template,
            artifact_source,
        })
    }
}

/// Map a `dialoguer::Error` (Ctrl+C, EOF on stdin, terminal lost,
/// terminal can't render dialoguer's escape sequences) into our
/// 3-line CLI error shape.
fn prompt_err(e: dialoguer::Error) -> CliError {
    CliError::new(
        "init prompt cancelled or stdin closed",
        e.to_string(),
        "rerun `mxnode init` interactively, or use --no-prompt with explicit flags",
    )
}

fn network_arg_to_env(arg: NetworkArg) -> Environment {
    match arg {
        NetworkArg::Mainnet => Environment::Mainnet,
        NetworkArg::Testnet => Environment::Testnet,
        NetworkArg::Devnet => Environment::Devnet,
    }
}

fn read_token(path: Option<&Path>) -> Result<Option<String>, CliError> {
    let Some(p) = path else { return Ok(None) };
    let raw = std::fs::read_to_string(p).map_err(|e| {
        CliError::new(
            "failed to read --token-file",
            e.to_string(),
            "ensure the file exists and is readable by the current user",
        )
    })?;
    let trimmed = raw.trim().to_string();
    Ok(if trimmed.is_empty() { None } else { Some(trimmed) })
}

/// Build the sparse config as a typed `toml::Value::Table` and serialize via
/// `toml::to_string_pretty`. Lets the toml crate handle quoting/escaping
/// instead of relying on hand-rolled `format!` (which breaks the moment a
/// user-supplied value contains `"`, `\`, or a newline).
fn render_sparse_config(answers: &Answers, token: Option<&str>) -> String {
    use toml::map::Map;
    use toml::Value;

    let mut root: Map<String, Value> = Map::new();
    root.insert("schema_version".to_string(), Value::Integer(1));

    // [network]
    let mut network: Map<String, Value> = Map::new();
    network.insert(
        "environment".to_string(),
        Value::String(answers.network.to_string()),
    );
    if answers.github_org != "multiversx" {
        network.insert(
            "github_org".to_string(),
            Value::String(answers.github_org.clone()),
        );
    }
    root.insert("network".to_string(), Value::Table(network));

    // [paths]
    let mut paths: Map<String, Value> = Map::new();
    paths.insert(
        "custom_home".to_string(),
        Value::String(answers.custom_home.display().to_string()),
    );
    if answers.custom_user != "ubuntu" {
        paths.insert(
            "custom_user".to_string(),
            Value::String(answers.custom_user.clone()),
        );
    }
    let custom_home_str = answers.custom_home.display().to_string();
    let key_dir_str = answers
        .key_dir
        .display()
        .to_string()
        .replacen(&custom_home_str, "{custom_home}", 1);
    paths.insert("node_keys".to_string(), Value::String(key_dir_str));
    root.insert("paths".to_string(), Value::Table(paths));

    if answers.name_template != "mx-chain-{env}-validator-{index}" {
        let mut node: Map<String, Value> = Map::new();
        node.insert(
            "name_template".to_string(),
            Value::String(answers.name_template.clone()),
        );
        root.insert("node".to_string(), Value::Table(node));
    }

    if answers.artifact_source != ArtifactSource::Source {
        let mut install: Map<String, Value> = Map::new();
        install.insert(
            "artifact_source".to_string(),
            Value::String(answers.artifact_source.to_string()),
        );
        root.insert("install".to_string(), Value::Table(install));
    }

    let body = toml::to_string_pretty(&Value::Table(root))
        .expect("toml::Value serialisation cannot fail for known-shape values");

    let mut out = String::new();
    out.push_str("# mxnode config — generated by `mxnode init`.\n");
    out.push_str("# Edit freely; mxnode preserves unknown keys on round-trip.\n\n");
    out.push_str(&body);

    if let Some(_token) = token {
        // Tokens are NEVER written to the config file. They live in env
        // (`MXNODE_GITHUB_TOKEN`) so they don't end up in shell history,
        // backups, or `mxnode config show` output.
        out.push_str("\n# A token was supplied via --token-file. Set it as an environment\n");
        out.push_str("# variable instead — mxnode never writes secrets to disk:\n");
        out.push_str("#   export MXNODE_GITHUB_TOKEN=...\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers_with(custom_user: &str, name_template: &str) -> Answers {
        Answers {
            network: Environment::Mainnet,
            custom_user: custom_user.to_string(),
            custom_home: std::path::PathBuf::from("/home/ubuntu"),
            key_dir: std::path::PathBuf::from("/home/ubuntu/VALIDATOR_KEYS"),
            github_org: "multiversx".to_string(),
            name_template: name_template.to_string(),
            artifact_source: ArtifactSource::Source,
        }
    }

    #[test]
    fn render_handles_values_with_double_quotes() {
        // Operator answered the prompt with a value containing `"` —
        // hand-rolled format! would break TOML; toml::to_string_pretty
        // escapes correctly.
        let answers = answers_with("validator-with\"quote", "mx-chain-{env}-validator-{index}");
        let rendered = render_sparse_config(&answers, None);
        // Round-trip parse — invalid TOML would fail here.
        let parsed: toml::Value = toml::from_str(&rendered).expect("must parse back");
        assert_eq!(
            parsed["paths"]["custom_user"].as_str(),
            Some("validator-with\"quote"),
        );
    }

    #[test]
    fn render_handles_values_with_newlines() {
        let answers = answers_with("ubuntu", "mx\nchain-{env}");
        let rendered = render_sparse_config(&answers, None);
        let parsed: toml::Value = toml::from_str(&rendered).expect("must parse back");
        assert_eq!(parsed["node"]["name_template"].as_str(), Some("mx\nchain-{env}"));
    }

    #[test]
    fn render_handles_values_with_backslash() {
        let answers = answers_with(r"ubuntu\windows", "mx-chain-{env}-validator-{index}");
        let rendered = render_sparse_config(&answers, None);
        let parsed: toml::Value = toml::from_str(&rendered).expect("must parse back");
        assert_eq!(
            parsed["paths"]["custom_user"].as_str(),
            Some(r"ubuntu\windows"),
        );
    }

    #[test]
    fn render_omits_default_user_to_keep_file_minimal() {
        let answers = answers_with("ubuntu", "mx-chain-{env}-validator-{index}");
        let rendered = render_sparse_config(&answers, None);
        assert!(
            !rendered.contains("custom_user"),
            "default user should be omitted; got:\n{rendered}",
        );
    }

    #[test]
    fn render_token_comment_appears_when_token_supplied() {
        let answers = answers_with("ubuntu", "mx-chain-{env}-validator-{index}");
        let with = render_sparse_config(&answers, Some("ghp_dummy"));
        let without = render_sparse_config(&answers, None);
        assert!(with.contains("MXNODE_GITHUB_TOKEN"));
        assert!(!without.contains("MXNODE_GITHUB_TOKEN"));
    }
}

fn write_config(target: &Path, body: &str, global: &GlobalArgs) -> Result<(), CliError> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CliError::new(
                "failed to create config directory",
                e.to_string(),
                "ensure the parent directory is writable",
            )
            .json_if(global.json)
        })?;
    }
    std::fs::write(target, body).map_err(|e| {
        CliError::new(
            "failed to write config.toml",
            e.to_string(),
            "ensure the parent directory is writable",
        )
        .json_if(global.json)
    })?;
    Ok(())
}


fn display_target() -> String {
    xdg_config_home()
        .map(|p| p.join("mxnode/config.toml").display().to_string())
        .unwrap_or_else(|_| "~/.config/mxnode/config.toml".to_string())
}

/// Per-platform defaults for `(custom_home, custom_user)`.
///
/// Linux: `/home/ubuntu` + `ubuntu` (matches the bash mainnet AMI
/// convention). macOS: `$HOME/.mxnode` + the operator's username (no
/// service-user concept; LaunchAgents run as the operator).
fn platform_defaults() -> (PathBuf, String) {
    match mxnode_core::Platform::current() {
        mxnode_core::Platform::Macos => {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/Users/operator"));
            let user = std::env::var("USER")
                .or_else(|_| std::env::var("LOGNAME"))
                .unwrap_or_else(|_| "operator".to_string());
            (home.join(".mxnode"), user)
        }
        _ => (PathBuf::from("/home/ubuntu"), "ubuntu".to_string()),
    }
}
