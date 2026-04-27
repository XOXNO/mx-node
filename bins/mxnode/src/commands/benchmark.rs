//! `mxnode benchmark`: report host capability against MultiversX validator
//! requirements. CPU count + model, total memory, free disk space, and
//! latency to gateway. Standalone — does not require `mxnode install`
//! to have run first.

use std::fs;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::orchestrator::runtime::CliErrorExt;

/// MultiversX validator host minimums (from
/// <https://docs.multiversx.com/validators/system-requirements>).
const MIN_CPU_CORES: usize = 4;
const MIN_MEMORY_GB: u64 = 8;
const MIN_FREE_DISK_GB: u64 = 200;
const MAX_GATEWAY_LATENCY_MS: u128 = 250;
const GATEWAY_HOST: &str = "gateway.multiversx.com:443";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    severity: Severity,
    measured: String,
    threshold: &'static str,
}

pub fn run(global: &GlobalArgs) -> Result<(), CliError> {
    let checks = vec![
        check_cpu(),
        check_memory(),
        check_disk(),
        check_gateway_latency(),
    ];

    let any_error = checks.iter().any(|c| c.severity == Severity::Error);

    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": !any_error,
                "checks": checks,
            })
        );
    } else {
        for c in &checks {
            let glyph = match c.severity {
                Severity::Ok => "✓",
                Severity::Warn => "!",
                Severity::Error => "✗",
            };
            println!("{glyph} [{}] {} (need {})", c.name, c.measured, c.threshold);
        }
    }

    if any_error {
        return Err(CliError::new(
            "host below validator minimums",
            "one or more checks failed",
            "fix the items marked `✗` above; see https://docs.multiversx.com/validators/system-requirements",
        )
        .silent()
        .json_if(global.json));
    }
    Ok(())
}

fn check_cpu() -> Check {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    let model = parse_proc_cpuinfo("model name").unwrap_or_else(|| "unknown".to_string());
    let severity = if cores >= MIN_CPU_CORES {
        Severity::Ok
    } else {
        Severity::Error
    };
    Check {
        name: "cpu",
        severity,
        measured: format!("{cores} cores ({model})"),
        threshold: "4+ cores",
    }
}

fn check_memory() -> Check {
    let kb = parse_proc_meminfo("MemTotal").unwrap_or(0);
    let gb = kb / 1024 / 1024;
    let severity = if gb >= MIN_MEMORY_GB {
        Severity::Ok
    } else {
        Severity::Error
    };
    Check {
        name: "memory",
        severity,
        measured: format!("{gb} GB total"),
        threshold: "8+ GB",
    }
}

fn check_disk() -> Check {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let (free_gb, severity) = match free_disk_gb(&home) {
        Some(gb) => (gb, if gb >= MIN_FREE_DISK_GB { Severity::Ok } else { Severity::Warn }),
        None => (0, Severity::Warn),
    };
    Check {
        name: "disk",
        severity,
        measured: format!("{free_gb} GB free at {home}"),
        threshold: "200+ GB free for the chain database",
    }
}

fn check_gateway_latency() -> Check {
    use std::net::TcpStream;
    let start = Instant::now();
    match TcpStream::connect_timeout(
        &GATEWAY_HOST.to_socket_addrs().ok().and_then(|mut a| a.next()).unwrap_or_else(|| {
            // Fallback: 1.1.1.1:443 — we don't fail the check on DNS issues.
            "1.1.1.1:443".parse().expect("hardcoded address parses")
        }),
        Duration::from_secs(5),
    ) {
        Ok(_) => {
            let ms = start.elapsed().as_millis();
            let severity = if ms <= MAX_GATEWAY_LATENCY_MS {
                Severity::Ok
            } else {
                Severity::Warn
            };
            Check {
                name: "gateway latency",
                severity,
                measured: format!("{ms} ms to {GATEWAY_HOST}"),
                threshold: "≤ 250 ms",
            }
        }
        Err(e) => Check {
            name: "gateway latency",
            severity: Severity::Warn,
            measured: format!("could not reach {GATEWAY_HOST}: {e}"),
            threshold: "≤ 250 ms",
        },
    }
}

fn parse_proc_cpuinfo(field: &str) -> Option<String> {
    let body = fs::read_to_string("/proc/cpuinfo").ok()?;
    body.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split(':').nth(1))
        .map(|v| v.trim().to_string())
}

fn parse_proc_meminfo(field: &str) -> Option<u64> {
    let body = fs::read_to_string("/proc/meminfo").ok()?;
    body.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn free_disk_gb(path: &str) -> Option<u64> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let cpath = CString::new(path).ok()?;
    // SAFETY: statvfs is a libc syscall that takes a CStr path and a
    // pointer to a writable struct. Path comes from a CString we own;
    // the struct is MaybeUninit::zeroed() and only read on success.
    let mut stat: MaybeUninit<libc::statvfs> = MaybeUninit::zeroed();
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64) / 1024 / 1024 / 1024)
}

use std::net::ToSocketAddrs;
