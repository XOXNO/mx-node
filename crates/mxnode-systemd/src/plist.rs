//! launchd plist renderer for macOS hosts.
//!
//! Plists are key/value XML; we hand-roll the writer rather than pull
//! `plist` as a dependency because we only emit a tiny known shape and
//! want it to be byte-stable for golden tests + adoption parity.
//!
//! What the rendered plist guarantees, mapping from the systemd unit:
//!   - `Label`            ← "com.multiversx.elrond-node-{INDEX}"
//!   - `ProgramArguments` ← [node binary, "-use-log-view", ..., "-rest-api-interface", "localhost:{PORT}", ...extra]
//!   - `WorkingDirectory` ← node-{i}
//!   - `RunAtLoad`        ← true (matches systemd `WantedBy=multi-user.target`)
//!   - `KeepAlive`        ← true (matches `Restart=always`)
//!   - `ThrottleInterval` ← restart_sec (matches `RestartSec`)
//!   - `StandardOutPath`  ← {workdir}/logs/stdout.log
//!   - `StandardErrorPath`← {workdir}/logs/stderr.log
//!   - `SoftResourceLimits.NumberOfFiles` ← limit_nofile (matches `LimitNOFILE`)
//!   - `ProcessType`      ← "Background"

use std::path::Path;

use crate::render::NodeUnitSpec;

/// Reverse-DNS prefix used for every LaunchAgent the operator runs as
/// part of an mxnode install. Surfaces in `launchctl list` and any
/// macOS Console.app filter the operator sets up.
pub const LAUNCH_AGENT_PREFIX: &str = "com.multiversx";

/// Stable plist filename for `index`. Used by both renderer and discovery.
pub fn launchd_label(index: mxnode_core::NodeIndex) -> String {
    format!("{LAUNCH_AGENT_PREFIX}.elrond-node-{}", index.get())
}

pub fn launchd_filename(index: mxnode_core::NodeIndex) -> String {
    format!("{}.plist", launchd_label(index))
}

/// Emit the plist for one node. Byte-stable so adoption can compare.
pub fn render_canonical_node_plist(spec: &NodeUnitSpec<'_>) -> String {
    let workdir = spec.workdir.display();
    let label = launchd_label(spec.index);
    let stdout_path = format!("{workdir}/logs/stdout.log");
    let stderr_path = format!("{workdir}/logs/stderr.log");

    let exec_args = build_exec_args(spec);

    let mut out = String::with_capacity(1024);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    out.push_str("<plist version=\"1.0\">\n");
    out.push_str("<dict>\n");

    push_kv_string(&mut out, "Label", &label);
    push_kv_array_strings(&mut out, "ProgramArguments", &exec_args);
    push_kv_string(&mut out, "WorkingDirectory", &workdir.to_string());
    push_kv_string(&mut out, "StandardOutPath", &stdout_path);
    push_kv_string(&mut out, "StandardErrorPath", &stderr_path);
    push_kv_bool(&mut out, "RunAtLoad", true);
    push_kv_bool(&mut out, "KeepAlive", true);
    push_kv_integer(&mut out, "ThrottleInterval", spec.restart_sec as i64);
    push_kv_string(&mut out, "ProcessType", "Background");

    // SoftResourceLimits.NumberOfFiles → LimitNOFILE equivalent.
    out.push_str("    <key>SoftResourceLimits</key>\n");
    out.push_str("    <dict>\n");
    out.push_str("        <key>NumberOfFiles</key>\n");
    out.push_str(&format!(
        "        <integer>{}</integer>\n",
        spec.limit_nofile,
    ));
    out.push_str("    </dict>\n");

    out.push_str("</dict>\n");
    out.push_str("</plist>\n");
    out
}

