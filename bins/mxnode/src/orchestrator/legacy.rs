//! Whitelist-only parser for the bash `config/variables.cfg`.
//!
//! Per Codex's audit, the file is only declarative for its first ~50 lines;
//! beyond that it does parameter expansion, command substitution, and curl
//! calls. We do **not** try to interpret any of that. We accept exactly the
//! set of header keys the bash exposes as user-customizable and refuse to
//! touch anything else.

use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LegacyError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// Reserved for future use when the parser grows context-aware error
    /// recovery; today we always stop cleanly at the first non-declarative
    /// line and never construct this variant.
    #[allow(dead_code)]
    #[error("could not parse legacy variables.cfg at line {line}: {detail}")]
    Parse { line: usize, detail: String },
}

/// Subset of `variables.cfg` keys mxnode imports during `migrate-from-bash`.
/// Adding a key here is a deliberate decision — see plan §"Migration".
const ALLOWED_KEYS: &[&str] = &[
    "ENVIRONMENT",
    "CUSTOM_HOME",
    "CUSTOM_USER",
    "NODE_KEYS_LOCATION",
    "GITHUBTOKEN",
    "NODE_EXTRA_FLAGS",
    "OVERRIDE_PROXYVER",
    "OVERRIDE_CONFIGVER",
    "GITHUB_ORG",
];

/// Whitelisted subset of variables imported from `variables.cfg`.
/// Keys not in `ALLOWED_KEYS` are surfaced via `unknown_keys` for warning,
/// never acted on.
#[derive(Debug, Default, Clone, Serialize)]
pub struct LegacyVariables {
    pub environment: Option<String>,
    pub custom_home: Option<PathBuf>,
    pub custom_user: Option<String>,
    pub node_keys_location: Option<String>,
    pub github_token: Option<String>,
    pub node_extra_flags: Option<String>,
    pub override_proxyver: Option<String>,
    pub override_configver: Option<String>,
    pub github_org: Option<String>,
    pub unknown_keys: Vec<String>,
}

