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
    propagate_version = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Override the config file path. Default: `~/.config/mxnode/mxnode.toml`,
    /// then `/etc/mxnode/mxnode.toml`.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Bypass `config validate`, system-requirements gates, and on-disk
    /// state checks. Use with care.
    #[arg(long, global = true)]
    pub force: bool,

    /// Emit machine-readable JSON. Stable schema across releases.
    /// Takes precedence over per-command `--format`.
    #[arg(long, global = true)]
    pub json: bool,

    /// Verbose output (info-level structured events). Honoured by every
    /// command. Mutually exclusive with `--quiet`.
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Suppress informational chatter. Errors still surface.
    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Skip the once-per-day "is a newer mxnode out?" check. Useful in
    /// CI, scripts, or for offline determinism. Equivalent to
    /// `MXNODE_NO_UPDATE_CHECK=1`.
    #[arg(long, global = true, env = "MXNODE_NO_UPDATE_CHECK")]
    pub no_update_check: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install nodes from scratch. `--add N` extends an existing install.
    Install(InstallArgs),

    /// Upgrade (or downgrade) nodes to a different tag.
    Upgrade(UpgradeArgs),

    /// Remove nodes, units, and binaries. Inverse of `install`.
    Uninstall(UninstallArgs),

    /// Start units.
    Start(LifecycleArgs),

    /// Stop units.
    Stop(LifecycleArgs),

    /// Restart units.
    Restart(RestartArgs),

    /// Health snapshot. `--watch` launches the live multi-node TUI.
    Status(StatusArgs),

    /// Tail or archive node logs.
    Logs(LogsArgs),

    /// Serve a Prometheus metrics endpoint.
    Metrics(MetricsArgs),

    /// Configuration commands.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Key management (check, generate, rename).
    Keys {
        #[command(subcommand)]
        command: KeysCommand,
    },

    /// Database commands.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },

    /// Full host diagnostic. `--benchmark` runs the bundled assessment too.
    Doctor(DoctorArgs),

    /// Import an existing bash install into `mxnode.toml`.
    ImportBash(crate::commands::import_bash::ImportBashArgs),

    /// Update the mxnode binary.
    SelfUpdate(SelfUpdateArgs),

    /// Generate shell completion scripts.
    Completions(CompletionsArgs),

    /// Print version (also available via --version).
    Version,

    /// Hidden: render N frames of the dashboard against an in-memory
    /// TestBackend and print `elapsed_ms=<n>` to stderr. Used by
    /// `cargo xtask bench-size`.
    #[cfg(feature = "bench-harness")]
    #[command(hide = true)]
    BenchRender(BenchRenderArgs),
}

#[cfg(feature = "bench-harness")]
#[derive(Debug, Args)]
pub struct BenchRenderArgs {
    /// How many frames to render.
    #[arg(long, default_value_t = 1000)]
    pub frames: u32,