fn build_exec_args(spec: &NodeUnitSpec<'_>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        spec.workdir.join("node").display().to_string(),
        "-use-log-view".to_string(),
        "-log-logger-name".to_string(),
        "-log-correlation".to_string(),
        "-log-level".to_string(),
        spec.log_level.to_string(),
        "-rest-api-interface".to_string(),
        format!("localhost:{}", spec.api_port),
    ];
    if let Some(mode) = spec.operation_mode {
        args.push("--operation-mode".to_string());
        args.push(mode.to_string());
    }
    // Operator-supplied extra flags are tokenised on whitespace. The
    // bash interpolates them as a single string into ExecStart=, which
    // is the same behaviour systemd applies; on macOS we have to be
    // explicit because plist ProgramArguments is a list, not a string.
    for flag in spec.extra_flags.split_whitespace() {
        if !flag.is_empty() {
            args.push(flag.to_string());
        }
    }
    args
}

fn push_kv_string(out: &mut String, key: &str, value: &str) {
    out.push_str(&format!("    <key>{key}</key>\n"));
    out.push_str(&format!("    <string>{}</string>\n", xml_escape(value)));
}

fn push_kv_bool(out: &mut String, key: &str, value: bool) {
    out.push_str(&format!("    <key>{key}</key>\n"));
    out.push_str(if value {
        "    <true/>\n"
    } else {
        "    <false/>\n"
    });
}

fn push_kv_integer(out: &mut String, key: &str, value: i64) {
    out.push_str(&format!("    <key>{key}</key>\n"));
    out.push_str(&format!("    <integer>{value}</integer>\n"));
}

fn push_kv_array_strings(out: &mut String, key: &str, values: &[String]) {
    out.push_str(&format!("    <key>{key}</key>\n"));
    out.push_str("    <array>\n");
    for v in values {
        out.push_str(&format!("        <string>{}</string>\n", xml_escape(v)));
    }
    out.push_str("    </array>\n");
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Default user LaunchAgent directory for the running operator. Returns
/// `None` when `$HOME` is unavailable (rare; e.g. systemd-run with a
/// scrubbed env). Callers fall back to `paths.runtime` in that case.
pub fn user_launch_agents_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents"))
}

