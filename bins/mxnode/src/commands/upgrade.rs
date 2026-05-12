//! `mxnode upgrade [--config-tag T --binary-tag T --proxy-tag T]
//!  [--strategy rolling|parallel] [--max-parallel N] [--select <expr>]
//!  [--skip-validators] [--dry-run]`
//!
//! Upgrade nodes to a new tag set. To downgrade, pass any
//! `--binary-tag T` already in the binstore — the acquirer reuses the
//! cached binary instead of re-acquiring.
//!
//! Crash recovery: a stale `inflight.toml` (recorded pid is dead) is
//! auto-cleared at the next invocation; the operator just reruns the
//! upgrade. There is no `--resume` flag — re-running the upgrade will
//! redo every per-node step, all of which are idempotent
//! (`systemctl stop` on a stopped unit, symlink swap to the same
//! target, etc.).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mxnode_core::{
    HostState, MigrationEntry, MigrationResult, NodeIndex, NodeState, Role, Tag,
};
use mxnode_rpc::NodeClient;
use mxnode_state::{
    inflight_path, swap_symlink, BinStore, Inflight, InflightCheck, InflightStep, OpKind,
    ProcessIdentity, StateStore,
};
use mxnode_systemd::Ctl; // trait used by upgrade_one_node param
use serde::Serialize;

use crate::cli::{GlobalArgs, Strategy, UpgradeArgs, UpgradeTarget};
use crate::errors::CliError;
use crate::events::{node_op_end, node_op_start, Outcome};
use crate::orchestrator::acquirer::{Artifact, BinaryAcquirer, SourceBuildAcquirer};
use crate::orchestrator::install::{
    apply_node_tomledit, copy_dir_recursive, copy_executable, install_seednode_configs,
    ConfigEdits, NodeTomlEdit,
};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

