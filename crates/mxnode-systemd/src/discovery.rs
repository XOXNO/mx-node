//! Pure parsing of systemd unit-file text.
//!
//! `parse_unit_text` is the discovery primitive that `adopt`, `rebuild-state`,
//! and `doctor` will use to extract structured directives from on-disk units.
//! Keeping it I/O-free here lets us unit-test with synthetic input; the
//! actual `systemctl cat` / `systemctl show` shell-out lives in the
//! orchestrator.
//!
//! What we extract:
//!   - `User=`
//!   - `WorkingDirectory=`
//!   - `ExecStart=` (raw and parsed `-rest-api-interface localhost:PORT` →
//!     api_port)
//!
//! Everything else is preserved as raw key/value pairs in `directives` so
//! callers can compare semantically without re-implementing the parser.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mxnode_core::NodeIndex;
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("unit text contains a directive outside any section: {0}")]
    DirectiveOutsideSection(String),
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: ParseError,
    },
}

/// Classification of a discovered systemd unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveredKind {
    Node(NodeIndex),
    Proxy,
}

impl DiscoveredKind {
    /// Try to classify a unit filename. Returns `None` for anything outside
    /// the `elrond-*` namespace mxnode owns.
    pub fn from_unit_filename(name: &str) -> Option<Self> {
        let stem = name.strip_suffix(".service")?;
        if stem == "elrond-proxy" {
            return Some(DiscoveredKind::Proxy);
        }
        if let Some(idx_str) = stem.strip_prefix("elrond-node-") {
            if let Ok(idx) = idx_str.parse::<u16>() {
                return Some(DiscoveredKind::Node(NodeIndex::new(idx)));
            }
        }
        None
    }
}

/// One unit observed on disk during a discovery pass.
#[derive(Debug, Clone)]
pub struct Discovered {
    pub kind: DiscoveredKind,
    /// Filename of the unit (e.g. `elrond-node-0.service`). Always preserved
    /// as `elrond-*` per plan D4.
    pub unit: String,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Verbatim file contents — preserved so `adopt --force-adopt` can
    /// round-trip the operator's hand-edited unit unchanged.
    pub raw_text: String,
    /// Structured view derived from `raw_text` via [`parse_unit_text`].
    pub view: UnitView,
    /// True when a `<unit>.d/` directory containing `.conf` drop-ins exists
    /// alongside the unit file. Mxnode does not currently merge drop-in
    /// directives; we surface this so adopt can warn loudly.
    pub has_drop_ins: bool,
    /// Names of drop-in files found (relative to the `.d/` directory). Empty
    /// when `has_drop_ins` is false.
    pub drop_ins: Vec<String>,
}

