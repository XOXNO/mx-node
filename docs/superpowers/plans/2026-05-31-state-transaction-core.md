# State-Transaction Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `mxnode.toml` read-modify-write invariant **unbypassable** by adding a `StateStore::transaction` API that owns lock → fresh-load → mutate → atomic-commit as one unit, then migrating every command that hand-rolls `lock`/`load`/`save` to it — so the lost-update class of bug (e.g. the `install --add` snapshot clobber) cannot be written.

**Architecture:** Today `StateStore` exposes `load()`/`load_file()` (no lock) plus `lock()`/`save()`/`save_file()`. The correct sequence (lock → reload-under-lock → mutate → save) is *convention*; some commands followed a pre-lock snapshot and lost concurrent writes. We add `transaction(|host| …)` (infallible closure) and `try_transaction(|host| -> Result<…>)` (fallible closure, for the install "already-installed" guard). Both acquire the exclusive flock, load the **full** `MxnodeFile` fresh under the lock, hand the closure `&mut file.host`, and on success persist the whole file atomically (preserving operator sections, exactly as `save()` already does); on closure error nothing is written. Then the 5 hand-rolled writers are migrated and the redundant `persist_state` helper is deleted.

**Tech Stack:** Rust 1.94.1, `thiserror`, `fs2` flock, `tempfile` atomic rename. No new dependencies.

**Out of scope (separate plans):** the "single Install Identity anchoring all paths" half of Pillar 2 (a config/path concern, not a persistence one); the `inflight.toml` crash-journal (a different file with its own `Inflight::save`); read-only callers (`status`/`doctor`/`logs`/`metrics`) which only `load()` and must stay lock-free.

---

## File Structure

| File | Responsibility | Change |
| --- | --- | --- |
| `crates/mxnode-state/src/store.rs` | `mxnode.toml` lock + atomic IO | Add `TxError<E>`, `transaction`, `try_transaction` + tests |
| `crates/mxnode-state/src/lib.rs` | crate exports | export `TxError` |
| `bins/mxnode/src/commands/install_add.rs` | `install --add` | replace inline lock→reload→merge→save with `transaction` |
| `bins/mxnode/src/commands/upgrade.rs` | upgrade migration log | replace 2 lock→reload→save sites with `transaction` |
| `bins/mxnode/src/commands/install.rs` + `bins/mxnode/src/orchestrator/install.rs` | fresh install | replace `persist_state` + `exists()` TOCTOU with `try_transaction` under-lock guard; delete `persist_state` |
| `bins/mxnode/src/commands/keys_rename.rs` | key rename | replace lock→save with `transaction` |
| `bins/mxnode/src/commands/import_bash.rs` | bash import | replace lock→save with `transaction` |

**Convention after this plan:** no command calls `store.lock()` + `store.save()`/`save_file()` directly; all `[host]` mutation flows through `transaction`/`try_transaction`. `lock`/`save`/`save_file` remain `pub` (the transaction + `backup` + tests use them) but are no longer the command-level API.

---

## Task 1: Add `transaction` / `try_transaction` to `StateStore`

**Files:**
- Modify: `crates/mxnode-state/src/store.rs` (after `save_file`, ~line 240; `StateError` is ~line 11)
- Modify: `crates/mxnode-state/src/lib.rs` (exports)
- Test: `crates/mxnode-state/src/store.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing tests**

Add to the `store.rs` test module (it already has tests that build a `StateStore` in a tempdir and call `lock`/`save_file`; mirror that setup — look at the existing `fn store_in(dir)` / tempdir helpers and reuse them):

```rust
#[test]
fn transaction_persists_mutation_and_is_visible_on_reload() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::new(dir.path());
    // Seed an install so load() returns Some.
    seed_minimal_install(&store); // helper: writes a MxnodeFile with host.install set (see Step 3 note)

    store
        .transaction(|host| {
            host.nodes.push(test_node(7));
        })
        .unwrap();

    let reloaded = store.load().unwrap().unwrap();
    assert!(reloaded.nodes.iter().any(|n| n.index.get() == 7));
}