#[tokio::main(flavor = "current_thread")]
pub async fn run(mut args: UpgradeArgs, global: &GlobalArgs) -> Result<(), CliError> {
    if let Some(UpgradeTarget::Proxy { proxy_tag }) = &args.target {
        return upgrade_proxy(proxy_tag.clone(), &args, global).await;
    }
    // `mxnode upgrade squad` flattens its own --binary-tag / --config-tag /
    // --proxy-tag onto the parent UpgradeArgs so the rest of the pipeline
    // (build_plan, execute_upgrade) only sees one set of fields. When
    // both the parent and the subcommand specify the same tag the
    // subcommand wins — operators reach for the explicit form last.
    let is_squad = if let Some(UpgradeTarget::Squad {
        binary_tag,
        config_tag,
        proxy_tag,
    }) = &args.target
    {
        if binary_tag.is_some() {
            args.binary_tag = binary_tag.clone();
        }
        if config_tag.is_some() {
            args.config_tag = config_tag.clone();
        }
        if proxy_tag.is_some() {
            args.proxy_tag = proxy_tag.clone();
        }
        true
    } else {
        false
    };

    let runtime = Runtime::from_global(global)?;

    // Self-healing inflight.toml: only a *live* peer mxnode upgrade is
    // a real conflict. A `Stale` (recorded pid is dead) or
    // `Indeterminate` (pid liveness can't be determined) lock is
    // garbage from a previous crash; auto-clear it and proceed instead
    // of asking the operator to run a manual unlock command. The audit
    // log entry for the previous run was already written by whichever
    // step failed — we don't try to second-guess what the dead process
    // would have done next.
    let inflight_loc = inflight_path(&runtime.paths.state);
    let identity = ProcessIdentity::current();
    let check = InflightCheck::from_path(&inflight_loc, identity).map_err(|e| {
        CliError::new(
            "failed to read inflight.toml",
            e.to_string(),
            "the file is corrupt; remove it manually then retry",
        )
        .json_if(global.json)
    })?;

    match check {
        InflightCheck::Live { other_pid, .. } => {
            return Err(CliError::new(
                format!("another mxnode upgrade is running (pid {other_pid})"),
                "inflight.toml records a live process",
                "wait for that invocation to finish, or kill it before retrying",
            )
            .json_if(global.json));
        }
        InflightCheck::StaleFromDeadProcess(prev) | InflightCheck::Indeterminate(prev) => {
            eprintln!(
                "→ clearing stale inflight.toml from previous run (pid {} step {:?}); proceeding",
                prev.identity.pid, prev.current_step,
            );
            let _ = Inflight::clear(&inflight_loc);
        }
        InflightCheck::Clear => {}
    }

    let store = StateStore::new(&runtime.paths.config_dir);
    let state = store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read mxnode.toml",
                e.to_string(),
                "run `mxnode install` first",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no mxnode.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode install` first",
            )
            .json_if(global.json)
        })?;

    let mut plan = build_plan(&args, &state, &runtime, global).await?;
    plan.is_squad = is_squad;

    if args.dry_run {
        return emit_dry_run(&plan, global);
    }

    // Persist the inflight.toml as a host-wide upgrade lock. Every
    // step inside `execute_upgrade` updates the `current_step` field
    // so a crash leaves a "where did we die" breadcrumb behind for
    // post-mortem inspection. The next mxnode invocation auto-clears
    // the file if our pid has gone away.
    let mut inflight = Inflight::new(
        OpKind::Upgrade,
        strategy_label(args.strategy),
        plan.selected.clone(),
    );
    inflight.target_binary_tag = Some(plan.binary_tag.clone());
    inflight.target_config_tag = plan.config_tag.clone();
    inflight.target_proxy_tag = plan.proxy_tag.clone();
    inflight.save(&inflight_loc).map_err(|e| {
        CliError::new(
            "failed to write inflight.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;

    let outcome = execute_upgrade(
        &plan,
        &state,
        &runtime,
        &inflight_loc,
        &mut inflight,
        global,
    )
    .await;

    // Always clear the inflight on a clean run; `execute_upgrade` returns
    // the migration entry plus per-node results regardless of partial
    // failure status.

    let started_after_swap = plan.start;
    persist_migration(
        &store,
        outcome,
        &inflight_loc,
        runtime.loaded.file.install.binary_keep as usize,
        started_after_swap,
        global,
    )
}

struct Plan {
    selected: Vec<NodeIndex>,
    binary_tag: Tag,
    config_tag: Option<Tag>,
    proxy_tag: Option<Tag>,
    /// Pre-cloned config repo path. `build_plan` clones the repo eagerly
    /// so it can read `binaryVersion` / `proxyVersion` while resolving
    /// the other two tags. `execute_upgrade` reuses the same path.
    config_repo_path: Option<std::path::PathBuf>,
    strategy: Strategy,
    skip_validators: bool,
    /// Start each node after the binary + config swap. Off by default
    /// — operators run `mxnode start --all` once they're satisfied.
    start: bool,
    /// Apply observer-squad-specific tomledit edits during ConfigApplied.
    /// Set when the operator runs `mxnode upgrade squad`.
    is_squad: bool,
}

async fn build_plan(
    args: &UpgradeArgs,
    state: &HostState,
    runtime: &Runtime,
    global: &GlobalArgs,
) -> Result<Plan, CliError> {
    use crate::orchestrator::config_repo::{acquire_config_repo, read_proxy_version_from_repo};
    use crate::orchestrator::tag_resolver::{
        resolve_binary_tag, resolve_config_tag, resolve_proxy_tag,
    };

    // Step 1 — resolve the config tag. Unlike the previous "leave
    // untouched unless asked" semantics, `mxnode upgrade` now always
    // refreshes the config repo: CLI > [overrides].configver >
    // GitHub-latest. This mirrors the bash `upgrade` function, which
    // treats the operator's choice of `CONFIGVER` as the entry point
    // and derives every other version from it.
    let environment = state.install.as_ref().map(|i| i.environment);
    let env = environment.ok_or_else(|| {
        CliError::new(
            "cannot resolve config tag without an environment",
            "state.install.environment is missing",
            "run `mxnode install` first, or pass --config-tag explicitly",
        )
        .json_if(global.json)
    })?;
    let resolved_config = resolve_config_tag(runtime, env, args.config_tag.as_deref())
        .await
        .map_err(|e| crate::commands::install::resolve_err(e, global))?;
    crate::commands::install::announce_resolved(global, "config", &resolved_config);
    let config_tag = resolved_config.tag;

    // Step 2 — clone the config repo eagerly so we can read
    // `binaryVersion` / `proxyVersion` from it (bash:`git_clone`).
    // Cached per-(env, tag) under `<binaries>/config-repos/<env>/<tag>`
    // — `execute_upgrade` reuses the same path without re-cloning.
    //
    // Dry-run + explicit `--binary-tag` shortcut: the operator already
    // overrode the config-driven resolution, so there's nothing to
    // learn from cloning the repo. Skip it so `--dry-run` stays a
    // zero-network preview. The actual `execute_upgrade` path always
    // clones, since the per-node config-copy step needs the repo.
    let skip_repo_clone = args.dry_run && args.binary_tag.is_some();
    let config_repo_path = if skip_repo_clone {
        None
    } else {
        let path = acquire_config_repo(
            &runtime.paths.binaries,
            &runtime.loaded.file.network.github_org,
            env,
            &config_tag,
        )
        .await
        .map_err(|e| {
            CliError::new(
                "failed to acquire config repo for upgrade",
                e.to_string(),
                "check network connectivity to github.com or pre-seed `<binaries>/config-repos/<env>/<tag>`",
            )
            .json_if(global.json)
        })?;
        Some(path)
    };

    // Step 3 — resolve the binary tag. Chain: CLI > [overrides].binaryver
    // > config repo's `binaryVersion` file > GitHub-latest of mx-chain-go.
    // The config-repo fallback is what makes `mxnode upgrade` with zero
    // flags do the same thing as bash: the config release we just
    // cloned declares which node tag it pairs with.
    let binary_tag = resolve_binary_tag_via_config(
        runtime,
        args.binary_tag.as_deref(),
        config_repo_path.as_deref(),
        global,
    )
    .await?;

    // Step 4 — resolve the proxy tag the same way (config repo's
    // `proxyVersion` as a fallback). Only emitted when the upgrade
    // target includes the proxy: bare `mxnode upgrade` doesn't touch
    // the proxy unit unless the operator passes --proxy-tag or sets
    // [overrides].proxyver. The proxy.run() flow uses its own resolver.
    let proxy_tag = if args.proxy_tag.is_some()
        || runtime.loaded.file.overrides.proxyver().is_some()
    {
        Some(
            resolve_proxy_tag_via_config(
                runtime,
                args.proxy_tag.as_deref(),
                config_repo_path.as_deref(),
                global,
            )
            .await?,
        )
    } else {
        // Hint a paired proxy tag without committing to upgrading
        // the proxy — populated only when the config repo declared one,
        // so the migration log entry can record what the config
        // recommended even when the operator skipped the proxy bump.
        config_repo_path
            .as_deref()
            .and_then(read_proxy_version_from_repo)
            .and_then(|raw| Tag::parse(&raw).ok())
    };

    // Local helper closures (`async fn` items inside `build_plan` would
    // need their own desugaring boilerplate). They keep the resolution
    // chains readable while routing the same `tag_resolver` errors as
    // before. `config_repo_path` is `None` only on the dry-run shortcut
    // where the operator explicitly passed `--binary-tag`; the
    // config-repo-derived fallback is unreachable in that case.
    async fn resolve_binary_tag_via_config(
        runtime: &Runtime,
        cli_value: Option<&str>,
        config_repo_path: Option<&std::path::Path>,
        global: &GlobalArgs,
    ) -> Result<Tag, CliError> {
        use crate::orchestrator::config_repo::read_binary_version_from_repo;
        if let Some(raw) = cli_value {
            let resolved = resolve_binary_tag(runtime, Some(raw))
                .await
                .map_err(|e| crate::commands::install::resolve_err(e, global))?;
            crate::commands::install::announce_resolved(global, "binary", &resolved);
            return Ok(resolved.tag);
        }
        if let Some(raw) = runtime.loaded.file.overrides.binaryver() {
            let resolved = resolve_binary_tag(runtime, Some(raw))
                .await
                .map_err(|e| crate::commands::install::resolve_err(e, global))?;
            crate::commands::install::announce_resolved(global, "binary", &resolved);
            return Ok(resolved.tag);
        }
        if let Some(repo) = config_repo_path {
            if let Some(raw) = read_binary_version_from_repo(repo) {
                let resolved = resolve_binary_tag(runtime, Some(&raw))
                    .await
                    .map_err(|e| crate::commands::install::resolve_err(e, global))?;
                announce_via_config(global, "binary", &resolved.tag);
                return Ok(resolved.tag);
            }
        }
        let resolved = resolve_binary_tag(runtime, None)
            .await
            .map_err(|e| crate::commands::install::resolve_err(e, global))?;
        crate::commands::install::announce_resolved(global, "binary", &resolved);
        Ok(resolved.tag)
    }

    async fn resolve_proxy_tag_via_config(
        runtime: &Runtime,
        cli_value: Option<&str>,
        config_repo_path: Option<&std::path::Path>,
        global: &GlobalArgs,
    ) -> Result<Tag, CliError> {
        use crate::orchestrator::config_repo::read_proxy_version_from_repo;
        if let Some(raw) = cli_value {
            let resolved = resolve_proxy_tag(runtime, Some(raw))
                .await
                .map_err(|e| crate::commands::install::resolve_err(e, global))?;
            crate::commands::install::announce_resolved(global, "proxy", &resolved);
            return Ok(resolved.tag);
        }
        if let Some(raw) = runtime.loaded.file.overrides.proxyver() {
            let resolved = resolve_proxy_tag(runtime, Some(raw))
                .await
                .map_err(|e| crate::commands::install::resolve_err(e, global))?;
            crate::commands::install::announce_resolved(global, "proxy", &resolved);
            return Ok(resolved.tag);
        }
        if let Some(repo) = config_repo_path {
            if let Some(raw) = read_proxy_version_from_repo(repo) {
                let resolved = resolve_proxy_tag(runtime, Some(&raw))
                    .await
                    .map_err(|e| crate::commands::install::resolve_err(e, global))?;
                announce_via_config(global, "proxy", &resolved.tag);
                return Ok(resolved.tag);
            }
        }
        let resolved = resolve_proxy_tag(runtime, None)
            .await
            .map_err(|e| crate::commands::install::resolve_err(e, global))?;
        crate::commands::install::announce_resolved(global, "proxy", &resolved);
        Ok(resolved.tag)
    }

    fn announce_via_config(global: &GlobalArgs, kind: &str, tag: &Tag) {
        if global.json || global.quiet {
            return;
        }
        eprintln!("  → {kind} {tag} (from config repo)");
    }

    // Selection: route every form through the same resolver as the
    // lifecycle commands so behaviour stays consistent. Default with
    // no selector is "every node". `--select`, `--node`, and `--shard`
    // are mutually exclusive (clap enforces).
    let selected: Vec<NodeIndex> =
        if args.select.is_some() || !args.node.is_empty() || args.shard.is_some() {
            use crate::orchestrator::selector::resolve;
            let lifecycle_args = crate::cli::LifecycleArgs {
                all: false,
                select: args.select.clone(),
                validators_only: false,
                observers_only: false,
                shard: args.shard.clone(),
                node: args.node.clone(),
            };
            resolve(state, &lifecycle_args).map_err(|e| {
                CliError::new(
                    "selector did not resolve",
                    e.to_string(),
                    "see `mxnode status` for valid indices and shards",
                )
                .json_if(global.json)
            })?
        } else {
            let mut v: Vec<NodeIndex> = state.nodes.iter().map(|n| n.index).collect();
            v.sort();
            v
        };

    if selected.is_empty() {
        return Err(CliError::new(
            "selector matched zero nodes",
            "no nodes to upgrade",
            "run `mxnode status` to see what's installed",
        )
        .json_if(global.json));
    }

    Ok(Plan {
        selected,
        binary_tag,
        config_tag: Some(config_tag),
        proxy_tag,
        config_repo_path,
        strategy: args.strategy,
        skip_validators: args.skip_validators,
        start: args.start,
        is_squad: false,
    })
}

fn strategy_label(s: Strategy) -> String {
    match s {
        Strategy::Rolling => "rolling".to_string(),
        Strategy::Parallel => "parallel".to_string(),
    }
}

fn emit_dry_run(plan: &Plan, global: &GlobalArgs) -> Result<(), CliError> {
    if global.json {
        let payload = serde_json::json!({
            "mode": "dry-run",
            "binary_tag": plan.binary_tag.to_string(),
            "config_tag": plan.config_tag.as_ref().map(|t| t.to_string()),
            "proxy_tag": plan.proxy_tag.as_ref().map(|t| t.to_string()),
            "strategy": strategy_label(plan.strategy),
            "skip_validators": plan.skip_validators,
            "start_after_swap": plan.start,
            "selected": plan.selected.iter().map(|i| i.get()).collect::<Vec<_>>(),
        });
        println!("{payload}");
    } else {
        println!("dry-run upgrade plan:");
        println!("  binary_tag: {}", plan.binary_tag);
        if let Some(t) = &plan.config_tag {
            println!("  config_tag: {t}");
        }
        if let Some(t) = &plan.proxy_tag {
            println!("  proxy_tag:  {t}");
        }
        println!("  strategy:   {}", strategy_label(plan.strategy));
        println!(
            "  selected:   {:?}",
            plan.selected.iter().map(|i| i.get()).collect::<Vec<_>>()
        );
        if plan.skip_validators {
            println!("  skip_validators: true");
        }
        if plan.start {
            println!("  start_after_swap: true (rolling restart + readiness probe)");
        } else {
            println!("  start_after_swap: false (nodes left stopped; run `mxnode start --all`)");
        }
    }
    Ok(())
}

struct UpgradeOutcome {
    binary_tag: Tag,
    config_tag: Option<Tag>,
    started_at: time::OffsetDateTime,
    duration_secs: u64,
    nodes_done: Vec<NodeIndex>,
    nodes_failed: Vec<NodeIndex>,
    rolled_back: bool,
    per_node: Vec<NodeResult>,
}

#[derive(Debug, Clone, Serialize)]
struct NodeResult {
    index: u16,
    unit: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn execute_upgrade(
    plan: &Plan,
    state: &HostState,
    runtime: &Runtime,
    inflight_loc: &Path,
    inflight: &mut Inflight,
    _global: &GlobalArgs,
) -> UpgradeOutcome {
    let started = time::OffsetDateTime::now_utc();
    let bin_store = BinStore::new(runtime.paths.binaries.clone());
    // Workdir for source-build clones; lives alongside the binary store
    // so an operator inspecting `<custom_home>/mxnode/build/` sees what
    // we cloned and built last.
    let build_workdir = runtime.paths.custom_home.join("mxnode/build");
    let acquirer: Arc<dyn BinaryAcquirer> = Arc::new(SourceBuildAcquirer::new(
        runtime.loaded.file.network.github_org.clone(),
        build_workdir,
    ));
    let ctl = crate::orchestrator::supervisor::build_supervisor();

    // Acquire the binary once; reuse for every node.
    let acquired_path = match acquirer.acquire(Artifact::Node, &plan.binary_tag).await {
        Ok(p) => p,
        Err(e) => {
            return failure_outcome(plan, started, format!("acquire: {e}"));
        }
    };

    let installed_path =
        match bin_store.install_binary("node", plan.binary_tag.as_str(), &acquired_path) {
            Ok(p) => p,
            Err(e) => {
                return failure_outcome(plan, started, format!("install_binary: {e}"));
            }
        };

    // `build_plan` cloned the config repo eagerly so it could read
    // `binaryVersion` / `proxyVersion`. Reuse the same cached path
    // here — `acquire_config_repo` is idempotent, but skipping the
    // round-trip keeps the audit log linear.
    let config_repo = plan.config_repo_path.clone();

    if let Err(e) = refresh_upgrade_utilities(
        &acquirer,
        &bin_store,
        runtime,
        &plan.binary_tag,
        config_repo.as_deref(),
    )
    .await
    {
        return failure_outcome(plan, started, e);
    }

    let mut nodes_done: Vec<NodeIndex> = Vec::new();
    let mut nodes_failed: Vec<NodeIndex> = Vec::new();
    let mut per_node: Vec<NodeResult> = Vec::new();

    for idx in &plan.selected {
        let Some(node) = state.nodes.iter().find(|n| n.index == *idx) else {
            nodes_failed.push(*idx);
            per_node.push(NodeResult {
                index: idx.get(),
                unit: format!("elrond-node-{}.service", idx.get()),
                ok: false,
                error: Some("node not found in state".to_string()),
            });
            continue;
        };
        inflight.current = Some(node.index);
        inflight.current_step = InflightStep::Resolving;
        let _ = inflight.save(inflight_loc);

        let node_ctx = UpgradeNodeContext {
            ctl: &ctl,
            state,
            installed_binary: &installed_path,
            inflight_loc,
            is_squad: plan.is_squad,
            config_repo: config_repo.as_deref(),
            runtime,
            start_after_swap: plan.start,
        };

        match upgrade_one_node(node_ctx, node, inflight).await {
            Ok(()) => {
                nodes_done.push(node.index);
                per_node.push(NodeResult {
                    index: node.index.get(),
                    unit: node.unit.clone(),
                    ok: true,
                    error: None,
                });
                inflight.completed.push(node.index);
                let _ = inflight.save(inflight_loc);
            }
            Err(e) => {
                nodes_failed.push(node.index);
                per_node.push(NodeResult {
                    index: node.index.get(),
                    unit: node.unit.clone(),
                    ok: false,
                    error: Some(e),
                });
                // First-failure stops the rolling sequence per plan §"Upgrade flow".
                break;
            }
        }
    }

    let duration_secs = (time::OffsetDateTime::now_utc() - started)
        .whole_seconds()
        .max(0) as u64;

    UpgradeOutcome {
        binary_tag: plan.binary_tag.clone(),
        config_tag: plan.config_tag.clone(),
        started_at: started,
        duration_secs,
        nodes_done,
        nodes_failed,
        rolled_back: false,
        per_node,
    }
}

fn failure_outcome(plan: &Plan, started: time::OffsetDateTime, error: String) -> UpgradeOutcome {
    UpgradeOutcome {
        binary_tag: plan.binary_tag.clone(),
        config_tag: plan.config_tag.clone(),
        started_at: started,
        duration_secs: 0,
        nodes_done: Vec::new(),
        nodes_failed: plan.selected.clone(),
        rolled_back: false,
        per_node: plan
            .selected
            .iter()
            .map(|i| NodeResult {
                index: i.get(),
                unit: format!("elrond-node-{}.service", i.get()),
                ok: false,
                error: Some(error.clone()),
            })
            .collect(),
    }
}

async fn refresh_upgrade_utilities(
    acquirer: &Arc<dyn BinaryAcquirer>,
    bin_store: &BinStore,
    runtime: &Runtime,
    binary_tag: &Tag,
    config_repo: Option<&Path>,
) -> Result<(), String> {
    let utils_dir = runtime.paths.elrond_utils_root();
    std::fs::create_dir_all(&utils_dir)
        .map_err(|e| format!("create {}: {e}", utils_dir.display()))?;

    let keygen_src = acquirer
        .acquire(Artifact::Keygenerator, binary_tag)
        .await
        .map_err(|e| format!("keygenerator binary: {e}"))?;
    let keygen_installed = bin_store
        .install_binary("keygenerator", binary_tag.as_str(), &keygen_src)
        .map_err(|e| format!("install_binary keygenerator: {e}"))?;
    copy_executable(&keygen_installed, &utils_dir.join("keygenerator"))
        .map_err(|e| format!("copy keygenerator: {e}"))?;

    let seednode_src = acquirer
        .acquire(Artifact::Seednode, binary_tag)
        .await
        .map_err(|e| format!("seednode binary: {e}"))?;
    let seednode_installed = bin_store
        .install_binary("seednode", binary_tag.as_str(), &seednode_src)
        .map_err(|e| format!("install_binary seednode: {e}"))?;
    let seednode_dir = utils_dir.join("seednode");
    std::fs::create_dir_all(seednode_dir.join("config"))
        .map_err(|e| format!("create {}: {e}", seednode_dir.display()))?;
    copy_executable(&seednode_installed, &seednode_dir.join("seednode"))
        .map_err(|e| format!("copy seednode: {e}"))?;
    if let Some(config_repo) = config_repo {
        install_seednode_configs(config_repo, &seednode_dir)
            .map_err(|e| format!("seednode configs: {e}"))?;
    }
    Ok(())
}

struct UpgradeNodeContext<'a> {
    ctl: &'a Arc<dyn Ctl>,
    state: &'a HostState,
    installed_binary: &'a Path,
    inflight_loc: &'a Path,
    is_squad: bool,
    config_repo: Option<&'a Path>,
    runtime: &'a Runtime,
    /// When false (default), `upgrade_one_node` stops + swaps and
    /// leaves the unit stopped. Operators run `mxnode start --all`
    /// afterwards. Mirrors the bash `upgrade` flow.
    start_after_swap: bool,
}

async fn upgrade_one_node(
    ctx: UpgradeNodeContext<'_>,
    node: &NodeState,
    inflight: &mut Inflight,
) -> Result<(), String> {
    use InflightStep::*;
    node_op_start("upgrade", node.index, &node.unit);

    // Each step writes its label into inflight.toml before running so a
    // crashed run leaves a "where did we die" breadcrumb on disk for
    // post-mortem `cat inflight.toml`. Steps are idempotent (e.g.
    // `systemctl stop` on an already-stopped unit, symlink swap to the
    // same target).

    inflight.current_step = Stopped;
    let _ = inflight.save(ctx.inflight_loc);
    if let Err(e) = ctx.ctl.stop(&node.unit).await {
        let cause = e.to_string();
        node_op_end(
            "upgrade",
            node.index,
            &node.unit,
            Outcome::Fail { cause: &cause },
        );
        return Err(format!("systemctl stop failed: {cause}"));
    }

    inflight.current_step = ConfigApplied;
    let _ = inflight.save(ctx.inflight_loc);
    if let Some(config_repo) = ctx.config_repo {
        apply_upstream_config_update(node, config_repo, ctx.runtime, ctx.is_squad).map_err(
            |e| {
                let cause = format!("config update: {e}");
                node_op_end(
                    "upgrade",
                    node.index,
                    &node.unit,
                    Outcome::Fail { cause: &cause },
                );
                cause
            },
        )?;
    } else if ctx.is_squad {
        apply_squad_config_edits(node).map_err(|e| {
            let cause = format!("squad config edits: {e}");
            node_op_end(
                "upgrade",
                node.index,
                &node.unit,
                Outcome::Fail { cause: &cause },
            );
            cause
        })?;
    }

    inflight.current_step = BinaryReplaced;
    let _ = inflight.save(ctx.inflight_loc);
    let symlink = node.workdir.join("node");
    if let Err(e) = swap_symlink(&symlink, ctx.installed_binary) {
        let cause = e.to_string();
        node_op_end(
            "upgrade",
            node.index,
            &node.unit,
            Outcome::Fail { cause: &cause },
        );
        return Err(format!("symlink swap failed: {cause}"));
    }

    // The bash `upgrade` flow stops every node, swaps binary + config,
    // and leaves the units stopped — the operator decides when to bring
    // consensus back. mxnode mirrors that by default; `--start` opts
    // back into the rolling restart + readiness probe.
    if ctx.start_after_swap {
        inflight.current_step = Started;
        let _ = inflight.save(ctx.inflight_loc);
        if let Err(e) = ctx.ctl.start(&node.unit).await {
            let cause = e.to_string();
            node_op_end(
                "upgrade",
                node.index,
                &node.unit,
                Outcome::Fail { cause: &cause },
            );
            return Err(format!("systemctl start failed: {cause}"));
        }

        inflight.current_step = NonceVerified;
        let _ = inflight.save(ctx.inflight_loc);
        // Readiness probe: wait for the node's nonce to be within K of
        // the highest known network nonce among siblings, OR for the
        // node to report `erd_is_syncing == 0` with a non-zero nonce.
        // Single-node hosts skip the cross-sibling comparison.
        if let Err(e) = wait_for_node_ready(ctx.state, node).await {
            let cause = format!("readiness probe: {e}");
            node_op_end(
                "upgrade",
                node.index,
                &node.unit,
                Outcome::Fail { cause: &cause },
            );
            return Err(cause);
        }
    }

    node_op_end("upgrade", node.index, &node.unit, Outcome::Ok);
    Ok(())
}

/// Copy a target upstream config repo into one node workdir while preserving
/// the operator-owned preferences file, then reapply mxnode's typed edits and
/// override maps. This mirrors the Bash upgrade flow (`cp config/*` followed
/// by restoring `prefs.toml`) without losing Rust-only config semantics.
fn apply_upstream_config_update(
    node: &NodeState,
    config_repo: &Path,
    runtime: &Runtime,
    is_squad: bool,
) -> Result<(), String> {
    let config_dir = node.workdir.join("config");
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("create {}: {e}", config_dir.display()))?;
    let prefs_path = config_dir.join("prefs.toml");
    let preserved_prefs = match std::fs::read_to_string(&prefs_path) {
        Ok(body) => Some(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("read {}: {e}", prefs_path.display())),
    };

    copy_dir_recursive(config_repo, &config_dir).map_err(|e| format!("copy config repo: {e}"))?;
    if let Some(body) = preserved_prefs {
        std::fs::write(&prefs_path, body)
            .map_err(|e| format!("restore {}: {e}", prefs_path.display()))?;
    }

    let edits = if is_squad || matches!(node.role, Role::Observer | Role::Multikey) {
        ConfigEdits::Observer
    } else {
        ConfigEdits::Validator
    };
    apply_node_tomledit(NodeTomlEdit {
        workdir: &node.workdir,
        display_name: &node.display_name,
        shard: node.shard,
        edits,
        role: node.role,
        redundancy_level: None,
        prefs_overrides: &runtime.loaded.file.overrides.prefs,
        config_overrides: &runtime.loaded.file.overrides.config,
    })
    .map_err(|e| e.to_string())
}

/// Apply observer-squad-specific TOML edits to the node's config dir.
/// Mirrors the bash `observers()` flow: enable `[DbLookupExtensions]` in
/// `mxnode.toml` and pin `DestinationShardAsObserver` in `prefs.toml`.
fn apply_squad_config_edits(node: &NodeState) -> Result<(), String> {
    use mxnode_systemd::{enable_db_lookup_extensions, set_destination_shard};
    use toml_edit::DocumentMut;

    let config_path = node.workdir.join("config/mxnode.toml");
    if config_path.exists() {
        let body = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("read {}: {e}", config_path.display()))?;
        let mut doc: DocumentMut = body
            .parse()
            .map_err(|e| format!("parse {}: {e}", config_path.display()))?;
        enable_db_lookup_extensions(&mut doc).map_err(|e| e.to_string())?;
        std::fs::write(&config_path, doc.to_string())
            .map_err(|e| format!("write {}: {e}", config_path.display()))?;
    }

    let prefs_path = node.workdir.join("config/prefs.toml");
    if prefs_path.exists() {
        let body = std::fs::read_to_string(&prefs_path)
            .map_err(|e| format!("read {}: {e}", prefs_path.display()))?;
        let mut doc: DocumentMut = body
            .parse()
            .map_err(|e| format!("parse {}: {e}", prefs_path.display()))?;
        set_destination_shard(&mut doc, node.shard).map_err(|e| e.to_string())?;
        std::fs::write(&prefs_path, doc.to_string())
            .map_err(|e| format!("write {}: {e}", prefs_path.display()))?;
    }
    Ok(())
}

/// Probe the node's local REST API until either:
///   - `erd_nonce` is within `K` of the highest sibling's
///     `erd_probable_highest_nonce`, OR
///   - `erd_is_syncing == 0` and `erd_nonce > 0`, OR
///   - the timeout expires (returns Err)
const NONCE_LAG_TOLERANCE: u64 = 5;
const NONCE_PROBE_TIMEOUT_SECS: u64 = 5 * 60;
const NONCE_POLL_INTERVAL_SECS: u64 = 3;

async fn wait_for_node_ready(state: &HostState, node: &NodeState) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(NONCE_PROBE_TIMEOUT_SECS);
    let target =
        NodeClient::new("127.0.0.1", node.api_port).map_err(|e| format!("rpc client init: {e}"))?;

    loop {
        let now = std::time::Instant::now();
        if now > deadline {
            return Err(format!(
                "node-{} did not reach ready state within {NONCE_PROBE_TIMEOUT_SECS}s",
                node.index.get(),
            ));
        }

        let target_status =
            match tokio::time::timeout(Duration::from_secs(2), target.status()).await {
                Ok(Ok(s)) => Some(s),
                _ => None,
            };

        if let Some(status) = target_status {
            let nonce = status.data.metrics.erd_nonce.unwrap_or(0);
            let is_syncing = status.data.metrics.erd_is_syncing.unwrap_or(1);
            if is_syncing == 0 && nonce > 0 {
                let network = highest_sibling_nonce(state, node).await;
                match network {
                    Some(net) if nonce + NONCE_LAG_TOLERANCE >= net => return Ok(()),
                    Some(net) => {
                        tracing::debug!(
                            target: "mxnode.event",
                            event = "upgrade.lag",
                            node = node.index.get(),
                            nonce,
                            network = net,
                        );
                    }
                    None => return Ok(()),
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(NONCE_POLL_INTERVAL_SECS)).await;
    }
}

/// Highest `erd_probable_highest_nonce` reported by any sibling node
/// (excluding `me`). Returns `None` when no sibling responds — the
/// caller treats that as "no comparison available, accept current state".
async fn highest_sibling_nonce(state: &HostState, me: &NodeState) -> Option<u64> {
    let mut highest: Option<u64> = None;
    for node in &state.nodes {
        if node.index == me.index {
            continue;
        }
        let Ok(client) = NodeClient::new("127.0.0.1", node.api_port) else {
            continue;
        };
        let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(2), client.status()).await
        else {
            continue;
        };
        let probable = status.data.metrics.erd_nonce.unwrap_or(0);
        if probable > 0 {
            highest = Some(highest.map_or(probable, |h| h.max(probable)));
        }
    }
    highest
}

fn record_kept_tag(tags: &mut Vec<Tag>, tag: &Tag, keep: usize) {
    tags.retain(|existing| existing != tag);
    tags.insert(0, tag.clone());
    tags.truncate(keep.max(1));
}

fn persist_migration(
    store: &StateStore,
    outcome: UpgradeOutcome,
    inflight_loc: &Path,
    binary_keep: usize,
    started_after_swap: bool,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let keep = binary_keep.max(1);
    let guard = store.lock().map_err(|e| {
        CliError::new(
            "failed to lock state",
            e.to_string(),
            "another mxnode op may be running",
        )
        .json_if(global.json)
    })?;
    let mut state = match store.load() {
        Ok(Some(s)) => s,
        Ok(None) => {
            drop(guard);
            return Err(CliError::new(
                "mxnode.toml went missing mid-upgrade",
                "expected the file we loaded earlier",
                "hand-edit mxnode.toml or re-run `mxnode install` to refresh",
            )
            .json_if(global.json));
        }
        Err(e) => {
            drop(guard);
            return Err(CliError::new(
                "failed to reload mxnode.toml",
                e.to_string(),
                "remove the file manually if it's corrupt",
            )
            .json_if(global.json));
        }
    };
    let from_config = state
        .install
        .as_ref()
        .and_then(|i| i.versions.config_tag.clone());
    let from_binary = state
        .install
        .as_ref()
        .and_then(|i| i.versions.binary_tag.clone());
    let entry = MigrationEntry {
        at: outcome.started_at,
        from_config,
        to_config: outcome.config_tag.clone(),
        from_binary,
        to_binary: Some(outcome.binary_tag.clone()),
        strategy: "rolling".to_string(),
        trigger: "cli".to_string(),
        result: if outcome.rolled_back {
            MigrationResult::RolledBack
        } else if outcome.nodes_failed.is_empty() {
            MigrationResult::Ok
        } else {
            MigrationResult::Partial
        },
        duration_secs: outcome.duration_secs,
        nodes_done: outcome.nodes_done.clone(),
        nodes_failed: outcome.nodes_failed.clone(),
    };
    state.migrations.entries.push(entry);
    if !outcome.nodes_done.is_empty() {
        // Bump the recorded binary tag so `mxnode status` reflects what's
        // actually deployed. `nodes_failed` stay on the previous tag —
        // `from_binary` of a future entry will read it from disk.
        if let Some(install) = state.install.as_mut() {
            install.versions.binary_tag = Some(outcome.binary_tag.clone());
            if let Some(config_tag) = &outcome.config_tag {
                install.versions.config_tag = Some(config_tag.clone());
            }
            record_kept_tag(&mut install.binaries.node, &outcome.binary_tag, keep);
            record_kept_tag(
                &mut install.binaries.keygenerator,
                &outcome.binary_tag,
                keep,
            );
            record_kept_tag(&mut install.binaries.seednode, &outcome.binary_tag, keep);
        }
    }
    store.save(&state, &guard).map_err(|e| {
        CliError::new(
            "failed to write mxnode.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;
    drop(guard);

    // Clear inflight.toml on terminal completion (success OR partial — the
    // operator can resume by running upgrade again with the same flags).
    let _ = Inflight::clear(inflight_loc);

    let any_failed = !outcome.nodes_failed.is_empty();
    if global.json {
        let mut payload = serde_json::json!({
            "ok": !any_failed,
            "binary_tag": outcome.binary_tag.to_string(),
            "config_tag": outcome.config_tag.as_ref().map(|t| t.to_string()),
            "duration_secs": outcome.duration_secs,
            "nodes_done": outcome.nodes_done.iter().map(|i| i.get()).collect::<Vec<_>>(),
            "nodes_failed": outcome.nodes_failed.iter().map(|i| i.get()).collect::<Vec<_>>(),
            "per_node": outcome.per_node,
        });
        if any_failed {
            payload["error"] = serde_json::json!({
                "summary": "upgrade reported failures",
                "cause": format!("{} of {} node(s) failed", outcome.nodes_failed.len(), outcome.per_node.len()),
                "try": "fix the failing nodes manually, then rerun for them",
            });
        }
        println!("{payload}");
    } else {
        for r in &outcome.per_node {
            let glyph = if r.ok { "✓" } else { "✗" };
            print!("{glyph} upgrade node-{}", r.index);
            if let Some(err) = &r.error {
                print!(" — {err}");
            }
            println!();
        }
        // The default upgrade leaves nodes stopped (matching bash). Make
        // the next step explicit so the operator doesn't sit on a host
        // with consensus paused wondering why `status` reads `failed`.
        if !any_failed && !started_after_swap {
            println!("\n→ run `mxnode start --all` to bring nodes back up");
        }
    }

    if any_failed {
        return Err(CliError::new(
            "upgrade reported failures",
            "see per-node errors above",
            "fix the failing nodes manually, then rerun for them",
        )
        .silent());
    }
    Ok(())
}

async fn upgrade_proxy(
    proxy_tag: Option<String>,
    args: &UpgradeArgs,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.config_dir);
    let mut state = store
        .load()
        .map_err(|e| {
            CliError::new(
                "failed to read mxnode.toml",
                e.to_string(),
                "run `mxnode install` first",
            )
            .json_if(global.json)
        })?
        .ok_or_else(|| {
            CliError::new(
                "no mxnode.toml on this host",
                format!("expected {}", store.state_path().display()),
                "run `mxnode install` first",
            )
            .json_if(global.json)
        })?;

    let proxy = state.proxy.as_ref().cloned().ok_or_else(|| {
        CliError::new(
            "this host has no proxy installed",
            "mxnode.toml has no [proxy] section",
            "run `mxnode observers` or skip; standalone validators don't need a proxy",
        )
        .json_if(global.json)
    })?;

    // Resolve target proxy tag: CLI subcommand value > top-level
    // --proxy-tag > [overrides].proxyver > GitHub-latest. We deliberately
    // skip the mxnode.toml fallback for the *target* — staying on the
    // currently-installed proxy isn't an upgrade.
    let cli_value = proxy_tag.as_deref().or(args.proxy_tag.as_deref());
    let resolved = crate::orchestrator::tag_resolver::resolve_proxy_tag(&runtime, cli_value)
        .await
        .map_err(|e| crate::commands::install::resolve_err(e, global))?;
    crate::commands::install::announce_resolved(global, "proxy", &resolved);
    let target_tag = resolved.tag;

    if args.dry_run {
        if global.json {
            println!(
                "{}",
                serde_json::json!({
                    "mode": "dry-run",
                    "target": "proxy",
                    "proxy_tag": target_tag.to_string(),
                    "unit": proxy.unit,
                })
            );
        } else {
            println!("dry-run upgrade proxy → {target_tag}");
        }
        return Ok(());
    }

    let bin_store = BinStore::new(runtime.paths.binaries.clone());
    let build_workdir = runtime.paths.custom_home.join("mxnode/build");
    let acquirer: Arc<dyn BinaryAcquirer> = Arc::new(SourceBuildAcquirer::new(
        runtime.loaded.file.network.github_org.clone(),
        build_workdir,
    ));
    let acquired = acquirer
        .acquire(Artifact::Proxy, &target_tag)
        .await
        .map_err(|e| {
            CliError::new(
                "failed to acquire proxy binary",
                e.to_string(),
                "ensure git+go are installed, or place the binary manually under \
                 {paths.binaries}/proxy/<tag>/proxy and rerun",
            )
            .json_if(global.json)
        })?;
    let installed = bin_store
        .install_binary("proxy", target_tag.as_str(), &acquired)
        .map_err(|e| {
            CliError::new(
                "failed to install proxy into binary store",
                e.to_string(),
                "ensure {paths.binaries} is writable",
            )
            .json_if(global.json)
        })?;

    let ctl = crate::orchestrator::supervisor::build_supervisor();
    let started = time::OffsetDateTime::now_utc();

    node_op_start("upgrade.proxy", NodeIndex::new(0), &proxy.unit);

    if let Err(e) = ctl.stop(&proxy.unit).await {
        let cause = e.to_string();
        node_op_end(
            "upgrade.proxy",
            NodeIndex::new(0),
            &proxy.unit,
            Outcome::Fail { cause: &cause },
        );
        return Err(CliError::new(
            "systemctl stop failed for proxy",
            cause,
            "inspect `journalctl -u elrond-proxy` for details",
        )
        .json_if(global.json));
    }

    let symlink = proxy.workdir.join("proxy");
    if let Err(e) = swap_symlink(&symlink, &installed) {
        node_op_end(
            "upgrade.proxy",
            NodeIndex::new(0),
            &proxy.unit,
            Outcome::Fail {
                cause: &e.to_string(),
            },
        );
        return Err(CliError::new(
            "symlink swap failed for proxy",
            e.to_string(),
            "check that {paths.binaries}/proxy/<tag>/proxy and {custom_home}/elrond-proxy/proxy are in the same filesystem",
        )
        .json_if(global.json));
    }

    if let Err(e) = ctl.start(&proxy.unit).await {
        let cause = e.to_string();
        node_op_end(
            "upgrade.proxy",
            NodeIndex::new(0),
            &proxy.unit,
            Outcome::Fail { cause: &cause },
        );
        return Err(CliError::new(
            "systemctl start failed for proxy",
            cause,
            "inspect `journalctl -u elrond-proxy` for details",
        )
        .json_if(global.json));
    }

    node_op_end("upgrade.proxy", NodeIndex::new(0), &proxy.unit, Outcome::Ok);

    state.migrations.entries.push(MigrationEntry {
        at: started,
        from_config: None,
        to_config: None,
        from_binary: state
            .install
            .as_ref()
            .and_then(|i| i.versions.proxy_tag.clone()),
        to_binary: Some(target_tag.clone()),
        strategy: "proxy".to_string(),
        trigger: "cli".to_string(),
        result: MigrationResult::Ok,
        duration_secs: (time::OffsetDateTime::now_utc() - started)
            .whole_seconds()
            .max(0) as u64,
        nodes_done: Vec::new(),
        nodes_failed: Vec::new(),
    });
    if let Some(install) = state.install.as_mut() {
        install.versions.proxy_tag = Some(target_tag.clone());
    }
    let guard = store.lock().map_err(|e| {
        CliError::new(
            "failed to lock state",
            e.to_string(),
            "another mxnode op may be running",
        )
        .json_if(global.json)
    })?;
    store.save(&state, &guard).map_err(|e| {
        CliError::new(
            "failed to write mxnode.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;
    drop(guard);

    if global.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "target": "proxy",
                "proxy_tag": target_tag.to_string(),
                "unit": proxy.unit,
            })
        );
    } else {
        println!("✓ upgrade proxy → {target_tag} ({})", proxy.unit);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::acquirer::MockAcquirer;
    use mxnode_config::{ConfigSource, Loaded};
    use mxnode_core::{MxnodeFile, Environment, Paths, Shard};

    fn runtime_for_tests(root: &Path) -> Runtime {
        let mut file = MxnodeFile::default();
        file.network.environment = Some(Environment::Testnet);
        Runtime {
            loaded: Loaded {
                file,
                source: ConfigSource::None,
                origins: Default::default(),
            },
            paths: Paths {
                custom_home: root.join("home"),
                custom_user: "validator".to_string(),
                node_keys: root.join("keys"),
                binaries: root.join("binaries"),
                config_dir: root.join("config"),
                state: root.join("state"),
                runtime: root.join("run"),
            },
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_upgrade_utilities_installs_binstore_and_legacy_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime_for_tests(tmp.path());
        let tag = Tag::parse("v1.7.99").unwrap();
        let mock = MockAcquirer::new().with_workdir(tmp.path().join("acquire"));
        mock.add(
            Artifact::Keygenerator,
            tag.as_str(),
            b"#!/bin/sh\necho keygen\n",
        );
        mock.add(
            Artifact::Seednode,
            tag.as_str(),
            b"#!/bin/sh\necho seednode\n",
        );
        let acquirer: Arc<dyn BinaryAcquirer> = Arc::new(mock);
        let bin_store = BinStore::new(runtime.paths.binaries.clone());

        let config_repo = tmp.path().join("config-repo");
        std::fs::create_dir_all(config_repo.join("seednode")).unwrap();
        std::fs::write(config_repo.join("seednode/mxnode.toml"), "port = 10000\n").unwrap();
        std::fs::write(config_repo.join("seednode/p2p.toml"), "seed = true\n").unwrap();

        refresh_upgrade_utilities(&acquirer, &bin_store, &runtime, &tag, Some(&config_repo))
            .await
            .unwrap();

        assert!(runtime
            .paths
            .binary_path("keygenerator", tag.as_str())
            .exists());
        assert!(runtime.paths.binary_path("seednode", tag.as_str()).exists());
        assert!(runtime
            .paths
            .elrond_utils_root()
            .join("keygenerator")
            .exists());
        assert!(runtime
            .paths
            .elrond_utils_root()
            .join("seednode/seednode")
            .exists());
        assert_eq!(
            std::fs::read_to_string(
                runtime
                    .paths
                    .elrond_utils_root()
                    .join("seednode/config/p2p.toml")
            )
            .unwrap(),
            "seed = true\n"
        );
    }

    #[test]
    fn upstream_config_update_preserves_prefs_and_applies_node_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime_for_tests(tmp.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("mxnode.toml"),
            r#"
[DbLookupExtensions]
Enabled = false
"#,
        )
        .unwrap();
        std::fs::write(
            repo.join("prefs.toml"),
            r#"
[Preferences]
NodeDisplayName = "upstream"
"#,
        )
        .unwrap();

        let workdir = tmp.path().join("node-0");
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        std::fs::write(
            workdir.join("config/prefs.toml"),
            r#"
[Preferences]
NodeDisplayName = "old"
RedundancyLevel = 2
"#,
        )
        .unwrap();
        let node = NodeState {
            index: NodeIndex::new(0),
            role: Role::Observer,
            shard: Shard::Zero,
            display_name: "observer-zero".to_string(),
            api_port: 8080,
            unit: "elrond-node-0.service".to_string(),
            unit_override: String::new(),
            workdir,
            last_known_pubkey: String::new(),
            last_action: String::new(),
            last_action_at: None,
        };

        apply_upstream_config_update(&node, &repo, &runtime, false).unwrap();

        let prefs = std::fs::read_to_string(node.workdir.join("config/prefs.toml")).unwrap();
        assert!(prefs.contains("NodeDisplayName = \"observer-zero\""));
        assert!(prefs.contains("RedundancyLevel = 2"));
        assert!(prefs.contains("DestinationShardAsObserver = \"0\""));

        let config = std::fs::read_to_string(node.workdir.join("config/mxnode.toml")).unwrap();
        assert!(config.contains("Enabled = true"));
    }

    #[test]
    fn record_kept_tag_deduplicates_and_trims_newest_first() {
        let mut tags = vec![
            Tag::parse("v1.0.0").unwrap(),
            Tag::parse("v0.9.0").unwrap(),
            Tag::parse("v0.8.0").unwrap(),
        ];
        record_kept_tag(&mut tags, &Tag::parse("v0.9.0").unwrap(), 2);
        assert_eq!(
            tags,
            vec![Tag::parse("v0.9.0").unwrap(), Tag::parse("v1.0.0").unwrap()]
        );
    }
}
