//! Clap subcommand tree for `mxnode`. Mirrors the surface defined in the
//! plan.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "mxnode",
    version,
    about = "MultiversX node operator CLI",
    long_about = "Install, upgrade, and operate MultiversX nodes (validators, observer squads, multikey nodes, the proxy) on Linux hosts via systemd.\n\nmxnode does NOT phone home. The only outbound network requests it makes are to GitHub Releases (for upgrades) and the local node REST API.",
    propagate_version = true,
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Override the config file path. By default, mxnode reads
    /// `~/.config/mxnode/config.toml` then `/etc/mxnode/config.toml`.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Bypass `config validate` and on-disk state checks. Use with care.
    #[arg(long, global = true)]
    pub skip_safety_checks: bool,

    /// Emit machine-readable JSON. Stable schema across releases.
    /// When set, every command's output is JSON; takes precedence over any
    /// per-command `--format` flag.
    #[arg(long, global = true)]
    pub json: bool,

    /// Disable colour output, even on a TTY. Equivalent to setting
    /// `NO_COLOR=1`. CI logs and `journalctl` consumers tend to lie about
    /// being TTYs, so an explicit opt-out is necessary.
    #[arg(long, global = true)]
    pub no_color: bool,

    #[arg(long, global = true)]
    pub verbose: bool,

    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Configuration commands.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Install nodes from scratch.
    Install(InstallArgs),

    /// Add more nodes to an existing install.
    AddNodes(AddNodesArgs),

    /// Start units.
    Start(LifecycleArgs),
    /// Stop units.
    Stop(LifecycleArgs),
    /// Restart units.
    Restart(RestartArgs),
    /// Show install + per-node health snapshot.
    Status(StatusArgs),
    /// Tail or archive node logs.
    Logs(LogsArgs),
    /// Serve a Prometheus metrics endpoint.
    Metrics(MetricsArgs),

    /// Upgrade (or downgrade) nodes to a different tag. To go back to
    /// an old version, pass `--binary-tag <T>` for any T already in
    /// the binstore — the acquirer reuses the cached binary instead of
    /// re-downloading.
    Upgrade(UpgradeArgs),

    /// Database commands.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },

    /// Run the bundled assessment benchmark.
    Benchmark,

    /// Run keygenerator (utility wrapper around mx-chain-go's keygen).
    Keygen(KeygenArgs),

    /// Key management.
    Keys {
        #[command(subcommand)]
        command: KeysCommand,
    },

    /// Re-apply per-node TOML edits (display name, observer pins, and any
    /// `[overrides.prefs]` / `[overrides.config]` from your config) without
    /// touching binaries or restarting units.
    ReapplyConfig(ReapplyConfigArgs),

    /// Rename a single node's `NodeDisplayName` in both `state.toml` and
    /// the on-disk `prefs.toml`. Unlike a hand-edit of `prefs.toml`, the
    /// new name is persisted in `state.toml` so subsequent
    /// `reapply-config` / `upgrade` passes preserve it.
    Rename(RenameArgs),

    /// Live multi-node dashboard (ratatui). Better-than-termui replacement.
    Dashboard(DashboardArgs),

    /// Remove nodes, units, and binaries.
    Cleanup(CleanupArgs),

    /// Import an existing `mx-chain-scripts` (bash) install into mxnode's
    /// `state.toml`. Dry-run by default; pass `--execute` to write.
    MigrateBash(crate::commands::migrate::MigrateBashArgs),

    /// Full host diagnostic; suggests fixes.
    Doctor(DoctorArgs),

    /// Print version (also available via --version).
    Version,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the merged config.
    Show {
        #[arg(long)] origin: bool,
        #[arg(long, value_enum, default_value_t = Format::Toml)] format: Format,
    },
    /// Print one dotted-path value.
    Get { path: String },
    /// Set one dotted-path value, writing back to the chosen scope.
    Set {
        path: String,
        value: String,
        #[arg(long, value_enum, default_value_t = Scope::User)] scope: Scope,
    },
    /// Open the chosen scope's file in $EDITOR.
    Edit {
        #[arg(long, value_enum, default_value_t = Scope::User)] scope: Scope,
    },
    /// Run pre-flight validation.
    Validate {
        /// Also check network reachability (token, repos).
        #[arg(long)] strict: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Format {
    Toml,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Scope {
    User,
    System,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Number of nodes to install. Ignored when `--squad` is set, and
    /// rejected for `--role multikey` (multikey is always a 4-shard
    /// squad by design). Defaults to 1.
    #[arg(long, conflicts_with = "squad")] pub count: Option<u16>,
    #[arg(long)] pub config_tag: Option<String>,
    #[arg(long)] pub binary_tag: Option<String>,
    #[arg(long)] pub proxy_tag: Option<String>,
    #[arg(long)] pub name_template: Option<String>,
    /// Role for every node in this install. Validator (default) expects
    /// an operator-supplied `node-{i}.zip` per node. Observer
    /// auto-generates a throwaway BLS key on first start. Multikey
    /// signs for an operator-supplied `allValidatorsKeys.pem` bundle
    /// and is **always** a 4-shard squad — `--squad` is implicit.
    #[arg(long, value_enum, default_value_t = RoleArg::Validator)]
    pub role: RoleArg,
    /// Install a 4-node squad pinned to shards 0, 1, 2, and metachain.
    /// Use with `--role validator` or `--role observer` to opt into
    /// the squad layout. Implicit and unnecessary for `--role multikey`.
    #[arg(long)] pub squad: bool,
    /// Also install the MultiversX proxy alongside the nodes. Off by
    /// default — many operators host the proxy on a separate box and
    /// point their squads at it over the network.
    #[arg(long)] pub with_proxy: bool,
    /// Path to the operator's `allValidatorsKeys.pem` (the bundle of
    /// every BLS key the multikey nodes will sign for). Required for
    /// `--role multikey`; rejected for any other role.
    ///
    /// If omitted on a multikey install, mxnode looks for
    /// `<node_keys>/allValidatorsKeys.pem` (the same directory that
    /// holds validator zips by convention) and uses it if present.
    #[arg(long, value_name = "PATH")] pub keys_file: Option<PathBuf>,
    /// Mark this multikey host as a backup for an existing primary.
    /// Both machines must share the **same** `allValidatorsKeys.pem`;
    /// only the redundancy level differs. Pass `--backup` for level 1
    /// (the common case), `--backup 2` for a backup-of-backup, etc.
    /// Omit entirely for a primary install (`RedundancyLevel = 0`).
    /// Requires `--role multikey`.
    #[arg(
        long,
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "1",
    )]
    pub backup: Option<u8>,
    #[arg(long)] pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct AddNodesArgs {
    #[arg(long, default_value_t = 1)] pub count: u16,
    #[arg(long, value_enum)] pub role: Option<RoleArg>,
    /// Override the `node.name_template` config value for the new nodes
    /// only. Existing nodes keep their persisted `display_name`. Useful
    /// when the second wave should be named differently from the first
    /// (e.g. `--name-template "extra-{index}"` while the original install
    /// used `mx-chain-mainnet-validator-{index}`).
    #[arg(long)] pub name_template: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RoleArg {
    Validator,
    Observer,
    Multikey,
}

