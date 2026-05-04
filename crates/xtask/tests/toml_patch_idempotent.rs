//! Verify the toml patcher produces identical output when applied
//! twice — a guard against silent drift if xtask edits the workspace
//! manifest in-place.

use xtask::matrix::Combo;
use xtask::toml_patch::apply_combo;

const FIXTURE: &str = r#"
[workspace]
resolver = "2"
members = ["a", "b"]

[profile.release]
lto = "thin"
codegen-units = 1
strip = "symbols"
panic = "abort"
"#;

#[test]
fn applying_baseline_combo_is_a_noop_for_opt_level() {
    let patched = apply_combo(FIXTURE, &Combo::baseline()).unwrap();
    let document: toml_edit::DocumentMut = patched.parse().unwrap();
    let release = &document["profile"]["release"];
    assert_eq!(release["lto"].as_str(), Some("thin"));
    // opt-level is the default (3) so the patcher does NOT write it.
    assert!(
        release.get("opt-level").is_none(),
        "baseline must not write opt-level"
    );
}

#[test]
fn applying_lto_fat_opt_z_changes_only_those_keys() {
    let mut combo = Combo::baseline();
    combo.profile.lto = "fat".to_string();
    combo.profile.opt_level = "z".to_string();
    let patched = apply_combo(FIXTURE, &combo).unwrap();
    let document: toml_edit::DocumentMut = patched.parse().unwrap();
    let release = &document["profile"]["release"];
    assert_eq!(release["lto"].as_str(), Some("fat"));
    assert_eq!(release["opt-level"].as_str(), Some("z"));
    assert_eq!(release["codegen-units"].as_integer(), Some(1));
    assert_eq!(release["strip"].as_str(), Some("symbols"));
    assert_eq!(release["panic"].as_str(), Some("abort"));
}

#[test]
fn apply_is_idempotent() {
    let mut combo = Combo::baseline();
    combo.profile.lto = "fat".to_string();
    combo.profile.opt_level = "z".to_string();
    combo.profile.strip = "debuginfo".to_string();

    let once = apply_combo(FIXTURE, &combo).unwrap();
    let twice = apply_combo(&once, &combo).unwrap();
    assert_eq!(once, twice, "second application must be a no-op");
}