/// Parse a `variables.cfg`-shaped file. Stops after the first empty-content
/// section header `#---...####`/`#-#` — this is the boundary the bash
/// itself draws between operator-customisable knobs and machine-derived
/// computation.
pub fn parse_variables_cfg(path: &Path) -> Result<LegacyVariables, LegacyError> {
    let raw = std::fs::read_to_string(path).map_err(|e| LegacyError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    parse_str(&raw)
}

/// Same parser, but works on an in-memory string. Used by tests and
/// callers that already have the bytes (e.g. `git show HEAD:variables.cfg`).
pub fn parse_str(text: &str) -> Result<LegacyVariables, LegacyError> {
    let mut out = LegacyVariables::default();

    for (i, raw_line) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw_line.trim();

        // The bash uses long `#---...---####` separators to mark the end
        // of the declarative block. After the first such marker we stop —
        // the rest is shell logic.
        if line.starts_with("#----") || line.starts_with("#####") {
            break;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // We only accept top-level `KEY=VALUE` assignments. `export FOO=bar`
        // is treated as the same shape after stripping the `export `.
        let body = line.strip_prefix("export ").unwrap_or(line);
        let Some((raw_key, raw_value)) = body.split_once('=') else {
            // Unknown shape (function definition, conditional, ...). We
            // stop the moment we see one — beyond this is shell logic, not
            // declaration.
            break;
        };
        let key = raw_key.trim();
        if !is_valid_identifier(key) {
            break;
        }
        let value = strip_quotes(raw_value.trim());
        // Honour `${X:-default}` only if X is one of the keys we already
        // captured. This is the *only* bashism we attempt.
        let resolved = expand_simple_default(value, &out);
        if ALLOWED_KEYS.contains(&key) {
            store(&mut out, key, &resolved, line_no)?;
        } else {
            out.unknown_keys.push(key.to_string());
        }
    }
    Ok(out)
}

fn is_valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn strip_quotes(s: &str) -> String {
    let trimmed = s.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Handle exactly `${KNOWN_KEY:-default}` and `${KNOWN_KEY}`. Anything more
/// elaborate is left as-is — we are not a shell.
fn expand_simple_default(value: String, captured: &LegacyVariables) -> String {
    let trimmed = value.trim();
    if !(trimmed.starts_with("${") && trimmed.ends_with('}')) {
        return value;
    }
    let inner = &trimmed[2..trimmed.len() - 1];
    let (key, default) = match inner.split_once(":-") {
        Some((k, d)) => (k.trim(), Some(d.trim())),
        None => (inner.trim(), None),
    };
    let captured_value = captured_get(captured, key);
    match (captured_value, default) {
        (Some(v), _) => v,
        (None, Some(d)) => d.to_string(),
        (None, None) => value,
    }
}

fn captured_get(c: &LegacyVariables, key: &str) -> Option<String> {
    let s = match key {
        "ENVIRONMENT" => c.environment.clone(),
        "CUSTOM_HOME" => c.custom_home.as_ref().map(|p| p.display().to_string()),
        "CUSTOM_USER" => c.custom_user.clone(),
        "NODE_KEYS_LOCATION" => c.node_keys_location.clone(),
        "GITHUBTOKEN" => c.github_token.clone(),
        "NODE_EXTRA_FLAGS" => c.node_extra_flags.clone(),
        "OVERRIDE_PROXYVER" => c.override_proxyver.clone(),
        "OVERRIDE_CONFIGVER" => c.override_configver.clone(),
        "GITHUB_ORG" => c.github_org.clone(),
        _ => None,
    };
    s.filter(|s| !s.is_empty())
}

fn store(out: &mut LegacyVariables, key: &str, value: &str, _line: usize) -> Result<(), LegacyError> {
    let v = value.to_string();
    let nonempty = if v.is_empty() { None } else { Some(v) };
    match key {
        "ENVIRONMENT" => out.environment = nonempty,
        "CUSTOM_HOME" => out.custom_home = nonempty.map(PathBuf::from),
        "CUSTOM_USER" => out.custom_user = nonempty,
        "NODE_KEYS_LOCATION" => out.node_keys_location = nonempty,
        "GITHUBTOKEN" => out.github_token = nonempty,
        "NODE_EXTRA_FLAGS" => out.node_extra_flags = nonempty,
        "OVERRIDE_PROXYVER" => out.override_proxyver = nonempty,
        "OVERRIDE_CONFIGVER" => out.override_configver = nonempty,
        "GITHUB_ORG" => out.github_org = nonempty,
        _ => unreachable!("ALLOWED_KEYS gate matches the match-arms above"),
    }
    Ok(())
}

/// Counts of bash dotfiles we care about under `$CUSTOM_HOME`.
#[derive(Debug, Default, Clone, Serialize)]
pub struct LegacyDotfiles {
    pub number_of_nodes: Option<u16>,
    pub installed_env: Option<String>,
    pub squad_install: Option<String>,
}

pub fn read_dotfiles(custom_home: &Path) -> LegacyDotfiles {
    LegacyDotfiles {
        number_of_nodes: read_optional(&custom_home.join(".numberofnodes"))
            .and_then(|s| s.trim().parse().ok()),
        installed_env: read_optional(&custom_home.join(".installedenv")).map(|s| s.trim().to_string()),
        squad_install: read_optional(&custom_home.join(".squad_install")).map(|s| s.trim().to_string()),
    }
}

fn read_optional(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_known_keys() {
        let text = r#"
ENVIRONMENT="mainnet"
CUSTOM_HOME="/srv/mx"
CUSTOM_USER="validator"
GITHUB_ORG="myfork"
"#;
        let parsed = parse_str(text).unwrap();
        assert_eq!(parsed.environment.as_deref(), Some("mainnet"));
        assert_eq!(parsed.custom_home, Some(PathBuf::from("/srv/mx")));
        assert_eq!(parsed.custom_user.as_deref(), Some("validator"));
        assert_eq!(parsed.github_org.as_deref(), Some("myfork"));
        assert!(parsed.unknown_keys.is_empty());
    }

    #[test]
    fn comments_and_blanks_are_ignored() {
        let text = "\
# leading comment

ENVIRONMENT=\"mainnet\"
# trailing comment
";
        let parsed = parse_str(text).unwrap();
        assert_eq!(parsed.environment.as_deref(), Some("mainnet"));
    }

    #[test]
    fn unknown_keys_are_recorded_not_acted_on() {
        let text = "MY_CUSTOM_BACKUP=/srv/backups\nENVIRONMENT=mainnet\n";
        let parsed = parse_str(text).unwrap();
        assert_eq!(parsed.environment.as_deref(), Some("mainnet"));
        assert_eq!(parsed.unknown_keys, vec!["MY_CUSTOM_BACKUP".to_string()]);
    }

    #[test]
    fn parser_stops_at_separator() {
        let text = r#"
ENVIRONMENT="mainnet"
#----------- DON'T CHANGE THESE -----------
CONFIGVER="$(curl ...)"
"#;
        let parsed = parse_str(text).unwrap();
        assert_eq!(parsed.environment.as_deref(), Some("mainnet"));
        assert!(
            !parsed.unknown_keys.iter().any(|k| k == "CONFIGVER"),
            "after the separator nothing should be parsed",
        );
    }

    #[test]
    fn handles_export_prefix() {
        let parsed = parse_str("export ENVIRONMENT=mainnet\n").unwrap();
        assert_eq!(parsed.environment.as_deref(), Some("mainnet"));
    }

    #[test]
    fn empty_quoted_value_becomes_none() {
        let parsed = parse_str("GITHUBTOKEN=\"\"\n").unwrap();
        assert_eq!(parsed.github_token, None);
    }

    #[test]
    fn handles_simple_default_expansion() {
        let parsed = parse_str("ENVIRONMENT=\"\"\nGITHUB_ORG=${GITHUB_ORG:-multiversx}\n").unwrap();
        assert_eq!(parsed.github_org.as_deref(), Some("multiversx"));
    }

    #[test]
    fn rejects_lines_after_first_function_definition() {
        let text = r#"
ENVIRONMENT=mainnet
function check_variables {
ENVIRONMENT=devnet
}
"#;
        let parsed = parse_str(text).unwrap();
        assert_eq!(parsed.environment.as_deref(), Some("mainnet"));
    }
}
