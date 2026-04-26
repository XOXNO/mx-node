use mxnode_config::{load, validate, ConfigSource, LoadOptions, Scope as ConfigScope};
use serde::Serialize;

use crate::cli::{ConfigCommand, Format, GlobalArgs};
use crate::errors::CliError;

pub fn run(cmd: ConfigCommand, global: &GlobalArgs) -> Result<(), CliError> {
    match cmd {
        ConfigCommand::Show { origin, format } => show(origin, format, global),
        ConfigCommand::Get { path } => get(path, global),
        ConfigCommand::Validate { strict } => run_validate(strict, global),
        ConfigCommand::Set { .. } | ConfigCommand::Edit { .. } => Err(CliError::new(
            "config set/edit not yet implemented",
            "Phase 0 ships read-only access; write paths land in Phase 1.",
            "use `mxnode config edit` once Phase 1 ships, or hand-edit ~/.config/mxnode/config.toml for now",
        )
        .json_if(global.json)),
    }
}

/// Stable, machine-readable summary of where the config layer came from.
/// Replaces the previous `Debug` repr (`"File { scope: User, path: \"...\" }"`)
/// with a typed shape so downstream tools can rely on it.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum SourceView {
    None,
    File { scope: &'static str, path: String },
    Explicit { path: String },
}

impl From<&ConfigSource> for SourceView {
    fn from(value: &ConfigSource) -> Self {
        match value {
            ConfigSource::None => SourceView::None,
            ConfigSource::File { scope, path } => SourceView::File {
                scope: match scope {
                    ConfigScope::User => "user",
                    ConfigScope::System => "system",
                },
                path: path.display().to_string(),
            },
            ConfigSource::Explicit(path) => SourceView::Explicit {
                path: path.display().to_string(),
            },
        }
    }
}

fn show(origin: bool, format: Format, global: &GlobalArgs) -> Result<(), CliError> {
    let opts = build_load_options(global);
    let loaded = load(&opts).map_err(|e| {
        CliError::new(
            "failed to load config",
            e.to_string(),
            "run `mxnode config validate` to see what mxnode could parse, or fix the file at the path shown",
        )
        .json_if(global.json)
    })?;

    // Universal --json (D10) wins over the per-command --format: when the
    // operator passes --json, every output is JSON regardless of --format.
    let effective_format = if global.json { Format::Json } else { format };

    match effective_format {
        Format::Toml => {
            let body = toml::to_string_pretty(&loaded.config).map_err(|e| {
                CliError::new(
                    "failed to render config as TOML",
                    e.to_string(),
                    "report this as a bug; the in-memory config diverged from the schema",
                )
                .json_if(global.json)
            })?;
            if origin {
                let source = SourceView::from(&loaded.source);
                println!("# config source: {}", serde_json::to_string(&source).unwrap_or_default());
                println!("# per-leaf origins:");
                for (path, origin) in loaded.origins.iter() {
                    println!("#   {} = {}", path, origin.label());
                }
            }
            print!("{body}");
        }
        Format::Json => {
            let origins = loaded
                .origins
                .iter()
                .map(|(k, v)| (k.clone(), v.label()))
                .collect::<std::collections::BTreeMap<_, _>>();
            let payload = serde_json::json!({
                "config": loaded.config,
                "source": SourceView::from(&loaded.source),
                "origins": origins,
            });
            println!("{payload}");
        }
    }
    Ok(())
}

fn get(path: String, global: &GlobalArgs) -> Result<(), CliError> {
    let opts = build_load_options(global);
    let loaded = load(&opts).map_err(|e| {
        CliError::new(
            "failed to load config",
            e.to_string(),
            "run `mxnode config validate` for details",
        )
        .json_if(global.json)
    })?;
    let body = toml::to_string(&loaded.config).map_err(|e| {
        CliError::new(
            "failed to serialize config",
            e.to_string(),
            "report this as a bug",
        )
        .json_if(global.json)
    })?;
    let value: toml::Value = toml::from_str(&body).map_err(|e| {
        CliError::new(
            "failed to reparse config",
            e.to_string(),
            "report this as a bug",
        )
        .json_if(global.json)
    })?;
    match resolve_path(&value, &path) {
        Some(v) => {
            if global.json {
                let json = serde_json::json!({ "path": path, "value": toml_to_json(v) });
                println!("{json}");
            } else {
                println!("{}", render_scalar(v));
            }
            Ok(())
        }
        None => Err(CliError::new(
            format!("no such config key: {path}"),
            "the dotted path did not resolve to a leaf",
            "run `mxnode config show` to list available keys",
        )
        .json_if(global.json)),
    }
}

fn run_validate(strict: bool, global: &GlobalArgs) -> Result<(), CliError> {
    let opts = build_load_options(global);
    let loaded = load(&opts).map_err(|e| {
        CliError::new(
            "failed to load config",
            e.to_string(),
            "fix the config file at the path shown above and rerun",
        )
        .json_if(global.json)
    })?;
    let mut report = validate(&loaded.config);
    if strict {
        report.warnings.push(
            "--strict checks (network reachability, token validity) are not yet wired up; \
             add them once mxnode-github gains an offline-tolerant probe path".to_string(),
        );
    }

    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": report.ok(),
                "errors": report.errors,
                "warnings": report.warnings,
            })
        );
    } else {
        for w in &report.warnings {
            println!("warn: {w}");
        }
        for e in &report.errors {
            println!("error: {e}");
        }
        if report.ok() {
            println!("ok");
        }
    }

    if report.ok() {
        Ok(())
    } else {
        Err(CliError::new(
            "config validation failed",
            format!("{} error(s)", report.errors.len()),
            "fix the errors listed above (or pass --skip-safety-checks for read-only ops)",
        )
        .json_if(global.json))
    }
}

fn build_load_options(global: &GlobalArgs) -> LoadOptions {
    LoadOptions {
        config_path: global.config.clone(),
        flags_overlay: None,
    }
}

fn resolve_path<'a>(value: &'a toml::Value, dotted: &str) -> Option<&'a toml::Value> {
    let mut cursor = value;
    for segment in dotted.split('.') {
        cursor = cursor.get(segment)?;
    }
    Some(cursor)
}

fn render_scalar(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(dt) => dt.to_string(),
        other => toml::to_string(other).unwrap_or_else(|_| String::from("<error>")),
    }
}

fn toml_to_json(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(arr) => serde_json::Value::Array(arr.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            let map: serde_json::Map<String, serde_json::Value> = t
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
    }
}

trait CliErrorExt {
    fn json_if(self, json: bool) -> CliError;
}

impl CliErrorExt for CliError {
    fn json_if(self, json: bool) -> CliError {
        if json {
            self.json()
        } else {
            self
        }
    }
}
