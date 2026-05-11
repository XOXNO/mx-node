//! Install orchestration shared by `mxnode install` (fresh) and
//! `mxnode install --add N` (extend an existing install).
//!
//! Phase 3 carries forward the bash mental model:
//!   1. acquire node (and proxy / keygenerator) binaries
//!   2. clone the config repo
//!   3. for each node-i: workdir + config copy + key install + unit
//!   4. write mxnode.toml
//!
//! The actual systemd unit install (sudo mv into /etc/systemd/system + sudo
//! systemctl enable) lives in the per-command modules so each command can
//! decide whether to enable on install or leave units disabled until
//! `mxnode start --all`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mxnode_core::{
    Environment, HostInstall, HostState, InstallKind, MigrationLog, NodeIndex, NodeState, Paths,
    ProxyState, Role, Shard, Tag, DEFAULT_PROXY_PORT, SCHEMA_VERSION,
};
use mxnode_state::{swap_symlink, BinStore, StateStore};
use mxnode_systemd::{
    apply_overrides, clear_cpu_flags, enable_db_lookup_extensions, flatten_inline_tables,
    render_canonical_node_plist, render_canonical_node_unit, render_canonical_proxy_unit,
    rewrite_proxy_config, set_destination_shard, set_node_display_name, set_redundancy_level,
    NodeUnitSpec, ObserverEntry, ProxyUnitSpec,
};
use thiserror::Error;
use toml_edit::DocumentMut;

use super::acquirer::{Artifact, BinaryAcquirer};
use super::config_repo::{acquire_config_repo, ConfigRepoError};

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("acquire failed: {0}")]
    Acquire(String),
    #[error("config repo: {0}")]
    ConfigRepo(#[from] ConfigRepoError),
    #[error("state store: {0}")]
    HostState(String),
    #[error("zip extract: {0}")]
    Zip(String),
    #[error("toml edit: {0}")]
    Toml(String),
    #[error("invalid plan: {0}")]
    Invalid(String),
}

/// Inputs for one install pass. Everything about how nodes get configured.
pub struct InstallPlan<'a> {
    pub paths: &'a Paths,
    pub environment: Environment,
    pub github_org: &'a str,
    pub binary_tag: Tag,
    pub config_tag: Tag,
    pub proxy_tag: Option<Tag>,
    pub node_count: u16,
    pub kind: InstallKind,
    /// One entry per node, populated by the calling command. Length must
    /// equal `node_count`.
    pub nodes: Vec<NodeSpec>,
    /// systemd unit knobs.
    pub api_port_base: u16,
    pub log_level: &'a str,
    pub limit_nofile: u32,
    pub restart_sec: u32,
    pub custom_user: &'a str,
    pub extra_flags: &'a str,
    pub operation_mode: Option<String>,
    pub name_template: &'a str,
    /// Source of the [Preferences] / [DbLookupExtensions] tweaks.
    pub config_edits: ConfigEdits,
    /// Whether to install + enable the proxy unit (observers-squad only).
    pub install_proxy: bool,
    /// `allValidatorsKeys.pem` to copy into every node's `config/`.
    /// Required for multikey installs (and rejected for everything
    /// else by the CLI layer); the orchestrator just performs the
    /// copy and stamps file permissions.
    pub multikey_keys_file: Option<PathBuf>,
    /// `Preferences.RedundancyLevel` for multikey nodes. Always
    /// stamped (even at 0) so the install choice is visible in
    /// `mxnode config show` and downstream tooling can read the
    /// value without falling back to the upstream default.
    pub redundancy_level: u8,
    /// Operator-supplied dotted-path overrides for every node's
    /// `prefs.toml`. Empty by default.
    pub prefs_overrides: &'a BTreeMap<String, toml::Value>,
    /// Operator-supplied dotted-path overrides for every node's
    /// `mxnode.toml`. Empty by default.
    pub config_overrides: &'a BTreeMap<String, toml::Value>,
}

/// Per-node spec written into mxnode.toml + used for tomledit.
#[derive(Debug, Clone)]
pub struct NodeSpec {
    pub index: NodeIndex,
    pub role: Role,
    pub shard: Shard,
    /// Override for `display_name`. Empty string means "use template".
    pub display_name: String,
}

/// What kind of TOML edits to apply to each node's config dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigEdits {
    /// Validators: only stamp NodeDisplayName.
    Validator,
    /// Observers / multikey: stamp NodeDisplayName, set
    /// DestinationShardAsObserver, enable [DbLookupExtensions].
    Observer,
}

/// Outcome the caller surfaces to the operator + persists to mxnode.toml.
pub struct InstallOutcome {
    pub state: HostState,
    /// Paths of the systemd unit files we rendered. The caller installs
    /// them into /etc/systemd/system (typically via sudo).
    pub unit_files: Vec<UnitFile>,
}

#[derive(Debug, Clone)]
pub struct UnitFile {
    pub name: String,
    pub contents: String,
}

