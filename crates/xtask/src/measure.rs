//! All on-disk + runtime measurements per spec §4.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use crate::tools::{check as tool_check, Tool};

#[derive(Debug, Default)]
pub struct SizeMeasurement {
    pub binary_bytes: u64,
    pub archive_gz_bytes: Option<u64>,
    pub archive_zst_bytes: Option<u64>,
    pub archive_xz_bytes: Option<u64>,
    pub sha256: String,
    pub tools_missing: Vec<String>,
}

pub fn measure_sizes(binary: &Path, work_dir: &Path) -> Result<SizeMeasurement> {
    let bytes = fs::metadata(binary)
        .with_context(|| format!("stat {}", binary.display()))?
        .len();

    let mut hasher = Sha256::new();
    let buf = fs::read(binary).with_context(|| format!("read {}", binary.display()))?;
    hasher.update(&buf);
    let digest = hex::encode(hasher.finalize());

    fs::create_dir_all(work_dir)?;
    let staged = work_dir.join("mxnode");
    fs::copy(binary, &staged)?;

    let mut out = SizeMeasurement {
        binary_bytes: bytes,
        sha256: digest,
        ..SizeMeasurement::default()
    };

    if tool_check(Tool::Zstd) {
        out.archive_zst_bytes = Some(make_archive(
            work_dir,
            "archive.tar.zst",
            &["-I", "zstd -19", "-cf"],
        )?);
    } else {
        out.tools_missing.push(Tool::Zstd.binary().to_string());
    }

    if tool_check(Tool::Xz) {
        out.archive_xz_bytes = Some(make_archive(work_dir, "archive.tar.xz", &["-cJf"])?);
    } else {
        out.tools_missing.push(Tool::Xz.binary().to_string());
    }

    // gzip is always present via libSystem / glibc tar.
    out.archive_gz_bytes = Some(make_archive(work_dir, "archive.tar.gz", &["-czf"])?);

    Ok(out)
}

fn make_archive(work_dir: &Path, name: &str, tar_flags: &[&str]) -> Result<u64> {
    let archive_path = work_dir.join(name);
    let mut cmd = Command::new("tar");
    cmd.current_dir(work_dir);
    cmd.args(tar_flags);
    cmd.arg(&archive_path);
    cmd.arg("mxnode");
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let status = cmd
        .status()
        .with_context(|| format!("run tar for {name}"))?;
    if !status.success() {
        return Err(anyhow!("tar failed for {name}: {status}"));
    }
    Ok(fs::metadata(&archive_path)?.len())
}

/// Cold-start via hyperfine. Returns `None` if hyperfine is missing or
/// the binary cannot be exec'd on this host (cross-target build).
pub fn measure_cold_start(binary: &Path) -> Result<Option<u64>> {
    if !tool_check(Tool::Hyperfine) {
        return Ok(None);
    }
    let out = Command::new(Tool::Hyperfine.binary())
        .args([
            "--warmup",
            "1",
            "--runs",
            "5",
            "--export-json",
            "/dev/stdout",
            "--",
        ])
        .arg(format!("{} --version", binary.display()))
        .output()
        .with_context(|| "spawn hyperfine")?;
    if !out.status.success() {
        return Ok(None);
    }
    // hyperfine emits the JSON last — find the first '{' on the line.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json_start = stdout.find('{').ok_or_else(|| {
        anyhow!("hyperfine produced no JSON: {}", stdout)
    })?;
    let json: serde_json::Value = serde_json::from_str(&stdout[json_start..])
        .with_context(|| format!("parse hyperfine output: {}", &stdout[json_start..]))?;
    let median_s = json["results"][0]["median"]
        .as_f64()
        .ok_or_else(|| anyhow!("hyperfine output missing median"))?;
    Ok(Some((median_s * 1000.0) as u64))
}

/// TUI render via the bench-harness-feature `bench-render` subcommand.
/// Calls the just-built binary with `bench-render --frames N` five
/// times and returns the median elapsed_ms.
pub fn measure_tui_render(binary: &Path, fixture: &Path, frames: u32) -> Result<Option<u64>> {
    let mut samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let out = Command::new(binary)
            .arg("bench-render")
            .arg("--frames")
            .arg(frames.to_string())
            .arg("--fixture")
            .arg(fixture)
            .output();
        let out = match out {
            Ok(o) => o,
            Err(_) => return Ok(None), // can't exec (cross-target)
        };
        if !out.status.success() {
            return Ok(None);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let ms = parse_elapsed(&stderr).ok_or_else(|| {
            anyhow!("bench-render stderr missing elapsed_ms: {stderr}")
        })?;
        samples.push(ms);
    }
    samples.sort_unstable();
    Ok(Some(samples[samples.len() / 2]))
}

fn parse_elapsed(stderr: &str) -> Option<u64> {
    for line in stderr.lines().rev() {
        if let Some(rest) = line.strip_prefix("elapsed_ms=") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[derive(Debug, Default)]
pub struct UpxResult {
    pub bytes_after: u64,
    pub upx_path: PathBuf,
}

/// In-place UPX compression on a *copy* of the binary so the original
/// stays available for the no-UPX size row.
pub fn run_upx(binary: &Path, work_dir: &Path, lzma: bool) -> Result<Option<UpxResult>> {
    if !tool_check(Tool::Upx) {
        return Ok(None);
    }
    let upx_path = work_dir.join("mxnode.upx");
    fs::copy(binary, &upx_path)?;
    let mut cmd = Command::new(Tool::Upx.binary());
    cmd.arg("--best").arg("--quiet");
    if lzma {
        cmd.arg("--lzma");
    }
    cmd.arg(&upx_path);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let status = cmd.status().with_context(|| "spawn upx")?;
    if !status.success() {
        // UPX refuses to compress some Mach-O variants; treat as skip.
        return Ok(None);
    }
    let bytes_after = fs::metadata(&upx_path)?.len();
    Ok(Some(UpxResult {
        bytes_after,
        upx_path,
    }))
}
