//! `mxnode install`: fresh-host install. One front door for every
//! shape — validator boxes, observer squads, multikey groups, mixed
//! roles. The previous CLI exposed three separate commands
//! (`install`, `observers`, `multikey`); they all called the same
//! orchestrator with different flags so we collapsed them into:
//!
//!   * `--role validator|observer|multikey` (default validator)
//!   * `--squad` — pin 4 nodes to shards 0/1/2/metachain (else free
//!     `--count N` with `Shard::Auto`)
//!   * `--with-proxy` — also install the MultiversX proxy (off by
//!     default; many operators host the proxy on a separate box)

use mxnode_core::{Environment, InstallKind, NodeIndex, Role, Shard, Tag};
use mxnode_state::StateStore;
use serde::Serialize;

use crate::cli::{GlobalArgs, InstallArgs, RoleArg};
use crate::errors::CliError;
use crate::events::global_op;
use crate::orchestrator::acquirer_factory::build_acquirer;
use crate::orchestrator::config_repo::{acquire_config_repo, read_go_version_from_repo};
use crate::orchestrator::install::{
    install_units, persist_state, run_install, ConfigEdits, InstallPlan, NodeSpec,
};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};
use crate::orchestrator::tag_resolver::{
    resolve_binary_tag, resolve_config_tag, resolve_proxy_tag, ResolveError, Resolved, Source,
};

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: InstallArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.state);
    if store.exists() {
        return Err(CliError::new(
            "state.toml already exists",
            format!("found {}", store.state_path().display()),
            "use `mxnode add-nodes` to extend, or `mxnode cleanup --yes --execute` to start over",
        )
        .json_if(global.json));
    }

    let environment = runtime
        .loaded
        .config
        .network
        .environment
        .ok_or_else(|| {
            CliError::new(
                "network.environment is not set",
                "install needs the operator's chosen network",
                "run any state-changing command (auto-init), or `mxnode config set network.environment <env>`",
            )
            .json_if(global.json)
        })?;

    let role = match args.role {
        RoleArg::Validator => Role::Validator,
        RoleArg::Observer => Role::Observer,
        RoleArg::Multikey => Role::Multikey,
    };
    let kind = match role {
        Role::Validator => InstallKind::Validators,
        Role::Observer => InstallKind::ObserversSquad,
        Role::Multikey => InstallKind::MultikeySquad,
    };
    let config_edits = match role {
        Role::Validator => ConfigEdits::Validator,
        Role::Observer | Role::Multikey => ConfigEdits::Observer,
    };
    let label: &'static str = match role {
        Role::Validator => "validators",
        Role::Observer => "observers",
        Role::Multikey => "multikey",
    };

    // Validate every arg-vs-role combination before any I/O so
    // operator typos fail fast (no GitHub round-trips, no apt
    // probe). Order: cheapest checks first.
    if matches!(role, Role::Multikey) && args.count.is_some() {
        return Err(CliError::new(
            "--count is rejected for --role multikey",
            "multikey installs are always a 4-shard squad (one node per shard)",
            "drop --count; mxnode picks count=4 and pins shards 0/1/2/metachain",
        )
        .json_if(global.json));
    }
    if matches!(role, Role::Multikey) && args.with_proxy {
        return Err(CliError::new(
            "--with-proxy is rejected for --role multikey",
            "multikey nodes hold validator BLS keys; co-locating a public RPC proxy \
             on the same host mixes signing infra with public-facing API and is unsafe",
            "host the proxy on a separate box (any --role observer --squad install), \
             or drop --with-proxy if you don't need a proxy here",
        )
        .json_if(global.json));
    }
    require_multikey_role("--backup", args.backup.is_some(), role, global)?;
    require_multikey_role("--keys-file", args.keys_file.is_some(), role, global)?;
    let multikey_keys_file = resolve_multikey_keys(&args, role, &runtime, global)?;
    let backup_level = args.backup.unwrap_or(0);

    let is_squad = args.squad || matches!(role, Role::Multikey);
    let count = if is_squad {
        4
    } else {
        args.count.unwrap_or(1).max(1)
    };

    // Resolve the three GitHub-API tag lookups concurrently. Each is
    // an independent HTTP round-trip on a fresh box; serial they cost
    // ~3x what concurrent does on a typical install.
    let proxy_fut = async {
        if args.with_proxy {
            resolve_proxy_tag(&runtime, args.proxy_tag.as_deref()).await.map(Some)
        } else {
            Ok(None)
        }
    };
    let (binary, config, proxy) = tokio::try_join!(
        resolve_binary_tag(&runtime, args.binary_tag.as_deref()),
        resolve_config_tag(&runtime, environment, args.config_tag.as_deref()),
        proxy_fut,
    )
    .map_err(|e| resolve_err(e, global))?;
    announce_resolved(global, "binary", &binary);
    announce_resolved(global, "config", &config);
    if let Some(p) = &proxy {
        announce_resolved(global, "proxy", p);
    }
    let binary_tag = binary.tag;
    let config_tag = config.tag;
    let proxy_tag = proxy.map(|p| p.tag);

    if let Some(path) = &multikey_keys_file {
        announce_keys_file(global, path);
        if backup_level != 0 {
            announce_redundancy(global, backup_level);
        }
    }

    // Resolve per-node display names. Interactive when stdin is a TTY
    // and the operator did not pass `--non-interactive`; the prompt
    // pre-fills each node with `name_template` expanded for that index
    // and accepts a blank line as "use the default". On non-TTY
    // (CI / piped) input we silently expand the template so automation
    // is never blocked waiting for a name.
    let resolved_template = args
        .name_template
        .as_deref()
        .unwrap_or(&runtime.loaded.config.node.name_template);
    let interactive = !args.non_interactive
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
        && !args.dry_run;
    let display_names = if count == 0 {
        Vec::new()
    } else {
        let indices: Vec<u16> = (0..count).collect();
        let mut stdin = std::io::stdin().lock();
        let mut stdout = std::io::stdout().lock();
        super::prompts::resolve_node_names(
            &mut stdin,
            &mut stdout,
            count,
            &indices,
            resolved_template,
            environment.as_str(),
            interactive,
        )
        .map_err(|e| {
            CliError::new(
                "failed to read node-name prompts from stdin",
                e.to_string(),
                "rerun with --non-interactive or pipe an input stream",
            )
            .json_if(global.json)
        })?
    };

    let nodes: Vec<NodeSpec> = (0..count)
        .map(|i| NodeSpec {
            index: NodeIndex::new(i),
            role,
            shard: if is_squad {
                squad_shard_for_index(i)
            } else {
                Shard::Auto
            },
            display_name: display_names[i as usize].clone(),
        })
        .collect();

    if args.dry_run {
        return emit_dry_run(global, environment, &binary_tag, &config_tag, count, label);
    }

    let plan = InstallPlan {
        paths: &runtime.paths,
        environment,
        github_org: &runtime.loaded.config.network.github_org,
        binary_tag: binary_tag.clone(),
        config_tag: config_tag.clone(),
        proxy_tag: proxy_tag.clone(),
        node_count: count,
        kind,
        nodes,
        api_port_base: runtime.loaded.config.node.api_port_base,
        log_level: &runtime.loaded.config.node.log_level,
        limit_nofile: runtime.loaded.config.node.limit_nofile,
        restart_sec: runtime.loaded.config.node.restart_sec,
        custom_user: &runtime.loaded.config.paths.custom_user,
        extra_flags: &runtime.loaded.config.node.extra_flags,
        name_template: args
            .name_template
            .as_deref()
            .unwrap_or(&runtime.loaded.config.node.name_template),
        config_edits,
        install_proxy: args.with_proxy,
        multikey_keys_file,
        redundancy_level: backup_level,
        prefs_overrides: &runtime.loaded.config.overrides.prefs,
        config_overrides: &runtime.loaded.config.overrides.config,
    };

    // Eagerly clone (or hit the cache for) the config repo so we can
    // read the upstream goVersion before bootstrapping the toolchain.
    // run_install hits the same cache and skips a second clone.
    let config_repo_path = acquire_config_repo(
        &runtime.paths.binaries,
        &runtime.loaded.config.network.github_org,
        environment,
        &config_tag,
    )
    .await
    .map_err(|e| install_err(e.into(), global))?;
    let upstream_go = read_go_version_from_repo(&config_repo_path);
    let acquirer = build_acquirer(&runtime, upstream_go.as_deref());
    let shape = if is_squad { "squad" } else { "node(s)" };
    global_op(
        "install",
        &format!("{count} {label} {shape} on {environment}"),
    );
    let outcome = run_install(plan, acquirer)
        .await
        .map_err(|e| install_err(e, global))?;

    install_units(&outcome.unit_files, true)
        .await
        .map_err(|e| install_err(e, global))?;

    let state_path = persist_state(&runtime.paths, &outcome.state)
        .map_err(|e| install_err(e, global))?;

    emit_success(global, &outcome, &state_path, &runtime.paths.node_keys)
}