/// Run the install plan: acquire binaries, copy config, render units.
pub async fn run_install(
    plan: InstallPlan<'_>,
    acquirer: Arc<dyn BinaryAcquirer>,
) -> Result<InstallOutcome, InstallError> {
    if plan.nodes.len() != plan.node_count as usize {
        return Err(InstallError::Invalid(format!(
            "nodes spec len {} does not match node_count {}",
            plan.nodes.len(),
            plan.node_count,
        )));
    }

    // 1. Acquire node binary + place into versioned BinStore.
    let bin_store = BinStore::new(plan.paths.binaries.clone());
    let node_src = acquirer
        .acquire(Artifact::Node, &plan.binary_tag)
        .await
        .map_err(|e| InstallError::Acquire(format!("node binary: {e}")))?;
    let node_installed = bin_store
        .install_binary("node", plan.binary_tag.as_str(), &node_src)
        .map_err(|e| InstallError::Acquire(format!("install_binary node: {e}")))?;

    // 2. Acquire keygenerator (always — observers/multikey need it for
    // their .pem files and validators benefit from having it on hand).
    let keygen_src = acquirer
        .acquire(Artifact::Keygenerator, &plan.binary_tag)
        .await
        .map_err(|e| InstallError::Acquire(format!("keygenerator binary: {e}")))?;
    let _keygen_installed = bin_store
        .install_binary("keygenerator", plan.binary_tag.as_str(), &keygen_src)
        .map_err(|e| InstallError::Acquire(format!("install_binary keygen: {e}")))?;
    // Place the keygenerator under elrond-utils for the observer_keys
    // flow to find it.
    let utils_dir = plan.paths.elrond_utils_root();
    fs::create_dir_all(&utils_dir).map_err(|e| InstallError::Io {
        path: utils_dir.display().to_string(),
        source: e,
    })?;
    copy_executable(&keygen_src, &utils_dir.join("keygenerator"))?;

    // 2b. Acquire seednode utility. The Rust CLI replaces termui/logviewer
    // with `dashboard`/`logs`, but seednode remains a distinct upstream
    // tool operators may need for emergency bootstrapping.
    let seednode_src = acquirer
        .acquire(Artifact::Seednode, &plan.binary_tag)
        .await
        .map_err(|e| InstallError::Acquire(format!("seednode binary: {e}")))?;
    let _seednode_installed = bin_store
        .install_binary("seednode", plan.binary_tag.as_str(), &seednode_src)
        .map_err(|e| InstallError::Acquire(format!("install_binary seednode: {e}")))?;
    let seednode_dir = utils_dir.join("seednode");
    fs::create_dir_all(seednode_dir.join("config")).map_err(|e| InstallError::Io {
        path: seednode_dir.display().to_string(),
        source: e,
    })?;
    copy_executable(&seednode_src, &seednode_dir.join("seednode"))?;

    // 3. Acquire proxy (only when this install ships one). The
    // rendered proxy unit lands in `unit_files` after the node loop
    // so the operator-visible install order is node-0..node-N, proxy.
    let mut proxy_state: Option<ProxyState> = None;
    let mut pending_proxy_unit: Option<UnitFile> = None;
    if plan.install_proxy {
        let proxy_tag = plan.proxy_tag.as_ref().ok_or_else(|| {
            InstallError::Invalid("install_proxy=true but no proxy_tag supplied".to_string())
        })?;
        let proxy_src = acquirer
            .acquire(Artifact::Proxy, proxy_tag)
            .await
            .map_err(|e| InstallError::Acquire(format!("proxy binary: {e}")))?;
        let proxy_installed = bin_store
            .install_binary("proxy", proxy_tag.as_str(), &proxy_src)
            .map_err(|e| InstallError::Acquire(format!("install_binary proxy: {e}")))?;

        let proxy_dir = plan.paths.elrond_proxy_root();
        fs::create_dir_all(proxy_dir.join("config")).map_err(|e| InstallError::Io {
            path: proxy_dir.display().to_string(),
            source: e,
        })?;
        // Symlink the active proxy binary into the proxy dir.
        let proxy_link = proxy_dir.join("proxy");
        swap_symlink(&proxy_link, &proxy_installed)
            .map_err(|e| InstallError::Acquire(format!("symlink proxy binary: {e}")))?;

        // Render canonical proxy unit. Returned to the caller via
        // `unit_files` below; the bash flow installs it alongside the
        // node units.
        let proxy_unit_text = render_canonical_proxy_unit(&ProxyUnitSpec {
            custom_user: plan.custom_user,
            proxy_dir: &proxy_dir,
            limit_nofile: plan.limit_nofile,
            restart_sec: plan.restart_sec,
        });
        pending_proxy_unit = Some(UnitFile {
            name: "elrond-proxy.service".to_string(),
            contents: proxy_unit_text,
        });

        // Seed the proxy config from the cloned mx-chain-proxy-go
        // sources so upstream sections (FullHistoryNodes, ApiLogging,
        // gas-cost tables, etc.) survive into operator hosts. Bash
        // patches the upstream file in place; we do the same via
        // toml_edit which preserves comments and unknown sections.
        // MockAcquirer-driven integration tests skip the upstream clone;
        // behaviour pinned by proxy_config_preserves_upstream_sections_when_seeded
        // + tomledit golden tests.
        let proxy_repo = super::config_repo::acquire_proxy_repo(
            &plan.paths.binaries,
            plan.github_org,
            proxy_tag,
        )
        .await
        .map_err(InstallError::ConfigRepo)?;

        let upstream_cfg = proxy_repo.join("cmd/proxy/config/mxnode.toml");
        let raw = fs::read_to_string(&upstream_cfg).map_err(|e| InstallError::Io {
            path: upstream_cfg.display().to_string(),
            source: e,
        })?;
        let mut proxy_config: DocumentMut = raw
            .parse()
            .map_err(|e| InstallError::Toml(format!("parse upstream proxy config: {e}")))?;

        // Build observer list AFTER we have the upstream document, so
        // any operator overrides applied to the upstream (gas tables,
        // logging) are not clobbered by a fresh-document patch.
        let observers: Vec<ObserverEntry> =
            build_default_observers(plan.api_port_base, plan.node_count);

        rewrite_proxy_config(&mut proxy_config, DEFAULT_PROXY_PORT, &observers)
            .map_err(|e| InstallError::Toml(format!("proxy config: {e}")))?;
        fs::write(
            proxy_dir.join("config/mxnode.toml"),
            proxy_config.to_string(),
        )
        .map_err(|e| InstallError::Io {
            path: proxy_dir.join("config/mxnode.toml").display().to_string(),
            source: e,
        })?;

        proxy_state = Some(ProxyState {
            present: true,
            unit: "elrond-proxy.service".to_string(),
            workdir: proxy_dir,
            server_port: DEFAULT_PROXY_PORT,
        });
    }

    // 4. Clone the config repo once and copy into each node-i.
    let config_repo = acquire_config_repo(
        &plan.paths.binaries,
        plan.github_org,
        plan.environment,
        &plan.config_tag,
    )
    .await?;
    install_seednode_configs(&config_repo, &seednode_dir)?;

    // 5. Per-node provisioning.
    let mut node_states: Vec<NodeState> = Vec::with_capacity(plan.node_count as usize);
    let mut unit_files: Vec<UnitFile> = Vec::with_capacity(plan.node_count as usize);
    for node in &plan.nodes {
        let workdir = plan.paths.node_workdir(node.index);
        fs::create_dir_all(workdir.join("config")).map_err(|e| InstallError::Io {
            path: workdir.display().to_string(),
            source: e,
        })?;
        // Empty side dirs the node binary expects on first start.
        for sub in ["db", "logs", "stats", "health-records"] {
            fs::create_dir_all(workdir.join(sub)).map_err(|e| InstallError::Io {
                path: workdir.display().to_string(),
                source: e,
            })?;
        }
        copy_dir_recursive(&config_repo, &workdir.join("config"))?;

        let api_port = plan.api_port_base.saturating_add(node.index.get());
        let display_name = if node.display_name.is_empty() {
            plan.name_template
                .replace("{env}", plan.environment.as_str())
                .replace("{index}", &node.index.get().to_string())
        } else {
            node.display_name.clone()
        };

        // Apply per-node tomledit.
        apply_node_tomledit(NodeTomlEdit {
            workdir: &workdir,
            display_name: &display_name,
            shard: node.shard,
            edits: plan.config_edits,
            role: node.role,
            redundancy_level: Some(plan.redundancy_level),
            prefs_overrides: plan.prefs_overrides,
            config_overrides: plan.config_overrides,
        })?;

        // Symlink node binary.
        let symlink = workdir.join("node");
        swap_symlink(&symlink, &node_installed).map_err(|e| {
            InstallError::Acquire(format!(
                "symlink node binary for index {}: {e}",
                node.index.get()
            ))
        })?;

        // Install keys per role:
        //
        //   * Validator: operator-supplied `node-{i}.zip` from
        //     `paths.node_keys`. Legacy single-BLS-key flow.
        //   * Observer: nothing — mx-chain-go auto-generates a
        //     throwaway BLS key on first start when the workdir has
        //     no `validatorKey.pem`. Generating one ahead of time was
        //     previous behaviour but adds nothing and just couples
        //     the install to a working keygenerator binary.
        //   * Multikey: copy `allValidatorsKeys.pem` into the
        //     workdir's `config/`. The node detects the bundle and
        //     enters multikey mode automatically; mx-chain-go also
        //     auto-generates the host's own observer key on first
        //     start (no separate keygen step needed).
        match node.role {
            Role::Validator => {
                install_validator_keys(&plan.paths.node_keys, node.index, &workdir)?;
            }
            Role::Observer => {
                // No-op. Documented above.
            }
            Role::Multikey => {
                let keys_file = plan.multikey_keys_file.as_deref().ok_or_else(|| {
                    InstallError::Invalid(
                        "multikey install reached the orchestrator without a keys file; \
                         the CLI layer must populate plan.multikey_keys_file"
                            .to_string(),
                    )
                })?;
                install_multikey_keys(keys_file, &workdir)?;
            }
        }

        // Render the supervisor config: systemd unit on Linux, launchd
        // plist on macOS. The contents differ; the *name* the
        // orchestrator passes downstream is always the systemd-style
        // name (`elrond-node-N.service`), and `supervisor::unit_filename`
        // translates to the macOS form at install time.
        let spec = NodeUnitSpec {
            index: node.index,
            custom_user: plan.custom_user,
            workdir: &workdir,
            api_port,
            log_level: plan.log_level,
            limit_nofile: plan.limit_nofile,
            restart_sec: plan.restart_sec,
            extra_flags: plan.extra_flags,
            operation_mode: plan.operation_mode.as_deref(),
        };
        let supervisor_text = match mxnode_core::Platform::current() {
            mxnode_core::Platform::Macos => render_canonical_node_plist(&spec),
            _ => render_canonical_node_unit(&spec),
        };
        let unit_name = format!("elrond-node-{}.service", node.index.get());
        unit_files.push(UnitFile {
            name: unit_name.clone(),
            contents: supervisor_text,
        });

        node_states.push(NodeState {
            index: node.index,
            role: node.role,
            shard: node.shard,
            display_name: display_name.clone(),
            api_port,
            unit: unit_name.clone(),
            unit_override: String::new(),
            workdir,
            last_known_pubkey: String::new(),
            last_action: String::new(),
            last_action_at: None,
        });
    }

    // Append the proxy unit so the caller's `install_units` writes it
    // alongside the node units. Order is intentional: node-0..N first,
    // proxy last, matching what the operator sees in `install` output.
    if let Some(proxy_unit) = pending_proxy_unit {
        unit_files.push(proxy_unit);
    }

    // 6. Build HostState.
    let install = HostInstall {
        kind: plan.kind,
        environment: plan.environment,
        github_org: plan.github_org.to_string(),
        node_count: plan.node_count,
        versions: mxnode_core::InstallVersions {
            config_tag: Some(plan.config_tag.clone()),
            binary_tag: Some(plan.binary_tag.clone()),
            proxy_tag: plan.proxy_tag.clone(),
            go_version: String::new(),
        },
        binaries: mxnode_core::InstallBinaries {
            node: vec![plan.binary_tag.clone()],
            proxy: plan
                .proxy_tag
                .as_ref()
                .map(|t| vec![t.clone()])
                .unwrap_or_default(),
            keygenerator: vec![plan.binary_tag.clone()],
            seednode: vec![plan.binary_tag.clone()],
        },
    };

    let state = HostState {
        schema_version: SCHEMA_VERSION,
        written_at: time::OffsetDateTime::now_utc(),
        written_by: format!("mxnode/{}", env!("CARGO_PKG_VERSION")),
        discovered: false,
        install: Some(install),
        nodes: node_states,
        proxy: proxy_state,
        migrations: MigrationLog::default(),
    };

    Ok(InstallOutcome { state, unit_files })
}

