//! Combo definitions for the binary-size matrix.
//!
//! A `Combo` is a fully-specified point in the search space: profile
//! knobs, dep prunes, post-processing choices, toolchain channel. The
//! `Phase` enum groups combos by experiment phase per the spec
//! (Phase 0 baseline, Phase 1 profile sweep, etc).

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Profile knobs that map directly to `[profile.release]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Profile {
    pub lto: String,
    pub opt_level: String,
    pub strip: String,
    pub codegen_units: u32,
    pub panic: String,
}

impl Profile {
    pub fn baseline() -> Self {
        Self {
            lto: "thin".to_string(),
            opt_level: "3".to_string(),
            strip: "symbols".to_string(),
            codegen_units: 1,
            panic: "abort".to_string(),
        }
    }
}

/// Dep prunes applied on top of the workspace defaults. Each `bool`
/// is true when the prune is *applied*. False = dep is left at its
/// current feature set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub struct DepPrunes {
    pub drop_env_filter: bool,
    pub release_max_level_info: bool,
    pub drop_clap_wrap_help: bool,
    pub drop_time_macros: bool,
    pub drop_prost_derive: bool,
    pub measure_only_no_live_logs: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub enum Upx {
    #[default]
    None,
    Best,
    BestLzma,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub enum Toolchain {
    #[default]
    Stable,
    NightlyBuildStd,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Combo {
    pub profile: Profile,
    pub deps: DepPrunes,
    pub upx: Upx,
    pub toolchain: Toolchain,
}

impl Combo {
    pub fn baseline() -> Self {
        Self {
            profile: Profile::baseline(),
            deps: DepPrunes::default(),
            upx: Upx::None,
            toolchain: Toolchain::Stable,
        }
    }

    /// Stable, deterministic 12-hex-char ID derived from the combo's
    /// fields. We hash a JSON serialisation so any field addition
    /// without a default value forces a new ID and prevents silent
    /// collisions with old CSV history.
    pub fn combo_id(&self) -> String {
        let json = serde_json::to_string(self).expect("Combo serialises");
        let mut h = Sha256::new();
        h.update(json.as_bytes());
        let digest = h.finalize();
        hex::encode(&digest[..6])
    }

    /// Compact human label, e.g. `lto=fat,opt=z,strip=sym`.
    pub fn combo_label(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("lto={}", self.profile.lto));
        parts.push(format!("opt={}", self.profile.opt_level));
        parts.push(format!("strip={}", short_strip(&self.profile.strip)));
        if self.deps.drop_env_filter {
            parts.push("no-env-filter".to_string());
        }
        if self.deps.release_max_level_info {
            parts.push("max-level-info".to_string());
        }
        if self.deps.drop_clap_wrap_help {
            parts.push("no-wrap-help".to_string());
        }
        if self.deps.drop_time_macros {
            parts.push("no-time-macros".to_string());
        }
        if self.deps.drop_prost_derive {
            parts.push("no-prost-derive".to_string());
        }
        if self.deps.measure_only_no_live_logs {
            parts.push("MEASURE-no-live-logs".to_string());
        }
        match self.upx {
            Upx::None => {}
            Upx::Best => parts.push("upx-best".to_string()),
            Upx::BestLzma => parts.push("upx-lzma".to_string()),
        }
        if matches!(self.toolchain, Toolchain::NightlyBuildStd) {
            parts.push("build-std".to_string());
        }
        parts.join(",")
    }
}

fn short_strip(s: &str) -> &'static str {
    match s {
        "symbols" => "sym",
        "debuginfo" => "dbg",
        _ => "?",
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Phase {
    Baseline,
    ProfileSweep,
    /// Nightly + `-Zbuild-std=std,panic_abort` +
    /// `-Zbuild-std-features=panic_immediate_abort`. Single combo per
    /// host-runnable target — only the host target is exercised because
    /// nightly cross-compile via zigbuild isn't wired up.
    NightlyBuildStdHost,
    // DepSurgery, PostProcess intentionally omitted until later tasks
    // land — they require winners from prior phases.
}

impl Phase {
    pub fn combos(self) -> Box<dyn Iterator<Item = Combo>> {
        match self {
            Phase::Baseline => Box::new(std::iter::once(Combo::baseline())),
            Phase::ProfileSweep => Box::new(profile_sweep()),
            Phase::NightlyBuildStdHost => Box::new(nightly_build_std_host()),
        }
    }
}

/// Two nightly combos, both layered with `panic_immediate_abort` via
/// build-std and targeting the host triple only:
///
/// 1. **perf-safe**: `lto=fat, opt=3, strip=symbols` — keeps the
///    dashboard hot path within the +5% TUI render bar; ships as the
///    default Stage E `-min` artefact for size-conscious operators on
///    bandwidth-constrained hosts.
/// 2. **size-max**: `lto=fat, opt=z, strip=symbols` — smallest
///    achievable binary, but TUI render regresses 2-3×; opt-in only.
///
/// Caller is expected to already have nightly + rust-src installed.
fn nightly_build_std_host() -> impl Iterator<Item = Combo> {
    let mut perf_safe = Combo::baseline();
    perf_safe.profile.lto = "fat".to_string();
    perf_safe.profile.opt_level = "3".to_string();
    perf_safe.profile.strip = "symbols".to_string();
    perf_safe.toolchain = Toolchain::NightlyBuildStd;

    let mut size_max = perf_safe.clone();
    size_max.profile.opt_level = "z".to_string();

    [perf_safe, size_max].into_iter()
}

fn profile_sweep() -> impl Iterator<Item = Combo> {
    let ltos = ["thin", "fat"];
    let opts = ["3", "s", "z"];
    let strips = ["symbols", "debuginfo"];
    let mut out = Vec::with_capacity(12);
    for lto in ltos {
        for opt in opts {
            for strip in strips {
                let mut c = Combo::baseline();
                c.profile.lto = lto.to_string();
                c.profile.opt_level = opt.to_string();
                c.profile.strip = strip.to_string();
                out.push(c);
            }
        }
    }
    out.into_iter()
}
