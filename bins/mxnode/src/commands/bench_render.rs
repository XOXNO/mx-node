//! Hidden bench-render subcommand. Shells out to
//! `mxnode_tui::bench::render_n_frames` and prints `elapsed_ms=<n>`
//! to stderr. The xtask harness parses this line.

use crate::cli::BenchRenderArgs;
use crate::errors::CliError;

pub fn run(args: BenchRenderArgs) -> Result<(), CliError> {
    let elapsed = mxnode_tui::bench::render_n_frames(&args.fixture, args.frames).map_err(|e| {
        CliError::new(
            "bench-render failed",
            e.to_string(),
            format!(
                "verify --fixture path exists and is a valid JSON map: {}",
                args.fixture.display()
            ),
        )
    })?;
    eprintln!("elapsed_ms={}", elapsed.as_millis());
    Ok(())
}