#[test]
fn try_transaction_rolls_back_on_closure_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::new(dir.path());
    seed_minimal_install(&store);
    let before = store.load().unwrap().unwrap().nodes.len();

    let res: Result<(), TxError<&str>> = store.try_transaction(|host| {
        host.nodes.push(test_node(9)); // mutate...
        Err("boom") // ...then fail
    });
    assert!(matches!(res, Err(TxError::Body("boom"))));

    // Nothing was written: the on-disk node count is unchanged.
    let after = store.load().unwrap().unwrap().nodes.len();
    assert_eq!(before, after, "closure error must not persist the mutation");
}

#[test]
fn transaction_preserves_operator_sections() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::new(dir.path());
    // Write a full file with an operator section populated + an install.
    let mut file = MxnodeFile::default();
    file.host.install = Some(test_install());
    file.network.github_org = "acme".to_string(); // an operator-owned field
    let guard = store.lock().unwrap();
    store.save_file(&file, &guard).unwrap();
    drop(guard);

    store.transaction(|host| host.nodes.push(test_node(1))).unwrap();

    let reloaded_file = store.load_file().unwrap().unwrap();
    assert_eq!(reloaded_file.network.github_org, "acme", "operator section must survive a host transaction");
    assert_eq!(reloaded_file.host.nodes.len(), 1);
}
```

Provide the small test helpers (`seed_minimal_install`, `test_node`, `test_install`) in the test module using the existing `mxnode_core` constructors the other store tests already use (read the current test module to copy the exact `HostState`/`NodeState`/`InstallSection` construction; do NOT invent fields).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mxnode-state transaction -- --nocapture`
Expected: FAIL to compile — `no method named transaction`, `cannot find type TxError`.

- [ ] **Step 3: Implement `TxError` + the two methods**

Add to `store.rs` (module scope, near `StateError`):

```rust
/// Error from a [`StateStore::try_transaction`]: either the store failed
/// to lock/load/save, or the caller's closure returned an error.
#[derive(Debug)]
pub enum TxError<E> {
    Store(StateError),
    Body(E),
}

impl<E: std::fmt::Display> std::fmt::Display for TxError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxError::Store(e) => write!(f, "{e}"),
            TxError::Body(e) => write!(f, "{e}"),
        }
    }
}
```

Add to `impl StateStore` (after `save_file`):

```rust
    /// Exclusive lock + fresh load + atomic commit, as one unit. The
    /// closure receives the current `[host]` inventory (an empty
    /// `HostState` when the file is absent or host-empty) and mutates it in
    /// place; the whole `MxnodeFile` is then persisted atomically, so
    /// operator sections are preserved. The flock is held for the entire
    /// load→mutate→commit, so a concurrent writer cannot interleave (no
    /// lost updates) and the caller cannot forget to save.
    pub fn transaction<R>(&self, f: impl FnOnce(&mut HostState) -> R) -> Result<R, StateError> {
        let guard = self.lock()?;
        let mut file = self.load_file()?.unwrap_or_default();
        let r = f(&mut file.host);
        self.save_file(&file, &guard)?;
        Ok(r)
    }

    /// Like [`Self::transaction`] but the closure may fail. On `Err(body)`
    /// nothing is written (the lock is released, the file untouched).
    pub fn try_transaction<R, E>(
        &self,
        f: impl FnOnce(&mut HostState) -> Result<R, E>,
    ) -> Result<R, TxError<E>> {
        let guard = self.lock().map_err(TxError::Store)?;
        let mut file = self.load_file().map_err(TxError::Store)?.unwrap_or_default();
        let r = f(&mut file.host).map_err(TxError::Body)?;
        self.save_file(&file, &guard).map_err(TxError::Store)?;
        Ok(r)
    }
```