/// Refuse if a multikey-only flag was given for a non-multikey role.
/// Centralises the "drop the flag, or pass --role multikey alongside
/// it" message so every gated flag emits the same shape.
fn require_multikey_role(
    flag: &str,
    flag_present: bool,
    role: Role,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    if flag_present && !matches!(role, Role::Multikey) {
        return Err(CliError::new(
            format!("{flag} only applies to --role multikey"),
            format!("current role is {role}"),
            format!("drop {flag}, or pass --role multikey alongside it"),
        )
        .json_if(global.json));
    }
    Ok(())
}

/// Resolve where to find `allValidatorsKeys.pem` for a multikey
/// install. Returns `Ok(None)` for non-multikey roles, an explicit
/// `--keys-file` path verbatim, or the auto-detected
/// `<node_keys>/allValidatorsKeys.pem`. The auto-detect branch is the
/// only one that tests for existence, because it has to distinguish
/// "operator dropped the bundle" from "operator forgot" — an explicit
/// path either copies cleanly or surfaces fs::copy's own io::Error
/// downstream.
fn resolve_multikey_keys(
    args: &InstallArgs,
    role: Role,
    runtime: &Runtime,
    global: &GlobalArgs,
) -> Result<Option<std::path::PathBuf>, CliError> {
    if !matches!(role, Role::Multikey) {
        return Ok(None);
    }
    if let Some(path) = &args.keys_file {
        return Ok(Some(path.clone()));
    }
    let auto = runtime.paths.node_keys.join("allValidatorsKeys.pem");
    if auto.exists() {
        return Ok(Some(auto));
    }
    Err(CliError::new(
        "multikey install requires allValidatorsKeys.pem",
        format!(
            "neither --keys-file given nor {} found",
            auto.display(),
        ),
        "pass --keys-file <path-to-allValidatorsKeys.pem>, or drop the bundle into the node_keys dir and re-run",
    )
    .json_if(global.json))
}

