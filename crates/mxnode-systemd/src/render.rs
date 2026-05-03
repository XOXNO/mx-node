use std::path::Path;

use mxnode_core::NodeIndex;

/// Inputs needed to render `elrond-node-{INDEX}.service`. Mirrors exactly the
/// variables the bash `systemd` function reads (`$INDEX`, `$CUSTOM_USER`,
/// `$WORKDIR`, `$APIPORT`, `$NODE_EXTRA_FLAGS`).
pub struct NodeUnitSpec<'a> {
    pub index: NodeIndex,
    pub custom_user: &'a str,
    pub workdir: &'a Path,
    pub api_port: u16,
    pub log_level: &'a str,
    pub limit_nofile: u32,
    pub restart_sec: u32,
    pub extra_flags: &'a str,
    pub operation_mode: Option<&'a str>,
}

/// Inputs for `elrond-proxy.service`.
pub struct ProxyUnitSpec<'a> {
    pub custom_user: &'a str,
    pub proxy_dir: &'a Path,
    pub limit_nofile: u32,
    pub restart_sec: u32,
}

/// Render `elrond-node-{INDEX}.service` in canonical systemd format.
///
/// Standard systemd convention: no leading indent inside sections, blank
/// lines have no trailing whitespace, no trailing space on `ExecStart`.
pub fn render_canonical_node_unit(spec: &NodeUnitSpec<'_>) -> String {
    let workdir = spec.workdir.display();
    let mut out = String::new();
    out.push_str("[Unit]\n");
    out.push_str(&format!(
        "Description=MultiversX Node-{}\n",
        spec.index.get()
    ));
    out.push_str("After=network-online.target\n");
    out.push('\n');
    out.push_str("[Service]\n");
    out.push_str(&format!("User={}\n", spec.custom_user));
    out.push_str(&format!("WorkingDirectory={}\n", workdir));
    let exec_base = format!(
        "ExecStart={workdir}/node -use-log-view -log-logger-name -log-correlation -log-level {log_level} -rest-api-interface localhost:{port}",
        workdir = workdir,
        log_level = spec.log_level,
        port = spec.api_port,
    );
    let mut exec = exec_base;
    if let Some(mode) = spec.operation_mode {
        exec.push_str(" --operation-mode ");
        exec.push_str(mode);
    }
    if spec.extra_flags.trim().is_empty() {
        out.push_str(&exec);
        out.push('\n');
    } else {
        out.push_str(&exec);
        out.push(' ');
        out.push_str(spec.extra_flags);
        out.push('\n');
    }
    out.push_str("StandardOutput=journal\n");
    out.push_str("StandardError=journal\n");
    out.push_str("Restart=always\n");
    out.push_str(&format!("RestartSec={}\n", spec.restart_sec));
    out.push_str(&format!("LimitNOFILE={}\n", spec.limit_nofile));
    out.push('\n');
    out.push_str("[Install]\n");
    out.push_str("WantedBy=multi-user.target\n");
    out
}

/// Render `elrond-proxy.service` in canonical systemd format.
pub fn render_canonical_proxy_unit(spec: &ProxyUnitSpec<'_>) -> String {
    let proxy_dir = spec.proxy_dir.display();
    let mut out = String::new();
    out.push_str("[Unit]\n");
    out.push_str("Description=MultiversX Proxy\n");
    out.push_str("After=network-online.target\n");
    out.push('\n');
    out.push_str("[Service]\n");
    out.push_str(&format!("User={}\n", spec.custom_user));
    out.push_str(&format!("WorkingDirectory={}\n", proxy_dir));
    out.push_str(&format!("ExecStart={}/proxy\n", proxy_dir));
    out.push_str("StandardOutput=journal\n");
    out.push_str("StandardError=journal\n");
    out.push_str("Restart=always\n");
    out.push_str(&format!("RestartSec={}\n", spec.restart_sec));
    out.push_str(&format!("LimitNOFILE={}\n", spec.limit_nofile));
    out.push('\n');
    out.push_str("[Install]\n");
    out.push_str("WantedBy=multi-user.target\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn golden_canonical_node_0() -> String {
        "\
[Unit]
Description=MultiversX Node-0
After=network-online.target

[Service]
User=ubuntu
WorkingDirectory=/home/ubuntu/elrond-nodes/node-0
ExecStart=/home/ubuntu/elrond-nodes/node-0/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8080
StandardOutput=journal
StandardError=journal
Restart=always
RestartSec=3
LimitNOFILE=4096

[Install]
WantedBy=multi-user.target
".to_string()
    }

    fn node_spec_0() -> (PathBuf, NodeIndex) {
        (
            PathBuf::from("/home/ubuntu/elrond-nodes/node-0"),
            NodeIndex::new(0),
        )
    }

    #[test]
    fn canonical_node_unit_has_no_leading_indent_or_trailing_space() {
        let (workdir, index) = node_spec_0();
        let spec = NodeUnitSpec {
            index,
            custom_user: "ubuntu",
            workdir: &workdir,
            api_port: 8080,
            log_level: "*:DEBUG",
            limit_nofile: 4096,
            restart_sec: 3,
            extra_flags: "",
            operation_mode: None,
        };
        let rendered = render_canonical_node_unit(&spec);
        assert_eq!(rendered, golden_canonical_node_0());
        assert!(
            !rendered.contains("  Description"),
            "canonical form must not have leading two-space indent",
        );
        assert!(
            !rendered.lines().any(|l| l.ends_with(' ')),
            "canonical form must not have trailing whitespace on any line",
        );
    }

    #[test]
    fn canonical_node_unit_appends_extra_flags_with_single_space() {
        let workdir = PathBuf::from("/srv/mxnode/elrond-nodes/node-3");
        let spec = NodeUnitSpec {
            index: NodeIndex::new(3),
            custom_user: "validator",
            workdir: &workdir,
            api_port: 8083,
            log_level: "*:INFO",
            limit_nofile: 8192,
            restart_sec: 5,
            extra_flags: "-profile-mode -display-name custom",
            operation_mode: None,
        };
        let rendered = render_canonical_node_unit(&spec);
        assert!(
            rendered.contains("localhost:8083 -profile-mode -display-name custom\n"),
            "rendered:\n{rendered}",
        );
        assert!(rendered.contains("Description=MultiversX Node-3"));
        assert!(rendered.contains("RestartSec=5"));
        assert!(rendered.contains("LimitNOFILE=8192"));
        assert!(rendered.contains("User=validator"));
    }

    #[test]
    fn canonical_node_unit_renders_operation_mode_before_extra_flags() {
        let workdir = PathBuf::from("/srv/mxnode/elrond-nodes/node-1");
        let spec = NodeUnitSpec {
            index: NodeIndex::new(1),
            custom_user: "validator",
            workdir: &workdir,
            api_port: 8081,
            log_level: "*:INFO",
            limit_nofile: 8192,
            restart_sec: 5,
            extra_flags: "-log-save",
            operation_mode: Some("full-archive"),
        };
        let rendered = render_canonical_node_unit(&spec);
        assert!(rendered.contains("localhost:8081 --operation-mode full-archive -log-save\n"));
    }
}
