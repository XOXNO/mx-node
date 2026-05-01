//! Auto-init: write a sparse `~/.config/mxnode/config.toml` from
//! detected environment when no config exists. Called transparently
//! by `Runtime::from_global` on first use of any state-changing
//! command. There is no user-facing `mxnode init` — the workflow is
//! "run any command, the config gets created on the fly with
//! sensible defaults; switch network later via `mxnode config set`".

use std::path::{Path, PathBuf};

use mxnode_config::user_config_path;
use mxnode_core::{ArtifactSource, Environment};

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::orchestrator::runtime::CliErrorExt;

/// Write a fresh config with auto-detected `$USER`/`$HOME` and
/// `network = mainnet`. No-op (returns Ok) if the file already
/// exists, since the runtime caller has already verified
/// `ConfigSource::None` — but defensive against TOCTOU.
pub fn auto_init(global: &GlobalArgs) -> Result<(), CliError> {
    let target = user_config_path().map_err(|e| {
        CliError::new(
            "could not determine where to write config.toml",
            e.to_string(),
            "set $XDG_CONFIG_HOME or $HOME so mxnode can place the file under <home>/.config/mxnode/",
        )
        .json_if(global.json)
    })?;
    if target.exists() {
        // Concurrent writer raced us; respect what's there.
        return Ok(());
    }
    let answers = build_answers();
    let body = render_sparse_config(&answers);
    write_config(&target, &body, global)
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

fn build_answers() -> Answers {
    let (custom_home, custom_user) = platform_defaults();
    let key_dir = custom_home.join("VALIDATOR_KEYS");
    Answers {
        network: Environment::Mainnet,
        custom_user,
        custom_home,
        key_dir,
        github_org: "multiversx".to_string(),
        name_template: "mx-chain-{env}-validator-{index}".to_string(),
        artifact_source: ArtifactSource::Source,
    }
}

/// Build the sparse config as a typed `toml::Value::Table` and
/// serialize via `toml::to_string_pretty`. Letting the toml crate
/// handle quoting/escaping is robust against operator-supplied values
/// containing `"`, `\`, or newlines (which a hand-rolled `format!`
/// would mangle).
fn render_sparse_config(answers: &Answers) -> String {
    use toml::map::Map;
    use toml::Value;

    let mut root: Map<String, Value> = Map::new();
    root.insert("schema_version".to_string(), Value::Integer(1));

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

    // Always emit `custom_home` + `custom_user` — the schema default
    // is `/home/ubuntu` + `ubuntu`, but auto-init detects the actual
    // login. Writing both keeps the file unambiguous: `mxnode config
    // show` reflects the host this config was generated on.
    let mut paths: Map<String, Value> = Map::new();
    paths.insert(
        "custom_home".to_string(),
        Value::String(answers.custom_home.display().to_string()),
    );
    paths.insert(
        "custom_user".to_string(),
        Value::String(answers.custom_user.clone()),
    );
    let custom_home_str = answers.custom_home.display().to_string();
    let key_dir_str =
        answers
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
    out.push_str("# mxnode config — generated automatically on first use.\n");
    out.push_str("# Edit freely; mxnode preserves unknown keys on round-trip.\n");
    out.push_str(
        "# Switch network with: `mxnode config set network.environment <testnet|devnet>`.\n\n",
    );
    out.push_str(&body);
    out
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

/// Per-host defaults for `(custom_home, custom_user)`. Detect the
/// operator's actual `$USER`/`$HOME` so the generated config maps to
/// the box mxnode is running on instead of the bash mainnet AMI's
/// hardcoded `/home/ubuntu`. `$USER` falls back through `$LOGNAME` →
/// the basename of `$HOME` → `"ubuntu"` so we always produce a
/// non-empty user even when env_clear() was used.
fn platform_defaults() -> (PathBuf, String) {
    let user = std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("LOGNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            std::env::var("HOME").ok().and_then(|h| {
                Path::new(&h)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty())
            })
        })
        .unwrap_or_else(|| "ubuntu".to_string());
    let home = dirs::home_dir().unwrap_or_else(|| match mxnode_core::Platform::current() {
        mxnode_core::Platform::Macos => PathBuf::from(format!("/Users/{user}")),
        _ => PathBuf::from(format!("/home/{user}")),
    });
    (home, user)
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
        // Unlikely in auto-init (we don't read operator input), but
        // future-proofs against a config-set path that might write
        // values containing `"`. Round-trip parse — invalid TOML
        // would fail here.
        let answers = answers_with("validator-with\"quote", "mx-chain-{env}-validator-{index}");
        let rendered = render_sparse_config(&answers);
        let parsed: toml::Value = toml::from_str(&rendered).expect("must parse back");
        assert_eq!(
            parsed["paths"]["custom_user"].as_str(),
            Some("validator-with\"quote"),
        );
    }

    #[test]
    fn render_handles_values_with_newlines() {
        let answers = answers_with("ubuntu", "mx\nchain-{env}");
        let rendered = render_sparse_config(&answers);
        let parsed: toml::Value = toml::from_str(&rendered).expect("must parse back");
        assert_eq!(
            parsed["node"]["name_template"].as_str(),
            Some("mx\nchain-{env}")
        );
    }

    #[test]
    fn render_handles_values_with_backslash() {
        let answers = answers_with(r"ubuntu\windows", "mx-chain-{env}-validator-{index}");
        let rendered = render_sparse_config(&answers);
        let parsed: toml::Value = toml::from_str(&rendered).expect("must parse back");
        assert_eq!(
            parsed["paths"]["custom_user"].as_str(),
            Some(r"ubuntu\windows"),
        );
    }

    #[test]
    fn render_always_emits_custom_user_and_home() {
        // Auto-detect produces the host's actual login; the schema
        // default (`ubuntu`/`/home/ubuntu`) is rarely correct. Both
        // fields land in the file unconditionally so `config show`
        // reflects this host.
        let answers = answers_with("ubuntu", "mx-chain-{env}-validator-{index}");
        let rendered = render_sparse_config(&answers);
        let parsed: toml::Value = toml::from_str(&rendered).expect("must parse back");
        assert_eq!(parsed["paths"]["custom_user"].as_str(), Some("ubuntu"));
        assert_eq!(
            parsed["paths"]["custom_home"].as_str(),
            Some("/home/ubuntu")
        );
    }
}