/// Where one node's plist should land on macOS.
pub fn user_launch_agent_path(home: &Path, index: mxnode_core::NodeIndex) -> std::path::PathBuf {
    home.join("Library/LaunchAgents")
        .join(launchd_filename(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxnode_core::NodeIndex;
    use std::path::PathBuf;

    fn spec_node_0<'a>(workdir: &'a PathBuf) -> NodeUnitSpec<'a> {
        NodeUnitSpec {
            index: NodeIndex::new(0),
            custom_user: "ignored-on-macos",
            workdir,
            api_port: 8080,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
            operation_mode: None,
        }
    }

    /// Captured-by-design golden plist. Locked so future changes have to
    /// update this string deliberately, the same way Linux units are
    /// locked against the bash output.
    fn golden_node_0() -> String {
        let mut s = String::new();
        s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        s.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
        s.push_str("<plist version=\"1.0\">\n");
        s.push_str("<dict>\n");
        s.push_str("    <key>Label</key>\n");
        s.push_str("    <string>com.multiversx.elrond-node-0</string>\n");
        s.push_str("    <key>ProgramArguments</key>\n");
        s.push_str("    <array>\n");
        s.push_str("        <string>/Users/op/.mxnode/elrond-nodes/node-0/node</string>\n");
        s.push_str("        <string>-use-log-view</string>\n");
        s.push_str("        <string>-log-logger-name</string>\n");
        s.push_str("        <string>-log-correlation</string>\n");
        s.push_str("        <string>-log-level</string>\n");
        s.push_str("        <string>*:DEBUG</string>\n");
        s.push_str("        <string>-rest-api-interface</string>\n");
        s.push_str("        <string>localhost:8080</string>\n");
        s.push_str("    </array>\n");
        s.push_str("    <key>WorkingDirectory</key>\n");
        s.push_str("    <string>/Users/op/.mxnode/elrond-nodes/node-0</string>\n");
        s.push_str("    <key>StandardOutPath</key>\n");
        s.push_str("    <string>/Users/op/.mxnode/elrond-nodes/node-0/logs/stdout.log</string>\n");
        s.push_str("    <key>StandardErrorPath</key>\n");
        s.push_str("    <string>/Users/op/.mxnode/elrond-nodes/node-0/logs/stderr.log</string>\n");
        s.push_str("    <key>RunAtLoad</key>\n");
        s.push_str("    <true/>\n");
        s.push_str("    <key>KeepAlive</key>\n");
        s.push_str("    <true/>\n");
        s.push_str("    <key>ThrottleInterval</key>\n");
        s.push_str("    <integer>3</integer>\n");
        s.push_str("    <key>ProcessType</key>\n");
        s.push_str("    <string>Background</string>\n");
        s.push_str("    <key>SoftResourceLimits</key>\n");
        s.push_str("    <dict>\n");
        s.push_str("        <key>NumberOfFiles</key>\n");
        s.push_str("        <integer>4096</integer>\n");
        s.push_str("    </dict>\n");
        s.push_str("</dict>\n");
        s.push_str("</plist>\n");
        s
    }

    #[test]
    fn renders_byte_identical_golden_for_index_zero() {
        let workdir = PathBuf::from("/Users/op/.mxnode/elrond-nodes/node-0");
        let plist = render_canonical_node_plist(&spec_node_0(&workdir));
        assert_eq!(plist, golden_node_0());
    }

    #[test]
    fn extra_flags_become_array_entries() {
        let workdir = PathBuf::from("/Users/op/.mxnode/elrond-nodes/node-3");
        let spec = NodeUnitSpec {
            index: NodeIndex::new(3),
            custom_user: "ignored",
            workdir: &workdir,
            api_port: 8083,
            log_level: "*:INFO",
            limit_nofile: 8192,
            restart_sec: 5,
            extra_flags: "-profile-mode -display-name custom",
            operation_mode: None,
        };
        let plist = render_canonical_node_plist(&spec);
        assert!(plist.contains("<string>-profile-mode</string>"));
        assert!(plist.contains("<string>-display-name</string>"));
        assert!(plist.contains("<string>custom</string>"));
        assert!(plist.contains("<integer>5</integer>")); // ThrottleInterval
        assert!(plist.contains("<integer>8192</integer>")); // SoftResourceLimits
        assert!(plist.contains("<string>localhost:8083</string>"));
    }

    #[test]
    fn operation_mode_becomes_program_arguments_entries() {
        let workdir = PathBuf::from("/Users/op/.mxnode/elrond-nodes/node-1");
        let spec = NodeUnitSpec {
            index: NodeIndex::new(1),
            custom_user: "ignored",
            workdir: &workdir,
            api_port: 8081,
            log_level: "*:INFO",
            limit_nofile: 8192,
            restart_sec: 5,
            extra_flags: "-log-save",
            operation_mode: Some("db-lookup-extension"),
        };
        let plist = render_canonical_node_plist(&spec);
        assert!(plist.contains("<string>--operation-mode</string>"));
        assert!(plist.contains("<string>db-lookup-extension</string>"));
        assert!(plist.contains("<string>-log-save</string>"));
    }

    #[test]
    fn xml_escape_handles_special_chars_in_workdir() {
        let workdir = PathBuf::from("/Users/op/path with \"quotes\" & ampersands");
        let plist = render_canonical_node_plist(&spec_node_0(&workdir));
        assert!(plist.contains("&amp;"));
        assert!(plist.contains("&quot;"));
        // Round-trip: parsing as XML should not blow up. We don't pull
        // a full XML parser; instead we assert that the entity-escaped
        // forms are present and that the raw `"` does not appear in
        // any string body.
        assert!(!plist.contains(" with \"quotes\""));
    }

    #[test]
    fn label_and_filename_match_convention() {
        assert_eq!(
            launchd_label(NodeIndex::new(0)),
            "com.multiversx.elrond-node-0"
        );
        assert_eq!(
            launchd_filename(NodeIndex::new(7)),
            "com.multiversx.elrond-node-7.plist"
        );
    }

    #[test]
    fn user_launch_agent_path_under_home() {
        let home = PathBuf::from("/Users/op");
        let p = user_launch_agent_path(&home, NodeIndex::new(2));
        assert_eq!(
            p,
            PathBuf::from("/Users/op/Library/LaunchAgents/com.multiversx.elrond-node-2.plist"),
        );
    }
}