/// Build a default `[[Observers]]` list for the proxy config.
///
/// For an observer squad, the bash maps node-0..node-2 → shards 0,1,2 and
/// node-3 → metachain (`u32::MAX`). For other counts we just pin
/// node-i to shard i and let the operator edit the proxy config later.
pub fn build_default_observers(api_port_base: u16, node_count: u16) -> Vec<ObserverEntry> {
    let mut out = Vec::with_capacity(node_count as usize);
    for i in 0..node_count {
        let port = api_port_base.saturating_add(i);
        let shard_id = if node_count == 4 && i == 3 {
            mxnode_core::Shard::Metachain.protocol_id().unwrap()
        } else {
            i as u32
        };
        out.push(ObserverEntry {
            shard_id,
            address: format!("http://127.0.0.1:{port}"),
        });
    }
    out
}

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), InstallError> {
    fs::create_dir_all(dst).map_err(|e| InstallError::Io {
        path: dst.display().to_string(),
        source: e,
    })?;
    for entry in fs::read_dir(src).map_err(|e| InstallError::Io {
        path: src.display().to_string(),
        source: e,
    })? {
        let entry = entry.map_err(|e| InstallError::Io {
            path: src.display().to_string(),
            source: e,
        })?;
        let from = entry.path();
        let name = match from.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        // Skip `.git` so the operator's per-node config dir doesn't carry
        // a 1-commit bare repo around.
        if name == ".git" {
            continue;
        }
        let to = dst.join(&name);
        let ft = entry.file_type().map_err(|e| InstallError::Io {
            path: from.display().to_string(),
            source: e,
        })?;
        if ft.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ft.is_file() {
            fs::copy(&from, &to).map_err(|e| InstallError::Io {
                path: to.display().to_string(),
                source: e,
            })?;
        }
        // Symlinks inside the upstream config repo are not used; ignore.
    }
    Ok(())
}

