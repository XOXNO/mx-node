//! mxnode internal automation. Currently exposes `bench-size` for the
//! release-binary size matrix harness (see
//! docs/superpowers/specs/2026-05-04-binary-size-design.md).

use anyhow::Result;
use clap::Parser;

mod cli;

fn main() -> Result<()> {
    let args = cli::Args::parse();
    match args.command {
        cli::Command::BenchSize(opts) => {
            println!("bench-size scaffold ready (combo={:?})", opts);
            Ok(())
        }
    }
}
