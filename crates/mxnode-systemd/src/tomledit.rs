//! Typed, comment-preserving mutators for the node and proxy TOML configs.
//!
//! Replaces the bash `sed -i` invocations that today rewrite:
//!   - `prefs.toml` → `NodeDisplayName = "..."`
//!   - `prefs.toml` → `DestinationShardAsObserver = "..."`
//!   - `config.toml` → `[DbLookupExtensions] Enabled = true`
//!   - proxy `config.toml` → `ServerPort = 8079` + observers list
//!
//! Every mutator is **idempotent**: applying it twice produces the same
//! bytes as applying it once. Comments and key ordering survive the
//! round-trip, which is the primary reason we use `toml_edit` instead of
//! `toml::Value`. Operators frequently annotate these files; sed silently
//! shredded those annotations.

use std::collections::BTreeMap;

use mxnode_core::Shard;
use thiserror::Error;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

#[derive(Debug, Error)]
pub enum TomlEditError {
    #[error("could not parse TOML: {0}")]
    Parse(#[from] toml_edit::TomlError),
    #[error("expected `{section}` to be a table; got something else")]
    NotATable { section: String },
    #[error("operator override `{path}` failed to convert to a TOML edit value: {reason}")]
    OverrideConvert { path: String, reason: String },
}

/// Set `NodeDisplayName = "<name>"` inside the `[Preferences]` table of
/// `prefs.toml`. Creates the table if it isn't present (the bash always
/// finds it because it ships with the config repo, but we don't trust the
/// invariant).
pub fn set_node_display_name(doc: &mut DocumentMut, name: &str) -> Result<(), TomlEditError> {
    let section = ensure_table(doc, "Preferences")?;
    section["NodeDisplayName"] = value(name);
    Ok(())
}

/// Set `DestinationShardAsObserver = "<value>"` inside `[Preferences]`.
///
/// The mx-chain-go binary parses this field as either a numeric shard id
/// (`"0"`, `"1"`, `"2"`), the literal string `"metachain"`, or the
/// literal `"disabled"` (lets the node pick its own shard). It does NOT
/// accept `"auto"` — passing that crashes the node at bootstrap with
/// `strconv.ParseUint: parsing "auto": invalid syntax`.
///
/// We therefore map `Shard::Auto` to a no-op (leave the upstream value
/// alone, which the config repos default to `"disabled"` — the same
/// "node picks its own shard" semantic). Operators who want explicit
/// pinning pass `Shard::Zero|One|Two|Metachain` and we write the
/// numeric form. `Shard::Disabled` is also passed through verbatim.
pub fn set_destination_shard(doc: &mut DocumentMut, shard: Shard) -> Result<(), TomlEditError> {
    if matches!(shard, Shard::Auto) {
        return Ok(());
    }
    let section = ensure_table(doc, "Preferences")?;
    section["DestinationShardAsObserver"] = value(shard.as_str());
    Ok(())
}

/// Set `[DbLookupExtensions] Enabled = true` inside the node's
/// `config.toml`. Used for observer squads.
pub fn enable_db_lookup_extensions(doc: &mut DocumentMut) -> Result<(), TomlEditError> {
    let section = ensure_table(doc, "DbLookupExtensions")?;
    section["Enabled"] = value(true);
    Ok(())
}

/// Set `[Preferences] RedundancyLevel = N` for multikey backups.
///
/// `0` means "primary multikey machine" (default — emit anyway so
/// `mxnode config show` reflects the install choice unambiguously).
/// `1+` mark backup machines that take over signing for the same
/// `allValidatorsKeys.pem` set when the lower-level instance fails.
/// All instances must share the *same* keys file; only this knob
/// differs across them.
pub fn set_redundancy_level(doc: &mut DocumentMut, level: u8) -> Result<(), TomlEditError> {
    let section = ensure_table(doc, "Preferences")?;
    section["RedundancyLevel"] = value(i64::from(level));
    Ok(())
}

/// Clear `[HardwareRequirements] CPUFlags = []` in the node's
/// `config.toml`. The upstream config pins x86-only flags
/// (`SSE4`, `SSE42`); on non-x86 hosts (Apple Silicon, Linux aarch64,
/// the AMD/ARM Mac Mini observer profile blessed by the MultiversX
/// docs) the node refuses to start because `cpuid.CPU.Supports("SSE4")`
/// returns false on ARM. An empty list satisfies the check vacuously.
///
/// We don't try to translate to ARM-equivalent feature names (NEON
/// etc.) because the upstream check exists primarily to filter out
/// truly ancient x86 boxes. ARM CPUs that run macOS / Linux today are
/// universally above the floor mxnode cares about.
pub fn clear_cpu_flags(doc: &mut DocumentMut) -> Result<(), TomlEditError> {
    let section = ensure_table(doc, "HardwareRequirements")?;
    section["CPUFlags"] = value(toml_edit::Array::new());
    Ok(())
}

/// Apply a flat dotted-path → TOML value override map to `doc`.
///
/// Dotted-path keys (e.g. `"Preferences.NodeDisplayName"`) are walked
/// segment by segment; intermediate tables are created if missing. The
/// supplied `toml::Value` is rendered to its TOML literal form and
/// re-parsed via `toml_edit` so the resulting `Item` carries the right
/// `Value` shape (string, integer, bool, array, inline table). This
/// round-trip is the simplest robust way to bridge between `toml`
/// (which figment hands us via `Config`) and `toml_edit` (which
/// preserves the operator's comments).
///
/// `template_substitutions` lets the caller swap `{index}` / `{shard}`
/// markers in *string* values (only — substitutions on bools or arrays
/// would surprise operators). Pass an empty slice to skip.
///
/// Idempotent: applying the same map twice produces the same bytes.
pub fn apply_overrides(
    doc: &mut DocumentMut,
    overrides: &BTreeMap<String, toml::Value>,
    template_substitutions: &[(&str, &str)],
) -> Result<(), TomlEditError> {
    for (path, raw_value) in overrides {
        let substituted = substitute_in_value(raw_value, template_substitutions);
        let item = toml_value_to_edit_item(&substituted, path)?;
        set_dotted_path(doc, path, item);
    }
    Ok(())
}

/// Walk `dotted` (e.g. `a.b.c`) and assign `value` to the leaf.
/// Intermediate tables are created on-demand. We use `toml_edit::Table`
/// containers so the resulting document still serialises as standard
/// table headers rather than inline tables.
fn set_dotted_path(doc: &mut DocumentMut, dotted: &str, value: Item) {
    let segments: Vec<&str> = dotted.split('.').collect();
    if segments.is_empty() {
        return;
    }
    if segments.len() == 1 {
        doc[segments[0]] = value;
        return;
    }

    // Walk to the parent of the leaf.
    let mut cursor: &mut Item = {
        let head = segments[0];
        if !doc.as_table().contains_key(head) || !doc[head].is_table() {
            doc[head] = Item::Table(Table::new());
        }
        &mut doc[head]
    };
    for seg in &segments[1..segments.len() - 1] {
        let table = match cursor.as_table_mut() {
            Some(t) => t,
            None => return, // path collides with a non-table; bail.
        };
        if !table.contains_key(seg) || !table[seg].is_table() {
            table[seg] = Item::Table(Table::new());
        }
        cursor = &mut table[seg];
    }
    if let Some(table) = cursor.as_table_mut() {
        table[segments[segments.len() - 1]] = value;
    }
}

/// Convert a `toml::Value` to a `toml_edit::Item` by serialising and
/// re-parsing through a `__tmp = <value>` wrapper. Handles every
/// TOML value shape including arrays and inline tables without us
/// hand-rolling case analysis.
fn toml_value_to_edit_item(v: &toml::Value, path: &str) -> Result<Item, TomlEditError> {
    let literal = serialize_inline(v).map_err(|e| TomlEditError::OverrideConvert {
        path: path.to_string(),
        reason: e,
    })?;
    let wrapped = format!("__tmp = {literal}\n");
    let doc: DocumentMut =
        wrapped
            .parse()
            .map_err(|e: toml_edit::TomlError| TomlEditError::OverrideConvert {
                path: path.to_string(),
                reason: e.to_string(),
            })?;
    Ok(doc["__tmp"].clone())
}

/// Render a `toml::Value` as an inline TOML literal suitable for a
/// `key = <value>` line. Unlike `toml::to_string` (which only handles
/// top-level tables), this recurses into arrays and produces inline
/// tables for nested table values.
fn serialize_inline(v: &toml::Value) -> Result<String, String> {
    match v {
        toml::Value::String(s) => Ok(format!("{:?}", s)),
        toml::Value::Integer(i) => Ok(i.to_string()),
        toml::Value::Float(f) => Ok(format!("{f}")),
        toml::Value::Boolean(b) => Ok(b.to_string()),
        toml::Value::Datetime(dt) => Ok(dt.to_string()),
        toml::Value::Array(arr) => {
            let parts: Result<Vec<_>, _> = arr.iter().map(serialize_inline).collect();
            Ok(format!("[{}]", parts?.join(", ")))
        }
        toml::Value::Table(t) => {
            let mut parts = Vec::with_capacity(t.len());
            for (k, val) in t {
                parts.push(format!("{} = {}", quote_key(k), serialize_inline(val)?));
            }
            Ok(format!("{{ {} }}", parts.join(", ")))
        }
    }
}

fn quote_key(k: &str) -> String {
    // Bare keys allow ASCII letters/digits/`-`/`_`; quote anything else.
    if !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        k.to_string()
    } else {
        format!("{:?}", k)
    }
}