pub(crate) fn copy_executable(src: &Path, dst: &Path) -> Result<(), InstallError> {
    fs::copy(src, dst).map_err(|e| InstallError::Io {
        path: dst.display().to_string(),
        source: e,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dst)
            .map_err(|e| InstallError::Io {
                path: dst.display().to_string(),
                source: e,
            })?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(dst, perms).map_err(|e| InstallError::Io {
            path: dst.display().to_string(),
            source: e,
        })?;
    }
    Ok(())
}

pub(crate) fn install_seednode_configs(
    config_repo: &Path,
    seednode_dir: &Path,
) -> Result<(), InstallError> {
    let config_dir = seednode_dir.join("config");
    fs::create_dir_all(&config_dir).map_err(|e| InstallError::Io {
        path: config_dir.display().to_string(),
        source: e,
    })?;
    for name in ["mxnode.toml", "p2p.toml"] {
        let src = config_repo.join("seednode").join(name);
        if src.exists() {
            fs::copy(&src, config_dir.join(name)).map_err(|e| InstallError::Io {
                path: config_dir.join(name).display().to_string(),
                source: e,
            })?;
        }
    }
    Ok(())
}

/// Inputs for [`apply_node_tomledit`]. Bundled into a struct so the
/// function stays under clippy's `too_many_arguments` floor; every
/// field has a single, well-documented role and call sites read more
/// like an init record than a parameter avalanche.
pub(crate) struct NodeTomlEdit<'a> {
    pub workdir: &'a Path,
    pub display_name: &'a str,
    pub shard: Shard,
    pub edits: ConfigEdits,
    pub role: Role,
    /// `Some(level)` stamps `RedundancyLevel = level` for multikey roles.
    /// `None` means "don't touch" — used by `config apply` so an
    /// operator's hand-edited value survives a re-apply pass.
    pub redundancy_level: Option<u8>,
    pub prefs_overrides: &'a BTreeMap<String, toml::Value>,
    pub config_overrides: &'a BTreeMap<String, toml::Value>,
}