Export `TxError` from `crates/mxnode-state/src/lib.rs` (add to the existing `pub use store::{…}` line).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mxnode-state 2>&1 | tail -3`
Expected: PASS (the three new tests + all pre-existing store tests).
Run: `cargo clippy -p mxnode-state --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/mxnode-state/src/store.rs crates/mxnode-state/src/lib.rs
git -c user.name="Claude Code" -c user.email=noreply@anthropic.com commit -m "feat(state): transaction API enforcing lock->reload->mutate->commit" -m "transaction()/try_transaction() hold the flock across a fresh load and the atomic save, so the read-modify-write invariant cannot be written incorrectly (no lost updates) and operator sections are preserved."
```

---

## Task 2: Migrate `install --add` to `transaction`

**Files:**
- Modify: `bins/mxnode/src/commands/install_add.rs` (the tail, ~lines 280-310, currently `store.lock()` → `store.load()` → merge → `store.save(&fresh, &guard)`)

**Context:** This site is already the *correct* lock→reload→merge→save (it was hand-fixed). Migrating it to `transaction` proves the API on the simplest real site and removes the hand-rolled guard/merge. Read the current tail first.

- [ ] **Step 1: Replace the inline lock/reload/save with `transaction`**

Replace the block that does `let guard = store.lock()…; let mut fresh = store.load()…; fresh.nodes.extend(…); install_mut.node_count …; store.save(&fresh, &guard)…; let state_path = store.state_path()…` with:

```rust
    let new_nodes = outcome.state.nodes.clone();
    let new_binaries = new_install.binaries.clone();
    store
        .transaction(|host| {
            host.nodes.extend(new_nodes);
            if let Some(install) = host.install.as_mut() {
                install.node_count = install.node_count.saturating_add(count);
                install.binaries = new_binaries;
            }
        })
        .map_err(|e| lock_err(e.to_string(), global))?;
    let state_path = store.state_path().to_path_buf();
```

(`new_install` is the `outcome.state.install` clone the current code already computes — keep that. Drop the now-unused `fresh`/`guard` locals and the `mxnode.toml vanished mid add` branch: with `transaction` the load happens under the lock and an absent file yields an empty `HostState`; if you want to preserve the "no install to extend" guard, the function already checked `install` exists at the top, so a concurrent uninstall is the only way `host.install` is None here — acceptable to let the closure's `if let Some` no-op, OR keep an explicit guard via `try_transaction` returning an error when `host.install` is None. Prefer `try_transaction` with that guard to preserve the original safety:)

```rust
    store
        .try_transaction(|host| -> Result<(), String> {
            if host.install.is_none() {
                return Err("install was removed concurrently; nothing to extend".to_string());
            }
            host.nodes.extend(new_nodes);
            if let Some(install) = host.install.as_mut() {
                install.node_count = install.node_count.saturating_add(count);
                install.binaries = new_binaries;
            }
            Ok(())
        })
        .map_err(|e| lock_err(e.to_string(), global))?;
```

- [ ] **Step 2: Verify**

Run: `cargo test -p mxnode 2>&1 | tail -3` and `cargo check -p mxnode`.
Expected: green; existing `install_add`/cli tests pass.
Run: `grep -n "store.lock()\|store.save(" bins/mxnode/src/commands/install_add.rs` → empty.

- [ ] **Step 3: Commit**

```bash
git add bins/mxnode/src/commands/install_add.rs
git -c user.name="Claude Code" -c user.email=noreply@anthropic.com commit -m "refactor(install-add): persist via StateStore::transaction"
```

---

## Task 3: Migrate the upgrade migration-log writes

**Files:**
- Modify: `bins/mxnode/src/commands/upgrade.rs` (two sites: `persist_migration` ~line 1159 and the proxy-upgrade migration ~line 1398, each currently `let guard = store.lock()…; let mut state = store.load()…; state.migrations.entries.push(entry); …; store.save(&state, &guard)…`)

**Context:** Both sites already lock→reload→push migration entry→save. Read each current block. Migrate to `transaction`. The migration entry + version bump computed *before* the transaction must be moved into the closure (or cloned in).

- [ ] **Step 1: Replace each block with `transaction`**

For `persist_migration` (node upgrade): the closure pushes the `MigrationEntry` and bumps `install.versions.binary_tag`/`config_tag` for the nodes that succeeded:

```rust
    store
        .transaction(|host| {
            host.migrations.entries.push(entry);
            if let Some(install) = host.install.as_mut() {
                // (keep whatever version-bump logic the current code applies on success)
                install.versions.binary_tag = Some(outcome.binary_tag.clone());
                if let Some(cfg) = outcome.config_tag.clone() {
                    install.versions.config_tag = Some(cfg);
                }
            }
        })
        .map_err(|e| /* existing CliError mapping for the lock/save failure */)?;
