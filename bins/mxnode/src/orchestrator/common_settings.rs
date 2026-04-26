//! Walk every `config/prefs.toml` + `config/config.toml` under a fleet of
//! node workdirs and surface keys whose values are **identical across
//! all nodes** — the operator's intentional tweaks that should live in a
//! single `[overrides.prefs]` / `[overrides.config]` section instead of
//! being repeated N times across N node directories.
//!
//! Per-node keys (`NodeDisplayName`, `DestinationShardAsObserver`) are
//! deliberately excluded so they don't collapse into a single override
//! and stomp the per-node values during the next install/upgrade.
//!
//! Array-of-tables (`[[Observers]]`, `[[Antiflood.WebServer.…]]`) are
//! also skipped — merging arrays semantically across operators with
//! different ordering choices is a foot-gun, not a convenience.

use std::collections::BTreeMap;
use std::path::Path;

/// Per-node keys that MUST stay per-node — collapsing them into a
/// fleet-wide override would corrupt every node's identity.
const PER_NODE_PREFS: &[&str] = &[
    "Preferences.NodeDisplayName",
    "Preferences.DestinationShardAsObserver",
];

/// Result of the common-settings sweep across a node fleet.
#[derive(Debug, Default, Clone)]
pub struct CommonSettings {
    /// Dotted-path → value pairs that were identical across every
    /// node's `prefs.toml` (excluding the per-node identity keys).
    pub prefs: BTreeMap<String, toml::Value>,
    /// Same for `config.toml`.
    pub config: BTreeMap<String, toml::Value>,
    /// How many keys differed across nodes — informational, surfaced
    /// in the migrate summary so the operator knows there's per-node
    /// drift the migration didn't roll up.
    pub differing_prefs_keys: usize,
    pub differing_config_keys: usize,
    /// Number of node workdirs we actually read from. < 2 means the
    /// detector returned the file's content as-is (1-node "common" is
    /// just that node's settings).
    pub nodes_scanned: usize,
}

/// Scan every `<workdir>/config/{prefs,config}.toml` and return the
/// keys whose values are identical across all parsed files. Files that
/// fail to read or parse are silently skipped — the operator is
/// migrating, not running clean tests.
pub fn detect<P: AsRef<Path>>(workdirs: &[P]) -> CommonSettings {
    let prefs_paths: Vec<_> = workdirs
        .iter()
        .map(|w| w.as_ref().join("config/prefs.toml"))
        .collect();
    let config_paths: Vec<_> = workdirs
        .iter()
        .map(|w| w.as_ref().join("config/config.toml"))
        .collect();
    let prefs_docs: Vec<toml::Value> = prefs_paths
        .iter()
        .filter_map(|p| read_toml(p))
        .collect();
    let config_docs: Vec<toml::Value> = config_paths
        .iter()
        .filter_map(|p| read_toml(p))
        .collect();
    let (prefs, prefs_diff) = common_keys(&prefs_docs, PER_NODE_PREFS);
    let (config, config_diff) = common_keys(&config_docs, &[]);
    CommonSettings {
        prefs,
        config,
        differing_prefs_keys: prefs_diff,
        differing_config_keys: config_diff,
        nodes_scanned: workdirs.len(),
    }
}

fn read_toml(path: &Path) -> Option<toml::Value> {
    let body = std::fs::read_to_string(path).ok()?;
    toml::from_str(&body).ok()
}

fn common_keys(
    docs: &[toml::Value],
    skip: &[&str],
) -> (BTreeMap<String, toml::Value>, usize) {
    if docs.is_empty() {
        return (BTreeMap::new(), 0);
    }
    // Flatten each doc into dotted-path → leaf value.
    let flat: Vec<BTreeMap<String, toml::Value>> =
        docs.iter().map(flatten).collect();
    let mut common = BTreeMap::new();
    let mut differing = 0usize;
    // Use the union of keys across all docs as candidates so we don't
    // miss settings that the first doc happens to lack.
    let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for m in &flat {
        keys.extend(m.keys().cloned());
    }
    for key in keys {
        if skip.iter().any(|s| *s == key) {
            continue;
        }
        let first = match flat[0].get(&key) {
            Some(v) => v,
            None => {
                // Key missing from doc 0 → not "common" by our
                // definition. Don't roll up.
                differing += 1;
                continue;
            }
        };
        let all_match = flat
            .iter()
            .all(|m| m.get(&key).map(|v| v == first).unwrap_or(false));
        if all_match {
            common.insert(key, first.clone());
        } else {
            differing += 1;
        }
    }
    (common, differing)
}

/// Walk a `toml::Value::Table`, emit `prefix.key` → leaf-value pairs.
/// Arrays-of-tables and arrays-of-anything are excluded (we don't
/// have a sensible "is identical" semantic for them across operators).
fn flatten(v: &toml::Value) -> BTreeMap<String, toml::Value> {
    let mut out = BTreeMap::new();
    if let toml::Value::Table(t) = v {
        walk_table(t, "", &mut out);
    }
    out
}

fn walk_table(
    t: &toml::value::Table,
    prefix: &str,
    out: &mut BTreeMap<String, toml::Value>,
) {
    for (k, v) in t {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::Table(inner) => walk_table(inner, &path, out),
            toml::Value::Array(_) => {
                // Skip — equality on long array-of-tables is brittle
                // (e.g. PreferredConnections in different orders); the
                // operator can re-add them as overrides manually.
            }
            scalar => {
                out.insert(path, scalar.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn detects_a_setting_common_to_every_node() {
        let docs = vec![
            t(r#"
                [Preferences]
                FullArchive = true
                NodeDisplayName = "node-0"
            "#),
            t(r#"
                [Preferences]
                FullArchive = true
                NodeDisplayName = "node-1"
            "#),
            t(r#"
                [Preferences]
                FullArchive = true
                NodeDisplayName = "node-2"
            "#),
        ];
        let (common, differing) = common_keys(&docs, PER_NODE_PREFS);
        assert!(common.contains_key("Preferences.FullArchive"));
        // NodeDisplayName excluded by skip list — not counted as
        // differing because we never considered it.
        assert_eq!(differing, 0);
    }

    #[test]
    fn excludes_keys_that_differ_across_nodes() {
        let docs = vec![
            t(r#"
                [Preferences]
                FullArchive = true
                Custom = 10
            "#),
            t(r#"
                [Preferences]
                FullArchive = true
                Custom = 20
            "#),
        ];
        let (common, differing) = common_keys(&docs, &[]);
        assert!(common.contains_key("Preferences.FullArchive"));
        assert!(!common.contains_key("Preferences.Custom"));
        assert_eq!(differing, 1);
    }

    #[test]
    fn empty_input_returns_empty() {
        let (common, differing) = common_keys(&[], &[]);
        assert!(common.is_empty());
        assert_eq!(differing, 0);
    }

    #[test]
    fn skips_arrays_of_tables() {
        let docs = vec![
            t(r#"
                [[Observers]]
                ShardId = 0
                Address = "http://a:8080"
            "#),
            t(r#"
                [[Observers]]
                ShardId = 0
                Address = "http://a:8080"
            "#),
        ];
        let (common, _) = common_keys(&docs, &[]);
        // Arrays explicitly skipped — even though the array is
        // identical across docs, we don't roll it up.
        assert!(common.is_empty());
    }
}
