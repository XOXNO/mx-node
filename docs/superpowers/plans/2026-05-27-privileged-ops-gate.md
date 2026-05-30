# Privileged-Ops Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route every privileged host mutation (systemctl enable/disable/daemon-reload/reset-failed, and the `mv`/`rm` of unit files under `/etc/systemd/system`) through the existing `Ctl` trait so they are exit-checked, fail-closed, platform-correct, and unit-testable — eliminating the scattered raw `Command::new("sudo")…status()` calls whose swallowed exit codes caused the daemon-reload-missing and cleanup-silent-failure bugs.

**Architecture:** The `mxnode-systemd` crate already exposes a `Ctl` trait (`SystemctlCtl` for Linux, `LaunchdCtl` for macOS) and a `FakeCtl` recording double. It covers `start/stop/restart/is_active/show_property` but **not** the privileged lifecycle ops, which callers hand-roll with raw `sudo`. We extend the trait with six privileged ops, implement them on all three backends (reusing `SystemctlCtl::run_mutation`'s exit-check discipline), then migrate the two raw-sudo call sites (`orchestrator/supervisor.rs`, `commands/uninstall.rs`) to the trait. Every migration is covered by a `FakeCtl` test that injects a failure and asserts it is **propagated, not swallowed**.

**Tech Stack:** Rust 1.94.1, `tokio` (current-thread), `async-trait`, `thiserror`. Test doubles via `mxnode_systemd::ctl_testing::FakeCtl`. No new dependencies.

**Out of scope (follow-up plans):** the `apt-get` privilege escalation in `mxnode-toolchain`, the `sudo` usage in `self_update.rs`, and the read-only `systemctl` probes in `doctor.rs`. This plan covers unit-file install/removal + the systemd manager ops only.

---

## File Structure

| File | Responsibility | Change |
| --- | --- | --- |
| `crates/mxnode-systemd/src/ctl.rs` | `Ctl` trait + `SystemctlCtl`/`LaunchdCtl`/`FakeCtl` | Add 6 trait methods + `CtlError::Io`; implement on all three backends; add pure `is_not_enabled` helper |
| `bins/mxnode/src/orchestrator/supervisor.rs` | Render + install unit files | Thread `&dyn Ctl` into `install_one_unit`/`install_unit_linux`; replace raw sudo with trait ops; delete `daemon_reload_linux` |
| `bins/mxnode/src/orchestrator/install.rs` | Install orchestration | `install_units` builds the supervisor once and passes it down |
| `bins/mxnode/src/commands/uninstall.rs` | Host cleanup | Replace raw sudo disable/rm/daemon-reload/reset-failed in `Step::apply` + executor with trait ops |

**Convention:** the trait is the only place privileged systemd side-effects are expressed. After this plan, `grep -rn 'Command::new("sudo")' bins/mxnode/src/orchestrator/supervisor.rs bins/mxnode/src/commands/uninstall.rs` returns nothing.

---

## Task 1: Extend the `Ctl` trait, `CtlError`, and `FakeCtl`

**Files:**
- Modify: `crates/mxnode-systemd/src/ctl.rs` (trait ~line 55, `CtlError` ~line 15, `FakeCtl` ~line 372)
- Test: `crates/mxnode-systemd/src/ctl.rs` (testing module / new `#[cfg(test)] mod trait_tests`)

- [ ] **Step 1: Write the failing test**

Add to the bottom of `crates/mxnode-systemd/src/ctl.rs`:

```rust
#[cfg(test)]
mod privileged_trait_tests {
    use super::testing::FakeCtl;
    use super::Ctl;
    use std::path::Path;

    #[tokio::test]
    async fn fake_records_privileged_ops_and_can_fail() {
        let ctl = FakeCtl::new();
        ctl.daemon_reload().await.unwrap();
        ctl.enable("elrond-node-0.service").await.unwrap();
        ctl.disable("elrond-node-0.service").await.unwrap();
        ctl.reset_failed().await.unwrap();
        ctl.install_unit_file(Path::new("/tmp/x"), Path::new("/etc/systemd/system/x"))
            .await
            .unwrap();
        ctl.remove_file(Path::new("/etc/systemd/system/x")).await.unwrap();

        let verbs: Vec<String> = ctl.calls().into_iter().map(|(v, _)| v).collect();
        assert_eq!(
            verbs,
            vec!["daemon-reload", "enable", "disable", "reset-failed", "install-unit", "remove-file"],
        );

        // Fail-injection: a configured failure must surface as Err, never swallowed.
        let ctl = FakeCtl::new();
        ctl.fail_on("remove-file");
        assert!(ctl.remove_file(Path::new("/etc/systemd/system/x")).await.is_err());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mxnode-systemd privileged_trait_tests -- --nocapture`
Expected: FAIL to compile — `no method named daemon_reload`, `no function fail_on`.

- [ ] **Step 3: Add the trait methods + `CtlError::Io`**

In `crates/mxnode-systemd/src/ctl.rs`, add `use std::ffi::OsStr;` and `use std::path::Path;` to the imports (it already imports `std::path::PathBuf`). Extend `CtlError`:

```rust
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
```

Extend the `Ctl` trait (after `show_property`):

```rust
    /// `systemctl daemon-reload`. Must run after any unit file is written
    /// to or removed from `/etc/systemd/system` so the manager doesn't act
    /// on a stale in-memory view.
    async fn daemon_reload(&self) -> Result<(), CtlError>;
    /// `systemctl enable <unit>`.
    async fn enable(&self, unit: &str) -> Result<(), CtlError>;
    /// `systemctl disable <unit>`. Idempotent: a unit that was never
    /// enabled is treated as success, not an error.
    async fn disable(&self, unit: &str) -> Result<(), CtlError>;
    /// `systemctl reset-failed` (all units). Clears lingering failed state
    /// after stopping/removing units.
    async fn reset_failed(&self) -> Result<(), CtlError>;
    /// Place a rendered unit file at `dest` (privileged on Linux:
    /// `/etc/systemd/system` is root-owned; unprivileged on macOS).
    async fn install_unit_file(&self, src: &Path, dest: &Path) -> Result<(), CtlError>;
    /// Remove a file at a privileged path (unit file under
    /// `/etc/systemd/system`). Missing file is success.
    async fn remove_file(&self, path: &Path) -> Result<(), CtlError>;
```

- [ ] **Step 4: Implement the six methods on `FakeCtl`**

Replace the `FakeCtl` struct + impl block in the `testing` module. Add a `fail_on` set and a `record_or_fail` helper, then implement the new methods (keep the existing `start/stop/restart/is_active/show_property` impls unchanged):

```rust
    use std::collections::HashSet;

    #[derive(Default)]
    pub struct FakeCtl {
        pub calls: Mutex<Vec<(String, String)>>,
        active_states: Mutex<HashMap<String, ActiveState>>,
        fail_on: Mutex<HashSet<String>>,
    }

    impl FakeCtl {
        pub fn new() -> Self { Self::default() }
        pub fn set_active(&self, unit: &str, state: ActiveState) {
            self.active_states.lock().unwrap().insert(unit.to_string(), state);
        }
        pub fn calls(&self) -> Vec<(String, String)> { self.calls.lock().unwrap().clone() }
        /// Make every future call to `verb` (e.g. "remove-file") return Err.
        pub fn fail_on(&self, verb: &str) { self.fail_on.lock().unwrap().insert(verb.to_string()); }

        fn record_or_fail(&self, verb: &str, target: &str) -> Result<(), CtlError> {
            self.calls.lock().unwrap().push((verb.to_string(), target.to_string()));
            if self.fail_on.lock().unwrap().contains(verb) {
                return Err(CtlError::NonZero { code: 1, stderr: format!("fake failure on {verb}") });
            }
            Ok(())
        }
    }
```

Add these to `impl Ctl for FakeCtl` (after `show_property`):

```rust
        async fn daemon_reload(&self) -> Result<(), CtlError> { self.record_or_fail("daemon-reload", "") }
        async fn enable(&self, unit: &str) -> Result<(), CtlError> { self.record_or_fail("enable", unit) }
        async fn disable(&self, unit: &str) -> Result<(), CtlError> { self.record_or_fail("disable", unit) }
        async fn reset_failed(&self) -> Result<(), CtlError> { self.record_or_fail("reset-failed", "") }
        async fn install_unit_file(&self, _src: &Path, dest: &Path) -> Result<(), CtlError> {
            self.record_or_fail("install-unit", &dest.display().to_string())
        }
        async fn remove_file(&self, path: &Path) -> Result<(), CtlError> {
            self.record_or_fail("remove-file", &path.display().to_string())
        }
```

Note: this step will not yet compile the crate — `SystemctlCtl` and `LaunchdCtl` don't implement the new methods. That is expected; Tasks 2 and 3 add them. To unblock the `FakeCtl` test in isolation, temporarily add the six methods to `SystemctlCtl`/`LaunchdCtl` returning `unimplemented!()`, OR implement Tasks 2–3 before re-running. Recommended: proceed straight to Task 2 and 3, then run this test at the end of Task 3.

- [ ] **Step 5: Commit** (after Tasks 2–3 make the crate compile; see Task 3 Step 5)

---

## Task 2: Implement privileged ops on `SystemctlCtl` (exit-checked)

**Files:**
- Modify: `crates/mxnode-systemd/src/ctl.rs` (`SystemctlCtl` impl ~line 75; `impl Ctl for SystemctlCtl` ~line 144)
- Test: `crates/mxnode-systemd/src/ctl.rs` (new unit test for the pure `is_not_enabled` helper)

- [ ] **Step 1: Write the failing test**

Add to `crates/mxnode-systemd/src/ctl.rs`:

```rust
#[cfg(test)]
mod systemctl_helpers {
    use super::is_not_enabled;

    #[test]
    fn not_enabled_is_tolerated() {
        assert!(is_not_enabled("Failed to disable unit: Unit file elrond-node-0.service does not exist."));
        assert!(is_not_enabled("The unit files have no installation config (WantedBy=...) and are not enabled."));
        assert!(!is_not_enabled("Interactive authentication required."));
        assert!(!is_not_enabled(""));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mxnode-systemd systemctl_helpers`
Expected: FAIL to compile — `cannot find function is_not_enabled`.

- [ ] **Step 3: Add `is_not_enabled`, `run_privileged`, and the trait impl**

Add a free function near the top of `ctl.rs` (module scope):

```rust
/// True when a `systemctl disable` failure means "there was nothing to
/// disable" (the unit was never enabled, or its file is already gone) —
/// which uninstall treats as success.
fn is_not_enabled(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("not enabled") || s.contains("does not exist") || s.contains("no such file")
}
```

Add a privileged runner to `impl SystemctlCtl` (alongside `run_mutation`). Unlike `run_mutation` it runs an arbitrary program (`mv`, `rm`) under `sudo`, not `systemctl`:

```rust
    async fn run_privileged(&self, program: &str, args: &[&std::ffi::OsStr]) -> Result<(), CtlError> {
        let mut cmd = if self.sudo {
            let mut c = Command::new("sudo");
            c.arg("--non-interactive").arg(program);
            c
        } else {
            Command::new(program)
        };
        for a in args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = cmd.output().await.map_err(CtlError::Spawn)?;
        if output.status.success() {
            return Ok(());
        }
        match output.status.code() {
            Some(code) => Err(CtlError::NonZero {
                code,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
            None => Err(CtlError::Signaled),
        }
    }
```

Add the six methods to `impl Ctl for SystemctlCtl`:

```rust
    async fn daemon_reload(&self) -> Result<(), CtlError> {
        self.run_mutation(&["daemon-reload"]).await
    }
    async fn enable(&self, unit: &str) -> Result<(), CtlError> {
        self.run_mutation(&["enable", unit]).await
    }
    async fn disable(&self, unit: &str) -> Result<(), CtlError> {
        match self.run_mutation(&["disable", unit]).await {
            Err(CtlError::NonZero { stderr, .. }) if is_not_enabled(&stderr) => Ok(()),
            other => other,
        }
    }
    async fn reset_failed(&self) -> Result<(), CtlError> {
        self.run_mutation(&["reset-failed"]).await
    }
    async fn install_unit_file(&self, src: &std::path::Path, dest: &std::path::Path) -> Result<(), CtlError> {
        self.run_privileged("mv", &[src.as_os_str(), dest.as_os_str()]).await
    }
    async fn remove_file(&self, path: &std::path::Path) -> Result<(), CtlError> {
        self.run_privileged("rm", &[std::ffi::OsStr::new("-f"), path.as_os_str()]).await
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mxnode-systemd systemctl_helpers`
Expected: PASS. (The crate still won't fully compile until Task 3 adds `LaunchdCtl` impls; if the test command fails to build on `LaunchdCtl`, do Task 3 Step 3 first, then re-run.)

- [ ] **Step 5: Commit** (after Task 3; see Task 3 Step 5)

---

## Task 3: Implement privileged ops on `LaunchdCtl` (unprivileged, fs-based)

**Files:**
- Modify: `crates/mxnode-systemd/src/ctl.rs` (`impl Ctl for LaunchdCtl` ~line 242)
- Test: `crates/mxnode-systemd/src/ctl.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod launchd_privileged {
    use super::{Ctl, LaunchdCtl};

    #[tokio::test]
    async fn install_and_remove_unit_file_are_fs_ops() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.plist");
        let dest = dir.path().join("dest.plist");
        std::fs::write(&src, b"<plist/>").unwrap();

        let ctl = LaunchdCtl::new();
        // Manager ops are no-ops on launchd (bootstrap/bootout handle lifecycle).
        ctl.daemon_reload().await.unwrap();
        ctl.enable("elrond-node-0.service").await.unwrap();
        ctl.disable("elrond-node-0.service").await.unwrap();
        ctl.reset_failed().await.unwrap();

        ctl.install_unit_file(&src, &dest).await.unwrap();
        assert!(dest.exists());
        ctl.remove_file(&dest).await.unwrap();
        assert!(!dest.exists());
        // Removing a missing file is success.
        ctl.remove_file(&dest).await.unwrap();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mxnode-systemd launchd_privileged`
Expected: FAIL to compile — `LaunchdCtl` doesn't implement the new trait methods.

- [ ] **Step 3: Implement the six methods on `LaunchdCtl`**

Add to `impl Ctl for LaunchdCtl`:

```rust
    async fn daemon_reload(&self) -> Result<(), CtlError> { Ok(()) }
    async fn enable(&self, _unit: &str) -> Result<(), CtlError> { Ok(()) }
    async fn disable(&self, _unit: &str) -> Result<(), CtlError> { Ok(()) }
    async fn reset_failed(&self) -> Result<(), CtlError> { Ok(()) }
    async fn install_unit_file(&self, src: &std::path::Path, dest: &std::path::Path) -> Result<(), CtlError> {
        std::fs::copy(src, dest)?;
        Ok(())
    }
    async fn remove_file(&self, path: &std::path::Path) -> Result<(), CtlError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
```

- [ ] **Step 4: Run the full crate test suite**

Run: `cargo test -p mxnode-systemd`
Expected: PASS — `privileged_trait_tests`, `systemctl_helpers`, `launchd_privileged`, and all pre-existing tests green.
Run: `cargo clippy -p mxnode-systemd --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit Tasks 1–3 together**

```bash
git add crates/mxnode-systemd/src/ctl.rs
git commit -m "feat(systemd): privileged ops (enable/disable/reload/reset-failed, unit-file mv/rm) on the Ctl trait

Folds the daemon-reload + enable/disable + unit-file install/removal that
callers hand-rolled with raw sudo into the Ctl trait, reusing the
exit-checked run_mutation discipline. Implemented on SystemctlCtl (sudo,
fail-closed), LaunchdCtl (unprivileged fs ops), and FakeCtl (record +
fail-injection)."
```

---

## Task 4: Migrate `supervisor::install_unit_linux` to the trait

**Files:**
- Modify: `bins/mxnode/src/orchestrator/supervisor.rs` (`install_one_unit` ~line 62, `install_unit_linux` ~line 83, delete `daemon_reload_linux`)
- Modify: `bins/mxnode/src/orchestrator/install.rs` (`install_units` — pass the ctl down)
- Test: `bins/mxnode/src/orchestrator/supervisor.rs` (new `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to `bins/mxnode/src/orchestrator/supervisor.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::install_one_unit;
    use mxnode_core::Platform;
    use mxnode_systemd::ctl_testing::FakeCtl;

    #[tokio::test]
    async fn linux_install_reloads_before_enable_and_propagates_failure() {
        // Happy path: mv -> daemon-reload -> enable, in order.
        let ctl = FakeCtl::new();
        install_one_unit(&ctl, Platform::Linux, "elrond-node-0.service", "[Unit]\n", true)
            .await
            .expect("install should succeed");
        let verbs: Vec<String> = ctl.calls().into_iter().map(|(v, _)| v).collect();
        let reload = verbs.iter().position(|v| v == "daemon-reload").unwrap();
        let enable = verbs.iter().position(|v| v == "enable").unwrap();
        assert!(verbs.contains(&"install-unit".to_string()));
        assert!(reload < enable, "daemon-reload must precede enable: {verbs:?}");

        // A failed enable must surface as an error, never be swallowed.
        let ctl = FakeCtl::new();
        ctl.fail_on("enable");
        let err = install_one_unit(&ctl, Platform::Linux, "elrond-node-0.service", "[Unit]\n", true).await;
        assert!(err.is_err(), "enable failure must propagate");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mxnode install_one_unit -- --nocapture`
Expected: FAIL to compile — `install_one_unit` currently takes no `&dyn Ctl` and runs raw sudo.

- [ ] **Step 3: Thread `&dyn Ctl` and replace raw sudo**

Change `install_one_unit`'s signature (add `ctl: &dyn mxnode_systemd::Ctl` as the first param) and rewrite `install_unit_linux`:

```rust
pub async fn install_one_unit(
    ctl: &dyn mxnode_systemd::Ctl,
    platform: Platform,
    unit_name: &str,
    contents: &str,
    enable: bool,
) -> Result<(), InstallUnitError> {
    let dest = unit_dir_for_platform(platform)
        .ok_or(InstallUnitError::UnsupportedPlatform)?
        .join(supervisor_filename(platform, unit_name));
    match platform {
        Platform::Linux => install_unit_linux(ctl, &dest, contents, unit_name, enable).await,
        Platform::Macos => install_unit_macos(ctl, &dest, contents, unit_name, enable).await,
    }
}

async fn install_unit_linux(
    ctl: &dyn mxnode_systemd::Ctl,
    dest: &Path,
    contents: &str,
    unit_name: &str,
    enable: bool,
) -> Result<(), InstallUnitError> {
    let tmp = std::env::temp_dir().join(unit_name);
    fs::write(&tmp, contents).map_err(|e| InstallUnitError::Io {
        path: tmp.display().to_string(),
        source: e,
    })?;
    ctl.install_unit_file(&tmp, dest)
        .await
        .map_err(|e| InstallUnitError::Privileged(format!("install unit {}: {e}", dest.display())))?;
    // Reload BEFORE enable so the manager sees the freshly-placed unit.
    ctl.daemon_reload()
        .await
        .map_err(|e| InstallUnitError::Privileged(format!("daemon-reload: {e}")))?;
    if enable {
        ctl.enable(unit_name)
            .await
            .map_err(|e| InstallUnitError::Privileged(format!("enable {unit_name}: {e}")))?;
    }
    Ok(())
}
```

Update `install_unit_macos` to take (and ignore, or use) `ctl` — its body already uses `ctl.install_unit_file` semantics via fs; route it through the trait too:

```rust
async fn install_unit_macos(
    ctl: &dyn mxnode_systemd::Ctl,
    dest: &Path,
    contents: &str,
    unit_name: &str,
    enable: bool,
) -> Result<(), InstallUnitError> {
    let tmp = std::env::temp_dir().join(unit_name);
    fs::write(&tmp, contents).map_err(|e| InstallUnitError::Io { path: tmp.display().to_string(), source: e })?;
    ctl.install_unit_file(&tmp, dest)
        .await
        .map_err(|e| InstallUnitError::Privileged(format!("install plist {}: {e}", dest.display())))?;
    if enable {
        ctl.enable(unit_name)
            .await
            .map_err(|e| InstallUnitError::Privileged(format!("bootstrap {unit_name}: {e}")))?;
    }
    Ok(())
}
```

Add the `Privileged` variant to `InstallUnitError` (in this file):

```rust
    #[error("privileged op failed: {0}")]
    Privileged(String),
```

Delete the `daemon_reload_linux` free function and its `reset_failed`-style helpers from this file (now on the trait).

- [ ] **Step 4: Pass the ctl from `install_units`**

In `bins/mxnode/src/orchestrator/install.rs`, find `install_units` (the loop that calls `install_one_unit`). Build the supervisor once and pass it:

```rust
pub async fn install_units(units: &[UnitFile], enable: bool) -> Result<(), InstallError> {
    let platform = Platform::current();
    let ctl = crate::orchestrator::supervisor::build_supervisor();
    for unit in units {
        crate::orchestrator::supervisor::install_one_unit(ctl.as_ref(), platform, &unit.name, &unit.contents, enable)
            .await
            .map_err(|e| /* existing InstallError mapping; add InstallUnitError::Privileged arm */)?;
    }
    Ok(())
}
```

Map the new `InstallUnitError::Privileged(msg)` in the existing `InstallError` conversion (wherever `InstallUnitError` is converted) to `InstallError::Io { path: "<privileged>".into(), source: std::io::Error::other(msg) }` or a dedicated `InstallError::Privileged(String)` variant — match whatever the surrounding code already does for `InstallUnitError::UnsupportedPlatform`.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p mxnode install_one_unit -- --nocapture`
Expected: PASS (both the ordering assertion and the enable-failure propagation).
Run: `cargo check -p mxnode`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add bins/mxnode/src/orchestrator/supervisor.rs bins/mxnode/src/orchestrator/install.rs
git commit -m "refactor(install): route unit-file install through the Ctl gate

install_one_unit takes &dyn Ctl and uses install_unit_file + daemon_reload
+ enable instead of raw sudo, so a failed step propagates instead of being
swallowed. Deletes the ad-hoc daemon_reload_linux helper."
```

---

## Task 5: Migrate `uninstall::Step` to the trait

**Files:**
- Modify: `bins/mxnode/src/commands/uninstall.rs` (`Step` enum ~line 189, `Step::apply` ~line 226, executor ~line 80, `build_plan` ~line 269)
- Test: `bins/mxnode/src/commands/uninstall.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod step_tests {
    use super::Step;
    use mxnode_systemd::ctl_testing::FakeCtl;
    use std::path::PathBuf;

    #[tokio::test]
    async fn disable_and_remove_go_through_the_gate() {
        let ctl = FakeCtl::new();
        Step::DisableUnit { unit: "elrond-node-0.service".into() }.apply(&ctl).await.unwrap();
        Step::RemoveUnitFile { path: PathBuf::from("/etc/systemd/system/elrond-node-0.service") }
            .apply(&ctl)
            .await
            .unwrap();
        let verbs: Vec<String> = ctl.calls().into_iter().map(|(v, _)| v).collect();
        assert_eq!(verbs, vec!["disable", "remove-file"]);
    }

    #[tokio::test]
    async fn remove_failure_is_not_swallowed() {
        let ctl = FakeCtl::new();
        ctl.fail_on("remove-file");
        let res = Step::RemoveUnitFile { path: PathBuf::from("/etc/systemd/system/x.service") }
            .apply(&ctl)
            .await;
        assert!(res.is_err(), "a failed unit-file removal must surface");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mxnode step_tests -- --nocapture`
Expected: FAIL to compile — `RemoveUnitFile` still carries a `sudo: bool` field and `apply` runs raw `Command::new("sudo")`.

- [ ] **Step 3: Simplify `Step` and route through the trait**

Change the `RemoveUnitFile` variant to drop `sudo` (the `Ctl` impl knows the platform):

```rust
enum Step {
    StopUnit { unit: String },
    DisableUnit { unit: String },
    RemoveUnitFile { path: PathBuf },
    RemoveDir { path: PathBuf },
}
```

Update `Step::summary` accordingly (replace the `sudo`-conditional arm with a single `format!("rm {}", path.display())`). Rewrite the privileged arms of `Step::apply`:

```rust
    async fn apply(&self, ctl: &dyn Ctl) -> Result<(), String> {
        match self {
            Step::StopUnit { unit } => {
                ctl.stop(unit).await.map_err(|e| e.to_string())?;
                Ok(())
            }
            Step::DisableUnit { unit } => {
                // Idempotent in the SystemctlCtl impl (not-enabled => Ok).
                ctl.disable(unit).await.map_err(|e| e.to_string())
            }
            Step::RemoveUnitFile { path } => {
                ctl.remove_file(path).await.map_err(|e| e.to_string())
            }
            Step::RemoveDir { path } => remove_dir_idempotent(path),
        }
    }
```

In `build_plan`, drop the `needs_sudo`/`sudo:` wiring — push `Step::RemoveUnitFile { path }` directly. Replace the post-loop raw daemon-reload/reset-failed block in the executor with trait calls:

```rust
    if matches!(Platform::current(), Platform::Linux) {
        if let Err(e) = ctl.daemon_reload().await {
            had_error = true;
            eprintln!("warn: daemon-reload failed: {e}");
        }
        if let Err(e) = ctl.reset_failed().await {
            had_error = true;
            eprintln!("warn: reset-failed failed: {e}");
        }
    }
```

Remove the now-unused `use std::process::{Command, Stdio};` if nothing else in the file uses them (the `remove_dir_idempotent` fs helper does not).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mxnode step_tests -- --nocapture`
Expected: PASS — `disable_and_remove_go_through_the_gate` and `remove_failure_is_not_swallowed`.
Run: `cargo test -p mxnode uninstall`
Expected: PASS — existing uninstall tests still green (update any test that constructed `RemoveUnitFile { sudo, .. }` to drop the field).

- [ ] **Step 5: Commit**

```bash
git add bins/mxnode/src/commands/uninstall.rs
git commit -m "refactor(uninstall): route disable/rm/daemon-reload/reset-failed through the Ctl gate

Replaces the raw sudo systemctl-disable and sudo rm (which swallowed exit
codes) with exit-checked trait ops; disable is idempotent for never-enabled
units. RemoveUnitFile no longer carries a sudo flag — the Ctl impl owns the
platform difference."
```

---

## Task 6: Remove dead code + full verification

**Files:**
- Modify: `bins/mxnode/src/orchestrator/supervisor.rs` (confirm `daemon_reload_linux` gone)
- Modify: any caller still referencing removed symbols

- [ ] **Step 1: Confirm no raw privileged sudo remains in the migrated files**

Run:
```bash
grep -rn 'Command::new("sudo")' bins/mxnode/src/orchestrator/supervisor.rs bins/mxnode/src/commands/uninstall.rs
grep -rn 'daemon_reload_linux' bins/mxnode/src crates/mxnode-systemd/src
```
Expected: both return nothing.

- [ ] **Step 2: Confirm no exhaustive `match` on `CtlError` broke from the new `Io` variant**

Run: `grep -rn "CtlError::" bins crates --include=*.rs | grep -v "Err(CtlError"`
Expected: every site either uses `.to_string()`/`.map_err` or includes a wildcard arm. Fix any exhaustive match by adding `CtlError::Io(_) => …` or a `_ =>` arm.

- [ ] **Step 3: Full workspace verification**

Run:
```bash
cargo check --workspace --all-targets
cargo test -p mxnode-systemd -p mxnode
cargo clippy -p mxnode-systemd -p mxnode --all-targets -- -D warnings
```
Expected: check clean; tests green (including the four new privileged tests and the two migration tests); clippy clean for the touched crates. (Pre-existing `doctor.rs`/`benchmark.rs` clippy debt and the `loader` default-equality test are out of scope — do not let them block, but do not introduce new lints.)

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "chore(systemd): drop dead daemon_reload_linux; verify privileged gate"
```

---

## Self-Review

**Spec coverage:**
- Privileged ops centralized on the trait → Tasks 1–3. ✓
- Exit-checked / fail-closed → `run_privileged` (Task 2), idempotent `disable` (Task 2), propagated errors at every migrated site (Tasks 4–5). ✓
- daemon-reload lifecycle owned in one place → `daemon_reload` on the trait, called after install (Task 4) and after removal (Task 5). ✓
- Testability via fail-injection → `FakeCtl::fail_on` (Task 1) used in Tasks 4 and 5. ✓
- No raw `sudo` left in migrated files → Task 6 Step 1. ✓

**Placeholder scan:** every code step contains complete code; the only deferred decisions (the exact `InstallError`/`InstallUnitError` conversion arm in Task 4 Step 4) are pinned to "match the existing `UnsupportedPlatform` arm," which is concrete in the file.

**Type consistency:** `install_one_unit(ctl, platform, unit_name, contents, enable)` — the same signature is used in the Task 4 test and the implementation. `FakeCtl::fail_on(verb)` / `calls()` defined in Task 1 are the exact names used in Tasks 4–5. `Step::RemoveUnitFile { path }` (no `sudo`) is consistent between the enum change, `summary`, `apply`, `build_plan`, and the Task 5 test.

**Note on `SystemctlCtl` real-sudo coverage:** the real backend's behavior cannot be unit-tested without root/systemd; it is a thin delegation to the exit-checked `run_mutation`/`run_privileged`, and its correctness is covered indirectly by the pure `is_not_enabled` test plus the integration sweep. A follow-up plan should add a container-based integration test that actually installs/removes a dummy unit.
