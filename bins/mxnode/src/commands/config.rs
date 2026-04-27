use std::path::{Path, PathBuf};
use std::process::Command;

use mxnode_config::{
    load, system_config_path, user_config_path, validate, ConfigSource, LoadOptions,
    Scope as ConfigScope,
};
use serde::Serialize;
use toml_edit::{value, DocumentMut};

use crate::cli::{ConfigCommand, Format, GlobalArgs, Scope as ScopeArg};
use crate::errors::CliError;

pub fn run(cmd: ConfigCommand, global: &GlobalArgs) -> Result<(), CliError> {
    match cmd {
        ConfigCommand::Show { origin, format } => show(origin, format, global),
        ConfigCommand::Get { path } => get(path, global),
        ConfigCommand::Validate { strict } => run_validate(strict, global),
        ConfigCommand::Set { path, value, scope } => set(path, value, scope, global),
        ConfigCommand::Edit { scope } => edit(scope, global),
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

/// Resolve the config file for the chosen scope. The user-scope path
/// is created on demand (with `~/.config/mxnode/`) so a first-use
/// `config set` works without a prior install. The system-scope path
/// is treated as opaque — the operator owns `/etc/mxnode/`.
fn scope_path(scope: ScopeArg) -> Result<PathBuf, CliError> {
    match scope {
        ScopeArg::User => user_config_path().map_err(|e| {
            CliError::new(
                "could not determine user config path",
                e.to_string(),
                "set $XDG_CONFIG_HOME or $HOME so mxnode can place the file under <home>/.config/mxnode/",
            )
        }),
        ScopeArg::System => Ok(system_config_path()),
    }
}

/// Coerce a raw CLI string into the most-specific TOML scalar that
/// round-trips: `true`/`false` → bool, integer literal → int, float
/// literal → float, otherwise a quoted string. Operators wanting
/// arrays or tables hand-edit instead of pushing them through
/// `config set` (the round-trip rules get fiddly fast).
fn coerce_value(raw: &str) -> toml_edit::Item {
    if raw.eq_ignore_ascii_case("true") {
        return value(true);
    }
    if raw.eq_ignore_ascii_case("false") {
        return value(false);
    }
    if let Ok(n) = raw.parse::<i64>() {
        return value(n);
    }
    if let Ok(n) = raw.parse::<f64>() {
        // Reject NaN / inf — TOML floats can't represent them.
        if n.is_finite() {
            return value(n);
        }
    }
    value(raw)
}

fn set(
    path: String,
    raw_value: String,
    scope: ScopeArg,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let target = scope_path(scope)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CliError::new(
                "could not create config directory",
                format!("{}: {e}", parent.display()),
                "ensure the parent directory is writable",
            )
            .json_if(global.json)
        })?;
    }
    let body = if target.exists() {
        std::fs::read_to_string(&target).map_err(|e| {
            CliError::new(
                "could not read config file",
                format!("{}: {e}", target.display()),
                "ensure the file is readable",
            )
            .json_if(global.json)
        })?
    } else {
        // Stamp a minimal header so the new file isn't blank.
        "# mxnode config — generated by `mxnode config set`.\nschema_version = 1\n".to_string()
    };
    let mut doc: DocumentMut = body.parse().map_err(|e: toml_edit::TomlError| {
        CliError::new(
            "could not parse config as TOML",
            format!("{}: {e}", target.display()),
            "fix the file by hand or back it up and re-run `mxnode config set`",
        )
        .json_if(global.json)
    })?;
    let item = coerce_value(&raw_value);
    write_dotted(&mut doc, &path, item);

    std::fs::write(&target, doc.to_string()).map_err(|e| {
        CliError::new(
            "could not write config file",
            format!("{}: {e}", target.display()),
            "ensure the parent directory is writable",
        )
        .json_if(global.json)
    })?;

    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "wrote": target.display().to_string(),
                "scope": scope_label(scope),
                "path": path,
                "value": raw_value,
            })
        );
    } else {
        println!("✓ {} set in {} scope", path, scope_label(scope));
        println!("  {}", target.display());
    }
    Ok(())
}

/// Walk `dotted` (e.g. `network.environment`) and assign `item` to the
/// leaf, creating intermediate tables as needed. Operator comments and
/// section ordering elsewhere in the document are preserved by
/// `toml_edit`.
fn write_dotted(doc: &mut DocumentMut, dotted: &str, item: toml_edit::Item) {
    let segments: Vec<&str> = dotted.split('.').collect();
    if segments.is_empty() {
        return;
    }
    if segments.len() == 1 {
        doc[segments[0]] = item;
        return;
    }
    let head = segments[0];
    if !doc.as_table().contains_key(head) || !doc[head].is_table() {
        doc[head] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let mut cursor = &mut doc[head];
    for seg in &segments[1..segments.len() - 1] {
        let cur_tbl = cursor.as_table_mut().expect("intermediate is a table");
        if !cur_tbl.contains_key(seg) || !cur_tbl[seg].is_table() {
            cur_tbl.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        cursor = &mut cur_tbl[seg];
    }
    let leaf = segments[segments.len() - 1];
    let cur_tbl = cursor.as_table_mut().expect("parent is a table");
    cur_tbl.insert(leaf, item);
}

fn edit(scope: ScopeArg, global: &GlobalArgs) -> Result<(), CliError> {
    let target = scope_path(scope)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CliError::new(
                "could not create config directory",
                format!("{}: {e}", parent.display()),
                "ensure the parent directory is writable",
            )
            .json_if(global.json)
        })?;
    }
    if !target.exists() {
        // Seed an empty file so the editor opens something rather than
        // hitting "no such file" depending on the editor.
        let header = "# mxnode config — generated by `mxnode config edit`.\nschema_version = 1\n";
        std::fs::write(&target, header).map_err(|e| {
            CliError::new(
                "could not seed config file",
                format!("{}: {e}", target.display()),
                "ensure the parent directory is writable",
            )
            .json_if(global.json)
        })?;
    }
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = Command::new(&editor)
        .arg(&target)
        .status()
        .map_err(|e| {
            CliError::new(
                "could not launch editor",
                format!("{editor} {}: {e}", target.display()),
                "set $EDITOR to a known-good editor and retry",
            )
            .json_if(global.json)
        })?;
    if !status.success() {
        return Err(CliError::new(
            "editor exited non-zero",
            format!("{editor} returned {:?}", status.code()),
            "the file is unchanged; rerun `mxnode config edit` after fixing your editor",
        )
        .json_if(global.json));
    }
    // Validate what came back so syntax errors surface immediately.
    let body = std::fs::read_to_string(&target).map_err(|e| {
        CliError::new(
            "could not re-read config after edit",
            format!("{}: {e}", target.display()),
            "ensure the file is readable",
        )
        .json_if(global.json)
    })?;
    if let Err(parse) = body.parse::<DocumentMut>() {
        return Err(CliError::new(
            "saved config is not valid TOML",
            format!("{}: {parse}", target.display()),
            "fix the syntax error and re-run `mxnode config validate` to check",
        )
        .json_if(global.json));
    }
    if !global.json {
        println!("✓ saved {}", target.display());
    } else {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "wrote": target.display().to_string(),
                "scope": scope_label(scope),
            })
        );
    }
    Ok(())
}

fn scope_label(scope: ScopeArg) -> &'static str {
    match scope {
        ScopeArg::User => "user",
        ScopeArg::System => "system",
    }
}

#[allow(dead_code)]
fn _path_marker(_p: &Path) {}