    /// Path to the snapshot fixture JSON.
    #[arg(long, value_name = "PATH")]
    pub fixture: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the merged config.
    Show {
        #[arg(long)]
        origin: bool,
        #[arg(long, value_enum, default_value_t = Format::Toml)]
        format: Format,
    },
    /// Print one dotted-path value.
    Get { path: String },
    /// Set one dotted-path value, writing back to the chosen scope.
    Set {
        path: String,
        value: String,
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
    },
    /// Open the chosen scope's file in $EDITOR.
    Edit {
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
    },
    /// Run pre-flight validation.
    Validate {
        /// Also check network reachability (token, repos).
        #[arg(long)]
        strict: bool,
    },
    /// Re-apply per-node config overrides without touching binaries
    /// or restarting units.
    Apply(ConfigApplyArgs),
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
    /// Extend an existing install by N more nodes instead of doing a
    /// fresh install. Bare `--add` means 1; pass `--add 3` for three.
    #[arg(
        long,
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "1",
    )]
    pub add: Option<u16>,
    /// Number of nodes to install. Ignored when `--squad` is set, and
    /// rejected for `--role multikey` (multikey is always a 4-shard
    /// squad by design). Defaults to 1.
    #[arg(long, conflicts_with = "squad")]
    pub count: Option<u16>,
    #[arg(long)]
    pub config_tag: Option<String>,
    #[arg(long)]
    pub binary_tag: Option<String>,
    #[arg(long)]
    pub proxy_tag: Option<String>,
    #[arg(long)]
    pub name_template: Option<String>,
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
    #[arg(long)]
    pub squad: bool,
    /// Pass a first-class mx-chain-go operation mode into every node's
    /// supervisor command line. Useful for archive, DB lookup,
    /// historical-balance, or snapshotless observer nodes.
    #[arg(long, value_enum)]
    pub operation_mode: Option<OperationModeArg>,
    /// Also install the MultiversX proxy alongside the nodes. Off by
    /// default — many operators host the proxy on a separate box and
    /// point their squads at it over the network.
    #[arg(long)]
    pub with_proxy: bool,
    /// Path to the operator's `allValidatorsKeys.pem` (the bundle of
    /// every BLS key the multikey nodes will sign for). Required for
    /// `--role multikey`; rejected for any other role.
    ///
    /// If omitted on a multikey install, mxnode looks for
    /// `<node_keys>/allValidatorsKeys.pem` (the same directory that
    /// holds validator zips by convention) and uses it if present.
    #[arg(long, value_name = "PATH")]
    pub keys_file: Option<PathBuf>,
    /// Mark this host as a backup of an existing primary. Pass
    /// `--backup` for level 1 (the common case), `--backup 2` for a
    /// backup-of-backup, etc. Omit entirely for a primary install
    /// (`RedundancyLevel = 0`).
    ///
    /// Allowed for `--role validator` and `--role multikey`. For
    /// multikey, both machines must share the **same**
    /// `allValidatorsKeys.pem`; only the redundancy level differs.
    /// Rejected for `--role observer` (observers don't sign blocks).
    #[arg(
        long,
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "1",
    )]
    pub backup: Option<u8>,
    #[arg(long)]
    pub dry_run: bool,
    /// Skip per-node `NodeDisplayName` prompts — every node gets its
    /// template-expanded default. Set automatically when stdin is not a
    /// TTY (CI, piped input, `< /dev/null`); pass explicitly to suppress
    /// prompts in an interactive terminal too.
    #[arg(long)]
    pub non_interactive: bool,
}

#[derive(Debug, Args)]
pub struct InstallAddArgs {
    #[arg(long, default_value_t = 1)]
    pub count: u16,
    #[arg(long, value_enum)]
    pub role: Option<RoleArg>,
    /// Override the `node.name_template` config value for the new nodes
    /// only. Existing nodes keep their persisted `display_name`. Useful
    /// when the second wave should be named differently from the first
    /// (e.g. `--name-template "extra-{index}"` while the original install
    /// used `mx-chain-mainnet-validator-{index}`).
    #[arg(long)]
    pub name_template: Option<String>,
    /// Pass a first-class mx-chain-go operation mode into the new
    /// nodes' supervisor command lines.
    #[arg(long, value_enum)]
    pub operation_mode: Option<OperationModeArg>,
    /// Skip per-node `NodeDisplayName` prompts — every new node gets its
    /// template-expanded default. Set automatically when stdin is not a
    /// TTY (CI, piped input, `< /dev/null`); pass explicitly to suppress
    /// prompts in an interactive terminal too.
    #[arg(long)]
    pub non_interactive: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RoleArg {
    Validator,
    Observer,
    Multikey,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OperationModeArg {
    FullArchive,
    DbLookupExtension,
    HistoricalBalances,
    SnapshotlessObserver,
}

impl OperationModeArg {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FullArchive => "full-archive",
            Self::DbLookupExtension => "db-lookup-extension",
            Self::HistoricalBalances => "historical-balances",
            Self::SnapshotlessObserver => "snapshotless-observer",
        }
    }
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
    #[arg(long, group = "lifecycle_selector")]
    pub all: bool,
    #[arg(long, group = "lifecycle_selector")]
    pub select: Option<String>,
    #[arg(long, group = "lifecycle_selector")]
    pub validators_only: bool,
    #[arg(long, group = "lifecycle_selector")]
    pub observers_only: bool,
    #[arg(long, group = "lifecycle_selector")]
    pub shard: Option<String>,
    #[arg(long, group = "lifecycle_selector")]
    pub node: Vec<u16>,
}