/// Substitute `{index}` / `{shard}` tokens inside string TOML values
/// (recursing into arrays + tables). Non-string values pass through
/// untouched — silently substituting tokens inside an integer would
/// surprise operators.
fn substitute_in_value(v: &toml::Value, subs: &[(&str, &str)]) -> toml::Value {
    if subs.is_empty() {
        return v.clone();
    }
    match v {
        toml::Value::String(s) => {
            let mut out = s.clone();
            for (token, value) in subs {
                out = out.replace(token, value);
            }
            toml::Value::String(out)
        }
        toml::Value::Array(arr) => {
            toml::Value::Array(arr.iter().map(|x| substitute_in_value(x, subs)).collect())
        }
        toml::Value::Table(t) => {
            let mut out = toml::value::Table::new();
            for (k, val) in t {
                out.insert(k.clone(), substitute_in_value(val, subs));
            }
            toml::Value::Table(out)
        }
        other => other.clone(),
    }
}

/// One observer entry rendered into the proxy `config.toml`'s
/// `[[Observers]]` array-of-tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverEntry {
    pub shard_id: u32,
    pub address: String,
}

/// Replace the proxy `[[Observers]]` array and set `ServerPort`.
///
/// The bash truncates the existing TOML at the first occurrence of the
/// metachain shard id (`4294967295`) and re-appends a fixed block. We
/// instead **clear** the array-of-tables and rebuild it from the supplied
/// list, which is robust regardless of what the upstream config file
/// happens to contain.
pub fn rewrite_proxy_config(
    doc: &mut DocumentMut,
    server_port: u16,
    observers: &[ObserverEntry],
) -> Result<(), TomlEditError> {
    // Search for the top-level `ServerPort` key and overwrite it. If the
    // upstream config nests it under a section we'd rather not guess —
    // surface the actual layout in tests rather than silently scattering
    // the value.
    if doc.as_table().contains_key("ServerPort") {
        doc["ServerPort"] = value(server_port as i64);
    } else {
        // Default to top-level when missing; that's the bash convention.
        doc["ServerPort"] = value(server_port as i64);
    }

    // Replace the observers array-of-tables. `toml_edit` represents these
    // as `Item::ArrayOfTables`; constructing one preserves the bracketed
    // header form on serialise.
    let mut array = ArrayOfTables::new();
    for obs in observers {
        let mut t = Table::new();
        t["ShardId"] = value(obs.shard_id as i64);
        t["Address"] = value(obs.address.clone());
        array.push(t);
    }
    doc["Observers"] = Item::ArrayOfTables(array);
    Ok(())
}