```

Apply the analogous change to the proxy-upgrade site (~1398), pushing its `MigrationEntry` and bumping `install.versions.proxy_tag`. Preserve the EXACT version-bump conditions the current code uses (read them — do not guess which tags get bumped on partial failure).

- [ ] **Step 2: Verify**

Run: `cargo test -p mxnode upgrade 2>&1 | tail -3`, `cargo check -p mxnode`.
Run: `grep -n "store.lock()\|store.save(" bins/mxnode/src/commands/upgrade.rs` → empty.

- [ ] **Step 3: Commit**

```bash
git add bins/mxnode/src/commands/upgrade.rs
git -c user.name="Claude Code" -c user.email=noreply@anthropic.com commit -m "refactor(upgrade): persist migration log via StateStore::transaction"
```

---

## Task 4: Migrate fresh install + delete `persist_state`

**Files:**
- Modify: `bins/mxnode/src/commands/install.rs` (early `exists()` guard ~line 33; the `persist_state(&runtime.paths, &outcome.state)` call ~line 295)
- Modify: `bins/mxnode/src/orchestrator/install.rs` (delete the `persist_state` fn ~line 820, and the `persist_state` re-export in `install.rs`'s `use` at line 23)

**Context:** Install currently checks `store.exists()`/`host_initialized()` early, runs the long `run_install`, then `persist_state` (which does its own lock→save with NO re-check). Two problems: the early existence check is a TOCTOU vs a concurrent install, and `persist_state` doesn't re-verify under the lock. Replace the final persist with a `try_transaction` whose closure performs the not-installed check **under the lock** and writes the new host.

- [ ] **Step 1: Replace `persist_state` with an under-lock guarded transaction**

```rust
    let new_host = outcome.state; // HostState produced by run_install
    store
        .try_transaction(|host| -> Result<(), String> {
            if host.install.is_some() || !host.nodes.is_empty() {
                return Err("an install already exists on this host (created concurrently); aborting".to_string());
            }
            *host = new_host;
            Ok(())
        })
        .map_err(|e| install_err_from_tx(e, global))?; // map TxError to the existing CliError shape
    let state_path = store.state_path().to_path_buf();
