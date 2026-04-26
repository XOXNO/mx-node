//! Command dispatch. Every command returns `Result<(), CliError>`; commands
//! that aren't yet implemented return a structured error so the surface is
//! honest about what works and what doesn't.

mod add_nodes;
mod benchmark;
mod cleanup;
mod config;
mod db;
mod doctor;
mod init;
mod install;
mod keys;
mod lifecycle;
mod logs;
mod metrics;
mod migrate;
mod multikey;
mod observers;
mod dashboard;
mod placeholder;
mod reapply_config;
mod rebuild_state;
mod rollback;
mod status;
mod unlock;
mod upgrade;
mod version;

use crate::cli::{Cli, Command};
use crate::errors::CliError;

pub fn dispatch(cli: Cli) -> Result<(), CliError> {
    let json = cli.global.json;

    match cli.command {
        Command::Version => version::run(json),
        Command::Config { command } => config::run(command, &cli.global),
        Command::Init(args) => init::run(args, &cli.global),
        Command::RebuildState => rebuild_state::run(&cli.global),
        Command::Unlock { force } => unlock::run(force, &cli.global),
        Command::Status(args) => status::run(args, &cli.global),
        Command::Logs(args) => logs::run(args, &cli.global),
        Command::Doctor => doctor::run(&cli.global),
        Command::Start(args) => lifecycle::run_start(args, &cli.global),
        Command::Stop(args) => lifecycle::run_stop(args, &cli.global),
        Command::Restart(args) => lifecycle::run_restart(args, &cli.global),
        Command::Db { command } => db::run(command, &cli.global),
        Command::Keys { command } => keys::run_keys(command, &cli.global),
        Command::Keygen(args) => keys::run_keygen(args, &cli.global),
        Command::Benchmark => benchmark::run(&cli.global),
        Command::Cleanup(args) => cleanup::run(args, &cli.global),
        Command::ReapplyConfig(args) => reapply_config::run(args, &cli.global),
        Command::Dashboard(args) => dashboard::run(args, &cli.global),
        Command::Metrics(args) => metrics::run(args, &cli.global),
        Command::Rollback(args) => rollback::run(args, &cli.global),
        Command::Upgrade(args) => upgrade::run(args, &cli.global),
        Command::Install(args) => install::run(args, &cli.global),
        Command::AddNodes(args) => add_nodes::run(args, &cli.global),
        Command::Observers { count } => observers::run(count, &cli.global),
        Command::Multikey { count } => multikey::run(count, &cli.global),
        Command::Migrate(args) => migrate::run(args, &cli.global),

        // Everything else is still routed through the placeholder until its
        // phase lands. Error message names the phase so operators know what
        // to expect.
        cmd => placeholder::not_implemented(cmd, json),
    }
}
