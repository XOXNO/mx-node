//! Command dispatch. Every command returns `Result<(), CliError>`.

mod add_nodes;
mod benchmark;
#[cfg(feature = "bench-harness")]
mod bench_render;
mod cleanup;
mod completions;
mod config;
mod dashboard;
mod db;
mod doctor;
pub(crate) mod init;
mod install;
mod keys;
mod lifecycle;
mod logs;
mod metrics;
pub(crate) mod migrate;
mod prompts;
mod reapply_config;
mod rename;
mod self_update;
mod status;
mod upgrade;
mod version;

use crate::cli::{Cli, Command, ConfigCommand, KeysCommand};
use crate::errors::CliError;

pub fn dispatch(cli: Cli) -> Result<(), CliError> {
    let json = cli.global.json;
    match cli.command {
        // ── Lifecycle (most common) ──
        Command::Install(args) => {
            // `--add N` flips install into extend-existing mode (was
            // the top-level `mxnode add-nodes` command). All other
            // selectors flow through the normal install path.
            if let Some(count) = args.add {
                add_nodes::run(args_to_add_nodes(args, count), &cli.global)
            } else {
                install::run(args, &cli.global)
            }
        }
        Command::Upgrade(args) => upgrade::run(args, &cli.global),
        Command::Uninstall(args) => cleanup::run(args, &cli.global),
        Command::Start(args) => lifecycle::run_start(args, &cli.global),
        Command::Stop(args) => lifecycle::run_stop(args, &cli.global),
        Command::Restart(args) => lifecycle::run_restart(args, &cli.global),

        // ── Observability ──
        Command::Status(args) => {
            // `--watch` on a TTY launches the live multi-node dashboard
            // (was the top-level `mxnode dashboard` command). On a
            // non-TTY `--watch` falls through to the table renderer
            // with a periodic refresh, same as before.
            if args.watch && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                dashboard::run(status_to_dashboard(&args), &cli.global)
            } else {
                status::run(args, &cli.global)
            }
        }
        Command::Logs(args) => logs::run(args, &cli.global),
        Command::Metrics(args) => metrics::run(args, &cli.global),

        // ── Configuration & data ──
        Command::Config { command } => match command {
            ConfigCommand::Apply(args) => reapply_config::run(args, &cli.global),
            other => config::run(other, &cli.global),
        },
        Command::Keys { command } => match command {
            KeysCommand::Generate(args) => keys::run_keygen(args, &cli.global),
            KeysCommand::Rename(args) => rename::run(args, &cli.global),
            KeysCommand::Check => keys::run_keys(KeysCommand::Check, &cli.global),
        },
        Command::Db { command } => db::run(command, &cli.global),

        // ── Operator tooling ──
        Command::Doctor(args) => {
            // `--benchmark` runs the doctor probes AND the bundled
            // host-assessment benchmark (was the top-level
            // `mxnode benchmark` command). Either succeeds or fails
            // independently; doctor's exit code reflects its findings.
            let run_bench = args.benchmark;
            doctor::run(args, &cli.global)?;
            if run_bench {
                benchmark::run(&cli.global)?;
            }
            Ok(())
        }
        Command::ImportBash(args) => migrate::run(args, &cli.global),

        // ── Built-ins ──
        Command::SelfUpdate(args) => self_update::run(args, &cli.global),
        Command::Completions(args) => completions::run(args, &cli.global),
        Command::Version => version::run(json),

        #[cfg(feature = "bench-harness")]
        Command::BenchRender(args) => bench_render::run(args),
    }
}

/// Translate `mxnode install --add N` args into the `AddNodesArgs`
/// shape `add_nodes::run` expects. Most fields are dropped — extending
/// an existing install only honours count + name template + operation
/// mode + non-interactive, mirroring the historical `add-nodes` CLI.
fn args_to_add_nodes(args: crate::cli::InstallArgs, count: u16) -> crate::cli::AddNodesArgs {
    crate::cli::AddNodesArgs {
        count,
        role: Some(args.role),
        name_template: args.name_template,
        operation_mode: args.operation_mode,
        non_interactive: args.non_interactive,
    }
}

/// Translate `mxnode status --watch` args into the `DashboardArgs`
/// shape `dashboard::run` expects. Status's `--interval` is in seconds
/// (operator-facing), dashboard's is in milliseconds (poll cadence),
/// so we convert. `--node` filtering isn't on `status` yet; default
/// to "every node".
fn status_to_dashboard(args: &crate::cli::StatusArgs) -> crate::cli::DashboardArgs {
    crate::cli::DashboardArgs {
        node: Vec::new(),
        interval: args.interval.saturating_mul(1000).max(100),
        host: "127.0.0.1".to_string(),
        ws_logs: false,
    }
}