```

Keep the early `host_initialized()`/`exists()` check as a *fast-fail UX* (so the operator gets an immediate error before the long build) — but the transaction's under-lock check is the *authoritative* guard. Map `TxError::Body(msg)`/`TxError::Store(e)` to the existing install `CliError` (reuse `install_err` for the store side; a simple `CliError::new("install conflict", msg, "run `mxnode status`")` for the body side).

Delete `persist_state` from `bins/mxnode/src/orchestrator/install.rs` and remove it from the `use crate::orchestrator::install::{…}` import in `install.rs`. Confirm no other caller of `persist_state` remains (`grep -rn persist_state bins crates`).

- [ ] **Step 2: Verify**

Run: `cargo test -p mxnode 2>&1 | tail -3`, `cargo check -p mxnode`.
Run: `grep -rn "persist_state" bins crates --include=*.rs` → empty.
Run: `grep -n "store.lock()\|store.save(" bins/mxnode/src/commands/install.rs` → empty.

- [ ] **Step 3: Commit**

```bash
git add bins/mxnode/src/commands/install.rs bins/mxnode/src/orchestrator/install.rs
git -c user.name="Claude Code" -c user.email=noreply@anthropic.com commit -m "refactor(install): persist via under-lock try_transaction; remove persist_state" -m "The fresh-install write now re-checks 'not already installed' while holding the lock (closing the exists()->run_install->save TOCTOU) and persists through the transaction API. The redundant persist_state helper is deleted."
```

---

## Task 5: Migrate `keys_rename` + `import_bash`

**Files:**
- Modify: `bins/mxnode/src/commands/keys_rename.rs` (~lines 126-140: `lock()` → … → `save(&state, &guard)`)
- Modify: `bins/mxnode/src/commands/import_bash.rs` (~lines 1150-1170: `lock()` → `save(&plan.state, &guard)`)

**Context:** Read each current block. `keys_rename` mutates node key references in the loaded host; `import_bash` writes a freshly-built host (`plan.state`). Migrate both to `transaction` (move the mutation into the closure). For `import_bash`, the closure assigns `*host = plan.state` (or merges, matching the current semantics — read whether it overwrites or merges and preserve that).

- [ ] **Step 1: `keys_rename`** — replace the lock/load(if any)/mutate/save with `transaction(|host| { /* the rename mutation currently applied to `state` */ })`.

- [ ] **Step 2: `import_bash`** — replace with `transaction(|host| { *host = plan.state.clone(); })` (or the current merge semantics). `plan.state` is the `HostState` import-bash built.

- [ ] **Step 3: Verify**

Run: `cargo test -p mxnode 2>&1 | tail -3`, `cargo check -p mxnode`.
Run: `grep -n "store.lock()\|store.save(" bins/mxnode/src/commands/keys_rename.rs bins/mxnode/src/commands/import_bash.rs` → empty.

- [ ] **Step 4: Commit**

```bash
git add bins/mxnode/src/commands/keys_rename.rs bins/mxnode/src/commands/import_bash.rs
git -c user.name="Claude Code" -c user.email=noreply@anthropic.com commit -m "refactor(keys-rename,import-bash): persist via StateStore::transaction"
```

---

## Task 6: Verify the invariant + full workspace gate

- [ ] **Step 1: No command hand-rolls lock+save anymore**

Run:
```bash
grep -rn "\.lock()" bins/mxnode/src/commands bins/mxnode/src/orchestrator --include=*.rs | grep -iv "stdin\|stdout\|stderr\|version_info\|self\.map\|inflight" 
grep -rn "store.save(\|store.save_file(\|persist_state" bins/mxnode/src --include=*.rs
```
Expected: the first returns nothing that is a `StateStore` lock (only stdio/inflight/in-memory locks remain); the second returns nothing. If a real `StateStore` write site remains, migrate it (or document why it's exempt, e.g. `update_gate`'s operator-section-only write which uses `save_file` for the update cache — if it writes ONLY operator sections and never `[host]`, it may legitimately stay on `lock`+`save_file`; note it explicitly).

- [ ] **Step 2: Full workspace verification**

Run:
```bash
cargo check --workspace --all-targets
cargo test -p mxnode-state -p mxnode
cargo clippy -p mxnode-state -p mxnode --all-targets -- -D warnings
```
Expected: check clean; tests green (the 3 new state tests + all command tests); clippy clean for the touched crates. (Pre-existing `doctor.rs`/`benchmark.rs` clippy debt and the `loader` default-equality test are out of scope; confirm any clippy error is NOT in a file this plan touched.)

- [ ] **Step 3: Commit any residual cleanup**

If Step 1 surfaced a stray site or a dead import, fix + commit:
```bash
git add -A
git -c user.name="Claude Code" -c user.email=noreply@anthropic.com commit -m "chore(state): finish transaction migration; verify no hand-rolled lock+save remains"
```

---

## Self-Review

**Spec coverage:** transaction API + tests (Task 1); all 5 hand-rolled `[host]` writers migrated — install_add (T2), upgrade ×2 (T3), install + persist_state removal (T4), keys_rename + import_bash (T5); invariant verified by grep (T6). ✓

**Placeholder scan:** the per-site closures specify the exact mutation; the only "read the current code" directives are for *preserving existing semantics* of complex sites (upgrade version-bump conditions, import_bash overwrite-vs-merge), which is correct — the migration must not change behavior, only the persistence mechanism.

**Type consistency:** `transaction<R>(FnOnce(&mut HostState) -> R) -> Result<R, StateError>` and `try_transaction<R, E>(FnOnce(&mut HostState) -> Result<R, E>) -> Result<R, TxError<E>>` are used identically in every task. `TxError::{Store,Body}` is the same in Task 1's definition and every `.map_err` site.

**Behavior-preservation note:** every migrated site previously did lock→(reload)→mutate→save; the transaction does lock→reload→mutate→save with the reload now *guaranteed* under the lock. For `install_add`, `upgrade` (which already reloaded under the lock) this is identical; for `install` it strictly *adds* an under-lock guard (closing a TOCTOU). No site loses durability (save_file's fsync + atomic rename + 0600 is unchanged).
