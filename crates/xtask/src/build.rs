//! Wrapper around `cargo zigbuild --release --target ... --bin mxnode`.
//! The harness runs each combo in an isolated `target/bench-size/<id>`
//! directory so simultaneous combos don't fight over the cache.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};

use crate::matrix::{Combo, Toolchain};

pub struct BuildArtefact {
    pub binary_path: PathBuf,
    pub build_secs: u64,
}

pub fn build(
    workspace_root: &Path,
    target: &str,
    combo: &Combo,
    target_dir: &Path,
    extra_features: &[&str],
) -> Result<BuildArtefact> {
    std::fs::create_dir_all(target_dir)
        .with_context(|| format!("create {}", target_dir.display()))?;

    let started = Instant::now();
    let mut cmd = Command::new("cargo");
    cmd.current_dir(workspace_root);

    // Toolchain branch.
    //
    // Stable: `cargo zigbuild` works for cross-compile to musl/macOS via
    //         zig as the C linker. This is the path matching release.yml.
    //
    // Nightly + build-std + immediate-abort: rebuilds std with the
    //         workspace's profile flags AND swaps the panic strategy to
    //         `immediate-abort` so std doesn't carry any panic-unwind
    //         tables. Combined effect on the dashboard binary: ~10%
    //         additional shrink on top of stable Stage C with no perf
    //         regression. Host-only because cargo-zigbuild is installed
    //         under the stable toolchain on most setups, and nightly
    //         cross-compile would need its own CI runner.
    //
    // Note: `panic_immediate_abort` was a `-Zbuild-std-features` value
    // in older nightlies. As of nightly-2026-04+, it became a real panic
    // strategy: pass `-Cpanic=immediate-abort` (gated by
    // `-Zunstable-options`) via RUSTFLAGS. The harness uses the new
    // syntax — old syntax now hard-errors at the compile_error! in
    // `library/core/src/panicking.rs`.
    let mut rustflags = String::from("-C strip=symbols");
    let toolchain_label = if combo.toolchain == Toolchain::NightlyBuildStd {
        cmd.arg("+nightly");
        cmd.arg("build");
        cmd.args(["-Z", "build-std=std,panic_abort", "-Z", "unstable-options"]);
        rustflags.push_str(" -Cpanic=immediate-abort -Zunstable-options");
        "cargo +nightly build"
    } else {
        cmd.arg("zigbuild");
        "cargo zigbuild"
    };

    cmd.args([
        "--release",
        "--target",
        target,
        "--bin",
        "mxnode",
        "--target-dir",
    ])
    .arg(target_dir);

    if !extra_features.is_empty() {
        cmd.arg("--features").arg(extra_features.join(","));
    }

    // strip=symbols mirrors the env in release.yml. opt-level is set via
    // the patched Cargo.toml, NOT via -C, to keep responsibility in one
    // place (the patcher).
    cmd.env("RUSTFLAGS", rustflags);

    let status = cmd
        .status()
        .with_context(|| format!("spawn {toolchain_label} for {target}"))?;
    if !status.success() {
        return Err(anyhow!(
            "{toolchain_label} failed for target={target} ({status})"
        ));
    }

    let binary_path = target_dir.join(target).join("release").join("mxnode");
    if !binary_path.exists() {
        return Err(anyhow!(
            "build claimed success but {} is missing",
            binary_path.display()
        ));
    }

    Ok(BuildArtefact {
        binary_path,
        build_secs: started.elapsed().as_secs(),
    })
}