fn ensure_table<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table, TomlEditError> {
    if !doc.as_table().contains_key(name) {
        doc[name] = Item::Table(Table::new());
    }
    doc.get_mut(name)
        .and_then(|item| item.as_table_mut())
        .ok_or(TomlEditError::NotATable {
            section: name.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> DocumentMut {
        s.parse().expect("valid TOML")
    }

    #[test]
    fn set_node_display_name_replaces_existing_value() {
        let mut doc = parse(
            r#"
# preferences for node
[Preferences]
NodeDisplayName = "old-name"
SomethingElse = 42
"#,
        );
        set_node_display_name(&mut doc, "new-name").unwrap();
        let out = doc.to_string();
        assert!(out.contains("NodeDisplayName = \"new-name\""));
        // Comment + sibling key preserved.
        assert!(out.contains("# preferences for node"));
        assert!(out.contains("SomethingElse = 42"));
    }

    #[test]
    fn set_node_display_name_creates_section_when_missing() {
        let mut doc = parse("# empty file\n");
        set_node_display_name(&mut doc, "fresh").unwrap();
        let out = doc.to_string();
        assert!(out.contains("[Preferences]"));
        assert!(out.contains("NodeDisplayName = \"fresh\""));
    }

    #[test]
    fn set_destination_shard_writes_protocol_string() {
        let mut doc = parse("[Preferences]\nDestinationShardAsObserver = \"disabled\"\n");
        set_destination_shard(&mut doc, Shard::Metachain).unwrap();
        assert!(doc
            .to_string()
            .contains("DestinationShardAsObserver = \"metachain\""));

        set_destination_shard(&mut doc, Shard::Zero).unwrap();
        assert!(doc
            .to_string()
            .contains("DestinationShardAsObserver = \"0\""));
    }

    #[test]
    fn enable_db_lookup_extensions_is_idempotent() {
        let mut doc = parse(
            r#"
[DbLookupExtensions]
Enabled = false
DbType = "leveldb"
"#,
        );
        enable_db_lookup_extensions(&mut doc).unwrap();
        let after_first = doc.to_string();
        enable_db_lookup_extensions(&mut doc).unwrap();
        let after_second = doc.to_string();
        assert_eq!(after_first, after_second, "operation must be idempotent");
        assert!(after_first.contains("Enabled = true"));
        // Sibling key preserved.
        assert!(after_first.contains("DbType = \"leveldb\""));
    }

    #[test]
    fn rewrite_proxy_config_replaces_observers_and_sets_port() {
        let mut doc = parse(
            r#"
ServerPort = 8080

[[Observers]]
ShardId = 0
Address = "http://example/old"

[[Observers]]
ShardId = 99
Address = "http://example/old2"
"#,
        );
        rewrite_proxy_config(
            &mut doc,
            8079,
            &[
                ObserverEntry {
                    shard_id: 0,
                    address: "http://127.0.0.1:8080".to_string(),
                },
                ObserverEntry {
                    shard_id: 4_294_967_295,
                    address: "http://127.0.0.1:8083".to_string(),
                },
            ],
        )
        .unwrap();
        let out = doc.to_string();
        assert!(out.contains("ServerPort = 8079"));
        // Old shard 99 entry must be gone.
        assert!(!out.contains("99"));
        // New entries present.
        assert!(out.contains("ShardId = 0"));
        assert!(out.contains("ShardId = 4294967295"));
        assert!(out.contains("http://127.0.0.1:8080"));
        assert!(out.contains("http://127.0.0.1:8083"));
    }

    #[test]
    fn apply_overrides_writes_typed_values_and_preserves_comments() {
        let mut doc = parse(
            r#"
# important comment
[Preferences]
NodeDisplayName = "old"
ExistingKey = 1
"#,
        );
        let mut map: BTreeMap<String, toml::Value> = BTreeMap::new();
        map.insert(
            "Preferences.FullArchive".to_string(),
            toml::Value::Boolean(true),
        );
        map.insert(
            "Antiflood.WebServer.SimultaneousRequests".to_string(),
            toml::Value::Integer(200),
        );
        map.insert(
            "Preferences.PreferredConnections".to_string(),
            toml::Value::Array(vec![toml::Value::String("/ip4/1.2.3.4".to_string())]),
        );
        apply_overrides(&mut doc, &map, &[]).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# important comment"));
        assert!(out.contains("ExistingKey = 1"));
        assert!(out.contains("FullArchive = true"));
        assert!(out.contains("SimultaneousRequests = 200"));
        assert!(out.contains("\"/ip4/1.2.3.4\""));
    }

    #[test]
    fn apply_overrides_is_idempotent() {
        let mut doc = parse("[Preferences]\nFullArchive = false\n");
        let mut map: BTreeMap<String, toml::Value> = BTreeMap::new();
        map.insert(
            "Preferences.FullArchive".to_string(),
            toml::Value::Boolean(true),
        );
        apply_overrides(&mut doc, &map, &[]).unwrap();
        let after_first = doc.to_string();
        apply_overrides(&mut doc, &map, &[]).unwrap();
        let after_second = doc.to_string();
        assert_eq!(after_first, after_second);
    }

    #[test]
    fn apply_overrides_substitutes_template_tokens_in_strings_only() {
        let mut doc = parse("");
        let mut map: BTreeMap<String, toml::Value> = BTreeMap::new();
        map.insert(
            "Preferences.NodeDisplayName".to_string(),
            toml::Value::String("myorg-{index}".to_string()),
        );
        // Integer with `{index}`-shaped value would never appear, but
        // confirm bools/integers don't get text-replaced even if subs given.
        map.insert(
            "Preferences.SomeNumber".to_string(),
            toml::Value::Integer(42),
        );
        apply_overrides(&mut doc, &map, &[("{index}", "3")]).unwrap();
        let out = doc.to_string();
        assert!(out.contains("NodeDisplayName = \"myorg-3\""));
        assert!(out.contains("SomeNumber = 42"));
    }

    #[test]
    fn round_trip_preserves_unrelated_comments() {
        let original = r#"# top comment
[Preferences]
# comment above name
NodeDisplayName = "old"
# trailing comment

[Other]
key = 1
"#;
        let mut doc = parse(original);
        set_node_display_name(&mut doc, "new").unwrap();
        let out = doc.to_string();
        assert!(out.contains("# top comment"));
        assert!(out.contains("# comment above name"));
        assert!(out.contains("# trailing comment"));
        assert!(out.contains("[Other]"));
        assert!(out.contains("key = 1"));
    }
}