/// Apply the well-known + operator-defined TOML edits to one node's
/// `config/` directory. Operator overrides are applied **after** the
/// well-known edits so they always win — operators can intentionally
/// undo a default mxnode chooses.
pub(crate) fn apply_node_tomledit(input: NodeTomlEdit<'_>) -> Result<(), InstallError> {
    let NodeTomlEdit {
        workdir,
        display_name,
        shard,
        edits,
        role,
        redundancy_level,
        prefs_overrides,
        config_overrides,
    } = input;
    // Index/shard substitutions for string overrides. We derive
    // `index` from the workdir's last segment (`node-N`) so we don't
    // need a separate parameter — the install orchestrator stamps the
    // workdir consistently.
    let index_str = workdir
        .file_name()
        .and_then(|f| f.to_str())
        .and_then(|s| s.strip_prefix("node-"))
        .unwrap_or("");
    let shard_str = shard.as_str();
    let subs = [("{index}", index_str), ("{shard}", shard_str)];

    let prefs_path = workdir.join("config/prefs.toml");
    if prefs_path.exists() {
        let body = fs::read_to_string(&prefs_path).map_err(|e| InstallError::Io {
            path: prefs_path.display().to_string(),
            source: e,
        })?;
        let mut doc: DocumentMut = body
            .parse()
            .map_err(|e| InstallError::Toml(format!("parse {}: {e}", prefs_path.display())))?;
        set_node_display_name(&mut doc, display_name)
            .map_err(|e| InstallError::Toml(e.to_string()))?;
        if matches!(edits, ConfigEdits::Observer) {
            set_destination_shard(&mut doc, shard)
                .map_err(|e| InstallError::Toml(e.to_string()))?;
        }
        // Stamp `RedundancyLevel` for the two roles that sign blocks:
        // multikey writes the chosen level unconditionally (including
        // 0 — primary) so `mxnode config show` reflects the install
        // decision; validators write only when level > 0 to keep the
        // upstream prefs.toml default untouched in the common case.
        // Observers never get the field — they don't sign at all.
        // `config apply` passes `None` so re-applying never clobbers
        // an operator's hand-edited value.
        if let Some(level) = redundancy_level {
            let stamp = match role {
                Role::Multikey => true,
                Role::Validator => level > 0,
                Role::Observer => false,
            };
            if stamp {
                set_redundancy_level(&mut doc, level)
                    .map_err(|e| InstallError::Toml(e.to_string()))?;
            }
        }
        if !prefs_overrides.is_empty() {
            apply_overrides(&mut doc, prefs_overrides, &subs)
                .map_err(|e| InstallError::Toml(e.to_string()))?;
        }
        fs::write(&prefs_path, doc.to_string()).map_err(|e| InstallError::Io {
            path: prefs_path.display().to_string(),
            source: e,
        })?;
    }

    // Edit mxnode.toml when any of these is true:
    //   1. Observer squads enable [DbLookupExtensions].
    //   2. Non-x86 hosts (Apple Silicon, Linux aarch64) need the
    //      [HardwareRequirements] CPUFlags array cleared.
    //   3. The operator supplied [overrides.config] entries.
    let config_path = workdir.join("config/mxnode.toml");
    let needs_observer_edits = matches!(edits, ConfigEdits::Observer);
    let needs_arm_bypass = !cfg!(target_arch = "x86_64");
    let has_op_overrides = !config_overrides.is_empty();
    if (needs_observer_edits || needs_arm_bypass || has_op_overrides) && config_path.exists() {
        let body = fs::read_to_string(&config_path).map_err(|e| InstallError::Io {
            path: config_path.display().to_string(),
            source: e,
        })?;
        // mx-chain-{testnet,…}-config from T2.0.0.0 onwards ships
        // multi-line inline tables (`{\n key = …,\n }`) which Go's
        // TOML parser tolerates but `toml_edit` rejects with
        // `invalid inline table / expected }`. Flatten them up front
        // so the rest of the file parses normally; the writer-side
        // of toml_edit re-emits whichever shape we hand it, so the
        // result still serialises cleanly.
        let normalised = flatten_inline_tables(&body);
        let mut doc: DocumentMut = normalised
            .parse()
            .map_err(|e| InstallError::Toml(format!("parse {}: {e}", config_path.display())))?;
        if needs_observer_edits {
            enable_db_lookup_extensions(&mut doc).map_err(|e| InstallError::Toml(e.to_string()))?;
        }
        if needs_arm_bypass {
            clear_cpu_flags(&mut doc).map_err(|e| InstallError::Toml(e.to_string()))?;
        }
        if has_op_overrides {
            apply_overrides(&mut doc, config_overrides, &subs)
                .map_err(|e| InstallError::Toml(e.to_string()))?;
        }
        fs::write(&config_path, doc.to_string()).map_err(|e| InstallError::Io {
            path: config_path.display().to_string(),
            source: e,
        })?;
    }
    Ok(())
}

