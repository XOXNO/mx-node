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
    // Nightly + build-std: rebuilds std with the workspace's profile
    //         flags + `panic_immediate_abort` (no panic unwind tables in
    //         std). Enables Stage E's last-mile size win. We don't use
    //         zigbuild here because cargo-zigbuild is installed under the
    //         stable toolchain only on most setups; vanilla
    //         `cargo +nightly build` is enough for host-only experiments
    //         (the spec scopes Stage E to host targets — cross-compile
    //         with nightly+build-std would need a dedicated CI runner
    //         and is deferred).
    if combo.toolchain == Toolchain::NightlyBuildStd {
        cmd.arg("+nightly");
        cmd.arg("build");
        cmd.args([
            "-Z",
            "build-std=std,panic_abort",
            "-Z",
            "build-std-features=panic_immediate_abort",
        ]);
    } else {
        cmd.arg("zigbuild");
    }

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

    // RUSTFLAGS — strip is also set in the profile but mirroring it in
    // the env matches release.yml exactly. opt-level is set via the
    // patched Cargo.toml, NOT via -C, to keep responsibility in one
    // place (the patcher).
    cmd.env("RUSTFLAGS", "-C strip=symbols");

    let status = cmd
        .status()
        .with_context(|| format!("spawn cargo build for {target}"))?;
    if !status.success() {
        return Err(anyhow!(
            "cargo zigbuild failed for target={target} ({status})"
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