/// Walk a macOS LaunchAgents directory and return one [`Discovered`]
/// entry per `com.multiversx.elrond-node-*.plist` /
/// `com.multiversx.elrond-proxy.plist` it finds.
///
/// Cross-platform mxnode adopt picks this OR `scan_systemd_dir` based on
/// `Platform::current()`. The returned [`UnitView`] shape is identical
/// to what systemd produces — `Service.User`, `Service.WorkingDirectory`,
/// `Service.ExecStart` — so adoption analysis works without branching.
pub fn scan_launchd_dir(dir: impl AsRef<Path>) -> Result<Vec<Discovered>, DiscoveryError> {
    let dir = dir.as_ref();
    let mut out: Vec<Discovered> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => {
            return Err(DiscoveryError::Io {
                path: dir.display().to_string(),
                source: e,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| DiscoveryError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(kind) = classify_plist_filename(name) else {
            continue;
        };
        let raw_text = fs::read_to_string(&path).map_err(|e| DiscoveryError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let view = parse_plist_text(&raw_text);
        out.push(Discovered {
            kind,
            // Translate to systemd-style unit name so the rest of the
            // orchestrator can stay platform-agnostic.
            unit: plist_filename_to_unit_name(name),
            path,
            raw_text,
            view,
            has_drop_ins: false,
            drop_ins: Vec::new(),
        });
    }
    out.sort_by(|a, b| match (&a.kind, &b.kind) {
        (DiscoveredKind::Node(x), DiscoveredKind::Node(y)) => x.cmp(y),
        (DiscoveredKind::Node(_), DiscoveredKind::Proxy) => std::cmp::Ordering::Less,
        (DiscoveredKind::Proxy, DiscoveredKind::Node(_)) => std::cmp::Ordering::Greater,
        (DiscoveredKind::Proxy, DiscoveredKind::Proxy) => std::cmp::Ordering::Equal,
    });
    Ok(out)
}

fn classify_plist_filename(name: &str) -> Option<DiscoveredKind> {
    // We only honour our own LaunchAgent prefix (`com.multiversx.`) so
    // operator-installed agents from unrelated apps don't get adopted.
    let stem = name.strip_suffix(".plist")?;
    let stem = stem.strip_prefix(crate::plist::LAUNCH_AGENT_PREFIX)?;
    let stem = stem.strip_prefix('.')?;
    if stem == "elrond-proxy" {
        return Some(DiscoveredKind::Proxy);
    }
    if let Some(idx_str) = stem.strip_prefix("elrond-node-") {
        if let Ok(idx) = idx_str.parse::<u16>() {
            return Some(DiscoveredKind::Node(NodeIndex::new(idx)));
        }
    }
    None
}

fn plist_filename_to_unit_name(name: &str) -> String {
    // `com.multiversx.elrond-node-0.plist` → `elrond-node-0.service`
    let stem = name.strip_suffix(".plist").unwrap_or(name);
    let stem = stem
        .strip_prefix(&format!("{}.", crate::plist::LAUNCH_AGENT_PREFIX))
        .unwrap_or(stem);
    format!("{stem}.service")
}

/// Best-effort plist parser that extracts the fields adoption cares
/// about. We don't pull a full XML parser; the plists mxnode authors
/// have a tiny known shape, and operator-edited ones we read field-by-
/// field. Anything we don't recognise lands in `directives` as raw
/// key/value pairs.
fn parse_plist_text(text: &str) -> UnitView {
    let mut directives: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut user: Option<String> = None;
    let mut working_directory: Option<PathBuf> = None;
    let mut exec_start: Option<String> = None;
    let mut api_port: Option<u16> = None;

    // Find the top-level <dict> body (everything between the outer
    // <dict> and </dict>). We don't try to handle nested dicts beyond
    // the SoftResourceLimits special-case; the systemd parse_unit_text
    // also only goes one level deep semantically.
    if let Some(start) = text.find("<dict>") {
        let body = &text[start + "<dict>".len()..];
        // Walk the keys + values.
        let pairs = extract_plist_pairs(body);
        for (key, value) in &pairs {
            match key.as_str() {
                "ProgramArguments" => {
                    exec_start = Some(value.clone());
                    api_port = parse_api_port_from_exec_start(value);
                }
                "WorkingDirectory" => {
                    working_directory = Some(PathBuf::from(value));
                }
                "Label" => {
                    // Operator-visible label, e.g. com.multiversx.elrond-node-0.
                    // We don't pull `User` from launchd because plists
                    // run as the operator by default; fill `user` with
                    // the `USER` env var so the systemd-shaped `UnitView`
                    // still has something sensible there.
                    let _ = value;
                    user = std::env::var("USER")
                        .or_else(|_| std::env::var("LOGNAME"))
                        .ok();
                }
                _ => {}
            }
            directives
                .entry("Service".to_string())
                .or_default()
                .push((key.clone(), value.clone()));
        }
    }

    UnitView {
        user,
        working_directory,
        exec_start,
        api_port,
        directives,
    }
}

/// Minimal plist key/value extractor — handles the shape the
/// `render_canonical_node_plist` writer produces: `<key>X</key>`
/// followed by `<string>Y</string>`, `<integer>N</integer>`,
/// `<true/>`/`<false/>`, or `<array>...<string>...</string>...</array>`.
/// Arrays are joined with single-space separators (matches the systemd
/// `ExecStart=` interpretation).
fn extract_plist_pairs(body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cursor = 0;
    while let Some(key_open) = body[cursor..].find("<key>") {
        let abs = cursor + key_open;
        let after_open = abs + "<key>".len();
        let Some(key_close) = body[after_open..].find("</key>") else {
            break;
        };
        let key = body[after_open..after_open + key_close].trim().to_string();
        let after_key = after_open + key_close + "</key>".len();
        // Find the next non-whitespace tag — the matching value.
        let rest = &body[after_key..];
        let trimmed = rest.trim_start();
        let consumed = rest.len() - trimmed.len();
        let value_start = after_key + consumed;
        let (value, advance) = read_plist_value(&body[value_start..]);
        out.push((key, value));
        cursor = value_start + advance;
    }
    out
}

fn read_plist_value(s: &str) -> (String, usize) {
    if let Some(rest) = s.strip_prefix("<string>") {
        if let Some(end) = rest.find("</string>") {
            return (
                xml_unescape(rest[..end].trim()),
                "<string>".len() + end + "</string>".len(),
            );
        }
    }
    if let Some(rest) = s.strip_prefix("<integer>") {
        if let Some(end) = rest.find("</integer>") {
            return (
                rest[..end].trim().to_string(),
                "<integer>".len() + end + "</integer>".len(),
            );
        }
    }
    if let Some(rest) = s.strip_prefix("<true/>") {
        return (
            "true".to_string(),
            "<true/>".len() + (s.len() - rest.len() - "<true/>".len()),
        );
    }
    if s.starts_with("<true/>") {
        return ("true".to_string(), "<true/>".len());
    }
    if s.starts_with("<false/>") {
        return ("false".to_string(), "<false/>".len());
    }
    if let Some(rest) = s.strip_prefix("<array>") {
        if let Some(end) = rest.find("</array>") {
            let inner = &rest[..end];
            let mut tokens: Vec<String> = Vec::new();
            let mut cur = inner;
            while let Some(idx) = cur.find("<string>") {
                let after = &cur[idx + "<string>".len()..];
                if let Some(close) = after.find("</string>") {
                    tokens.push(xml_unescape(after[..close].trim()));
                    cur = &after[close + "</string>".len()..];
                } else {
                    break;
                }
            }
            return (tokens.join(" "), "<array>".len() + end + "</array>".len());
        }
    }
    if let Some(rest) = s.strip_prefix("<dict>") {
        if let Some(end) = rest.find("</dict>") {
            // Nested dict — preserve nothing, just skip past it.
            return (String::new(), "<dict>".len() + end + "</dict>".len());
        }
    }
    // Unrecognised; advance by one to avoid an infinite loop.
    (String::new(), 1)
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Walk a systemd directory (typically `/etc/systemd/system`) and return one
/// [`Discovered`] entry per `elrond-node-{INDEX}.service` /
/// `elrond-proxy.service` it finds.
///
/// Pure I/O reading the on-disk unit text. We do not invoke `systemctl cat`
/// here — that's a Phase 1+ optimisation. For the v0.1 use cases (adopt,
/// rebuild-state, doctor) reading the file is sufficient because mxnode
/// never authors drop-ins itself; if the operator did, `has_drop_ins` flags
/// it and adopt refuses.
/// Cross-platform supervisor scan. Walks the directory once with each
/// backend and unions the results. This way:
///   - on Linux, where `/etc/systemd/system/` only contains
///     `*.service`, the launchd pass is a cheap no-op
///   - on macOS, where `~/Library/LaunchAgents/` only contains
///     `*.plist`, the systemd pass is a cheap no-op
///   - tests that author synthetic `.service` files in a tempdir get
///     consistent results regardless of which OS the test host is
///   - operators on hybrid hosts (rare) get a complete picture
///
/// Returned entries always use systemd-style `unit` names
/// (`elrond-node-N.service`) so the orchestrator stays single-shape.
pub fn scan_supervisor_dir(dir: impl AsRef<Path>) -> Result<Vec<Discovered>, DiscoveryError> {
    let dir_path = dir.as_ref();
    let mut combined = scan_systemd_dir(dir_path)?;
    let mut from_launchd = scan_launchd_dir(dir_path)?;
    // Dedup by unit name; systemd entries win when both files exist.
    let known: std::collections::HashSet<String> =
        combined.iter().map(|d| d.unit.clone()).collect();
    from_launchd.retain(|d| !known.contains(&d.unit));
    combined.extend(from_launchd);
    combined.sort_by(|a, b| match (&a.kind, &b.kind) {
        (DiscoveredKind::Node(x), DiscoveredKind::Node(y)) => x.cmp(y),
        (DiscoveredKind::Node(_), DiscoveredKind::Proxy) => std::cmp::Ordering::Less,
        (DiscoveredKind::Proxy, DiscoveredKind::Node(_)) => std::cmp::Ordering::Greater,
        (DiscoveredKind::Proxy, DiscoveredKind::Proxy) => std::cmp::Ordering::Equal,
    });
    Ok(combined)
}

pub fn scan_systemd_dir(dir: impl AsRef<Path>) -> Result<Vec<Discovered>, DiscoveryError> {
    let dir = dir.as_ref();
    let mut out: Vec<Discovered> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => {
            return Err(DiscoveryError::Io {
                path: dir.display().to_string(),
                source: e,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| DiscoveryError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(kind) = DiscoveredKind::from_unit_filename(name) else {
            continue;
        };
        let raw_text = fs::read_to_string(&path).map_err(|e| DiscoveryError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let view = parse_unit_text(&raw_text).map_err(|e| DiscoveryError::Parse {
            path: path.display().to_string(),
            source: e,
        })?;
        let drop_in_dir = dir.join(format!("{name}.d"));
        let drop_ins = list_drop_ins(&drop_in_dir)?;
        out.push(Discovered {
            kind,
            unit: name.to_string(),
            path,
            raw_text,
            view,
            has_drop_ins: !drop_ins.is_empty(),
            drop_ins,
        });
    }
    // Stable order: proxy last; nodes by index.
    out.sort_by(|a, b| match (&a.kind, &b.kind) {
        (DiscoveredKind::Node(x), DiscoveredKind::Node(y)) => x.cmp(y),
        (DiscoveredKind::Node(_), DiscoveredKind::Proxy) => std::cmp::Ordering::Less,
        (DiscoveredKind::Proxy, DiscoveredKind::Node(_)) => std::cmp::Ordering::Greater,
        (DiscoveredKind::Proxy, DiscoveredKind::Proxy) => std::cmp::Ordering::Equal,
    });
    Ok(out)
}

fn list_drop_ins(dir: &Path) -> Result<Vec<String>, DiscoveryError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(DiscoveryError::Io {
                path: dir.display().to_string(),
                source: e,
            })
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| DiscoveryError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("conf") {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Structured view of one parsed unit. Designed for semantic comparison —
/// whitespace, comments, and section ordering are normalized away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitView {
    pub user: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub exec_start: Option<String>,
    pub api_port: Option<u16>,
    /// All key/value pairs grouped by section, after stripping comments and
    /// trimming surrounding whitespace. Multiple values for the same key
    /// inside one section are preserved in order.
    pub directives: BTreeMap<String, Vec<(String, String)>>,
}

impl UnitView {
    /// Convenience: extract the per-section list as a flat
    /// "section.key=value" iterator. Useful for diffing.
    pub fn flatten(&self) -> impl Iterator<Item = String> + '_ {
        self.directives
            .iter()
            .flat_map(|(section, kvs)| kvs.iter().map(move |(k, v)| format!("{section}.{k}={v}")))
    }
}

pub fn parse_unit_text(text: &str) -> Result<UnitView, ParseError> {
    let mut directives: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut current_section: Option<String> = None;
    let mut continuation: Option<(String, String, String)> = None; // (section, key, value-so-far)

    for raw_line in text.lines() {
        // Honour systemd's `\` line continuation: collapse continued lines
        // before any other processing. (We don't need to — none of our units
        // use it today — but we may encounter them on hand-edited hosts.)
        let line = raw_line.trim_end_matches('\r');

        // `;` and `#` comments are stripped, but only when they appear at
        // the start of a line (systemd treats inline `#` literally).
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }

        // Section header.
        if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 3 {
            // Flush any pending continuation into the previous section.
            if let Some((sec, key, val)) = continuation.take() {
                directives.entry(sec).or_default().push((key, val));
            }
            current_section = Some(trimmed[1..trimmed.len() - 1].to_string());
            continue;
        }

        let Some(section) = current_section.as_ref() else {
            return Err(ParseError::DirectiveOutsideSection(trimmed.to_string()));
        };

        // Continuation lines append to the previous value with a single
        // space separator.
        if let Some((sec, key, mut val)) = continuation.take() {
            // The previous value ended with `\`; append this line.
            let next = trimmed.trim_end_matches('\\').trim();
            if !val.is_empty() && !next.is_empty() {
                val.push(' ');
            }
            val.push_str(next);
            if trimmed.ends_with('\\') {
                continuation = Some((sec, key, val));
            } else {
                directives.entry(sec).or_default().push((key, val));
            }
            continue;
        }

        // Standard `Key=Value` line.
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if let Some(stripped) = value.strip_suffix('\\') {
                continuation = Some((section.clone(), key, stripped.trim().to_string()));
            } else {
                directives
                    .entry(section.clone())
                    .or_default()
                    .push((key, value));
            }
        }
    }

    if let Some((sec, key, val)) = continuation.take() {
        directives.entry(sec).or_default().push((key, val));
    }

    let mut view = UnitView {
        user: None,
        working_directory: None,
        exec_start: None,
        api_port: None,
        directives,
    };

    // Pull the directives most callers care about into typed fields. We
    // pick the *last* assignment per key, mirroring systemd semantics.
    if let Some(service) = view.directives.get("Service") {
        for (k, v) in service.iter() {
            match k.as_str() {
                "User" => view.user = Some(v.clone()),
                "WorkingDirectory" => view.working_directory = Some(PathBuf::from(v)),
                "ExecStart" => {
                    view.exec_start = Some(v.clone());
                    view.api_port = parse_api_port_from_exec_start(v);
                }
                _ => {}
            }
        }
    }

    Ok(view)
}

/// The bash `ExecStart=` line includes `-rest-api-interface localhost:PORT`.
/// Extracting the port lets us seed `state.toml::nodes[].api_port` from a
/// pure parse instead of probing the running service.
fn parse_api_port_from_exec_start(exec_start: &str) -> Option<u16> {
    // Tokenise on whitespace, ignoring args inside quotes (the bash never
    // quotes here). Find the value following `-rest-api-interface` and
    // extract the substring after the last `:`.
    let mut iter = exec_start.split_whitespace();
    while let Some(token) = iter.next() {
        if token == "-rest-api-interface" {
            if let Some(value) = iter.next() {
                if let Some((_, port)) = value.rsplit_once(':') {
                    return port.parse().ok();
                }
            }
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{render_canonical_node_unit, NodeUnitSpec};
    use mxnode_core::NodeIndex;
    use std::path::PathBuf;

    fn spec_node_0<'a>(workdir: &'a PathBuf) -> NodeUnitSpec<'a> {
        NodeUnitSpec {
            index: NodeIndex::new(0),
            custom_user: "ubuntu",
            workdir,
            api_port: 8080,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
        }
    }

    #[test]
    fn parses_canonical_node_unit() {
        let workdir = PathBuf::from("/home/ubuntu/elrond-nodes/node-0");
        let unit = render_canonical_node_unit(&spec_node_0(&workdir));
        let view = parse_unit_text(&unit).unwrap();
        assert_eq!(view.user.as_deref(), Some("ubuntu"));
        assert_eq!(view.working_directory, Some(workdir));
        assert_eq!(view.api_port, Some(8080));
        assert!(view.exec_start.is_some());
    }

    /// The unit parser is whitespace-insensitive (trims leading
    /// indentation per line) so it survives operator hand-edits or
    /// units generated by other tooling. Verify that property here
    /// without depending on a specific legacy renderer.
    #[test]
    fn parser_tolerates_two_space_indented_input() {
        let indented = "\
[Unit]
  Description=MultiversX Node-0
  After=network-online.target

  [Service]
  User=ubuntu
  WorkingDirectory=/home/ubuntu/elrond-nodes/node-0
  ExecStart=/home/ubuntu/elrond-nodes/node-0/node -rest-api-interface localhost:8080
  Restart=always
";
        let view = parse_unit_text(indented).unwrap();
        assert_eq!(view.user.as_deref(), Some("ubuntu"));
        assert_eq!(view.api_port, Some(8080));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let unit = "\
[Unit]
# leading comment
Description=Example

; another comment style
[Service]
User=ubuntu
ExecStart=/bin/true
";
        let view = parse_unit_text(unit).unwrap();
        assert_eq!(view.user.as_deref(), Some("ubuntu"));
        assert_eq!(view.exec_start.as_deref(), Some("/bin/true"));
    }

    #[test]
    fn directive_outside_section_is_an_error() {
        let unit = "User=oops\n";
        let err = parse_unit_text(unit).unwrap_err();
        assert!(matches!(err, ParseError::DirectiveOutsideSection(_)));
    }

    #[test]
    fn line_continuations_are_collapsed() {
        let unit = "\
[Service]
ExecStart=/bin/foo \\
  --first-flag value \\
  --second-flag other
User=ubuntu
";
        let view = parse_unit_text(unit).unwrap();
        assert_eq!(
            view.exec_start.as_deref(),
            Some("/bin/foo --first-flag value --second-flag other"),
        );
        assert_eq!(view.user.as_deref(), Some("ubuntu"));
    }

    #[test]
    fn extracts_api_port_from_complex_exec_start() {
        let port = parse_api_port_from_exec_start(
            "/path/to/node -use-log-view -log-level *:DEBUG -rest-api-interface 127.0.0.1:8085 -extra",
        );
        assert_eq!(port, Some(8085));
    }

    #[test]
    fn missing_rest_api_interface_returns_none() {
        assert_eq!(
            parse_api_port_from_exec_start("/path/to/node -no-port-flag"),
            None
        );
    }

    #[test]
    fn flatten_emits_section_dotted_pairs() {
        let unit = "\
[Service]
User=ubuntu
ExecStart=/bin/true
";
        let view = parse_unit_text(unit).unwrap();
        let flat: Vec<String> = view.flatten().collect();
        assert!(flat.contains(&"Service.User=ubuntu".to_string()));
        assert!(flat.contains(&"Service.ExecStart=/bin/true".to_string()));
    }

    #[test]
    fn classifies_node_and_proxy_filenames() {
        assert!(matches!(
            DiscoveredKind::from_unit_filename("elrond-node-0.service"),
            Some(DiscoveredKind::Node(idx)) if idx.get() == 0,
        ));
        assert!(matches!(
            DiscoveredKind::from_unit_filename("elrond-node-7.service"),
            Some(DiscoveredKind::Node(idx)) if idx.get() == 7,
        ));
        assert!(matches!(
            DiscoveredKind::from_unit_filename("elrond-proxy.service"),
            Some(DiscoveredKind::Proxy),
        ));
    }

    #[test]
    fn ignores_unrelated_filenames() {
        assert!(DiscoveredKind::from_unit_filename("nginx.service").is_none());
        assert!(DiscoveredKind::from_unit_filename("elrond-node-x.service").is_none());
        assert!(DiscoveredKind::from_unit_filename("elrond-node-.service").is_none());
        assert!(DiscoveredKind::from_unit_filename("elrond-node-0").is_none());
    }

    /// `scan_systemd_dir` against a tempdir with synthetic units.
    #[test]
    fn scan_finds_nodes_and_proxy_in_stable_order() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let workdir = PathBuf::from("/home/ubuntu/elrond-nodes/node-0");
        let node_text = render_canonical_node_unit(&NodeUnitSpec {
            index: NodeIndex::new(0),
            custom_user: "ubuntu",
            workdir: &workdir,
            api_port: 8080,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
        });
        std::fs::write(dir.join("elrond-node-1.service"), &node_text).unwrap();
        std::fs::write(dir.join("elrond-node-0.service"), &node_text).unwrap();
        std::fs::write(dir.join("elrond-proxy.service"), &node_text).unwrap();
        // Unrelated files should be ignored.
        std::fs::write(dir.join("nginx.service"), "[Service]\nUser=root\n").unwrap();

        let found = scan_systemd_dir(dir).unwrap();
        let kinds: Vec<DiscoveredKind> = found.iter().map(|d| d.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                DiscoveredKind::Node(NodeIndex::new(0)),
                DiscoveredKind::Node(NodeIndex::new(1)),
                DiscoveredKind::Proxy,
            ],
            "ordering must be node-0, node-1, proxy",
        );
    }

    #[test]
    fn scan_returns_empty_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");
        let found = scan_systemd_dir(&nonexistent).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn scan_detects_drop_ins_alongside_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("elrond-node-0.service"),
            "[Service]\nUser=ubuntu\nExecStart=/bin/true\n",
        )
        .unwrap();
        let drop_dir = dir.join("elrond-node-0.service.d");
        std::fs::create_dir(&drop_dir).unwrap();
        std::fs::write(drop_dir.join("override.conf"), "[Service]\nNice=10\n").unwrap();
        std::fs::write(drop_dir.join("local.conf"), "[Service]\nMemoryHigh=12G\n").unwrap();
        std::fs::write(drop_dir.join("not-a-conf.txt"), "ignored").unwrap();

        let found = scan_systemd_dir(dir).unwrap();
        assert_eq!(found.len(), 1);
        let d = &found[0];
        assert!(d.has_drop_ins);
        assert_eq!(
            d.drop_ins,
            vec!["local.conf".to_string(), "override.conf".to_string()]
        );
    }

    #[test]
    fn scan_launchd_finds_node_plists() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let workdir = PathBuf::from("/Users/op/.mxnode/elrond-nodes/node-0");
        let plist = crate::plist::render_canonical_node_plist(&NodeUnitSpec {
            index: NodeIndex::new(0),
            custom_user: "op",
            workdir: &workdir,
            api_port: 8080,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
        });
        std::fs::write(dir.join("com.multiversx.elrond-node-0.plist"), &plist).unwrap();
        std::fs::write(dir.join("com.multiversx.elrond-proxy.plist"), &plist).unwrap();
        std::fs::write(dir.join("unrelated.plist"), b"<plist>x</plist>").unwrap();

        let found = scan_launchd_dir(dir).unwrap();
        let kinds: Vec<DiscoveredKind> = found.iter().map(|d| d.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                DiscoveredKind::Node(NodeIndex::new(0)),
                DiscoveredKind::Proxy,
            ],
        );
        assert_eq!(found[0].unit, "elrond-node-0.service");
        assert_eq!(found[1].unit, "elrond-proxy.service");
    }

    #[test]
    fn parse_plist_text_extracts_workdir_and_api_port() {
        let workdir = PathBuf::from("/Users/op/.mxnode/elrond-nodes/node-3");
        let plist = crate::plist::render_canonical_node_plist(&NodeUnitSpec {
            index: NodeIndex::new(3),
            custom_user: "op",
            workdir: &workdir,
            api_port: 8083,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
        });
        let view = parse_plist_text(&plist);
        assert_eq!(view.working_directory, Some(workdir));
        assert_eq!(view.api_port, Some(8083));
        let exec = view
            .exec_start
            .expect("ExecStart populated from ProgramArguments");
        assert!(exec.contains("/node "));
        assert!(exec.contains("localhost:8083"));
    }

    #[test]
    fn scan_supervisor_dir_is_a_thin_wrapper() {
        // Linux mode (default for the dev box's runtime path on CI)
        // and macOS mode hit the same shape; the helper exists only so
        // the orchestrator doesn't have to branch.
        let tmp = tempfile::tempdir().unwrap();
        let found = scan_supervisor_dir(tmp.path()).unwrap();
        assert!(
            found.is_empty(),
            "empty dir → empty result regardless of platform"
        );
    }

    #[test]
    fn scan_yields_view_consistent_with_parse_unit_text() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let workdir = PathBuf::from("/home/ubuntu/elrond-nodes/node-0");
        let text = render_canonical_node_unit(&NodeUnitSpec {
            index: NodeIndex::new(0),
            custom_user: "ubuntu",
            workdir: &workdir,
            api_port: 8080,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
        });
        std::fs::write(dir.join("elrond-node-0.service"), &text).unwrap();
        let found = scan_systemd_dir(dir).unwrap();
        assert_eq!(found.len(), 1);
        let d = &found[0];
        assert_eq!(d.view.user.as_deref(), Some("ubuntu"));
        assert_eq!(d.view.api_port, Some(8080));
        assert_eq!(d.raw_text, text);
    }
}