/// Copy `allValidatorsKeys.pem` into the multikey node's `config/`
/// directory and tighten file permissions to 0600 (the file holds
/// every BLS private key the operator owns; world-readable would be
/// catastrophic). Caller has already verified that `keys_file` exists
/// — failing here is an operator-environment problem, not a misuse.
fn install_multikey_keys(keys_file: &Path, workdir: &Path) -> Result<(), InstallError> {
    let dest = workdir.join("config/allValidatorsKeys.pem");
    fs::copy(keys_file, &dest).map_err(|e| InstallError::Io {
        path: dest.display().to_string(),
        source: e,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dest)
            .map_err(|e| InstallError::Io {
                path: dest.display().to_string(),
                source: e,
            })?
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&dest, perms).map_err(|e| InstallError::Io {
            path: dest.display().to_string(),
            source: e,
        })?;
    }
    Ok(())
}

fn install_validator_keys(
    keys_dir: &Path,
    index: NodeIndex,
    workdir: &Path,
) -> Result<(), InstallError> {
    let zip_name = format!("node-{}.zip", index.get());
    let zip_path = keys_dir.join(&zip_name);
    if !zip_path.exists() {
        // Mirrors the bash: warn and continue. Operators commonly install
        // first then drop keys later. The unit will fail-to-start without
        // them, which is the right signal.
        tracing::warn!(
            target: "mxnode.event",
            event = "install.keys.missing",
            index = index.get(),
            expected = zip_path.display().to_string(),
            "validator key zip not found; node will fail to start until it's placed",
        );
        return Ok(());
    }

    // Use the system `unzip -j` (junk paths) so the .pem files land
    // directly in node-{i}/config/ — same as the bash.
    let dest = workdir.join("config");
    let status = std::process::Command::new("unzip")
        .arg("-jo") // junk paths + overwrite
        .arg(&zip_path)
        .arg("-d")
        .arg(&dest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|e| InstallError::Zip(format!("unzip not on PATH: {e}")))?;
    if !status.success() {
        return Err(InstallError::Zip(format!(
            "unzip exited {:?} for {}",
            status.code(),
            zip_path.display(),
        )));
    }
    Ok(())
}

/// Write rendered units (or plists) into the platform-appropriate
/// supervisor directory and (optionally) enable them.
///
/// On Linux: `sudo mv` into `/etc/systemd/system` + `sudo systemctl
/// enable`. On macOS: plain `cp` into `~/Library/LaunchAgents` +
/// `launchctl bootstrap`. The branch lives in
/// [`crate::orchestrator::supervisor::install_one_unit`]; we just
/// translate `InstallError` for each per-unit failure.
pub async fn install_units(units: &[UnitFile], enable: bool) -> Result<(), InstallError> {
    use crate::orchestrator::supervisor::{install_one_unit, InstallUnitError};
    use mxnode_core::Platform;
    let platform = Platform::current();
    for unit in units {
        // Render the on-disk filename per platform; the caller already
        // produced the right *contents* via render_canonical_node_unit
        // (Linux) or render_canonical_node_plist (macOS), so we only
        // need to make sure the file ends up at the right path.
        if let Err(e) = install_one_unit(platform, &unit.name, &unit.contents, enable).await {
            return Err(match e {
                InstallUnitError::Io { path, source } => InstallError::Io { path, source },
                InstallUnitError::UnsupportedPlatform => {
                    InstallError::Invalid(format!("platform {:?} is not yet supported", platform,))
                }
            });
        }
    }
    Ok(())
}

