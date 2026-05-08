//! Auto-init: write a sparse `~/.config/mxnode/mxnode.toml` from
//! detected environment when no config exists. Called transparently
//! by `Runtime::from_global` on first use of any state-changing
//! command. There is no user-facing `mxnode init` — the workflow is
//! "run any command, the config gets created on the fly with
//! sensible defaults; switch network later via `mxnode config set`".

use std::path::Path;

use mxnode_config::user_config_path;
use mxnode_core::{ArtifactSource, Environment};

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::orchestrator::runtime::CliErrorExt;

/// Write a fresh config with auto-detected `$USER`/`$HOME` and the
/// operator-chosen network (mainnet by default). No-op (returns
/// `Ok(None)`) if the file already exists, since the runtime caller
/// has already verified `ConfigSource::None` — but defensive against
/// TOCTOU. On success, returns `Some(Environment)` so the runtime
/// banner can name the actual network the operator picked.
pub fn auto_init(global: &GlobalArgs) -> Result<Option<Environment>, CliError> {
    let target = user_config_path().map_err(|e| {
        CliError::new(
            "could not determine where to write mxnode.toml",
            e.to_string(),
            "set $XDG_CONFIG_HOME or $HOME so mxnode can place the file under <home>/.config/mxnode/",
        )
        .json_if(global.json)
    })?;
    if target.exists() {
        // Concurrent writer raced us; respect what's there.
        return Ok(None);
    }

    // Prompt for the network on a TTY; the `--json` and non-TTY paths
    // fall through to the historical default so CI / automation is
    // never blocked waiting for stdin.
    let interactive = !global.json && std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let network = super::prompts::prompt_for_network(&mut stdin, &mut stdout, interactive)
        .map_err(|e| {
            CliError::new(
                "failed to read network choice from stdin",
                e.to_string(),
                "rerun with `--json` to skip the prompt and use mainnet",
            )
            .json_if(global.json)
        })?;

    let answers = build_answers(network);
    let body = render_sparse_config(&answers);
    write_config(&target, &body, global)?;
    Ok(Some(network))
}

#[derive(Debug)]
struct Answers {
    network: Environment,
    github_org: String,
    name_template: String,
    artifact_source: ArtifactSource,
}

fn build_answers(network: Environment) -> Answers {
    Answers {
        network,
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

    // Don't bake `custom_home` / `custom_user` / `node_keys` into the
    // file — they resolve from the runtime `$HOME` / `$USER` of
    // whoever runs `mxnode` (see `mxnode_config::resolve_paths`).
    // Writing them at init time risks pinning a stale value (e.g.
    // the schema default `/home/ubuntu` on a host where the operator
    // user is `truststaking`); operators on shared-deploy layouts
    // opt in by setting them explicitly later via `mxnode config set
    // paths.custom_home <path>`.

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
            "failed to write mxnode.toml",
            e.to_string(),
            "ensure the parent directory is writable",
        )
        .json_if(global.json)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers_with(name_template: &str) -> Answers {
        Answers {
            network: Environment::Mainnet,
            github_org: "multiversx".to_string(),
            name_template: name_template.to_string(),
            artifact_source: ArtifactSource::Source,
        }
    }

    #[test]
    fn render_round_trips_through_toml() {
        // Renderer escaping must produce valid TOML even when answers
        // carry `"` / `\` / newlines (future-proof against a wizard
        // that might let operators type values containing these).
        let answers = answers_with("mx-chain-{env}\nwith-newline");
        let rendered = render_sparse_config(&answers);
        // The only assertion that matters: it parses back.
        let _: toml::Value = toml::from_str(&rendered).expect("must parse back");
    }

    #[test]
    fn render_omits_custom_user_and_home() {
        // Post-v0.8.33: auto-init does NOT bake `custom_user` /
        // `custom_home` into the file. They resolve from the runtime
        // `$USER` / `$HOME` of whoever runs `mxnode`, so the same
        // file works on every host without re-init. Operators on
        // shared-deploy layouts opt in by setting them explicitly.
        let answers = answers_with("mx-chain-{env}-validator-{index}");
        let rendered = render_sparse_config(&answers);
        let parsed: toml::Value = toml::from_str(&rendered).expect("must parse back");
        // `[paths]` may exist (other fields), but custom_home/user
        // must be absent so the resolver picks them up from env.
        if let Some(paths) = parsed.get("paths") {
            assert!(
                paths.get("custom_home").is_none(),
                "custom_home must not be persisted: {paths:?}",
            );
            assert!(
                paths.get("custom_user").is_none(),
                "custom_user must not be persisted: {paths:?}",
            );
        }
    }
}
