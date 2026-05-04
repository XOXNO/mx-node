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
mod seednode;
mod self_update;
mod status;
mod upgrade;
mod version;

use crate::cli::{Cli, Command};
use crate::errors::CliError;

pub fn dispatch(cli: Cli) -> Result<(), CliError> {
    let json = cli.global.json;
    match cli.command {
        Command::Version => version::run(json),
        Command::Config { command } => config::run(command, &cli.global),
        Command::Status(args) => status::run(args, &cli.global),
        Command::Logs(args) => logs::run(args, &cli.global),
        Command::Doctor(args) => doctor::run(args, &cli.global),
        Command::Completions(args) => completions::run(args, &cli.global),
        Command::Start(args) => lifecycle::run_start(args, &cli.global),
        Command::Stop(args) => lifecycle::run_stop(args, &cli.global),
        Command::Restart(args) => lifecycle::run_restart(args, &cli.global),
        Command::Db { command } => db::run(command, &cli.global),
        Command::Keys { command } => keys::run_keys(command, &cli.global),
        Command::Keygen(args) => keys::run_keygen(args, &cli.global),
        Command::Seednode(args) => seednode::run(args, &cli.global),
        Command::Benchmark => benchmark::run(&cli.global),
        Command::Cleanup(args) => cleanup::run(args, &cli.global),
        Command::MigrateBash(args) => migrate::run(args, &cli.global),
        Command::ReapplyConfig(args) => reapply_config::run(args, &cli.global),
        Command::Rename(args) => rename::run(args, &cli.global),
        Command::SelfUpdate(args) => self_update::run(args, &cli.global),
        Command::Dashboard(args) => dashboard::run(args, &cli.global),
        Command::Metrics(args) => metrics::run(args, &cli.global),
        Command::Upgrade(args) => upgrade::run(args, &cli.global),
        Command::Install(args) => install::run(args, &cli.global),
        Command::AddNodes(args) => add_nodes::run(args, &cli.global),
        #[cfg(feature = "bench-harness")]
        Command::BenchRender(args) => bench_render::run(args),
    }
}