fn announce_keys_file(global: &GlobalArgs, path: &std::path::Path) {
    if global.json {
        return;
    }
    println!("multikey keys → {} (will be copied into every node)", path.display());
}

fn announce_redundancy(global: &GlobalArgs, level: u8) {
    if global.json {
        return;
    }
    println!("redundancy level → {level} (backup machine; same keys as primary)");
}

/// Map a [`ResolveError`] into the 3-line CLI error shape.
pub(super) fn resolve_err(e: ResolveError, global: &GlobalArgs) -> CliError {
    let (summary, hint) = match &e {
        ResolveError::InvalidTag { flag, .. } => (
            format!("invalid {flag}"),
            "supply a valid version tag (e.g. v1.7.13)".to_string(),
        ),
        ResolveError::Github { repo, .. } => (
            format!("could not look up the latest release of {repo}"),
            "pass --binary-tag / --config-tag / --proxy-tag explicitly, or set [overrides] in your config; \
             alternatively export MXNODE_GITHUB_TOKEN if you've hit the unauthenticated rate limit"
                .to_string(),
        ),
        ResolveError::UnparseableLatest { repo, tag, .. } => (
            format!("github returned a tag we can't parse for {repo}: `{tag}`"),
            "pass the desired tag explicitly with --binary-tag / --config-tag / --proxy-tag".to_string(),
        ),
    };
    CliError::new(summary, e.to_string(), hint).json_if(global.json)
}

