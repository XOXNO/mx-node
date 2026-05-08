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
pub async fn run(mut args: InstallArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let runtime = Runtime::from_global(global)?;
    let store = StateStore::new(&runtime.paths.config_dir);
    if store.host_initialized() {
        return Err(CliError::new(
            "mxnode.toml already exists",
            format!("found {}", store.state_path().display()),
            "use `mxnode add-nodes` to extend, or `mxnode cleanup --yes --execute` to start over",
        )
        .json_if(global.json));
    }

    let environment = match runtime.loaded.file.network.environment {
        Some(e) => e,
        None => prompt_for_missing_network(global)?,
    };

    // Run the install wizard for `mxnode install` typed bare on a TTY.
    // Power users who pass any explicit selector (`--role`, `--count`,
    // `--squad`, etc.) skip the wizard and get the historical
    // flag-driven flow. `--non-interactive` and `--dry-run` always skip.
    run_install_wizard(&mut args, global)?;

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
    // `--backup` (RedundancyLevel) is allowed for validators AND
    // multikey — both can run a backup-of-primary host. Observers
    // don't sign blocks, so the field is meaningless there.
    if args.backup.is_some() && matches!(role, Role::Observer) {
        return Err(CliError::new(
            "--backup is rejected for --role observer",
            "observers don't sign blocks; RedundancyLevel has no effect on them",
            "drop --backup, or use --role validator or --role multikey",
        )
        .json_if(global.json));
    }
    require_multikey_role("--keys-file", args.keys_file.is_some(), role, global)?;
    let multikey_keys_file = resolve_multikey_keys(&args, role, &runtime, global)?;
    let backup_level = args.backup.unwrap_or(0);
    let operation_mode = args
        .operation_mode
        .map(|m| m.as_str().to_string())
        .or_else(|| runtime.loaded.file.node.operation_mode.clone());
    validate_operation_mode_extra_flags(
        operation_mode.as_deref(),
        &runtime.loaded.file.node.extra_flags,
        global,
    )?;

    let is_squad = args.squad || matches!(role, Role::Multikey);
    let count = if is_squad {
        4
    } else {
        args.count.unwrap_or(1).max(1)
    };
    if !args.dry_run {
        enforce_install_requirements(&runtime, environment, role, count, global)?;
    }

    // Resolve the three GitHub-API tag lookups concurrently. Each is
    // an independent HTTP round-trip on a fresh box; serial they cost
    // ~3x what concurrent does on a typical install.
    let proxy_fut = async {
        if args.with_proxy {
            resolve_proxy_tag(&runtime, args.proxy_tag.as_deref())
                .await
                .map(Some)
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
        .unwrap_or(&runtime.loaded.file.node.name_template);
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
        github_org: &runtime.loaded.file.network.github_org,
        binary_tag: binary_tag.clone(),
        config_tag: config_tag.clone(),
        proxy_tag: proxy_tag.clone(),
        node_count: count,
        kind,
        nodes,
        api_port_base: runtime.loaded.file.node.api_port_base,
        log_level: &runtime.loaded.file.node.log_level,
        limit_nofile: runtime.loaded.file.node.limit_nofile,
        restart_sec: runtime.loaded.file.node.restart_sec,
        custom_user: &runtime.loaded.file.paths.custom_user,
        extra_flags: &runtime.loaded.file.node.extra_flags,
        operation_mode,
        name_template: args
            .name_template
            .as_deref()
            .unwrap_or(&runtime.loaded.file.node.name_template),
        config_edits,
        install_proxy: args.with_proxy,
        multikey_keys_file,
        redundancy_level: backup_level,
        prefs_overrides: &runtime.loaded.file.overrides.prefs,
        config_overrides: &runtime.loaded.file.overrides.config,
    };

    // Eagerly clone (or hit the cache for) the config repo so we can
    // read the upstream goVersion before bootstrapping the toolchain.
    // run_install hits the same cache and skips a second clone.
    let config_repo_path = acquire_config_repo(
        &runtime.paths.binaries,
        &runtime.loaded.file.network.github_org,
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

    let state_path =
        persist_state(&runtime.paths, &outcome.state).map_err(|e| install_err(e, global))?;

    emit_success(global, &outcome, &state_path, &runtime.paths.node_keys)
}

/// Resolve `network.environment` when the loaded config doesn't have
/// it set. On a TTY, prompt the operator (mainnet / testnet / devnet)
/// and persist the answer into `mxnode.toml` so subsequent runs skip
/// the prompt. Off a TTY (CI / `--json` / piped stdin), preserve the
/// historical hard error so automation isn't blocked waiting for
/// stdin — operators can pass the environment via `--config <PATH>`
/// pointing at a pre-baked file or run `mxnode config set
/// network.environment <env>` first.
fn prompt_for_missing_network(global: &GlobalArgs) -> Result<Environment, CliError> {
    use std::io::IsTerminal;
    let interactive = !global.json && std::io::stdin().is_terminal();
    if !interactive {
        return Err(CliError::new(
            "network.environment is not set",
            "install needs the operator's chosen network",
            "run `mxnode config set network.environment <mainnet|testnet|devnet>`, or run `mxnode install` interactively",
        )
        .json_if(global.json));
    }

    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let chosen = super::prompts::prompt_for_network(&mut stdin, &mut stdout, true).map_err(
        |e| {
            CliError::new(
                "failed to read network choice from stdin",
                e.to_string(),
                "rerun with `--json` and run `mxnode config set network.environment <env>` instead",
            )
            .json_if(global.json)
        },
    )?;
    persist_network_environment(chosen, global)?;
    Ok(chosen)
}

/// Atomically write `network.environment = <chosen>` to the user's
/// `mxnode.toml`. Routes through `toml_edit::DocumentMut` so any
/// existing comments / section ordering survive untouched (same path
/// `mxnode config set` uses).
fn persist_network_environment(env: Environment, global: &GlobalArgs) -> Result<(), CliError> {
    use toml_edit::{value, DocumentMut};
    let target = mxnode_config::user_config_path().map_err(|e| {
        CliError::new(
            "could not resolve mxnode.toml path",
            e.to_string(),
            "set $XDG_CONFIG_HOME or $HOME so mxnode can locate the file",
        )
        .json_if(global.json)
    })?;
    let body = std::fs::read_to_string(&target).map_err(|e| {
        CliError::new(
            "could not read mxnode.toml",
            format!("{}: {e}", target.display()),
            "ensure the file is readable",
        )
        .json_if(global.json)
    })?;
    let mut doc: DocumentMut = body.parse().map_err(|e: toml_edit::TomlError| {
        CliError::new(
            "could not parse mxnode.toml",
            format!("{}: {e}", target.display()),
            "fix the file by hand or back it up and re-run `mxnode install`",
        )
        .json_if(global.json)
    })?;
    doc["network"]["environment"] = value(env.to_string());
    std::fs::write(&target, doc.to_string()).map_err(|e| {
        CliError::new(
            "could not write mxnode.toml",
            format!("{}: {e}", target.display()),
            "ensure the parent directory is writable",
        )
        .json_if(global.json)
    })?;
    if !global.json {
        eprintln!("→ saved network.environment = {env} to {}", target.display());
    }
    Ok(())
}

fn enforce_install_requirements(
    runtime: &Runtime,
    environment: Environment,
    role: Role,
    count: u16,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    if global.skip_safety_checks {
        return Ok(());
    }
    let context =
        super::doctor::planned_system_requirements_context(count as usize, environment, role);
    let findings = super::doctor::check_system_requirements_with_context(runtime, context);
    let errors: Vec<_> = findings
        .iter()
        .filter(|f| f.severity == super::doctor::Severity::Error)
        .collect();
    if errors.is_empty() {
        return Ok(());
    }
    let details = errors
        .iter()
        .map(|f| format!("{}: {}", f.check, f.summary))
        .collect::<Vec<_>>()
        .join("; ");
    Err(CliError::new(
        "host does not meet MultiversX system requirements",
        details,
        "run `mxnode doctor` for full details, fix the requirements, or pass --skip-safety-checks to override deliberately",
    )
    .json_if(global.json))
}

pub(super) fn validate_operation_mode_extra_flags(
    operation_mode: Option<&str>,
    extra_flags: &str,
    global: &GlobalArgs,
) -> Result<(), CliError> {
    if operation_mode.is_some() && extra_flags_contain_operation_mode(extra_flags) {
        return Err(CliError::new(
            "--operation-mode conflicts with node.extra_flags",
            "node.extra_flags already contains an operation-mode flag",
            "remove the raw operation-mode flag from config, or drop --operation-mode",
        )
        .json_if(global.json));
    }
    Ok(())
}

fn extra_flags_contain_operation_mode(extra_flags: &str) -> bool {
    extra_flags.split_whitespace().any(|flag| {
        matches!(flag, "-operation-mode" | "--operation-mode")
            || flag.starts_with("-operation-mode=")
            || flag.starts_with("--operation-mode=")
    })
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
    println!(
        "multikey keys → {} (will be copied into every node)",
        path.display()
    );
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

/// Run the interactive install wizard when `mxnode install` is typed
/// bare on a TTY. Mutates `args` in place so the rest of `run` reads
/// the operator's choices via the same fields the CLI flags would
/// have populated. No-op (and returns `Ok(())`) when the operator
/// already supplied any selector, set `--non-interactive`, set
/// `--dry-run`, or stdin is not a TTY.
fn run_install_wizard(args: &mut InstallArgs, global: &GlobalArgs) -> Result<(), CliError> {
    if args.non_interactive || args.dry_run {
        return Ok(());
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Ok(());
    }
    if !is_bare_install(args) {
        // Operator passed at least one selector — they know what they
        // want; don't prompt.
        return Ok(());
    }

    use super::prompts::{
        prompt_for_count, prompt_for_install_type, prompt_for_redundancy, prompt_for_yes_no,
        InstallType,
    };
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let read_io = |e: std::io::Error| {
        CliError::new(
            "failed to read install-wizard input from stdin",
            e.to_string(),
            "rerun with --non-interactive plus the explicit flags you want",
        )
        .json_if(global.json)
    };

    let install_type = prompt_for_install_type(&mut stdin, &mut stdout, true).map_err(read_io)?;
    apply_install_type(args, install_type);

    if matches!(
        install_type,
        InstallType::Validators | InstallType::Observers
    ) {
        let count = prompt_for_count(&mut stdin, &mut stdout, 1, true).map_err(read_io)?;
        args.count = Some(count);
    }

    // Proxy is operator-optional for the observers-squad layout. Bash
    // always co-installs the proxy; mxnode lets the operator opt out
    // (many production setups host the proxy on a separate box).
    // Multikey rejects --with-proxy entirely (signing + public RPC on
    // the same host is unsafe), and free-count validators / observers
    // don't have the shard-pinning a proxy needs to route requests, so
    // we don't ask there either.
    if matches!(install_type, InstallType::ObserversSquad) {
        let want_proxy = prompt_for_yes_no(
            &mut stdin,
            &mut stdout,
            "Install MultiversX proxy alongside the squad?",
            true,
            true,
        )
        .map_err(read_io)?;
        args.with_proxy = want_proxy;
    }

    if matches!(
        install_type,
        InstallType::Validators | InstallType::MultikeySquad,
    ) {
        let level = prompt_for_redundancy(&mut stdin, &mut stdout, true).map_err(read_io)?;
        // For multikey, persist the explicit choice (including 0) so
        // `mxnode config show` reflects the install-time decision.
        // For validators, only persist non-zero — RedundancyLevel = 0
        // is the prefs.toml upstream default and writing it is noise.
        if level > 0 || matches!(install_type, InstallType::MultikeySquad) {
            args.backup = Some(level);
        }
    }
    Ok(())
}

/// True when the operator typed `mxnode install` bare — every selector
/// is at its clap default. Triggers the wizard.
fn is_bare_install(args: &InstallArgs) -> bool {
    args.count.is_none()
        && !args.squad
        && !args.with_proxy
        && args.keys_file.is_none()
        && args.backup.is_none()
        && args.binary_tag.is_none()
        && args.config_tag.is_none()
        && args.proxy_tag.is_none()
        && args.name_template.is_none()
        && matches!(args.role, RoleArg::Validator)
}

/// Translate the wizard's [`super::prompts::InstallType`] choice into
/// the equivalent flag combination on `InstallArgs`.
fn apply_install_type(args: &mut InstallArgs, install_type: super::prompts::InstallType) {
    use super::prompts::InstallType;
    match install_type {
        InstallType::Validators => {
            args.role = RoleArg::Validator;
            args.squad = false;
            args.with_proxy = false;
        }
        InstallType::Observers => {
            args.role = RoleArg::Observer;
            args.squad = false;
            args.with_proxy = false;
        }
        InstallType::ObserversSquad => {
            args.role = RoleArg::Observer;
            args.squad = true;
            // Proxy is asked separately further down the wizard so the
            // operator can opt out (many production setups host the
            // proxy on a different box). Default to off here; the
            // dedicated yes/no prompt flips it on when accepted.
            args.with_proxy = false;
        }
        InstallType::MultikeySquad => {
            args.role = RoleArg::Multikey;
            // The orchestrator forces squad=true when role=Multikey
            // anyway, but mirroring the bash flow here keeps the
            // post-wizard `args` self-consistent and makes
            // `--dry-run` output match what the operator picked.
            args.squad = true;
            args.with_proxy = false;
        }
    }
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
        E::ConfigRepo(inner) => {
            // Several distinct failure modes share this branch. Route
            // the operator at the actual cause instead of always
            // suggesting they double-check the org/env/tag.
            let msg = inner.to_string();
            if msg.contains("could not create directory") {
                (
                    "could not create the config-repo cache directory",
                    "check `mxnode config get paths.binaries` — the resolved path probably points at a directory the current user can't write (commonly a stale `paths.custom_home = /home/ubuntu` from auto-init on a host where the operator user is something else; fix with `mxnode config set paths.custom_home /home/<your-user>`)",
                )
            } else if msg.contains("io error spawning git") {
                (
                    "could not run `git` to clone the config repo",
                    "ensure `git` is installed and executable on PATH; run `mxnode doctor` to verify",
                )
            } else {
                (
                    "could not clone the config repo",
                    "check that the org+env+tag combination resolves to a public mx-chain-{env}-config repo, and that the host has network access",
                )
            }
        }
        E::Io { .. } => (
            "io error during install",
            "ensure the configured paths are writable by the current user",
        ),
        E::HostState(_) => (
            "could not persist mxnode.toml",
            "another mxnode op may be running; wait for it to finish",
        ),
        E::Zip(_) => (
            "key zip extraction failed",
            "check that NODE_KEYS_LOCATION/node-N.zip is a valid zip archive",
        ),
        E::Toml(_) => (
            "config TOML edit failed",
            "the upstream config repo's prefs.toml or mxnode.toml may have a non-standard layout",
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
        println!("  mxnode.toml: {}", state_path.display());
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
                    println!(
                        "    node-{} → (none — set node.name_template in config)",
                        n.index
                    );
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
    use super::{enforce_install_requirements, squad_shard_for_index};
    use crate::cli::GlobalArgs;
    use crate::orchestrator::runtime::Runtime;
    use mxnode_config::{ConfigSource, Loaded};
    use mxnode_core::{MxnodeFile, Environment, Paths, Role, Shard};

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

    #[test]
    fn install_requirements_gate_can_be_bypassed_explicitly() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime_for_tests(tmp.path());
        let global = global_for_tests(true);
        enforce_install_requirements(
            &runtime,
            Environment::Mainnet,
            Role::Validator,
            u16::MAX,
            &global,
        )
        .expect("--skip-safety-checks must bypass the install gate");
    }

    #[test]
    fn install_requirements_gate_reports_planned_node_count() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = runtime_for_tests(tmp.path());
        let global = global_for_tests(false);
        let err = enforce_install_requirements(
            &runtime,
            Environment::Mainnet,
            Role::Validator,
            u16::MAX,
            &global,
        )
        .expect_err("unrealistic node count must fail requirements");
        assert!(err.cause.contains(&format!("{} node(s)", u16::MAX)));
        assert!(err.cause.contains("requirements.cpu"));
    }

    fn global_for_tests(skip_safety_checks: bool) -> GlobalArgs {
        GlobalArgs {
            config: None,
            skip_safety_checks,
            json: false,
            no_color: false,
            verbose: false,
            quiet: false,
        no_update_check: true,
        }
    }

    fn runtime_for_tests(custom_home: &std::path::Path) -> Runtime {
        let mut file = MxnodeFile::default();
        file.network.environment = Some(Environment::Mainnet);
        Runtime {
            loaded: Loaded {
                file,
                source: ConfigSource::None,
                origins: Default::default(),
            },
            paths: Paths {
                custom_home: custom_home.to_path_buf(),
                ..Paths::default()
            },
        }
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
