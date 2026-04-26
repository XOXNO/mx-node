//! mxnode CLI entry point.
//!
//! Phase 0 wires the full subcommand tree but only `version`, `config show`,
//! `config get`, and `config validate` actually do work. Every other
//! subcommand prints a "not yet implemented (Phase X)" message via the
//! `unimplemented` helper so the surface area is visible.

mod cli;
mod commands;
mod errors;
mod events;
mod orchestrator;

/// Default proxy listen port the bash uses (8079). Matches
/// `mxnode_core::DEFAULT_PROXY_PORT` but referenced here so the orchestrator
/// can keep core's constants out of its public surface.
pub const DEFAULT_PROXY_PORT_FALLBACK: u16 = mxnode_core::DEFAULT_PROXY_PORT;

use clap::Parser;

use crate::cli::Cli;
use crate::errors::report_error;

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.global.verbose, cli.global.quiet);

    let exit = match commands::dispatch(cli) {
        Ok(()) => 0,
        Err(err) => {
            report_error(err);
            1
        }
    };
    std::process::exit(exit);
}

fn init_tracing(verbose: bool, quiet: bool) {
    // Interactive default is `warn` — the structured `op.start` /
    // `op.end` audit events live at INFO and would otherwise spam
    // every state-changing command with timestamped lines that read
    // like debug output. `--verbose` reinstates INFO so the audit
    // trail is still available when the operator wants it. WARN-level
    // op.end (failures) and any genuine warning surface either way.
    let filter = if quiet {
        "error"
    } else if verbose {
        "info"
    } else {
        "warn"
    };
    // Explicit env-var precedence: a non-empty `RUST_LOG` wins; an
    // unset OR empty value falls back to the CLI-derived filter.
    // `EnvFilter::try_from_default_env()` accepts empty as "default
    // INFO", which leaks our structured op events to the terminal.
    let env_filter = match std::env::var("RUST_LOG") {
        Ok(s) if !s.trim().is_empty() => tracing_subscriber::EnvFilter::new(s),
        _ => tracing_subscriber::EnvFilter::new(filter),
    };
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .init();
}
