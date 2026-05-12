//! `mxnode install --add N --role R`: extend an existing install.
//!
//! Refuses on observers-squad / multikey-squad installs (matches the bash
//! `add_node` which exits when `.squad_install` is present). Operators
//! who want a mixed host run `mxnode uninstall` + a fresh install.

use mxnode_core::{InstallKind, NodeIndex, Role, Shard};
use mxnode_state::StateStore;

use crate::cli::{InstallAddArgs, GlobalArgs, RoleArg};
use crate::errors::CliError;
use crate::events::global_op;
use crate::orchestrator::acquirer_factory::build_acquirer;
use crate::orchestrator::config_repo::{acquire_config_repo, read_go_version_from_repo};
use crate::orchestrator::install::{
    install_units, persist_state, run_install, ConfigEdits, InstallPlan, NodeSpec,
};
use crate::orchestrator::runtime::{CliErrorExt, Runtime};

use std::path::PathBuf;

use super::install::{emit_success, install_err};

/// When extending a multikey install, look at node-0's existing
/// `config/allValidatorsKeys.pem` and reuse it for the new nodes.
/// `install --add` doesn't accept `--keys-file`; the assumption is
/// that every multikey node on a host signs for the same key set, so
/// the original bundle is the right one. Returns `None` when the file
/// isn't there (mismatched install, or operator never ran multikey).
fn existing_multikey_keys(runtime: &Runtime) -> Option<PathBuf> {
    let candidate = runtime
        .paths
        .elrond_nodes_root()
        .join("node-0/config/allValidatorsKeys.pem");
    candidate.exists().then_some(candidate)
}

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: InstallAddArgs, global: &GlobalArgs) -> Result<(), CliError> {
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

    let install = state.install.clone().ok_or_else(|| {
        CliError::new(
            "mxnode.toml has no [install] section",
            "expected an existing install",
            "run `mxnode install` first",
        )
        .json_if(global.json)
    })?;

    if matches!(
        install.kind,
        InstallKind::ObserversSquad | InstallKind::MultikeySquad,
    ) {
        return Err(CliError::new(
            "cannot add nodes to a squad install",
            format!(
                "this install is `{}`; squad layouts have a fixed shape (one node per shard)",
                install.kind,
            ),
            "run `mxnode uninstall --yes --execute` and reinstall with the new node count",
        )
        .json_if(global.json));
    }

    let environment = install.environment;
    let count = args.count.max(1);
    let role = match args.role.unwrap_or(RoleArg::Validator) {
        RoleArg::Validator => Role::Validator,
        RoleArg::Observer => Role::Observer,
        RoleArg::Multikey => Role::Multikey,
    };
    let operation_mode = args
        .operation_mode
        .map(|m| m.as_str().to_string())
        .or_else(|| runtime.loaded.file.node.operation_mode.clone());
    super::install::validate_operation_mode_extra_flags(
        operation_mode.as_deref(),
        &runtime.loaded.file.node.extra_flags,
        global,
    )?;

    // Compute the next-N indices.
    let highest_existing = state.nodes.iter().map(|n| n.index.get()).max().unwrap_or(0);
    let start = if state.nodes.is_empty() {
        0
    } else {
        highest_existing + 1
    };

    // Resolve per-node display names. Interactive when stdin is a TTY
    // and the operator did not pass `--non-interactive`; mirrors the
    // install flow so `install --add` UX matches.
    let resolved_template = args
        .name_template
        .as_deref()
        .unwrap_or(&runtime.loaded.file.node.name_template);
    let interactive = !args.non_interactive && std::io::IsTerminal::is_terminal(&std::io::stdin());
    let display_names = if count == 0 {
        Vec::new()
    } else {
        let indices: Vec<u16> = (0..count).map(|i| start + i).collect();
        let mut stdin = std::io::stdin().lock();
        let mut stdout = std::io::stdout().lock();
        super::prompts::resolve_node_names(
            &mut stdin,
            &mut stdout,
            count,
            &indices,
            resolved_template,
            environment.as_str(),
            role.as_str(),
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
            index: NodeIndex::new(start + i),
            role,
            shard: Shard::Auto,
            display_name: display_names[i as usize].clone(),
        })
        .collect();

    let binary_tag = install.versions.binary_tag.clone().ok_or_else(|| {
        CliError::new(
            "state.install.versions.binary_tag is unset",
            "cannot extend without knowing the deployed tag",
            "run hand-edit and re-run to refresh, or pass an explicit override in config",
        )
        .json_if(global.json)
    })?;
    let config_tag = install
        .versions
        .config_tag
        .clone()
        .or_else(|| {
            // Fall back to overrides if state lost the tag for some
            // reason (legacy installs imported via migrate-from-bash).
            runtime
                .loaded
                .file
                .overrides
                .configver()
                .and_then(|s| s.parse().ok())
        })
        .ok_or_else(|| {
            CliError::new(
                "no config_tag recorded",
                "cannot extend without the deployed config repo tag",
                "set [overrides].configver in config and rerun",
            )
            .json_if(global.json)
        })?;

    let plan = InstallPlan {
        paths: &runtime.paths,
        environment,
        github_org: &runtime.loaded.file.network.github_org,
        binary_tag: binary_tag.clone(),
        config_tag: config_tag.clone(),
        proxy_tag: install.versions.proxy_tag.clone(),
        node_count: count,
        kind: install.kind,
        nodes,
        api_port_base: runtime.loaded.file.node.api_port_base,
        log_level: &runtime.loaded.file.node.log_level,
        limit_nofile: runtime.loaded.file.node.limit_nofile,
        restart_sec: runtime.loaded.file.node.restart_sec,
        custom_user: &runtime.paths.custom_user,
        extra_flags: &runtime.loaded.file.node.extra_flags,
        operation_mode,
        name_template: args
            .name_template
            .as_deref()
            .unwrap_or(&runtime.loaded.file.node.name_template),
        config_edits: match install.kind {
            InstallKind::Validators | InstallKind::Mixed => ConfigEdits::Validator,
            _ => ConfigEdits::Observer,
        },
        // `install --add` never installs/replaces the proxy.
        install_proxy: false,
        // `install --add` inherits the original install's keys-file:
        // there's no UX surface for changing the multikey bundle on
        // an already-installed host. A future `mxnode keys rotate`
        // would be the right home for that.
        multikey_keys_file: existing_multikey_keys(&runtime),
        redundancy_level: 0,
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
    global_op(
        "install --add",
        &format!("{count} {role} on {environment}", role = role),
    );
    let outcome = run_install(plan, acquirer)
        .await
        .map_err(|e| install_err(e, global))?;

    install_units(&outcome.unit_files, true)
        .await
        .map_err(|e| install_err(e, global))?;

    // Merge the new nodes into the existing state.
    let mut merged = state.clone();
    let new_install = outcome
        .state
        .install
        .as_ref()
        .expect("install populated by orchestrator")
        .clone();
    merged.nodes.extend(outcome.state.nodes.clone());
    if let Some(install_mut) = merged.install.as_mut() {
        install_mut.node_count = install_mut.node_count.saturating_add(count);
        install_mut.binaries = new_install.binaries;
    }
    state = merged;

    let state_path = persist_state(&runtime.paths, &state).map_err(|e| install_err(e, global))?;

    emit_success(global, &outcome, &state_path, &runtime.paths.node_keys)
}
