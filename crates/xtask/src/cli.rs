//! clap surface for `cargo xtask`. Subcommands are added as the
//! corresponding modules land — see plan tasks for sequencing.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", version, about = "mxnode internal automation")]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the binary-size benchmark matrix.
    BenchSize(BenchSizeOpts),
}

#[derive(Debug, clap::Args)]
pub struct BenchSizeOpts {
    /// Restrict to a single target triple; default is all four release targets.
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,

    /// Run only the baseline combo (Phase 0). Useful for first-time setup.
    #[arg(long)]
    pub baseline_only: bool,

    /// Run a fixed shortlist of combos rather than the full matrix.
    /// Used by CI to confirm local picks on the self-hosted Linux runner.
    #[arg(long)]
    pub shortlist: bool,

    /// Where to write `results.csv` and `REPORT.md`.
    #[arg(long, default_value = "dist/bench-size", value_name = "DIR")]
    pub out_dir: PathBuf,
}
