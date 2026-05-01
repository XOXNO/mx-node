use mxnode_core::Config;

/// Aggregated validation feedback for `mxnode config validate`.
///
/// Errors block state-changing ops; warnings surface but don't block.
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Static checks that don't touch the network or the disk.
///
/// Network-dependent checks (token works, repos reachable) live behind
/// `--strict` and call into `mxnode-github`; that's wired up in the CLI
/// orchestrator, not here.
pub fn validate(cfg: &Config) -> ValidationReport {
    let mut report = ValidationReport::default();

    if cfg.network.environment.is_none() {
        report
            .errors
            .push("network.environment is unset; choose one of mainnet|testnet|devnet".to_string());
    }

    if cfg.paths.custom_user.trim().is_empty() {
        report.errors.push("paths.custom_user is empty".to_string());
    }

    if cfg.node.api_port_base < 1024 {
        report.warnings.push(format!(
            "node.api_port_base = {} is in the privileged range; nodes typically need a non-root systemd User",
            cfg.node.api_port_base,
        ));
    }

    if cfg.node.limit_nofile < 4096 {
        report.warnings.push(format!(
            "node.limit_nofile = {} is below the recommended 4096; nodes may hit fd limits",
            cfg.node.limit_nofile,
        ));
    }

    if cfg.install.binary_keep == 0 {
        report
            .errors
            .push("install.binary_keep must be at least 1 to allow rollback".to_string());
    }

    if cfg.proxy.observers_shards.is_empty() {
        report.warnings.push(
            "proxy.observers_shards is empty; the proxy will have no shard mappings".to_string(),
        );
    }

    // Per-field shape validation. systemd unit files are line-based; any
    // newline / NUL inside `extra_flags` would produce a unit file the
    // kernel refuses to parse, and an unbalanced double-quote breaks the
    // ExecStart= directive. Catching these at config-load is much friendlier
    // than discovering them when systemctl rejects the rendered unit.
    if let Some(reason) = invalid_extra_flags(&cfg.node.extra_flags) {
        report
            .errors
            .push(format!("node.extra_flags is invalid: {reason}"));
    }
    for node in &cfg.nodes {
        if let Some(reason) = invalid_extra_flags(&node.extra_flags) {
            report.errors.push(format!(
                "nodes[index={}].extra_flags is invalid: {reason}",
                node.index.get(),
            ));
        }
    }

    report
}

/// Returns `Some(reason)` if the supplied flags string would break the
/// rendered systemd unit. Empty strings are accepted (default).
fn invalid_extra_flags(flags: &str) -> Option<String> {
    if flags.contains('\n') || flags.contains('\r') {
        return Some("must not contain newlines (would split the unit file)".to_string());
    }
    if flags.contains('\0') {
        return Some("must not contain NUL".to_string());
    }
    // Heuristic for unbalanced quotes — if there's an odd number of `"`
    // characters, the rendered ExecStart= line will be malformed. We don't
    // try to track full shell quoting; a perfectly-balanced shell quote
    // sequence is fine, an obviously-broken one isn't.
    let quote_count = flags.matches('"').count();
    if !quote_count.is_multiple_of(2) {
        return Some("contains an unbalanced double-quote".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_invalid_until_environment_set() {
        let cfg = Config::default();
        let r = validate(&cfg);
        assert!(!r.ok());
        assert!(r.errors.iter().any(|e| e.contains("environment")));
    }

    #[test]
    fn minimal_valid_config_passes() {
        let mut cfg = Config::default();
        cfg.network.environment = Some(mxnode_core::Environment::Mainnet);
        let r = validate(&cfg);
        assert!(r.ok(), "errors: {:?}", r.errors);
    }

    #[test]
    fn binary_keep_zero_errors() {
        let mut cfg = Config::default();
        cfg.network.environment = Some(mxnode_core::Environment::Mainnet);
        cfg.install.binary_keep = 0;
        let r = validate(&cfg);
        assert!(!r.ok());
        assert!(r.errors.iter().any(|e| e.contains("binary_keep")));
    }

    #[test]
    fn low_limit_nofile_warns() {
        let mut cfg = Config::default();
        cfg.network.environment = Some(mxnode_core::Environment::Mainnet);
        cfg.node.limit_nofile = 1024;
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r.warnings.iter().any(|w| w.contains("limit_nofile")));
    }

    #[test]
    fn extra_flags_with_newline_rejected() {
        let mut cfg = Config::default();
        cfg.network.environment = Some(mxnode_core::Environment::Mainnet);
        cfg.node.extra_flags = "-display-name something\nmalicious".to_string();
        let r = validate(&cfg);
        assert!(!r.ok());
        assert!(r.errors.iter().any(|e| e.contains("newlines")));
    }

    #[test]
    fn extra_flags_with_carriage_return_rejected() {
        let mut cfg = Config::default();
        cfg.network.environment = Some(mxnode_core::Environment::Mainnet);
        cfg.node.extra_flags = "-flag\rother".to_string();
        let r = validate(&cfg);
        assert!(!r.ok());
    }

    #[test]
    fn extra_flags_with_unbalanced_quote_rejected() {
        let mut cfg = Config::default();
        cfg.network.environment = Some(mxnode_core::Environment::Mainnet);
        cfg.node.extra_flags = "-display-name \"unterminated".to_string();
        let r = validate(&cfg);
        assert!(!r.ok());
        assert!(r
            .errors
            .iter()
            .any(|e| e.contains("unbalanced double-quote")));
    }

    #[test]
    fn extra_flags_with_balanced_quotes_accepted() {
        let mut cfg = Config::default();
        cfg.network.environment = Some(mxnode_core::Environment::Mainnet);
        cfg.node.extra_flags = "-display-name \"my node\" -profile-mode true".to_string();
        let r = validate(&cfg);
        assert!(r.ok(), "errors: {:?}", r.errors);
    }

    #[test]
    fn empty_extra_flags_accepted() {
        let mut cfg = Config::default();
        cfg.network.environment = Some(mxnode_core::Environment::Mainnet);
        cfg.node.extra_flags = String::new();
        assert!(validate(&cfg).ok());
    }

    #[test]
    fn per_node_extra_flags_validated_individually() {
        use mxnode_core::config::NodeOverride;
        use mxnode_core::{NodeIndex, Role, Shard};
        let mut cfg = Config::default();
        cfg.network.environment = Some(mxnode_core::Environment::Mainnet);
        cfg.nodes.push(NodeOverride {
            index: NodeIndex::new(2),
            role: Role::Validator,
            shard: Shard::Auto,
            display_name: String::new(),
            extra_flags: "valid -flag".to_string(),
        });
        cfg.nodes.push(NodeOverride {
            index: NodeIndex::new(3),
            role: Role::Validator,
            shard: Shard::Auto,
            display_name: String::new(),
            extra_flags: "broken\nnewline".to_string(),
        });
        let r = validate(&cfg);
        assert!(!r.ok());
        assert!(
            r.errors.iter().any(|e| e.contains("nodes[index=3]")),
            "errors should call out node index 3 specifically: {:?}",
            r.errors,
        );
    }
}