/// Mutually-exclusive ways to pick which nodes a lifecycle command targets.
///
/// `clap`'s `ArgGroup` (multiple = false) enforces "at most one of these"
/// without us writing manual validation in every command handler. We don't
/// require one — commands that need a target check for empty selection at
/// runtime and refuse with a structured error.
#[derive(Debug, Args)]
#[command(group = clap::ArgGroup::new("lifecycle_selector")
    .required(false)
    .multiple(false)
    .args(["all", "select", "validators_only", "observers_only", "shard", "node"]))]
pub struct LifecycleArgs {
    #[arg(long, group = "lifecycle_selector")] pub all: bool,
    #[arg(long, group = "lifecycle_selector")] pub select: Option<String>,
    #[arg(long, group = "lifecycle_selector")] pub validators_only: bool,
    #[arg(long, group = "lifecycle_selector")] pub observers_only: bool,
    #[arg(long, group = "lifecycle_selector")] pub shard: Option<String>,
    #[arg(long, group = "lifecycle_selector")] pub node: Vec<u16>,
}

#[derive(Debug, Args)]
pub struct RestartArgs {
    #[command(flatten)] pub select: LifecycleArgs,
    #[arg(long, value_enum, default_value_t = Strategy::Rolling)] pub strategy: Strategy,
    #[arg(long, default_value_t = 1)] pub max_parallel: u16,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Strategy {
    Rolling,
    Parallel,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[arg(long)] pub watch: bool,
    /// Refresh interval in seconds when `--watch` is set. Ignored otherwise.
    #[arg(long, default_value_t = 5, value_name = "SECS")] pub interval: u64,
    #[arg(long, value_enum, default_value_t = StatusFormat::Table)] pub format: StatusFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StatusFormat {
    Table,
    Json,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    #[arg(long)] pub node: Vec<u16>,
    #[arg(long)] pub since: Option<String>,
    /// Tail logs as they arrive. Cannot be combined with `--save-archive`.
    #[arg(long, short = 'f', conflicts_with = "save_archive")] pub follow: bool,
    /// Replicates `get_logs` — produces a tar.gz under $CUSTOM_HOME/mx-chain-logs.
    /// Cannot be combined with `--follow`.
    #[arg(long)] pub save_archive: bool,
}