/// Print which source produced a tag so operators see when GitHub-latest was hit.
/// Suppressed under `--json` (the success report already carries the tag).
pub(super) fn announce_resolved(global: &GlobalArgs, kind: &str, resolved: &Resolved) {
    if global.json {
        return;
    }
    let where_from = match resolved.source {
        Source::Cli => "via CLI flag",
        Source::Override => "via [overrides] in config",
        Source::GithubLatest => "via GitHub latest release",
    };
    println!("resolved {kind} tag → {} ({where_from})", resolved.tag);
}

pub(super) fn emit_dry_run(
    global: &GlobalArgs,
    env: Environment,
    binary_tag: &Tag,
    config_tag: &Tag,
    count: u16,
    kind: &str,
) -> Result<(), CliError> {
    if global.json {
        let payload = serde_json::json!({
            "mode": "dry-run",
            "kind": kind,
            "environment": env.to_string(),
            "binary_tag": binary_tag.to_string(),
            "config_tag": config_tag.to_string(),
            "node_count": count,
        });
        println!("{payload}");
    } else {
        println!("dry-run install plan:");
        println!("  kind:        {kind}");
        println!("  environment: {env}");
        println!("  binary_tag:  {binary_tag}");
        println!("  config_tag:  {config_tag}");
        println!("  count:       {count}");
    }
    Ok(())
}

pub(super) fn install_err(
    e: crate::orchestrator::install::InstallError,
    global: &GlobalArgs,
) -> CliError {
    use crate::orchestrator::install::InstallError as E;
    let (summary, hint) = match &e {
        E::Acquire(_) => (
            "binary acquisition failed",
            "for source builds: ensure git+go are installed; for release mode: ensure the tag publishes a `multiversx_*_linux_<arch>.zip`",
        ),
        E::ConfigRepo(_) => (
            "could not clone the config repo",
            "check that the org+env+tag combination resolves to a public mx-chain-{env}-config repo",
        ),
        E::Io { .. } => (
            "io error during install",
            "ensure the configured paths are writable by the current user",
        ),
        E::State(_) => (
            "could not persist state.toml",
            "another mxnode op may be running; wait for it to finish",
        ),
        E::Zip(_) => (
            "key zip extraction failed",
            "check that NODE_KEYS_LOCATION/node-N.zip is a valid zip archive",
        ),
        E::Toml(_) => (
            "config TOML edit failed",
            "the upstream config repo's prefs.toml or config.toml may have a non-standard layout",
        ),
        E::Invalid(_) => (
            "install plan is invalid",
            "report this as a bug — the orchestrator built an inconsistent plan",
        ),
    };
    CliError::new(summary, e.to_string(), hint).json_if(global.json)
}