#[derive(Debug, Args)]
pub struct RestartArgs {
    #[command(flatten)]
    pub select: LifecycleArgs,
    #[arg(long, value_enum, default_value_t = Strategy::Rolling)]
    pub strategy: Strategy,
    #[arg(long, default_value_t = 1)]
    pub max_parallel: u16,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Strategy {
    Rolling,
    Parallel,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub watch: bool,
    /// Refresh interval in seconds when `--watch` is set. Ignored otherwise.
    #[arg(long, default_value_t = 5, value_name = "SECS")]
    pub interval: u64,
    #[arg(long, value_enum, default_value_t = StatusFormat::Table)]
    pub format: StatusFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StatusFormat {
    Table,
    Json,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    #[arg(long)]
    pub node: Vec<u16>,
    #[arg(long)]
    pub since: Option<String>,
    /// Tail logs as they arrive. Cannot be combined with `--save-archive`.
    #[arg(long, short = 'f', conflicts_with = "save_archive")]
    pub follow: bool,
    /// Replicates `get_logs` — produces a tar.gz under $CUSTOM_HOME/mx-chain-logs.
    /// Cannot be combined with `--follow`.
    #[arg(long)]
    pub save_archive: bool,
    /// Stream over the node's `/log` WebSocket, matching the upstream
    /// logviewer protocol. Use `--log-level` to request a custom runtime
    /// logger profile.
    #[arg(long, conflicts_with = "save_archive")]
    pub ws: bool,
    /// Host used for `--ws` connections. Ports still come from mxnode.toml.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Runtime logger pattern sent to the node's `/log` WebSocket.
    #[arg(long, value_name = "PATTERN")]
    pub log_level: Option<String>,
    /// Save WebSocket log output to `$CUSTOM_HOME/mx-chain-logs/`.
    #[arg(long)]
    pub log_save: bool,
    /// Request logger correlation fields over `/log`.
    #[arg(long)]
    pub log_correlation: bool,
    /// Request logger names over `/log`.
    #[arg(long)]
    pub log_logger_name: bool,
    /// Use `wss://` instead of `ws://` for `/log`.
    #[arg(long)]
    pub use_wss: bool,
}

#[derive(Debug, Args)]
pub struct MetricsArgs {
    #[arg(long, default_value_t = 9090)]
    pub port: u16,
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

    #[arg(long)]
    pub config_tag: Option<String>,
    #[arg(long)]
    pub binary_tag: Option<String>,
    #[arg(long)]
    pub proxy_tag: Option<String>,
    #[arg(long, value_enum, default_value_t = Strategy::Rolling)]
    pub strategy: Strategy,
    #[arg(long, default_value_t = 1)]
    pub max_parallel: u16,
    /// Free-form selector expression, same grammar as lifecycle commands
    /// (e.g. `role=validator AND shard=0`).
    #[arg(long, group = "upgrade_selector")]
    pub select: Option<String>,
    /// Limit the upgrade to specific node indices. Repeatable.
    #[arg(long, group = "upgrade_selector")]
    pub node: Vec<u16>,
    /// Limit the upgrade to one shard (`0`, `1`, `2`, or `metachain`).
    #[arg(long, group = "upgrade_selector")]
    pub shard: Option<String>,
    #[arg(long)]
    pub skip_validators: bool,
    /// Start each node after its binary + config is swapped. Off by
    /// default — `mxnode upgrade` mirrors the bash flow (stop, swap,
    /// leave stopped) so the operator can verify the new config before
    /// resuming consensus. Run `mxnode start --all` when ready, or pass
    /// `--start` to opt back into the rolling restart + readiness probe.
    #[arg(long)]
    pub start: bool,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum UpgradeTarget {
    /// Upgrade only the proxy. Reads `--proxy-tag` from the subcommand
    /// (or falls back to the parent `mxnode upgrade --proxy-tag`).
    Proxy {
        #[arg(long)]
        proxy_tag: Option<String>,
    },
    /// Upgrade the squad: every node + the proxy if installed. The
    /// observer-shape config edits (`[DbLookupExtensions] Enabled`,
    /// shard pinning) are re-applied during the upgrade — useful when
    /// the upstream config repo changed a knob you've been overriding.
    Squad {
        #[arg(long)]
        binary_tag: Option<String>,
        #[arg(long)]
        config_tag: Option<String>,
        #[arg(long)]
        proxy_tag: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DbCommand {
    Prune {
        #[arg(long)]
        node: Vec<u16>,
        #[arg(long)]
        epochs: Option<u32>,
    },
    Remove {
        #[arg(long)]
        node: Vec<u16>,
        /// Required to confirm intent. Without it, the command refuses.
        #[arg(long)]
        yes: bool,
    },
    Reseed {
        #[arg(long)]
        node: Vec<u16>,
        #[arg(long)]
        yes: bool,
    },
    /// Run a stopped node in mx-chain-go import-db mode against a source
    /// directory that contains a `db/` subdirectory.
    Import {
        #[arg(long)]
        node: u16,
        #[arg(long, value_name = "DIR")]
        source: PathBuf,
        /// Skip block-header signature checks. Only use when the import
        /// DB comes from a trusted source.
        #[arg(long)]
        no_sig_check: bool,
        /// Empty the target node's db/ before importing. Requires
        /// `--yes`; without this flag the target db/ must already be
        /// empty or absent.
        #[arg(long)]
        replace: bool,
        /// Required with `--replace` to confirm destructive intent.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Validate and print a full shard import-db plan. The source root may
    /// either contain `db/` directly or immediate child directories that do.
    ImportPlan {
        #[arg(long, value_name = "DIR")]
        source_root: PathBuf,
        /// Include `--no-sig-check` in generated import commands.
        #[arg(long)]
        no_sig_check: bool,
        /// Include `--replace --yes` in generated import commands after
        /// confirming target db/ replacement.
        #[arg(long)]
        replace: bool,
        /// Require every mapped node to have config/external.toml configured
        /// with `[ElasticSearchConnector] Enabled = true` and a non-empty URL.
        #[arg(long)]
        require_elasticsearch: bool,
        /// Required with `--replace`.
        #[arg(long)]
        yes: bool,
        /// Also write the generated plan JSON to this path.
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Args)]
pub struct KeysGenerateArgs {
    #[arg(long, value_name = "INDEX")]
    pub r#for: Option<u16>,
    #[arg(long)]
    pub output: Option<PathBuf>,
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
    /// Also run the bundled host-assessment benchmark (CPU + memory
    /// + disk IO).
    #[arg(long)]
    pub benchmark: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DoctorFix {
    Journald,
}

#[derive(Debug, Subcommand)]
pub enum KeysCommand {
    /// Verify that node-{INDEX}.zip is present for every node.
    Check,
    /// Run keygenerator (wrapper around mx-chain-go's keygen).
    Generate(KeysGenerateArgs),
    /// Rename a node's `NodeDisplayName`.
    Rename(KeysRenameArgs),
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Required to confirm intent. Without it, the command refuses.
    #[arg(long)]
    pub yes: bool,
    /// Keep the versioned binstore (`{custom_home}/mxnode/binaries`)
    /// and the build cache around. Useful when re-installing
    /// immediately after cleanup to avoid re-downloading + re-building.
    #[arg(long)]
    pub keep_binaries: bool,
    /// Keep the operator's `~/.config/mxnode/mxnode.toml` so a
    /// subsequent `mxnode install` does not have to re-prompt /
    /// re-auto-init. Default cleanup removes it along with the rest
    /// of the mxnode footprint.
    #[arg(long)]
    pub keep_config: bool,
    /// Actually perform the cleanup. Cleanup is dry-run by default —
    /// pass `--execute` to opt in to real deletion. Cannot be combined
    /// with `--dry-run`.
    #[arg(long, conflicts_with = "dry_run")]
    pub execute: bool,
    /// Force dry-run even if `--execute` is also set in some upstream
    /// wrapper. Default behaviour anyway; the flag exists for clarity.
    #[arg(long)]
    pub dry_run: bool,
}

impl UninstallArgs {
    /// Returns true when cleanup should actually mutate the host.
    pub fn should_execute(&self) -> bool {
        self.execute && !self.dry_run
    }
}

#[derive(Debug, Args)]
pub struct DashboardArgs {
    /// Limit the dashboard to a subset of nodes (default: every node in
    /// mxnode.toml). Repeat for multiple, e.g. `--node 0 --node 2`.
    #[arg(long)]
    pub node: Vec<u16>,
    /// REST poll cadence per node, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    pub interval: u64,
    /// Override the host the dashboard talks to. Default: 127.0.0.1.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Stream logs over the node's `/log` WebSocket. WS gives structured
    /// level-tagged lines straight from the logger. Default source is
    /// `journalctl --unit <unit> --follow` on Linux (matching `mxnode
    /// logs`) and a tail of `<workdir>/logs/*.log` on macOS.
    #[arg(long)]
    pub ws_logs: bool,
}

#[derive(Debug, Args)]
pub struct ConfigApplyArgs {
    /// Restart units after the per-node edits land. Off by default — most
    /// operator overrides take effect on the next natural restart, and we
    /// don't want a config-only command surprising validators with a
    /// rolling restart.
    #[arg(long)]
    pub restart: bool,
    /// Limit which nodes get the new edits. Default: all known nodes.
    #[arg(long)]
    pub node: Vec<u16>,
}

#[derive(Debug, Args)]
pub struct SelfUpdateArgs {
    /// Pin a specific release tag (e.g. `--tag v0.8.6`). Default:
    /// resolve `latest` from GitHub.
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,
    /// Print "current X / latest Y" and exit without downloading.
    #[arg(long)]
    pub check: bool,
    /// Reinstall even when the running binary is already at the
    /// requested version.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct KeysRenameArgs {
    /// Node index to rename. Must exist in `mxnode.toml`.
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
        let cmd = Cli::command();
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
        let bare =
            Cli::try_parse_from(["mxnode", "install", "--role", "multikey", "--backup"]).unwrap();
        let level2 =
            Cli::try_parse_from(["mxnode", "install", "--role", "multikey", "--backup", "2"])
                .unwrap();
        for (cli, expected) in [(no_backup, None), (bare, Some(1)), (level2, Some(2))] {
            match cli.command {
                Command::Install(args) => assert_eq!(args.backup, expected),
                other => panic!("expected Install, got {other:?}"),
            }
        }
    }

    #[test]
    fn install_operation_mode_parses_kebab_values() {
        let cli = Cli::try_parse_from([
            "mxnode",
            "install",
            "--role",
            "observer",
            "--operation-mode",
            "snapshotless-observer",
        ])
        .unwrap();
        match cli.command {
            Command::Install(args) => assert!(matches!(
                args.operation_mode,
                Some(OperationModeArg::SnapshotlessObserver)
            )),
            other => panic!("expected Install, got {other:?}"),
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
        let result = Cli::try_parse_from(["mxnode", "logs", "--follow", "--save-archive"]);
        assert!(result.is_err(), "follow + save-archive must conflict");
    }

    #[test]
    fn logs_ws_logviewer_flags_parse() {
        let cli = Cli::try_parse_from([
            "mxnode",
            "logs",
            "--ws",
            "--node",
            "0",
            "--host",
            "localhost",
            "--log-level",
            "*:DEBUG,api:INFO",
            "--log-save",
            "--log-correlation",
            "--log-logger-name",
            "--use-wss",
        ])
        .unwrap();
        match cli.command {
            Command::Logs(args) => {
                assert!(args.ws);
                assert_eq!(args.node, vec![0]);
                assert_eq!(args.host, "localhost");
                assert_eq!(args.log_level.as_deref(), Some("*:DEBUG,api:INFO"));
                assert!(args.log_save);
                assert!(args.log_correlation);
                assert!(args.log_logger_name);
                assert!(args.use_wss);
            }
            other => panic!("expected Logs, got {other:?}"),
        }
    }

    #[test]
    fn uninstall_dry_run_default_is_safe() {
        let cli = Cli::try_parse_from(["mxnode", "uninstall", "--yes"]).unwrap();
        match cli.command {
            Command::Uninstall(args) => {
                assert!(args.yes);
                assert!(!args.execute);
                assert!(
                    !args.should_execute(),
                    "uninstall must default to dry-run; only --execute opts in",
                );
            }
            other => panic!("expected Uninstall, got {other:?}"),
        }
    }

    #[test]
    fn uninstall_with_execute_actually_executes() {
        let cli =
            Cli::try_parse_from(["mxnode", "uninstall", "--yes", "--execute"]).unwrap();
        match cli.command {
            Command::Uninstall(args) => {
                assert!(args.execute);
                assert!(args.should_execute());
            }
            other => panic!("expected Uninstall, got {other:?}"),
        }
    }

    #[test]
    fn uninstall_execute_and_dry_run_conflict() {
        let result =
            Cli::try_parse_from(["mxnode", "uninstall", "--yes", "--execute", "--dry-run"]);
        assert!(
            result.is_err(),
            "--execute and --dry-run must be mutually exclusive"
        );
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
            Command::Db {
                command: DbCommand::Remove { yes, node },
            } => {
                assert!(yes);
                assert_eq!(node, vec![0]);
            }
            other => panic!("expected Db Remove, got {other:?}"),
        }
    }

    #[test]
    fn db_import_parses_source_and_safety_flags() {
        let cli = Cli::try_parse_from([
            "mxnode",
            "db",
            "import",
            "--node",
            "2",
            "--source",
            "/tmp/import-db",
            "--no-sig-check",
            "--replace",
            "--yes",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            Command::Db {
                command:
                    DbCommand::Import {
                        node,
                        source,
                        no_sig_check,
                        replace,
                        yes,
                        dry_run,
                    },
            } => {
                assert_eq!(node, 2);
                assert_eq!(source, PathBuf::from("/tmp/import-db"));
                assert!(no_sig_check);
                assert!(replace);
                assert!(yes);
                assert!(dry_run);
            }
            other => panic!("expected Db Import, got {other:?}"),
        }
    }

    #[test]
    fn db_import_plan_parses_source_root_and_output() {
        let cli = Cli::try_parse_from([
            "mxnode",
            "db",
            "import-plan",
            "--source-root",
            "/srv/imports",
            "--no-sig-check",
            "--replace",
            "--require-elasticsearch",
            "--yes",
            "--output",
            "/tmp/plan.json",
        ])
        .unwrap();
        match cli.command {
            Command::Db {
                command:
                    DbCommand::ImportPlan {
                        source_root,
                        no_sig_check,
                        replace,
                        require_elasticsearch,
                        yes,
                        output,
                    },
            } => {
                assert_eq!(source_root, PathBuf::from("/srv/imports"));
                assert!(no_sig_check);
                assert!(replace);
                assert!(require_elasticsearch);
                assert!(yes);
                assert_eq!(output, Some(PathBuf::from("/tmp/plan.json")));
            }
            other => panic!("expected Db ImportPlan, got {other:?}"),
        }
    }

    #[test]
    fn completions_parses_shell_value() {
        let cli = Cli::try_parse_from(["mxnode", "completions", "bash"]).unwrap();
        match cli.command {
            Command::Completions(args) => assert_eq!(args.shell, clap_complete::Shell::Bash),
            other => panic!("expected Completions, got {other:?}"),
        }
    }

    #[test]
    fn install_add_extends_existing_install() {
        // Bare `--add` defaults to 1 node; `--add N` extends by N.
        let bare = Cli::try_parse_from(["mxnode", "install", "--add"]).unwrap();
        let three = Cli::try_parse_from(["mxnode", "install", "--add", "3"]).unwrap();
        for (cli, expected) in [(bare, Some(1)), (three, Some(3))] {
            match cli.command {
                Command::Install(args) => assert_eq!(args.add, expected),
                other => panic!("expected Install, got {other:?}"),
            }
        }
    }

    #[test]
    fn keys_generate_parses() {
        let cli = Cli::try_parse_from(["mxnode", "keys", "generate"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Keys {
                command: KeysCommand::Generate(_)
            }
        ));
    }

    #[test]
    fn config_apply_parses() {
        let cli = Cli::try_parse_from(["mxnode", "config", "apply"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Config {
                command: ConfigCommand::Apply(_)
            }
        ));
    }

    #[test]
    fn import_bash_parses() {
        let cli = Cli::try_parse_from(["mxnode", "import-bash"]).unwrap();
        assert!(matches!(cli.command, Command::ImportBash(_)));
    }
}
