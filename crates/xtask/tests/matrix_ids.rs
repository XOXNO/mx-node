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