pub(super) fn emit_success(
    global: &GlobalArgs,
    outcome: &crate::orchestrator::install::InstallOutcome,
    state_path: &std::path::Path,
    node_keys_dir: &std::path::Path,
) -> Result<(), CliError> {
    let install = outcome
        .state
        .install
        .as_ref()
        .ok_or_else(|| CliError::new("install outcome missing", "internal", "report a bug"))?;
    // Validator installs need operator-supplied `node-{i}.zip` archives;
    // observer/multikey installs generate their own keys via keygenerator
    // and don't need anything dropped under `node_keys`. Surface the
    // right next-step instead of printing the validator path
    // unconditionally.
    let needs_zips = matches!(install.kind, mxnode_core::InstallKind::Validators);
    let node_names: Vec<NodeNameReport> = outcome
        .state
        .nodes
        .iter()
        .map(|n| NodeNameReport {
            index: n.index.get(),
            display_name: n.display_name.clone(),
        })
        .collect();
    if global.json {
        let report = InstallReport {
            ok: true,
            kind: install.kind.to_string(),
            environment: install.environment.to_string(),
            node_count: install.node_count,
            binary_tag: install.versions.binary_tag.as_ref().map(|t| t.to_string()),
            config_tag: install.versions.config_tag.as_ref().map(|t| t.to_string()),
            state_path: state_path.display().to_string(),
            units: outcome.unit_files.iter().map(|u| u.name.clone()).collect(),
            nodes: node_names,
            node_keys_dir: needs_zips.then(|| node_keys_dir.display().to_string()),
        };
        println!("{}", serde_json::to_string(&report).unwrap_or_default());
    } else {
        println!("✓ install: {}", install);
        println!("  state.toml: {}", state_path.display());
        println!(
            "  units:      {}",
            outcome
                .unit_files
                .iter()
                .map(|u| u.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        if !node_names.is_empty() {
            println!("  names:");
            for n in &node_names {
                if n.display_name.is_empty() {
                    println!("    node-{} → (none — set node.name_template in config)", n.index);
                } else {
                    println!("    node-{} → {}", n.index, n.display_name);
                }
            }
        }
        println!();
        if needs_zips {
            println!(
                "next: place node-{{0..{n}}}.zip under {dir}",
                n = install.node_count.saturating_sub(1),
                dir = node_keys_dir.display(),
            );
            println!("      then `mxnode start --all` to bring units up.");
        } else {
            println!("next: `mxnode start --all` to bring units up");
            println!("      (observer/multikey keys were generated automatically).");
        }
    }
    Ok(())
}

/// Squad shard mapping: the canonical bash `observing_squad` /
/// `multikey_group` layout — index 0 → shard 0, 1 → 1, 2 → 2, 3 →
/// metachain. Squads are always 4-node so any other index is a
/// programmer error; we surface it as `Shard::Auto` so the node still
/// boots rather than panicking on a stray 5th index.
fn squad_shard_for_index(index: u16) -> Shard {
    match index {
        0 => Shard::Zero,
        1 => Shard::One,
        2 => Shard::Two,
        3 => Shard::Metachain,
        _ => Shard::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::squad_shard_for_index;
    use mxnode_core::Shard;

    #[test]
    fn squad_mapping_pins_canonical_shards() {
        assert_eq!(squad_shard_for_index(0), Shard::Zero);
        assert_eq!(squad_shard_for_index(1), Shard::One);
        assert_eq!(squad_shard_for_index(2), Shard::Two);
        assert_eq!(squad_shard_for_index(3), Shard::Metachain);
    }

    #[test]
    fn squad_mapping_falls_back_to_auto_for_extra_indices() {
        // Squads are 4-node, but defensive: out-of-range index
        // should not panic.
        assert_eq!(squad_shard_for_index(4), Shard::Auto);
    }
}

#[derive(Debug, Serialize)]
struct InstallReport {
    ok: bool,
    kind: String,
    environment: String,
    node_count: u16,
    binary_tag: Option<String>,
    config_tag: Option<String>,
    state_path: String,
    units: Vec<String>,
    /// Per-node `(index, NodeDisplayName)` pairs as actually stamped
    /// into each `prefs.toml`. Operators rely on this to verify their
    /// `--name-template` (or `node.name_template`) resolved as expected.
    nodes: Vec<NodeNameReport>,
    /// Where the operator must drop `node-{i}.zip` archives. Present
    /// only for validator installs; `null` for observer/multikey
    /// (which generate their own keys via keygenerator).
    #[serde(skip_serializing_if = "Option::is_none")]
    node_keys_dir: Option<String>,
}

#[derive(Debug, Serialize)]
struct NodeNameReport {
    index: u16,
    display_name: String,
}
