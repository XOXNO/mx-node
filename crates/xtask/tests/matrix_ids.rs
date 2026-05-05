//! Verify that combo IDs are deterministic and stable across runs —
//! the CSV history depends on this.

use xtask::matrix::{Combo, Phase};

#[test]
fn combo_id_is_deterministic() {
    let c1 = Combo::baseline();
    let c2 = Combo::baseline();
    assert_eq!(c1.combo_id(), c2.combo_id());
    assert!(!c1.combo_id().is_empty());
}

#[test]
fn different_combos_produce_different_ids() {
    let baseline = Combo::baseline();
    let mut alt = Combo::baseline();
    alt.profile.lto = "fat".to_string();
    assert_ne!(baseline.combo_id(), alt.combo_id());
}

#[test]
fn combo_label_is_human_readable() {
    let mut c = Combo::baseline();
    c.profile.lto = "fat".to_string();
    c.profile.opt_level = "z".to_string();
    let label = c.combo_label();
    assert!(label.contains("lto=fat"));
    assert!(label.contains("opt=z"));
}

#[test]
fn baseline_phase_iterates_one_combo() {
    let combos: Vec<_> = Phase::Baseline.combos().collect();
    assert_eq!(combos.len(), 1);
    assert_eq!(combos[0].combo_id(), Combo::baseline().combo_id());
}

#[test]
fn profile_sweep_produces_twelve_combos() {
    let combos: Vec<_> = Phase::ProfileSweep.combos().collect();
    // 2 lto × 3 opt × 2 strip = 12
    assert_eq!(combos.len(), 12);
}

#[test]
fn nightly_build_std_host_yields_two_combos_perf_safe_then_size_max() {
    use xtask::matrix::Toolchain;
    let combos: Vec<_> = Phase::NightlyBuildStdHost.combos().collect();
    assert_eq!(combos.len(), 2);

    // Perf-safe variant first — `opt=3` keeps the TUI render hot path
    // inside the +5% bar, gates Stage E's default `-min` artefact.
    let perf_safe = &combos[0];
    assert_eq!(perf_safe.profile.lto, "fat");
    assert_eq!(perf_safe.profile.opt_level, "3");
    assert_eq!(perf_safe.profile.strip, "symbols");
    assert_eq!(perf_safe.toolchain, Toolchain::NightlyBuildStd);
    assert!(perf_safe.combo_label().contains("build-std"));

    // Size-max variant second — `opt=z` regresses TUI render 2-3×; opt-in only.
    let size_max = &combos[1];
    assert_eq!(size_max.profile.opt_level, "z");
    assert_eq!(size_max.toolchain, Toolchain::NightlyBuildStd);
}
