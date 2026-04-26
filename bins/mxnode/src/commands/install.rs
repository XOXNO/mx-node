//! `mxnode install [--count N]`: fresh-host install for validators.
//!
//! Default `kind = Validators` (per the bash `install` menu option). For
//! observer squads / multikey squads, see `commands/observers.rs` and
//! `commands/multikey.rs` which wrap the same orchestrator with the
//! shape-specific `ConfigEdits` and `install_proxy` flags.

use mxnode_core::{Environment, InstallKind, NodeIndex, Role, Shard, Tag};
use mxnode_state::StateStore;
use serde::Serialize;

use crate::cli::{GlobalArgs, InstallArgs, RoleArg};
use crate::errors::CliError;
use crate::events::global_op;
use crate::orchestrator::acquirer_factory::build_acquirer;
use crate::orchestrator::install::{
    install_units, persist_state, run_install, ConfigEdits, InstallPlan, NodeSpec,
};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};
use crate::orchestrator::tag_resolver::{
    resolve_binary_tag, resolve_config_tag, ResolveError, Resolved, Source,
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
                "run `mxnode init` (or `mxnode config set network.environment <env>`)",
            )
            .json_if(global.json)
        })?;

    let count = args.count.unwrap_or(1).max(1);
    let binary = resolve_binary_tag(&runtime, args.binary_tag.as_deref())
        .await
        .map_err(|e| resolve_err(e, global))?;
    let config = resolve_config_tag(&runtime, environment, args.config_tag.as_deref())
        .await
        .map_err(|e| resolve_err(e, global))?;
    announce_resolved(global, "binary", &binary);
    announce_resolved(global, "config", &config);
    let binary_tag = binary.tag;
    let config_tag = config.tag;

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

    let nodes: Vec<NodeSpec> = (0..count)
        .map(|i| NodeSpec {
            index: NodeIndex::new(i),
            role,
            // Single-node installs default to Auto so the node picks a
            // shard at runtime; multi-node observer squads still go
            // through `mxnode observers` for the deterministic 0/1/2/meta
            // mapping.
            shard: Shard::Auto,
            display_name: String::new(),
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
        proxy_tag: None,
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
        install_proxy: false,
        prefs_overrides: &runtime.loaded.config.overrides.prefs,
        config_overrides: &runtime.loaded.config.overrides.config,
    };

    let acquirer = build_acquirer(&runtime);
    global_op("install", &format!("{count} validator(s) on {environment}"));
    let outcome = run_install(plan, acquirer)
        .await
        .map_err(|e| install_err(e, global))?;

    install_units(&outcome.unit_files, true)
        .await
        .map_err(|e| install_err(e, global))?;

    let state_path = persist_state(&runtime.paths, &outcome.state)
        .map_err(|e| install_err(e, global))?;

    emit_success(global, &outcome, &state_path)
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
            "another mxnode op may be running; wait or run `mxnode unlock --force`",
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
) -> Result<(), CliError> {
    let install = outcome
        .state
        .install
        .as_ref()
        .ok_or_else(|| CliError::new("install outcome missing", "internal", "report a bug"))?;
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
        println!();
        println!("next: place node-{{0..N-1}}.zip under {} (validators only),", install.node_count);
        println!("      then `mxnode start --all` to bring units up.");
    }
    Ok(())
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
}