#[derive(Debug, Args)]
pub struct MetricsArgs {
    #[arg(long, default_value_t = 9090)] pub port: u16,
}

/// Mutually-exclusive selector flags on `mxnode upgrade`. `clap`'s
/// ArgGroup with `multiple = false` enforces "at most one" at parse
/// time, matching how `LifecycleArgs` handles the same set so both
/// surfaces stay consistent.
#[derive(Debug, Args)]
#[command(group = clap::ArgGroup::new("upgrade_selector")
    .required(false)
    .multiple(false)
    .args(["select", "node", "shard"]))]
pub struct UpgradeArgs {
    #[command(subcommand)]
    pub target: Option<UpgradeTarget>,

    #[arg(long)] pub config_tag: Option<String>,
    #[arg(long)] pub binary_tag: Option<String>,
    #[arg(long)] pub proxy_tag: Option<String>,
    #[arg(long, value_enum, default_value_t = Strategy::Rolling)] pub strategy: Strategy,
    #[arg(long, default_value_t = 1)] pub max_parallel: u16,
    /// Free-form selector expression, same grammar as lifecycle commands
    /// (e.g. `role=validator AND shard=0`).
    #[arg(long, group = "upgrade_selector")] pub select: Option<String>,
    /// Limit the upgrade to specific node indices. Repeatable.
    #[arg(long, group = "upgrade_selector")] pub node: Vec<u16>,
    /// Limit the upgrade to one shard (`0`, `1`, `2`, or `metachain`).
    #[arg(long, group = "upgrade_selector")] pub shard: Option<String>,
    #[arg(long)] pub skip_validators: bool,
    #[arg(long)] pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum UpgradeTarget {
    /// Upgrade only the proxy. Reads `--proxy-tag` from the subcommand
    /// (or falls back to the parent `mxnode upgrade --proxy-tag`).
    Proxy {
        #[arg(long)] proxy_tag: Option<String>,
    },
    /// Upgrade the squad: every node + the proxy if installed. The
    /// observer-shape config edits (`[DbLookupExtensions] Enabled`,
    /// shard pinning) are re-applied during the upgrade — useful when
    /// the upstream config repo changed a knob you've been overriding.
    Squad {
        #[arg(long)] binary_tag: Option<String>,
        #[arg(long)] config_tag: Option<String>,
        #[arg(long)] proxy_tag: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DbCommand {
    Prune {
        #[arg(long)] node: Vec<u16>,
        #[arg(long)] epochs: Option<u32>,
    },
    Remove {
        #[arg(long)] node: Vec<u16>,
        /// Required to confirm intent. Without it, the command refuses.
        #[arg(long)] yes: bool,
    },
    Reseed {
        #[arg(long)] node: Vec<u16>,
        #[arg(long)] yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct KeygenArgs {
    #[arg(long, value_name = "INDEX")] pub r#for: Option<u16>,
    #[arg(long)] pub output: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Apply a known fix to the host. Currently only `journald` is
    /// supported (caps `/etc/systemd/journald.conf` retention so node
    /// logs don't fill `/var/log/journal`). Without this flag, the
    /// relevant check still runs and reports its finding, but no
    /// system mutation occurs.
    #[arg(long, value_enum)]
    pub fix: Option<DoctorFix>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DoctorFix {
    Journald,
}

#[derive(Debug, Subcommand)]
pub enum KeysCommand {
    /// Verify that node-{INDEX}.zip is present for every node.
    Check,
}

#[derive(Debug, Args)]
pub struct CleanupArgs {
    /// Required to confirm intent. Without it, the command refuses.
    #[arg(long)] pub yes: bool,
    /// Keep the versioned binstore (`{custom_home}/mxnode/binaries`)
    /// and the build cache around. Useful when re-installing
    /// immediately after cleanup to avoid re-downloading + re-building.
    #[arg(long)] pub keep_binaries: bool,
    /// Keep the operator's `~/.config/mxnode/config.toml` so a
    /// subsequent `mxnode install` does not have to re-prompt /
    /// re-auto-init. Default cleanup removes it along with the rest
    /// of the mxnode footprint.
    #[arg(long)] pub keep_config: bool,
    /// Actually perform the cleanup. Cleanup is dry-run by default —
    /// pass `--execute` to opt in to real deletion. Cannot be combined
    /// with `--dry-run`.
    #[arg(long, conflicts_with = "dry_run")] pub execute: bool,
    /// Force dry-run even if `--execute` is also set in some upstream
    /// wrapper. Default behaviour anyway; the flag exists for clarity.
    #[arg(long)] pub dry_run: bool,
}

impl CleanupArgs {
    /// Returns true when cleanup should actually mutate the host.
    /// The orchestrator will call this when Phase 1 wires `cleanup`; the
    /// `dead_code` allow keeps the warning quiet until then.
    #[allow(dead_code)]
    pub fn should_execute(&self) -> bool {
        self.execute && !self.dry_run
    }
}

#[derive(Debug, Args)]
pub struct DashboardArgs {
    /// Limit the dashboard to a subset of nodes (default: every node in
    /// state.toml). Repeat for multiple, e.g. `--node 0 --node 2`.
    #[arg(long)] pub node: Vec<u16>,
    /// REST poll cadence per node, in milliseconds.
    #[arg(long, default_value_t = 1000)] pub interval: u64,
    /// Override the host the dashboard talks to. Default: 127.0.0.1.
    #[arg(long, default_value = "127.0.0.1")] pub host: String,
    /// Stream logs over the node's `/log` WebSocket. WS gives structured
    /// level-tagged lines straight from the logger. Default source is
    /// `journalctl --unit <unit> --follow` on Linux (matching `mxnode
    /// logs`) and a tail of `<workdir>/logs/*.log` on macOS.
    #[arg(long)] pub ws_logs: bool,
}

#[derive(Debug, Args)]
pub struct ReapplyConfigArgs {
    /// Restart units after the per-node edits land. Off by default — most
    /// operator overrides take effect on the next natural restart, and we
    /// don't want a config-only command surprising validators with a
    /// rolling restart.
    #[arg(long)] pub restart: bool,
    /// Limit which nodes get the new edits. Default: all known nodes.
    #[arg(long)] pub node: Vec<u16>,
}

#[derive(Debug, Args)]
pub struct RenameArgs {
    /// Node index to rename. Must exist in `state.toml`.
    #[arg(long)]
    pub node: u16,
    /// New `NodeDisplayName` value. Trimmed; rejected if empty.
    #[arg(long, value_name = "NAME")]
    pub to: String,
    /// Restart the unit after the rename. Off by default —
    /// `NodeDisplayName` only refreshes on the next natural restart, and
    /// we don't want a rename to surprise validators with a roll.
    #[arg(long)]
    pub restart: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_help_text_renders() {
        let mut cmd = Cli::command();
        // Force `Command` to do its full validation pass; this catches
        // mistakes like duplicate arg names or invalid arg-group references
        // at test time rather than first user invocation.
        cmd.debug_assert();
    }

    /// `--role multikey --squad` parses cleanly. Squad is implicit for
    /// multikey, but we accept the explicit flag so operators who type
    /// it from muscle memory don't get rejected. Documented in
    /// install-shapes.mdx as "the flag is accepted as a no-op".
    #[test]
    fn install_role_multikey_with_explicit_squad_parses() {
        let cli = Cli::try_parse_from(["mxnode", "install", "--role", "multikey", "--squad"])
            .expect("--role multikey --squad must parse");
        match cli.command {
            Command::Install(args) => {
                assert!(matches!(args.role, RoleArg::Multikey));
                assert!(args.squad);
            }
            other => panic!("expected Install, got {other:?}"),
        }
    }

    /// `--backup` with no value defaults to 1; `--backup 2` parses to
    /// Some(2); omitting `--backup` leaves it None.
    #[test]
    fn install_backup_flag_defaults_to_one() {
        let no_backup = Cli::try_parse_from(["mxnode", "install", "--role", "multikey"]).unwrap();
        let bare = Cli::try_parse_from(["mxnode", "install", "--role", "multikey", "--backup"]).unwrap();
        let level2 = Cli::try_parse_from(["mxnode", "install", "--role", "multikey", "--backup", "2"]).unwrap();
        for (cli, expected) in [(no_backup, None), (bare, Some(1)), (level2, Some(2))] {
            match cli.command {
                Command::Install(args) => assert_eq!(args.backup, expected),
                other => panic!("expected Install, got {other:?}"),
            }
        }
    }

    /// Regression: lifecycle selectors are mutually exclusive.
    /// Combining `--all` with `--validators-only` or `--shard` must fail
    /// parsing. Without the `ArgGroup`, the previous CLI silently accepted
    /// these and the command had to validate at runtime.
    #[test]
    fn lifecycle_all_and_validators_only_conflict() {
        let result = Cli::try_parse_from(["mxnode", "start", "--all", "--validators-only"]);
        assert!(result.is_err(), "expected mutually-exclusive parse error");
    }

    #[test]
    fn lifecycle_select_and_shard_conflict() {
        let result = Cli::try_parse_from([
            "mxnode",
            "stop",
            "--select",
            "role=validator",
            "--shard",
            "0",
        ]);
        assert!(result.is_err(), "expected mutually-exclusive parse error");
    }

    #[test]
    fn lifecycle_with_a_single_selector_parses() {
        let cli = Cli::try_parse_from(["mxnode", "start", "--validators-only"])
            .expect("single selector must parse");
        match cli.command {
            Command::Start(args) => {
                assert!(args.validators_only);
                assert!(!args.observers_only);
                assert!(!args.all);
            }
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[test]
    fn logs_follow_and_save_archive_conflict() {
        let result = Cli::try_parse_from([
            "mxnode",
            "logs",
            "--follow",
            "--save-archive",
        ]);
        assert!(result.is_err(), "follow + save-archive must conflict");
    }

    #[test]
    fn cleanup_dry_run_default_is_safe() {
        let cli = Cli::try_parse_from(["mxnode", "cleanup", "--yes"]).unwrap();
        match cli.command {
            Command::Cleanup(args) => {
                assert!(args.yes);
                assert!(!args.execute);
                assert!(
                    !args.should_execute(),
                    "cleanup must default to dry-run; only --execute opts in",
                );
            }
            other => panic!("expected Cleanup, got {other:?}"),
        }
    }

    #[test]
    fn cleanup_with_execute_actually_executes() {
        let cli = Cli::try_parse_from(["mxnode", "cleanup", "--yes", "--execute"]).unwrap();
        match cli.command {
            Command::Cleanup(args) => {
                assert!(args.execute);
                assert!(args.should_execute());
            }
            other => panic!("expected Cleanup, got {other:?}"),
        }
    }

    #[test]
    fn cleanup_execute_and_dry_run_conflict() {
        let result = Cli::try_parse_from([
            "mxnode",
            "cleanup",
            "--yes",
            "--execute",
            "--dry-run",
        ]);
        assert!(result.is_err(), "--execute and --dry-run must be mutually exclusive");
    }

    #[test]
    fn no_color_flag_is_globally_available() {
        let cli = Cli::try_parse_from(["mxnode", "--no-color", "version"]).unwrap();
        assert!(cli.global.no_color);
    }

    #[test]
    fn json_flag_is_globally_available() {
        let cli = Cli::try_parse_from(["mxnode", "--json", "version"]).unwrap();
        assert!(cli.global.json);
    }

    #[test]
    fn status_watch_interval_defaults_to_five_seconds() {
        let cli = Cli::try_parse_from(["mxnode", "status", "--watch"]).unwrap();
        match cli.command {
            Command::Status(args) => {
                assert!(args.watch);
                assert_eq!(args.interval, 5);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn db_remove_requires_yes_flag_to_express_intent() {
        // We don't enforce `--yes` at parse time (it's a runtime check), but
        // we do verify the flag exists and parses through.
        let cli = Cli::try_parse_from(["mxnode", "db", "remove", "--yes", "--node", "0"]).unwrap();
        match cli.command {
            Command::Db { command: DbCommand::Remove { yes, node } } => {
                assert!(yes);
                assert_eq!(node, vec![0]);
            }
            other => panic!("expected Db Remove, got {other:?}"),
        }
    }
}