/// Persist the `HostState` produced by [`run_install`] under the lock + atomic
/// write contract enforced by [`StateStore`].
pub fn persist_state(paths: &Paths, state: &HostState) -> Result<PathBuf, InstallError> {
    let store = StateStore::new(&paths.config_dir);
    let guard = store
        .lock()
        .map_err(|e| InstallError::HostState(e.to_string()))?;
    store
        .save(state, &guard)
        .map_err(|e| InstallError::HostState(e.to_string()))?;
    drop(guard);
    Ok(store.state_path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn build_default_observers_for_squad_maps_metachain_last() {
        let observers = build_default_observers(8080, 4);
        assert_eq!(observers.len(), 4);
        assert_eq!(observers[0].shard_id, 0);
        assert_eq!(observers[1].shard_id, 1);
        assert_eq!(observers[2].shard_id, 2);
        assert_eq!(
            observers[3].shard_id,
            mxnode_core::Shard::Metachain.protocol_id().unwrap()
        );
        assert!(observers[0].address.ends_with(":8080"));
        assert!(observers[3].address.ends_with(":8083"));
    }

    #[test]
    fn build_default_observers_for_arbitrary_count_uses_index_as_shard() {
        let observers = build_default_observers(8080, 2);
        assert_eq!(observers[0].shard_id, 0);
        assert_eq!(observers[1].shard_id, 1);
    }

    #[test]
    fn copy_dir_recursive_skips_dotgit() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join(".git")).unwrap();
        std::fs::write(src.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(src.path().join("mxnode.toml"), "key = 1\n").unwrap();
        copy_dir_recursive(src.path(), dst.path()).unwrap();
        assert!(dst.path().join("mxnode.toml").exists());
        assert!(!dst.path().join(".git").exists());
    }

    #[test]
    fn validator_key_install_warns_when_zip_absent() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("node-0");
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        // No zip -> warn-and-continue, returns Ok.
        let r = install_validator_keys(dir.path(), NodeIndex::new(0), &workdir);
        assert!(r.is_ok());
    }

    #[test]
    fn apply_node_tomledit_validator_only_stamps_display_name() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("node-0");
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        std::fs::write(
            workdir.join("config/prefs.toml"),
            "[Preferences]\nNodeDisplayName = \"\"\nDestinationShardAsObserver = \"disabled\"\n",
        )
        .unwrap();

        apply_node_tomledit(NodeTomlEdit {
            workdir: &workdir,
            display_name: "test-name",
            shard: Shard::Auto,
            edits: ConfigEdits::Validator,
            role: Role::Validator,
            redundancy_level: Some(0),
            prefs_overrides: &BTreeMap::new(),
            config_overrides: &BTreeMap::new(),
        })
        .unwrap();

        let body = std::fs::read_to_string(workdir.join("config/prefs.toml")).unwrap();
        assert!(body.contains("NodeDisplayName = \"test-name\""));
        // Validator edits leave shard untouched.
        assert!(body.contains("DestinationShardAsObserver = \"disabled\""));
        // Validator + redundancy 0 must NOT stamp RedundancyLevel —
        // only multikey installs are load-bearing for that knob.
        assert!(!body.contains("RedundancyLevel"));
    }

    #[test]
    fn apply_node_tomledit_observer_pins_shard_and_enables_db_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("node-0");
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        std::fs::write(
            workdir.join("config/prefs.toml"),
            "[Preferences]\nNodeDisplayName = \"\"\nDestinationShardAsObserver = \"disabled\"\n",
        )
        .unwrap();
        std::fs::write(
            workdir.join("config/mxnode.toml"),
            "[DbLookupExtensions]\nEnabled = false\n",
        )
        .unwrap();

        apply_node_tomledit(NodeTomlEdit {
            workdir: &workdir,
            display_name: "obs-0",
            shard: Shard::Metachain,
            edits: ConfigEdits::Observer,
            role: Role::Observer,
            redundancy_level: Some(0),
            prefs_overrides: &BTreeMap::new(),
            config_overrides: &BTreeMap::new(),
        })
        .unwrap();

        let prefs = std::fs::read_to_string(workdir.join("config/prefs.toml")).unwrap();
        assert!(prefs.contains("DestinationShardAsObserver = \"metachain\""));
        let config = std::fs::read_to_string(workdir.join("config/mxnode.toml")).unwrap();
        assert!(config.contains("Enabled = true"));
    }

    #[test]
    fn apply_node_tomledit_multikey_stamps_redundancy_level() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("node-0");
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        std::fs::write(
            workdir.join("config/prefs.toml"),
            "[Preferences]\nNodeDisplayName = \"\"\nDestinationShardAsObserver = \"disabled\"\nRedundancyLevel = 0\n",
        )
        .unwrap();

        apply_node_tomledit(NodeTomlEdit {
            workdir: &workdir,
            display_name: "mk-0",
            shard: Shard::Zero,
            edits: ConfigEdits::Observer,
            role: Role::Multikey,
            redundancy_level: Some(2), // backup-of-backup
            prefs_overrides: &BTreeMap::new(),
            config_overrides: &BTreeMap::new(),
        })
        .unwrap();

        let prefs = std::fs::read_to_string(workdir.join("config/prefs.toml")).unwrap();
        assert!(prefs.contains("RedundancyLevel = 2"));
        // Same call also pins the shard via the observer edits.
        assert!(prefs.contains("DestinationShardAsObserver = \"0\""));
    }

    /// `config apply` passes `redundancy_level: None` to keep operator
    /// hand-edits intact. Even on a multikey node, `None` must leave
    /// the existing `RedundancyLevel` line alone.
    #[test]
    fn apply_node_tomledit_validator_with_nonzero_redundancy_stamps() {
        // The wizard now lets validators set RedundancyLevel for
        // backup hosts (matching the relaxed --backup validation);
        // confirm the orchestrator stamps non-zero levels into prefs.
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("node-0");
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        std::fs::write(
            workdir.join("config/prefs.toml"),
            "[Preferences]\nNodeDisplayName = \"\"\nDestinationShardAsObserver = \"disabled\"\nRedundancyLevel = 0\n",
        )
        .unwrap();

        apply_node_tomledit(NodeTomlEdit {
            workdir: &workdir,
            display_name: "validator-backup",
            shard: Shard::Auto,
            edits: ConfigEdits::Validator,
            role: Role::Validator,
            redundancy_level: Some(1),
            prefs_overrides: &BTreeMap::new(),
            config_overrides: &BTreeMap::new(),
        })
        .unwrap();

        let prefs = std::fs::read_to_string(workdir.join("config/prefs.toml")).unwrap();
        assert!(prefs.contains("RedundancyLevel = 1"));
    }

    #[test]
    fn apply_node_tomledit_multikey_with_none_redundancy_preserves_existing() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("node-0");
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        std::fs::write(
            workdir.join("config/prefs.toml"),
            "[Preferences]\nNodeDisplayName = \"\"\nDestinationShardAsObserver = \"disabled\"\nRedundancyLevel = 7\n",
        )
        .unwrap();

        apply_node_tomledit(NodeTomlEdit {
            workdir: &workdir,
            display_name: "mk-0",
            shard: Shard::Zero,
            edits: ConfigEdits::Observer,
            role: Role::Multikey,
            redundancy_level: None,
            prefs_overrides: &BTreeMap::new(),
            config_overrides: &BTreeMap::new(),
        })
        .unwrap();

        let prefs = std::fs::read_to_string(workdir.join("config/prefs.toml")).unwrap();
        assert!(prefs.contains("RedundancyLevel = 7"));
    }

    #[test]
    fn tag_parses_for_install_specs() {
        let _ = Tag::from_str("v1.7.13").unwrap();
    }

    #[test]
    fn proxy_config_preserves_upstream_sections_when_seeded() {
        use std::fs;
        use toml_edit::DocumentMut;

        let tmp = tempfile::tempdir().unwrap();
        let proxy_config_dir = tmp.path().join("config");
        fs::create_dir_all(&proxy_config_dir).unwrap();

        // Simulate upstream cmd/proxy/config/mxnode.toml with sections we must preserve.
        let upstream = r#"
[GeneralSettings]
ServerPort = 8080
RequestTimeoutSec = 60

[[FullHistoryNodes]]
ShardId = 0
Address = "http://upstream:8083"

[ApiLogging]
LoggingEnabled = true
ThresholdInMicroSeconds = 10000
"#;
        fs::write(proxy_config_dir.join("mxnode.toml"), upstream).unwrap();

        // Seed from upstream then patch.
        let raw = fs::read_to_string(proxy_config_dir.join("mxnode.toml")).unwrap();
        let mut doc: DocumentMut = raw.parse().unwrap();
        let observers = build_default_observers(8080, 4);
        mxnode_systemd::rewrite_proxy_config(&mut doc, mxnode_core::DEFAULT_PROXY_PORT, &observers)
            .unwrap();
        fs::write(proxy_config_dir.join("mxnode.toml"), doc.to_string()).unwrap();

        let out = fs::read_to_string(proxy_config_dir.join("mxnode.toml")).unwrap();
        assert!(
            out.contains("RequestTimeoutSec = 60"),
            "upstream value lost: {out}"
        );
        assert!(out.contains("[ApiLogging]"), "upstream section lost: {out}");
        assert!(out.contains("ThresholdInMicroSeconds = 10000"));
        assert!(out.contains("ServerPort = 8079"), "port not patched: {out}");
        assert!(
            out.contains("http://127.0.0.1:8080"),
            "observer not stamped: {out}"
        );
    }

    #[test]
    fn install_seednode_configs_copies_expected_files_only_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let seed_cfg = repo.join("seednode");
        fs::create_dir_all(&seed_cfg).unwrap();
        fs::write(seed_cfg.join("mxnode.toml"), "port = 10000\n").unwrap();
        fs::write(seed_cfg.join("p2p.toml"), "seed = true\n").unwrap();

        let seednode_dir = tmp.path().join("elrond-utils/seednode");
        install_seednode_configs(&repo, &seednode_dir).unwrap();

        assert_eq!(
            fs::read_to_string(seednode_dir.join("config/mxnode.toml")).unwrap(),
            "port = 10000\n",
        );
        assert_eq!(
            fs::read_to_string(seednode_dir.join("config/p2p.toml")).unwrap(),
            "seed = true\n",
        );
    }
}
