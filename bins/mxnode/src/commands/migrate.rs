//! `mxnode import-bash` (alias: `migrate-bash`): import an existing `mx-chain-scripts` (bash) install
//! into mxnode's cache-derived `mxnode.toml` and (optionally) merge
//! operator-customised settings from the bash `variables.cfg` and the
//! rendered systemd unit files into `~/.config/mxnode/mxnode.toml`.
//!
//! Three sources are consulted, all read-only — bash files on disk are
//! never modified:
//!
//!   1. `$CUSTOM_HOME/.installedenv`, `.numberofnodes`, optionally
//!      `.squad_install` — bash sentinels that drive the cache-derived
//!      state. Required.
//!   2. `<scripts_dir>/config/variables.cfg` — operator's customised
//!      variables (CUSTOM_HOME, CUSTOM_USER, NODE_KEYS_LOCATION,
//!      GITHUB_ORG, NODE_EXTRA_FLAGS, OVERRIDE_*, GITHUBTOKEN). Optional;
//!      a missing file just means we don't fill those config fields.
//!   3. `<systemd_dir>/elrond-node-*.service` and `elrond-proxy.service`
//!      — rendered systemd units. We extract `User=`, `WorkingDirectory=`
//!      and the `ExecStart=` line. The trailing portion of `ExecStart`
//!      after the canonical bash flags becomes that node's per-node
//!      `extra_flags` override (only when it differs from the global).
//!      Optional; a missing scan just means no per-node overrides.
//!
//! Merge policy (mxnode.toml only — mxnode.toml has its own
//! "refuse-to-overwrite" rule):
//!
//!   * A bash-derived value is written only when the corresponding
//!     mxnode field is at its schema default. Operators who already
//!     customised mxnode's mxnode.toml win — we never silently flip
//!     their explicit choice.
//!   * GITHUBTOKEN is persisted into the unified `mxnode.toml` under
//!     `[secrets].github_token`. The file lives at mode 0600 (owner
//!     read/write only) — that's the same isolation the bash flow had
//!     for `variables.cfg`, plus type-level redaction so accidental
//!     `tracing::info!("{cfg:?}")` cannot leak it. Operators can still
//!     override at runtime via `MXNODE_GITHUB_TOKEN` (env wins over
//!     file). The token is partially masked (first 4 + tail length)
//!     in dry-run output and `mxnode config show`.
//!   * Existing TOML comments and section ordering in mxnode.toml are
//!     preserved by routing writes through `toml_edit::DocumentMut`,
//!     same as `mxnode config set`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Args;
use mxnode_config::resolve_paths;
use mxnode_config::{load, user_config_path_or_default, ConfigSource, LoadOptions};
use mxnode_core::{
    HostInstall, MxnodeFile, Environment, InstallKind, NodeIndex, NodeState, Paths,
    ProxyState, Role, Shard, HostState, DEFAULT_API_PORT_BASE, DEFAULT_PROXY_PORT,
};
use thiserror::Error;
use toml_edit::{value, DocumentMut};

use crate::cli::GlobalArgs;
use crate::errors::CliError;
use crate::orchestrator::runtime::CliErrorExt;

#[derive(Debug, Error)]
pub enum MigrateError {
    #[error("could not parse {field}: {detail}")]
    Parse { field: &'static str, detail: String },
    #[error("not a bash install — missing {0}")]
    NotBash(&'static str),
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

// ─────────────────────────────────────────────────────────────────────
// 1. Sentinel files (.installedenv / .numberofnodes / .squad_install)
// ─────────────────────────────────────────────────────────────────────

/// Inspect a `$CUSTOM_HOME` looking for the bash sentinels (`.installedenv`,
/// `.numberofnodes`, optionally `.squad_install`) and return the
/// cache-derived [`HostState`] without touching disk. Tags inside
/// `InstallVersions` are intentionally left at their default `None` —
/// bash does not record them as data, and `mxnode.toml` must not lie about
/// what's installed. A subsequent `mxnode upgrade` resolves them from GitHub.
pub fn infer_state_from_bash(custom_home: &Path) -> Result<HostState, MigrateError> {
    let env_raw = fs::read_to_string(custom_home.join(".installedenv"))
        .map_err(|_| MigrateError::NotBash(".installedenv"))?;
    let environment = match env_raw.trim() {
        "mainnet" => Environment::Mainnet,
        "testnet" => Environment::Testnet,
        "devnet" => Environment::Devnet,
        other => {
            return Err(MigrateError::Parse {
                field: ".installedenv",
                detail: format!("unknown environment {other:?}"),
            });
        }
    };

    let count: u16 = fs::read_to_string(custom_home.join(".numberofnodes"))
        .map_err(|_| MigrateError::NotBash(".numberofnodes"))?
        .trim()
        .parse()
        .map_err(|e: std::num::ParseIntError| MigrateError::Parse {
            field: ".numberofnodes",
            detail: format!("{e}"),
        })?;

    let kind = match fs::read_to_string(custom_home.join(".squad_install"))
        .as_deref()
        .map(str::trim)
    {
        Ok("Observers Squad") => InstallKind::ObserversSquad,
        Ok("Multikey Squad") => InstallKind::MultikeySquad,
        // Either the file is absent, unreadable, or holds an unknown
        // marker. Bash treats anything other than the two recognised
        // strings as "validators install"; mirror that.
        _ => InstallKind::Validators,
    };

    let role = match kind {
        InstallKind::Validators => Role::Validator,
        InstallKind::ObserversSquad => Role::Observer,
        InstallKind::MultikeySquad => Role::Multikey,
        // Unreachable: bash has no `Mixed` install concept; the inference
        // above can only return Validators/ObserversSquad/MultikeySquad.
        // If a future contributor adds Mixed to that match, this arm forces
        // them to update the role mapping in the same change.
        InstallKind::Mixed => unreachable!("bash sentinels do not produce InstallKind::Mixed"),
    };

    // Squad layout: indices 0/1/2 → shards 0/1/2, index 3 → metachain.
    // Validator layout: shard is operator-driven, leave Auto.
    let shard_for = |i: u16| -> Shard {
        match (kind, i) {
            (InstallKind::Validators, _) => Shard::Auto,
            (_, 0) => Shard::Zero,
            (_, 1) => Shard::One,
            (_, 2) => Shard::Two,
            (_, 3) => Shard::Metachain,
            // bash squads are always 4 nodes; surface anything beyond that as Auto
            // so we don't fabricate a shard assignment we cannot justify.
            _ => Shard::Auto,
        }
    };

    let paths = Paths {
        custom_home: custom_home.to_path_buf(),
        ..Paths::default()
    };

    let nodes: Vec<NodeState> = (0..count)
        .map(|i| {
            let index = NodeIndex::new(i);
            NodeState {
                index,
                role,
                shard: shard_for(i),
                display_name: String::new(),
                api_port: DEFAULT_API_PORT_BASE + i,
                unit: Paths::node_unit_name(index),
                unit_override: String::new(),
                workdir: paths.node_workdir(index),
                last_known_pubkey: String::new(),
                last_action: String::new(),
                last_action_at: None,
            }
        })
        .collect();

    let proxy = if kind == InstallKind::ObserversSquad {
        Some(ProxyState {
            present: true,
            unit: Paths::proxy_unit_name().to_string(),
            workdir: paths.elrond_proxy_root(),
            server_port: DEFAULT_PROXY_PORT,
        })
    } else {
        None
    };

    let mut state = HostState::empty("mxnode/migrate-bash");
    state.discovered = true;
    state.install = Some(HostInstall::observed(
        kind,
        environment,
        "multiversx",
        count,
    ));
    state.nodes = nodes;
    state.proxy = proxy;
    Ok(state)
}

// ─────────────────────────────────────────────────────────────────────
// 2. variables.cfg parser
// ─────────────────────────────────────────────────────────────────────

/// Subset of bash `variables.cfg` fields we propagate into mxnode config.
/// Everything else (color codes, derived values, jq pipelines) is
/// ignored. Empty bash values map to `None` (no migration intent).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BashVariables {
    pub environment: Option<String>,
    pub custom_home: Option<PathBuf>,
    pub custom_user: Option<String>,
    pub node_keys_location: Option<PathBuf>,
    pub github_token: Option<String>,
    pub github_org: Option<String>,
    pub node_extra_flags: Option<String>,
    pub override_proxyver: Option<String>,
    pub override_configver: Option<String>,
}

/// Parse a bash `variables.cfg`, extracting the operator-customisable
/// fields. Tolerant: lines we don't recognise are ignored, and empty
/// `KEY=""` assignments map to `None` (the operator left it at default).
///
/// We only honour the first occurrence of each key — bash files are
/// sourced top-to-bottom, but the operator-edited section is always at
/// the top and any later assignment is mxnode-irrelevant tooling.
pub fn parse_variables_cfg(path: &Path) -> Result<BashVariables, MigrateError> {
    let content = fs::read_to_string(path).map_err(|e| MigrateError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let mut out = BashVariables::default();
    for raw in content.lines() {
        // Strip inline comments. Bash uses `#` outside of quoted strings;
        // variables.cfg never quotes hashes inside values for the keys
        // we care about, so a naive split is safe.
        let line = raw.split('#').next().unwrap_or(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        // Strip optional surrounding quotes — bash accepts both forms.
        let val = val.trim();
        let val = val
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| val.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(val)
            .trim();
        if val.is_empty() {
            continue;
        }
        // Skip bash expansions — they're derived values we'd evaluate
        // wrong without a shell context.
        if val.contains('$') {
            continue;
        }
        match key {
            "ENVIRONMENT" if out.environment.is_none() => {
                out.environment = Some(val.to_string());
            }
            "CUSTOM_HOME" if out.custom_home.is_none() => {
                out.custom_home = Some(PathBuf::from(val));
            }
            "CUSTOM_USER" if out.custom_user.is_none() => {
                out.custom_user = Some(val.to_string());
            }
            "NODE_KEYS_LOCATION" if out.node_keys_location.is_none() => {
                out.node_keys_location = Some(PathBuf::from(val));
            }
            "GITHUBTOKEN" if out.github_token.is_none() => {
                out.github_token = Some(val.to_string());
            }
            "GITHUB_ORG" if out.github_org.is_none() => {
                out.github_org = Some(val.to_string());
            }
            "NODE_EXTRA_FLAGS" if out.node_extra_flags.is_none() => {
                out.node_extra_flags = Some(val.to_string());
            }
            "OVERRIDE_PROXYVER" if out.override_proxyver.is_none() => {
                out.override_proxyver = Some(val.to_string());
            }
            "OVERRIDE_CONFIGVER" if out.override_configver.is_none() => {
                out.override_configver = Some(val.to_string());
            }
            _ => {}
        }
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────
// 3. .service file parser
// ─────────────────────────────────────────────────────────────────────

/// Facts extracted from a single `elrond-node-N.service` file. None of
/// these are required — a missing field just means we couldn't infer it
/// (operator hand-edited the unit, or the bash template diverged from
/// what we know).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ServiceFacts {
    pub user: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub exec_start: Option<String>,
    pub api_port: Option<u16>,
    /// Tail of `ExecStart` after the canonical bash flags. Empty when
    /// the operator did not customise. `None` when we couldn't parse
    /// the canonical prefix (operator rewrote the line).
    pub extra_flags: Option<String>,
}

/// Parse one systemd unit file. Tolerant: only `User=`, `WorkingDirectory=`
/// and `ExecStart=` are inspected.
pub fn parse_service_file(path: &Path) -> Result<ServiceFacts, MigrateError> {
    let content = fs::read_to_string(path).map_err(|e| MigrateError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let mut out = ServiceFacts::default();
    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("User=") {
            if out.user.is_none() {
                out.user = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("WorkingDirectory=") {
            if out.working_directory.is_none() {
                out.working_directory = Some(PathBuf::from(v.trim()));
            }
        } else if let Some(v) = line.strip_prefix("ExecStart=") {
            if out.exec_start.is_none() {
                let v = v.trim().to_string();
                let (port, extras) = split_canonical_exec_start(&v);
                out.api_port = port;
                out.extra_flags = extras;
                out.exec_start = Some(v);
            }
        }
    }
    Ok(out)
}

/// Split a bash-rendered `ExecStart` line into `(api_port, extra_flags)`.
///
/// The canonical bash template (functions.cfg:307) is:
///
/// ```text
/// $WORKDIR/node -use-log-view -log-logger-name -log-correlation \
///   -log-level *:DEBUG -rest-api-interface localhost:$APIPORT \
///   $NODE_EXTRA_FLAGS
/// ```
///
/// We tokenise by whitespace, find `-rest-api-interface`, parse the
/// port from `localhost:<N>`, and join everything after that pair as
/// the operator-customised tail. Returns `(None, None)` if the
/// canonical prefix is absent (operator rewrote the unit by hand).
fn split_canonical_exec_start(line: &str) -> (Option<u16>, Option<String>) {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let Some(rest_idx) = tokens.iter().position(|t| *t == "-rest-api-interface") else {
        return (None, None);
    };
    let port = tokens
        .get(rest_idx + 1)
        .and_then(|addr| addr.rsplit(':').next())
        .and_then(|s| s.parse::<u16>().ok());
    let tail: Vec<&str> = tokens.iter().skip(rest_idx + 2).copied().collect();
    let extras = tail.join(" ");
    (port, Some(extras))
}

/// Scan a directory for `elrond-node-*.service` files and parse each.
/// Returns a map keyed by node index (parsed from the filename).
/// Missing or unreadable directory → empty map (caller decides whether
/// that's a warning or fine).
pub fn scan_service_dir(dir: &Path) -> BTreeMap<u16, ServiceFacts> {
    let mut out = BTreeMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(idx_str) = name
            .strip_prefix("elrond-node-")
            .and_then(|s| s.strip_suffix(".service"))
        else {
            continue;
        };
        let Ok(idx) = idx_str.parse::<u16>() else {
            continue;
        };
        if let Ok(facts) = parse_service_file(&path) {
            out.insert(idx, facts);
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────
// 4. Migration plan + merge logic
// ─────────────────────────────────────────────────────────────────────

/// One config-merge action computed from a bash source. Used both to
/// drive the actual TOML edit and to render a human-readable dry-run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPatch {
    /// Dotted TOML path, e.g. `"network.github_org"`.
    pub key: String,
    /// New value to write (already rendered as a string for display).
    pub value: String,
    /// Where the value came from for the dry-run audit trail.
    pub source: PatchSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchSource {
    VariablesCfg,
    ServiceFile,
}

impl PatchSource {
    fn as_str(self) -> &'static str {
        match self {
            PatchSource::VariablesCfg => "variables.cfg",
            PatchSource::ServiceFile => ".service",
        }
    }
}

/// One per-node `[[nodes]]` override emitted when a service file's
/// extra_flags diverges from the global value. Role / shard mirror
/// what mxnode.toml inferred for the same node — schema requires them
/// to be set, and we don't fabricate values that contradict reality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerNodePatch {
    pub index: u16,
    pub role: String,
    pub shard: String,
    pub extra_flags: String,
    pub operation_mode: Option<String>,
}

/// Aggregated migration intent. The dry-run renderer reads this; the
/// `--execute` path applies it.
#[derive(Debug, Clone)]
pub struct MigrationPlan {
    /// Resolved bash `$CUSTOM_HOME` the plan was built from. Echoed in
    /// the dry-run header so the operator sees which path was scanned
    /// (especially useful when auto-detection picks a non-default
    /// home on hosts without a mxnode.toml).
    pub custom_home: PathBuf,
    pub state: HostState,
    pub config_patches: Vec<ConfigPatch>,
    pub per_node_patches: Vec<PerNodePatch>,
    /// Plain warnings (count mismatches, parse oddities) for the
    /// operator. Never fatal.
    pub warnings: Vec<String>,
    /// Token surfaced back to the operator (never written to disk).
    /// `None` when variables.cfg had no token.
    pub github_token: Option<String>,
}

/// Build the full migration plan. None of the optional sources
/// (variables.cfg, systemd dir) need to exist — the function degrades
/// gracefully and surfaces a warning per missing input.
pub fn build_migration_plan(
    custom_home: &Path,
    scripts_dir: Option<&Path>,
    systemd_dir: &Path,
    existing_config: &MxnodeFile,
) -> Result<MigrationPlan, MigrateError> {
    let mut state = infer_state_from_bash(custom_home)?;
    let mut warnings = Vec::new();

    // 1. variables.cfg → config patches
    let mut config_patches = Vec::new();
    let mut github_token = None;
    let bash_vars = if let Some(scripts_dir) = scripts_dir {
        let path = scripts_dir.join("config").join("variables.cfg");
        if path.exists() {
            match parse_variables_cfg(&path) {
                Ok(vars) => Some(vars),
                Err(e) => {
                    warnings.push(format!("variables.cfg parse failed: {e}"));
                    None
                }
            }
        } else {
            warnings.push(format!(
                "variables.cfg not found at {} (skipping config merge)",
                path.display()
            ));
            None
        }
    } else {
        None
    };

    let defaults = MxnodeFile::default();
    if let Some(ref vars) = bash_vars {
        github_token = vars.github_token.clone();
        plan_variables_cfg_patches(
            vars,
            existing_config,
            &defaults,
            &mut config_patches,
            &mut warnings,
        );
    }

    // 2. service files → per-node extra_flags overrides + cross-source
    //    fallback for `paths.custom_user` when variables.cfg is absent
    //    or didn't set it.
    let services = scan_service_dir(systemd_dir);
    apply_prefs_display_names(&mut state, &services, &mut warnings);
    plan_service_file_patches(&services, existing_config, &defaults, &mut config_patches);
    if services.is_empty() {
        warnings.push(format!(
            "no elrond-node-*.service found under {} (skipping per-node overrides)",
            systemd_dir.display()
        ));
    } else if services.len() != state.nodes.len() {
        warnings.push(format!(
            "sentinels declare {} nodes but {} service files exist under {}",
            state.nodes.len(),
            services.len(),
            systemd_dir.display(),
        ));
    }

    // The "global" extra_flags after the merge: prefer the bash
    // NODE_EXTRA_FLAGS (which is the user's intended default) over
    // mxnode's existing value, when the latter is at the schema
    // default. Otherwise mxnode wins (operator already customised).
    let mut global_runtime = effective_global_runtime(&bash_vars, existing_config, &defaults);
    if global_runtime.operation_mode.is_none()
        && existing_config.node.operation_mode == defaults.node.operation_mode
    {
        if let Some(mode) = common_service_operation_mode(&state, &services, &mut warnings) {
            config_patches.push(ConfigPatch {
                key: "node.operation_mode".to_string(),
                value: mode.clone(),
                source: PatchSource::ServiceFile,
            });
            global_runtime.operation_mode = Some(mode);
        }
    }
    let mut per_node_patches = Vec::new();
    for (idx, facts) in &services {
        let Some(unit_extras) = facts.extra_flags.as_deref() else {
            warnings.push(format!(
                "elrond-node-{idx}.service: ExecStart did not match the canonical bash template (skipping)"
            ));
            continue;
        };
        let normalized = normalize_operation_mode_flags(unit_extras);
        for warning in normalized.warnings {
            warnings.push(format!("elrond-node-{idx}.service: {warning}"));
        }
        let trimmed = normalized.extra_flags.trim();
        // Mirror the inferred state's role/shard for this index so the
        // [[nodes]] override doesn't contradict mxnode.toml. If the
        // service file exists for an index outside mxnode.toml's range
        // (e.g. operator added a node without updating .numberofnodes),
        // skip it — emitting a half-populated override would lie.
        let Some(node_state) = state.nodes.iter().find(|n| n.index.get() == *idx) else {
            warnings.push(format!(
                "elrond-node-{idx}.service: no matching node in sentinels (skipping per-node override)"
            ));
            continue;
        };
        let operation_mode = normalized.operation_mode;
        if trimmed.is_empty() && operation_mode == global_runtime.operation_mode {
            continue;
        }
        if trimmed == global_runtime.extra_flags.trim()
            && operation_mode == global_runtime.operation_mode
        {
            continue;
        }
        per_node_patches.push(PerNodePatch {
            index: *idx,
            role: node_state.role.as_str().to_string(),
            shard: shard_serde_name(node_state.shard).to_string(),
            extra_flags: trimmed.to_string(),
            operation_mode,
        });
    }

    Ok(MigrationPlan {
        custom_home: custom_home.to_path_buf(),
        state,
        config_patches,
        per_node_patches,
        warnings,
        github_token,
    })
}

fn apply_prefs_display_names(
    state: &mut HostState,
    services: &BTreeMap<u16, ServiceFacts>,
    warnings: &mut Vec<String>,
) {
    for node in &mut state.nodes {
        let idx = node.index.get();
        let workdir = services
            .get(&idx)
            .and_then(|facts| facts.working_directory.as_ref())
            .unwrap_or(&node.workdir);
        let prefs_path = workdir.join("config").join("prefs.toml");
        match read_node_display_name_from_prefs(&prefs_path) {
            Ok(Some(name)) => node.display_name = name,
            Ok(None) => {}
            Err(e) => warnings.push(format!("{}: {e}", prefs_path.display())),
        }
    }
}

fn read_node_display_name_from_prefs(path: &Path) -> Result<Option<String>, String> {
    let body = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not read prefs.toml: {e}")),
    };
    extract_node_display_name_from_prefs(&body)
}

fn extract_node_display_name_from_prefs(body: &str) -> Result<Option<String>, String> {
    let mut in_preferences = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') {
            let header = trimmed.split('#').next().unwrap_or(trimmed).trim();
            in_preferences = header == "[Preferences]";
            continue;
        }
        if !in_preferences {
            continue;
        }

        let Some((key, raw_value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() != "NodeDisplayName" {
            continue;
        }

        let value_doc = format!("value = {}", raw_value.trim());
        let parsed: toml::Value = value_doc
            .parse()
            .map_err(|e| format!("could not parse Preferences.NodeDisplayName: {e}"))?;
        return Ok(parsed
            .get("value")
            .and_then(|name| name.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned));
    }
    Ok(None)
}

/// Compute config patches from variables.cfg, only filling fields that
/// are still at the schema default in the operator's existing config.
fn plan_variables_cfg_patches(
    vars: &BashVariables,
    existing: &MxnodeFile,
    defaults: &MxnodeFile,
    patches: &mut Vec<ConfigPatch>,
    warnings: &mut Vec<String>,
) {
    if let Some(ref env) = vars.environment {
        // Only propagate when mxnode hasn't been told about a network
        // yet. We accept mainnet/testnet/devnet only — bash uses the
        // same names. An unknown bash ENVIRONMENT would have already
        // failed in `infer_state_from_bash`, so we don't double-validate.
        if existing.network.environment == defaults.network.environment
            && matches!(env.as_str(), "mainnet" | "testnet" | "devnet")
        {
            patches.push(ConfigPatch {
                key: "network.environment".to_string(),
                value: env.clone(),
                source: PatchSource::VariablesCfg,
            });
        }
    }
    if let Some(ref custom_home) = vars.custom_home {
        if existing.paths.custom_home == defaults.paths.custom_home {
            patches.push(ConfigPatch {
                key: "paths.custom_home".to_string(),
                value: custom_home.display().to_string(),
                source: PatchSource::VariablesCfg,
            });
        }
    }
    if let Some(ref custom_user) = vars.custom_user {
        if existing.paths.custom_user == defaults.paths.custom_user {
            patches.push(ConfigPatch {
                key: "paths.custom_user".to_string(),
                value: custom_user.clone(),
                source: PatchSource::VariablesCfg,
            });
        }
    }
    if let Some(ref node_keys) = vars.node_keys_location {
        if existing.paths.node_keys == defaults.paths.node_keys {
            patches.push(ConfigPatch {
                key: "paths.node_keys".to_string(),
                value: node_keys.display().to_string(),
                source: PatchSource::VariablesCfg,
            });
        }
    }
    if let Some(ref org) = vars.github_org {
        // Bash's "GITHUB_ORG=multiversx" is the implicit default when
        // unset. Don't bother writing it — schema default already
        // matches.
        if org != "multiversx" && existing.network.github_org == defaults.network.github_org {
            patches.push(ConfigPatch {
                key: "network.github_org".to_string(),
                value: org.clone(),
                source: PatchSource::VariablesCfg,
            });
        }
    }
    if let Some(ref flags) = vars.node_extra_flags {
        let normalized = normalize_operation_mode_flags(flags);
        for warning in normalized.warnings {
            warnings.push(format!("variables.cfg NODE_EXTRA_FLAGS: {warning}"));
        }
        if existing.node.extra_flags == defaults.node.extra_flags
            && !normalized.extra_flags.trim().is_empty()
        {
            patches.push(ConfigPatch {
                key: "node.extra_flags".to_string(),
                value: normalized.extra_flags,
                source: PatchSource::VariablesCfg,
            });
        }
        if existing.node.operation_mode == defaults.node.operation_mode {
            if let Some(mode) = normalized.operation_mode {
                patches.push(ConfigPatch {
                    key: "node.operation_mode".to_string(),
                    value: mode,
                    source: PatchSource::VariablesCfg,
                });
            }
        }
    }
    if let Some(ref ver) = vars.override_proxyver {
        if existing.overrides.proxyver.is_empty() {
            patches.push(ConfigPatch {
                key: "overrides.proxyver".to_string(),
                value: ver.clone(),
                source: PatchSource::VariablesCfg,
            });
        }
    }
    if let Some(ref ver) = vars.override_configver {
        if existing.overrides.configver.is_empty() {
            patches.push(ConfigPatch {
                key: "overrides.configver".to_string(),
                value: ver.clone(),
                source: PatchSource::VariablesCfg,
            });
        }
    }
}

/// `Shard::as_str()` returns the proxy-config form (`"0"`, `"1"`,
/// `"2"`); the serde representation written to / read from TOML files
/// is the lowercase variant name (`"zero"`, `"one"`, `"two"`). Per-node
/// overrides go through serde, so we need this mapping.
fn shard_serde_name(shard: Shard) -> &'static str {
    match shard {
        Shard::Zero => "zero",
        Shard::One => "one",
        Shard::Two => "two",
        Shard::Metachain => "metachain",
        Shard::Disabled => "disabled",
        Shard::Auto => "auto",
    }
}

/// Patches we can derive from the systemd unit files alone (when
/// variables.cfg is missing or didn't set the field). Only `User=`
/// becomes a config patch — `WorkingDirectory` is too dependent on the
/// node-N suffix bash adds, and we already capture `ExecStart` extras
/// as per-node `[[nodes]]` overrides elsewhere.
///
/// We require all service files to agree on the `User=` value before
/// emitting a patch — disagreement means the operator hand-edited
/// somewhere and we should let them resolve it.
fn plan_service_file_patches(
    services: &BTreeMap<u16, ServiceFacts>,
    existing: &MxnodeFile,
    defaults: &MxnodeFile,
    patches: &mut Vec<ConfigPatch>,
) {
    if existing.paths.custom_user != defaults.paths.custom_user {
        return;
    }
    if patches.iter().any(|p| p.key == "paths.custom_user") {
        return;
    }
    let users: Vec<&str> = services
        .values()
        .filter_map(|f| f.user.as_deref())
        .collect();
    if users.is_empty() {
        return;
    }
    let first = users[0];
    if !users.iter().all(|u| *u == first) {
        return;
    }
    patches.push(ConfigPatch {
        key: "paths.custom_user".to_string(),
        value: first.to_string(),
        source: PatchSource::ServiceFile,
    });
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RuntimeFlags {
    extra_flags: String,
    operation_mode: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct NormalizedOperationFlags {
    extra_flags: String,
    operation_mode: Option<String>,
    warnings: Vec<String>,
}

/// What global runtime flags will be after the merge — used when
/// deciding whether service-file values diverge enough to need a
/// `[[nodes]]` override.
fn effective_global_runtime(
    bash_vars: &Option<BashVariables>,
    existing: &MxnodeFile,
    defaults: &MxnodeFile,
) -> RuntimeFlags {
    let from_bash = bash_vars
        .as_ref()
        .and_then(|v| v.node_extra_flags.as_deref())
        .map(normalize_operation_mode_flags)
        .unwrap_or_default();
    let extra_flags = if existing.node.extra_flags != defaults.node.extra_flags {
        existing.node.extra_flags.clone()
    } else {
        from_bash.extra_flags
    };
    let operation_mode = if existing.node.operation_mode != defaults.node.operation_mode {
        existing.node.operation_mode.clone()
    } else {
        from_bash.operation_mode
    };
    RuntimeFlags {
        extra_flags,
        operation_mode,
    }
}

fn common_service_operation_mode(
    state: &HostState,
    services: &BTreeMap<u16, ServiceFacts>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let mut common: Option<String> = None;
    for node in &state.nodes {
        let facts = services.get(&node.index.get())?;
        let extras = facts.extra_flags.as_deref()?;
        let normalized = normalize_operation_mode_flags(extras);
        for warning in normalized.warnings {
            warnings.push(format!(
                "elrond-node-{}.service: {warning}",
                node.index.get()
            ));
        }
        let mode = normalized.operation_mode?;
        match common.as_deref() {
            None => common = Some(mode),
            Some(existing) if existing == mode => {}
            Some(_) => return None,
        }
    }
    common
}

const VALID_OPERATION_MODES: &[&str] = &[
    "full-archive",
    "db-lookup-extension",
    "historical-balances",
    "snapshotless-observer",
];

fn normalize_operation_mode_flags(flags: &str) -> NormalizedOperationFlags {
    let mut out = Vec::new();
    let mut operation_mode: Option<String> = None;
    let mut warnings = Vec::new();
    let tokens: Vec<&str> = flags.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        if matches!(token, "-operation-mode" | "--operation-mode") {
            let Some(value) = tokens.get(i + 1).copied() else {
                warnings.push(
                    "operation-mode flag has no value; leaving it in extra_flags".to_string(),
                );
                out.push(token);
                i += 1;
                continue;
            };
            if record_operation_mode(token, value, &mut operation_mode, &mut warnings) {
                i += 2;
                continue;
            }
            out.push(token);
            out.push(value);
            i += 2;
            continue;
        }
        if let Some(value) = token
            .strip_prefix("-operation-mode=")
            .or_else(|| token.strip_prefix("--operation-mode="))
        {
            if record_operation_mode(token, value, &mut operation_mode, &mut warnings) {
                i += 1;
                continue;
            }
        }
        out.push(token);
        i += 1;
    }
    NormalizedOperationFlags {
        extra_flags: out.join(" "),
        operation_mode,
        warnings,
    }
}

fn record_operation_mode(
    raw: &str,
    value: &str,
    operation_mode: &mut Option<String>,
    warnings: &mut Vec<String>,
) -> bool {
    if !VALID_OPERATION_MODES.contains(&value) {
        warnings.push(format!(
            "unknown operation mode {value:?}; leaving {raw:?} in extra_flags"
        ));
        return false;
    }
    match operation_mode.as_deref() {
        None => {
            *operation_mode = Some(value.to_string());
            true
        }
        Some(existing) if existing == value => true,
        Some(existing) => {
            warnings.push(format!(
                "multiple operation modes ({existing:?}, {value:?}); leaving {raw:?} in extra_flags"
            ));
            false
        }
    }
}

/// Best-effort discovery of the bash `$CUSTOM_HOME` when no
/// `mxnode.toml` exists and the operator hasn't passed `--from`. Tried
/// in order:
///
///   1. `home` itself if it carries the bash sentinel `.installedenv`.
///   2. `CUSTOM_HOME` declared in `<home>/mx-chain-scripts/config/variables.cfg`
///      (handles the case where bash was installed under a different
///      account than the one running mxnode).
///   3. `home` as a final fallback so the downstream "missing
///      `.installedenv`" error points at a path the operator can
///      reason about, not the schema default `/home/ubuntu`.
fn detect_custom_home_in(home: &Path) -> PathBuf {
    if home.join(".installedenv").is_file() {
        return home.to_path_buf();
    }
    let cfg = home.join("mx-chain-scripts/config/variables.cfg");
    if cfg.is_file() {
        if let Ok(vars) = parse_variables_cfg(&cfg) {
            if let Some(custom) = vars.custom_home {
                return custom;
            }
        }
    }
    home.to_path_buf()
}

/// Convenience wrapper that resolves `$HOME` at the env boundary.
/// Returns `None` only when `$HOME` is unset (rare on the systems the
/// migrate command targets).
fn auto_detect_custom_home() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(detect_custom_home_in(&home))
}

// ─────────────────────────────────────────────────────────────────────
// 5. CLI plumbing
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct MigrateBashArgs {
    /// Path to the bash `$CUSTOM_HOME` to import. Defaults to
    /// `paths.custom_home` from the resolved mxnode config (mirrors the
    /// bash `CUSTOM_HOME` variable).
    #[arg(long, value_name = "PATH")]
    pub from: Option<PathBuf>,

    /// Path to the cloned `mx-chain-scripts` directory (the one that
    /// contains `config/variables.cfg`). Defaults to
    /// `<custom_home>/mx-chain-scripts`. Pass `--no-scripts` to skip
    /// the variables.cfg scan entirely.
    #[arg(long, value_name = "PATH")]
    pub scripts_dir: Option<PathBuf>,

    /// Skip the variables.cfg scan even if a scripts directory exists.
    /// Useful when migrating from a host where the bash repo has been
    /// removed but the install is still live.
    #[arg(long)]
    pub no_scripts: bool,

    /// Directory holding the rendered systemd unit files. Defaults to
    /// `/etc/systemd/system`. Override for tests or non-standard hosts.
    #[arg(long, value_name = "PATH", default_value = "/etc/systemd/system")]
    pub systemd_dir: PathBuf,

    /// Apply the migration. Without this flag, the inferred plan is
    /// printed and neither `mxnode.toml` nor `mxnode.toml` is modified.
    #[arg(long)]
    pub execute: bool,
}

/// Import an existing bash install into mxnode's `mxnode.toml` (and
/// optionally merge variables.cfg + service-file findings into the
/// user's `mxnode.toml`). Dry-run by default; pass `--execute` to
/// persist. Bash files on disk are never modified.
///
/// We deliberately do NOT go through `Runtime::from_global` here:
/// that path triggers auto-init when no mxnode.toml exists, which
/// would seed the file with `$USER`/`$HOME`-based defaults BEFORE
/// migrate runs — and the merge rule "only fill schema-default
/// fields" would then incorrectly skip those values, leaving the
/// host's bash-derived install paths unmigrated. Instead, we load
/// any existing mxnode.toml directly (no auto-init), and treat its
/// absence as "fresh host → use schema defaults as the baseline."
pub fn run(args: MigrateBashArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let opts = LoadOptions {
        config_path: global.config.clone(),
        flags_overlay: None,
    };
    let loaded = load(&opts).map_err(|e| {
        CliError::new(
            "failed to load config",
            e.to_string(),
            "fix the file at the path shown above, or pass --config <PATH>",
        )
        .json_if(global.json)
    })?;
    let paths = resolve_paths(&loaded.file).map_err(|e| {
        CliError::new(
            "failed to resolve filesystem paths",
            e.to_string(),
            "fix paths.* in your config — or pass --from to point at the bash CUSTOM_HOME",
        )
        .json_if(global.json)
    })?;

    // For the merge baseline, use the schema default whenever no
    // config file exists (ConfigSource::None). Auto-init has not run,
    // so the loaded `config` is already at schema defaults — this is
    // primarily to make the intent explicit.
    let baseline_config: MxnodeFile = match loaded.source {
        ConfigSource::None => MxnodeFile::default(),
        _ => loaded.file.clone(),
    };

    // Resolution order for the bash $CUSTOM_HOME:
    //
    //   1. `--from` if the operator passed it (always wins).
    //   2. `paths.custom_home` from the loaded config — but **only**
    //      if it actually hosts a bash install (`.installedenv`
    //      present). The schema default `/home/ubuntu` never matches
    //      reality on hosts with a different service account, and any
    //      previous `mxnode <cmd>` will have auto-init'd a config
    //      with that default in place — so blindly trusting
    //      `paths.custom_home` here defeats the auto-detect path.
    //   3. Auto-detect: `$HOME` directly, then parse
    //      `$HOME/mx-chain-scripts/config/variables.cfg` for an
    //      explicit `CUSTOM_HOME` (the bash installer always clones
    //      its scripts repo into the user's home).
    //   4. Last resort: `paths.custom_home` even without sentinels —
    //      `infer_state_from_bash` will surface a clear "missing
    //      .installedenv" error pointing at it.
    let custom_home = args.from.clone().unwrap_or_else(|| {
        if paths.custom_home.join(".installedenv").is_file() {
            return paths.custom_home.clone();
        }
        if let Some(detected) = auto_detect_custom_home() {
            return detected;
        }
        paths.custom_home.clone()
    });

    let scripts_dir = if args.no_scripts {
        None
    } else {
        Some(
            args.scripts_dir
                .clone()
                .unwrap_or_else(|| custom_home.join("mx-chain-scripts")),
        )
    };

    let plan = build_migration_plan(
        &custom_home,
        scripts_dir.as_deref(),
        &args.systemd_dir,
        &baseline_config,
    )
    .map_err(|e| {
        CliError::new(
            "could not infer mxnode state from bash install",
            e.to_string(),
            "verify .installedenv and .numberofnodes exist under the path passed via --from",
        )
        .json_if(global.json)
    })?;

    if !args.execute {
        print_dry_run(&plan, global);
        return Ok(());
    }

    // ── apply mxnode.toml ──
    let store = mxnode_state::StateStore::new(&paths.config_dir);
    if store.host_initialized() {
        return Err(CliError::new(
            "refusing to overwrite existing mxnode.toml",
            format!("{} already exists", store.state_path().display()),
            "delete or move the existing mxnode.toml first, or run on a fresh host",
        )
        .json_if(global.json));
    }
    let guard = store.lock().map_err(|e| {
        CliError::new(
            "failed to acquire mxnode.toml lock",
            e.to_string(),
            "ensure no other mxnode invocation is running, then retry",
        )
        .json_if(global.json)
    })?;
    store.save(&plan.state, &guard).map_err(|e| {
        CliError::new(
            "failed to write mxnode.toml",
            e.to_string(),
            "ensure mxnode has write access to the state directory",
        )
        .json_if(global.json)
    })?;
    drop(guard);

    // ── apply config + secrets patches (best-effort, non-fatal) ──
    // The unified mxnode.toml now holds the operator sections AND the
    // secrets, so we route both through the same toml_edit pass to
    // preserve operator comments / section ordering.
    let needs_doc_edit = !plan.config_patches.is_empty()
        || !plan.per_node_patches.is_empty()
        || plan.github_token.is_some();
    let config_changes = if needs_doc_edit {
        match apply_config_patches(
            &plan.config_patches,
            &plan.per_node_patches,
            plan.github_token.as_deref(),
        ) {
            Ok(path) => Some(path),
            Err(e) => {
                eprintln!("warn: failed to apply mxnode.toml merge: {e}");
                None
            }
        }
    } else {
        None
    };

    print_apply_summary(&plan, store.state_path(), config_changes.as_deref(), global);
    Ok(())
}

/// Merge config + per-node + secrets patches into the unified
/// `mxnode.toml`. Comments and section ordering are preserved by
/// routing through `toml_edit::DocumentMut`, same as `mxnode config set`.
///
/// The file is held at mode 0600. We re-apply that on every write so a
/// concurrent loose-mode flip (operator running `chmod`, rogue
/// toolchain) gets corrected here too.
fn apply_config_patches(
    patches: &[ConfigPatch],
    per_node: &[PerNodePatch],
    github_token: Option<&str>,
) -> Result<PathBuf, String> {
    let target = user_config_path_or_default().map_err(|e| e.to_string())?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let body = if target.exists() {
        fs::read_to_string(&target).map_err(|e| format!("{}: {e}", target.display()))?
    } else {
        "# mxnode unified file — generated by `mxnode import-bash`.\nschema_version = 1\n"
            .to_string()
    };
    let mut doc: DocumentMut = body
        .parse()
        .map_err(|e: toml_edit::TomlError| format!("{}: {e}", target.display()))?;

    for patch in patches {
        write_dotted(&mut doc, &patch.key, value(patch.value.clone()));
    }
    if !per_node.is_empty() {
        merge_node_overrides(&mut doc, per_node);
    }
    if let Some(tok) = github_token {
        write_dotted(&mut doc, "secrets.github_token", value(tok.to_string()));
    }

    fs::write(&target, doc.to_string()).map_err(|e| format!("{}: {e}", target.display()))?;
    // Re-tighten the mode in case the existing file was at 0644 and our
    // write preserved that. Best effort — the StateStore loader auto-
    // tightens too, but doing it here keeps the on-disk state correct
    // even before the next load.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&target, perms);
    }
    Ok(target)
}

/// Append `[[nodes]]` array-of-tables entries (one per index that
/// diverges from the global). Entries that already exist for the same
/// index get their `extra_flags` updated; new indices are pushed.
fn merge_node_overrides(doc: &mut DocumentMut, per_node: &[PerNodePatch]) {
    use toml_edit::{ArrayOfTables, Item, Table};

    // Ensure a `nodes` array-of-tables exists.
    if !matches!(doc.get("nodes"), Some(Item::ArrayOfTables(_))) {
        doc.insert("nodes", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let Some(Item::ArrayOfTables(arr)) = doc.get_mut("nodes") else {
        return;
    };

    for patch in per_node {
        // Find an existing entry with this index, or push a new one.
        let mut found = false;
        for entry in arr.iter_mut() {
            let same_index = entry
                .get("index")
                .and_then(|v| v.as_integer())
                .map(|n| n as u16)
                == Some(patch.index);
            if same_index {
                entry.insert("extra_flags", value(patch.extra_flags.clone()));
                if let Some(mode) = &patch.operation_mode {
                    entry.insert("operation_mode", value(mode.clone()));
                }
                found = true;
                break;
            }
        }
        if !found {
            let mut t = Table::new();
            t.insert("index", value(patch.index as i64));
            // role / shard mirror what mxnode.toml inferred for this
            // node — never placeholders, so the override never
            // contradicts mxnode.toml. NodeOverride's schema requires
            // both fields; the operator can refine later if needed.
            t.insert("role", value(patch.role.clone()));
            t.insert("shard", value(patch.shard.clone()));
            t.insert("display_name", value(""));
            t.insert("extra_flags", value(patch.extra_flags.clone()));
            if let Some(mode) = &patch.operation_mode {
                t.insert("operation_mode", value(mode.clone()));
            }
            arr.push(t);
        }
    }
}

/// Write a TOML scalar at a dotted key (`"a.b.c"`), creating tables as
/// needed. Lifted from `commands/config.rs::write_dotted` so migrate
/// stays self-contained.
fn write_dotted(doc: &mut DocumentMut, dotted: &str, item: toml_edit::Item) {
    let segments: Vec<&str> = dotted.split('.').collect();
    if segments.is_empty() {
        return;
    }
    if segments.len() == 1 {
        doc[segments[0]] = item;
        return;
    }
    let head = segments[0];
    if !doc.as_table().contains_key(head) || !doc[head].is_table() {
        doc[head] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let mut cursor = &mut doc[head];
    for seg in &segments[1..segments.len() - 1] {
        let cur_tbl = cursor.as_table_mut().expect("intermediate is a table");
        if !cur_tbl.contains_key(seg) || !cur_tbl[seg].is_table() {
            cur_tbl.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        cursor = &mut cur_tbl[seg];
    }
    let leaf = segments[segments.len() - 1];
    let cur_tbl = cursor.as_table_mut().expect("parent is a table");
    cur_tbl.insert(leaf, item);
}

// ─────────────────────────────────────────────────────────────────────
// 6. Output rendering
// ─────────────────────────────────────────────────────────────────────

/// Mask all but the first 4 + last 0 chars of the token. Conservative
/// — better that operators verify by other means than for the dashboard
/// to ever leak the full secret.
fn mask_token(token: &str) -> String {
    let visible = token.chars().take(4).collect::<String>();
    let tail_len = token.chars().count().saturating_sub(4);
    format!("{visible}{}", "*".repeat(tail_len.min(32)))
}

fn print_dry_run(plan: &MigrationPlan, global: &GlobalArgs) {
    if global.json {
        let body = serde_json::json!({
            "mode": "dry-run",
            "custom_home": plan.custom_home.display().to_string(),
            "node_count": plan.state.nodes.len(),
            "kind": plan.state.install.as_ref().map(|i| i.kind.as_str()),
            "environment": plan.state.install.as_ref().map(|i| i.environment.as_str()),
            "proxy": plan.state.proxy.is_some(),
            "nodes": plan.state.nodes.iter().map(|n| serde_json::json!({
                "index": n.index.get(),
                "role": n.role.as_str(),
                "shard": n.shard.as_str(),
                "display_name": n.display_name,
                "unit": n.unit,
                "api_port": n.api_port,
            })).collect::<Vec<_>>(),
            "config_patches": plan.config_patches.iter().map(|p| serde_json::json!({
                "key": p.key,
                "value": p.value,
                "source": p.source.as_str(),
            })).collect::<Vec<_>>(),
            "per_node_patches": plan.per_node_patches.iter().map(|p| serde_json::json!({
                "index": p.index,
                "extra_flags": p.extra_flags,
                "operation_mode": p.operation_mode.as_deref(),
            })).collect::<Vec<_>>(),
            "github_token_present": plan.github_token.is_some(),
            "warnings": plan.warnings,
        });
        println!("{body}");
        return;
    }
    println!("dry-run — pass --execute to apply");
    println!(
        "  source: {} (use --from to override)",
        plan.custom_home.display()
    );
    if let Some(install) = plan.state.install.as_ref() {
        println!(
            "  inferred {} nodes, kind={}, env={}",
            install.node_count, install.kind, install.environment,
        );
    }
    for n in &plan.state.nodes {
        println!(
            "  node-{}: {} on {} ({}) display_name={:?}",
            n.index.get(),
            n.role,
            n.shard,
            n.unit,
            n.display_name,
        );
    }
    if plan.state.proxy.is_some() {
        println!("  + proxy");
    }
    if !plan.config_patches.is_empty() {
        println!();
        println!("mxnode.toml patches (only fields still at default):");
        for p in &plan.config_patches {
            println!(
                "  {} = {:?}    (from {})",
                p.key,
                p.value,
                p.source.as_str()
            );
        }
    }
    if !plan.per_node_patches.is_empty() {
        println!();
        println!("per-node runtime overrides (from .service files):");
        for p in &plan.per_node_patches {
            println!(
                "  node-{}: extra_flags = {:?}, operation_mode = {:?}",
                p.index, p.extra_flags, p.operation_mode,
            );
        }
    }
    if let Some(token) = &plan.github_token {
        println!();
        println!("found GITHUBTOKEN in variables.cfg → will write to [secrets].github_token:");
        println!("  {}", mask_token(token));
        println!("  the file is held at mode 0600; export MXNODE_GITHUB_TOKEN");
        println!("  to override at runtime without editing the file.");
    }
    if !plan.warnings.is_empty() {
        println!();
        println!("warnings:");
        for w in &plan.warnings {
            println!("  - {w}");
        }
    }
}

fn print_apply_summary(
    plan: &MigrationPlan,
    state_path: &Path,
    config_path: Option<&Path>,
    global: &GlobalArgs,
) {
    if global.json {
        let body = serde_json::json!({
            "ok": true,
            "wrote_state": state_path.display().to_string(),
            "wrote_config": config_path.map(|p| p.display().to_string()),
            "node_count": plan.state.nodes.len(),
            "config_patches_applied": plan.config_patches.len(),
            "per_node_patches_applied": plan.per_node_patches.len(),
            "github_token_present": plan.github_token.is_some(),
            "warnings": plan.warnings,
        });
        println!("{body}");
        return;
    }
    println!("wrote {}", state_path.display());
    if let Some(p) = config_path {
        println!(
            "merged variables.cfg / service files into {} ({} field(s), {} per-node)",
            p.display(),
            plan.config_patches.len(),
            plan.per_node_patches.len(),
        );
    }
    if let Some(token) = &plan.github_token {
        println!();
        println!(
            "wrote GITHUBTOKEN → [secrets].github_token in {} (mode 0600)",
            config_path
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<config>".to_string()),
        );
        println!("  masked: {}", mask_token(token));
        println!("  override at runtime with: export MXNODE_GITHUB_TOKEN='<token>'");
    }
    if !plan.warnings.is_empty() {
        println!();
        for w in &plan.warnings {
            println!("warn: {w}");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// 7. Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_fixture(kind: &str, count: u16, env: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".installedenv"), env).unwrap();
        fs::write(dir.path().join(".numberofnodes"), count.to_string()).unwrap();
        if !kind.is_empty() {
            fs::write(dir.path().join(".squad_install"), kind).unwrap();
        }
        dir
    }

    fn write_prefs(custom_home: &Path, idx: u16, name: &str) {
        let config_dir = custom_home
            .join("elrond-nodes")
            .join(format!("node-{idx}"))
            .join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("prefs.toml"),
            format!(
                r#"[Preferences]
   DestinationShardAsObserver = "0"
   NodeDisplayName = "{name}"
   Identity = "BOBER"
   RedundancyLevel = 1
"#,
            ),
        )
        .unwrap();
    }

    // ── sentinel-only inference (existing behaviour, unchanged) ──

    #[test]
    fn infers_observers_squad_with_proxy() {
        let dir = bash_fixture("Observers Squad", 4, "mainnet");
        let state = infer_state_from_bash(dir.path()).unwrap();
        assert_eq!(state.nodes.len(), 4);
        let install = state.install.as_ref().expect("install present");
        assert_eq!(install.environment, Environment::Mainnet);
        assert_eq!(install.kind, InstallKind::ObserversSquad);
        assert_eq!(install.node_count, 4);
        assert!(state.proxy.is_some());
        assert_eq!(state.nodes[3].shard, Shard::Metachain);
        assert_eq!(state.nodes[0].shard, Shard::Zero);
        assert_eq!(state.nodes[0].role, Role::Observer);
        assert_eq!(state.nodes[0].api_port, 8080);
        assert_eq!(state.nodes[0].unit, "elrond-node-0.service");
        assert!(state.discovered);
    }

    #[test]
    fn infers_multikey_squad_without_proxy() {
        let dir = bash_fixture("Multikey Squad", 4, "testnet");
        let state = infer_state_from_bash(dir.path()).unwrap();
        let install = state.install.as_ref().expect("install present");
        assert_eq!(install.kind, InstallKind::MultikeySquad);
        assert!(state.proxy.is_none());
        assert_eq!(state.nodes[0].role, Role::Multikey);
    }

    #[test]
    fn infers_validators_without_squad_file() {
        let dir = bash_fixture("", 2, "devnet");
        let state = infer_state_from_bash(dir.path()).unwrap();
        let install = state.install.as_ref().expect("install present");
        assert_eq!(install.kind, InstallKind::Validators);
        assert_eq!(install.environment, Environment::Devnet);
        assert!(state.proxy.is_none());
        assert_eq!(state.nodes[0].role, Role::Validator);
        assert_eq!(state.nodes[0].shard, Shard::Auto);
    }

    #[test]
    fn errors_when_not_a_bash_install() {
        let dir = tempfile::tempdir().unwrap();
        let err = infer_state_from_bash(dir.path()).unwrap_err();
        assert!(matches!(err, MigrateError::NotBash(".installedenv")));
    }

    #[test]
    fn detect_custom_home_returns_home_when_sentinels_live_directly_in_it() {
        // bash installer puts .installedenv straight under $HOME →
        // detect should pick $HOME without consulting variables.cfg.
        let home = bash_fixture("Multikey Squad", 4, "mainnet");
        assert_eq!(
            detect_custom_home_in(home.path()),
            home.path().to_path_buf()
        );
    }

    #[test]
    fn detect_custom_home_reads_variables_cfg_when_home_is_not_the_install_path() {
        // bash was installed under a different account but the
        // variables.cfg lives under $HOME (operator cloned the scripts
        // into their own home). Pick CUSTOM_HOME from there.
        let home = tempfile::tempdir().unwrap();
        let scripts = home.path().join("mx-chain-scripts/config");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(
            scripts.join("variables.cfg"),
            r#"CUSTOM_HOME="/srv/mxnode""#,
        )
        .unwrap();
        assert_eq!(
            detect_custom_home_in(home.path()),
            PathBuf::from("/srv/mxnode")
        );
    }

    #[test]
    fn detect_custom_home_falls_back_to_home_when_no_signals_present() {
        // No .installedenv, no variables.cfg. Returning $HOME makes the
        // downstream "missing .installedenv" error point at a path the
        // operator can recognise (vs. the schema default /home/ubuntu).
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            detect_custom_home_in(home.path()),
            home.path().to_path_buf()
        );
    }

    #[test]
    fn errors_on_unknown_environment() {
        let dir = bash_fixture("", 1, "mythicnet");
        let err = infer_state_from_bash(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            MigrateError::Parse {
                field: ".installedenv",
                ..
            }
        ));
    }

    #[test]
    fn install_versions_are_left_unset_on_import() {
        let dir = bash_fixture("Observers Squad", 4, "mainnet");
        let state = infer_state_from_bash(dir.path()).unwrap();
        let install = state.install.as_ref().unwrap();
        assert!(install.versions.config_tag.is_none());
        assert!(install.versions.binary_tag.is_none());
        assert!(install.versions.proxy_tag.is_none());
        assert!(install.versions.go_version.is_empty());
    }

    // ── variables.cfg parsing ──

    #[test]
    fn parses_variables_cfg_realistic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("variables.cfg");
        fs::write(
            &path,
            r#"#!/bin/bash
ENVIRONMENT="mainnet"
CUSTOM_HOME="/srv/multiversx"
CUSTOM_USER="mvx"
NODE_KEYS_LOCATION="/srv/multiversx/keys"
GITHUBTOKEN="ghp_abcdefghijklmno"
NODE_EXTRA_FLAGS="-display-name custom-name -profile-mode true"
GITHUB_ORG="myfork"
OVERRIDE_PROXYVER="v1.2.3"
OVERRIDE_CONFIGVER=""
"#,
        )
        .unwrap();
        let v = parse_variables_cfg(&path).unwrap();
        assert_eq!(v.environment.as_deref(), Some("mainnet"));
        assert_eq!(v.custom_home, Some(PathBuf::from("/srv/multiversx")));
        assert_eq!(v.custom_user.as_deref(), Some("mvx"));
        assert_eq!(
            v.node_keys_location,
            Some(PathBuf::from("/srv/multiversx/keys"))
        );
        assert_eq!(v.github_token.as_deref(), Some("ghp_abcdefghijklmno"));
        assert_eq!(
            v.node_extra_flags.as_deref(),
            Some("-display-name custom-name -profile-mode true")
        );
        assert_eq!(v.github_org.as_deref(), Some("myfork"));
        assert_eq!(v.override_proxyver.as_deref(), Some("v1.2.3"));
        assert!(v.override_configver.is_none());
    }

    #[test]
    fn variables_cfg_skips_bash_expansions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("variables.cfg");
        fs::write(
            &path,
            r#"
GITHUB_ORG=${GITHUB_ORG:-multiversx}
CUSTOM_HOME="/home/ubuntu"
"#,
        )
        .unwrap();
        let v = parse_variables_cfg(&path).unwrap();
        // Bash expansion for GITHUB_ORG is skipped; CUSTOM_HOME is captured.
        assert!(v.github_org.is_none());
        assert_eq!(v.custom_home, Some(PathBuf::from("/home/ubuntu")));
    }

    #[test]
    fn variables_cfg_handles_inline_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("variables.cfg");
        fs::write(&path, "CUSTOM_USER=ubuntu  # the system user\n").unwrap();
        let v = parse_variables_cfg(&path).unwrap();
        assert_eq!(v.custom_user.as_deref(), Some("ubuntu"));
    }

    // ── service file parsing ──

    fn write_service(dir: &Path, idx: u16, exec_start: &str) -> PathBuf {
        let path = dir.join(format!("elrond-node-{idx}.service"));
        fs::write(
            &path,
            format!(
                "[Unit]\nDescription=MultiversX Node-{idx}\n\n[Service]\nUser=ubuntu\nWorkingDirectory=/home/ubuntu/elrond-nodes/node-{idx}\nExecStart={exec_start}\n[Install]\nWantedBy=multi-user.target\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn parses_canonical_exec_start_no_extras() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_service(dir.path(), 0, "/home/ubuntu/elrond-nodes/node-0/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8080");
        let f = parse_service_file(&path).unwrap();
        assert_eq!(f.user.as_deref(), Some("ubuntu"));
        assert_eq!(f.api_port, Some(8080));
        assert_eq!(f.extra_flags.as_deref(), Some(""));
    }

    #[test]
    fn parses_exec_start_with_extras() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_service(dir.path(), 2, "/srv/mx/elrond-nodes/node-2/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8082 -display-name shard2-runner -profile-mode true");
        let f = parse_service_file(&path).unwrap();
        assert_eq!(f.api_port, Some(8082));
        assert_eq!(
            f.extra_flags.as_deref(),
            Some("-display-name shard2-runner -profile-mode true")
        );
    }

    #[test]
    fn exec_start_without_canonical_prefix_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_service(
            dir.path(),
            0,
            "/usr/local/bin/some-other-binary --foo --bar",
        );
        let f = parse_service_file(&path).unwrap();
        assert_eq!(f.api_port, None);
        assert_eq!(f.extra_flags, None);
    }

    #[test]
    fn scan_service_dir_finds_nodes() {
        let dir = tempfile::tempdir().unwrap();
        write_service(dir.path(), 0, "/wd/node -rest-api-interface localhost:8080");
        write_service(dir.path(), 1, "/wd/node -rest-api-interface localhost:8081");
        // Ignored: not a node service.
        fs::write(dir.path().join("elrond-proxy.service"), "[Service]\n").unwrap();
        // Ignored: bad index.
        fs::write(dir.path().join("elrond-node-x.service"), "[Service]\n").unwrap();
        let map = scan_service_dir(dir.path());
        assert_eq!(map.len(), 2);
        assert!(map.contains_key(&0));
        assert!(map.contains_key(&1));
    }

    // ── plan integration ──

    #[test]
    fn plan_merges_variables_cfg_into_default_config() {
        let custom_home = bash_fixture("Observers Squad", 4, "mainnet");
        let scripts_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(scripts_dir.path().join("config")).unwrap();
        fs::write(
            scripts_dir.path().join("config/variables.cfg"),
            r#"
CUSTOM_HOME="/srv/mvx"
CUSTOM_USER="mvx"
GITHUB_ORG="myfork"
NODE_EXTRA_FLAGS="-display-name node-{index}"
GITHUBTOKEN="ghp_abcdefg1234567"
"#,
        )
        .unwrap();
        let systemd_dir = tempfile::tempdir().unwrap();
        let plan = build_migration_plan(
            custom_home.path(),
            Some(scripts_dir.path()),
            systemd_dir.path(),
            &MxnodeFile::default(),
        )
        .unwrap();

        let keys: Vec<_> = plan.config_patches.iter().map(|p| p.key.as_str()).collect();
        assert!(keys.contains(&"paths.custom_home"));
        assert!(keys.contains(&"paths.custom_user"));
        assert!(keys.contains(&"network.github_org"));
        assert!(keys.contains(&"node.extra_flags"));
        assert_eq!(plan.github_token.as_deref(), Some("ghp_abcdefg1234567"));
    }

    #[test]
    fn plan_skips_fields_already_set_in_existing_config() {
        let custom_home = bash_fixture("Observers Squad", 4, "mainnet");
        let scripts_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(scripts_dir.path().join("config")).unwrap();
        fs::write(
            scripts_dir.path().join("config/variables.cfg"),
            r#"
CUSTOM_HOME="/srv/from-bash"
GITHUB_ORG="bash-org"
"#,
        )
        .unwrap();
        let systemd_dir = tempfile::tempdir().unwrap();

        let mut existing = MxnodeFile::default();
        existing.paths.custom_home = Some(PathBuf::from("/srv/already-set"));
        existing.network.github_org = "operator-set".to_string();

        let plan = build_migration_plan(
            custom_home.path(),
            Some(scripts_dir.path()),
            systemd_dir.path(),
            &existing,
        )
        .unwrap();

        let keys: Vec<_> = plan.config_patches.iter().map(|p| p.key.as_str()).collect();
        // Both keys are already operator-set in mxnode → migration leaves them alone.
        assert!(!keys.contains(&"paths.custom_home"));
        assert!(!keys.contains(&"network.github_org"));
    }

    #[test]
    fn plan_emits_per_node_overrides_when_service_files_diverge() {
        let custom_home = bash_fixture("Observers Squad", 4, "mainnet");
        let scripts_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(scripts_dir.path().join("config")).unwrap();
        fs::write(
            scripts_dir.path().join("config/variables.cfg"),
            r#"
NODE_EXTRA_FLAGS="-profile-mode true"
"#,
        )
        .unwrap();
        let systemd_dir = tempfile::tempdir().unwrap();
        // Node 0: matches the global → no override
        write_service(
            systemd_dir.path(),
            0,
            "/wd/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8080 -profile-mode true",
        );
        // Node 1: diverges → override
        write_service(
            systemd_dir.path(),
            1,
            "/wd/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8081 -profile-mode true -display-name special",
        );

        let plan = build_migration_plan(
            custom_home.path(),
            Some(scripts_dir.path()),
            systemd_dir.path(),
            &MxnodeFile::default(),
        )
        .unwrap();

        assert_eq!(plan.per_node_patches.len(), 1);
        let patch = &plan.per_node_patches[0];
        assert_eq!(patch.index, 1);
        // Role and shard mirror mxnode.toml — the diverging override
        // must not contradict the inferred install layout.
        assert_eq!(patch.role, "observer");
        assert_eq!(patch.shard, "one");
        assert!(patch.extra_flags.contains("-display-name special"));
        assert_eq!(patch.operation_mode, None);
    }

    #[test]
    fn plan_normalizes_common_operation_mode_from_service_files() {
        let custom_home = bash_fixture("Observers Squad", 2, "mainnet");
        let scripts_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(scripts_dir.path().join("config")).unwrap();
        fs::write(
            scripts_dir.path().join("config/variables.cfg"),
            r#"NODE_EXTRA_FLAGS="-profile-mode true""#,
        )
        .unwrap();
        let systemd_dir = tempfile::tempdir().unwrap();
        write_service(
            systemd_dir.path(),
            0,
            "/wd/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8080 -operation-mode full-archive -profile-mode true",
        );
        write_service(
            systemd_dir.path(),
            1,
            "/wd/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8081 -operation-mode full-archive -profile-mode true",
        );

        let plan = build_migration_plan(
            custom_home.path(),
            Some(scripts_dir.path()),
            systemd_dir.path(),
            &MxnodeFile::default(),
        )
        .unwrap();

        let op_patch = plan
            .config_patches
            .iter()
            .find(|p| p.key == "node.operation_mode")
            .expect("global operation_mode patch");
        assert_eq!(op_patch.value, "full-archive");
        assert!(plan.per_node_patches.is_empty());
    }

    #[test]
    fn plan_normalizes_per_node_operation_mode_from_service_files() {
        let custom_home = bash_fixture("Observers Squad", 2, "mainnet");
        let systemd_dir = tempfile::tempdir().unwrap();
        write_service(
            systemd_dir.path(),
            0,
            "/wd/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8080 --operation-mode=db-lookup-extension -profile-mode true",
        );
        write_service(
            systemd_dir.path(),
            1,
            "/wd/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8081 --operation-mode=snapshotless-observer -profile-mode true",
        );

        let plan = build_migration_plan(
            custom_home.path(),
            None,
            systemd_dir.path(),
            &MxnodeFile::default(),
        )
        .unwrap();

        assert!(!plan
            .config_patches
            .iter()
            .any(|p| p.key == "node.operation_mode"));
        assert_eq!(plan.per_node_patches.len(), 2);
        assert_eq!(
            plan.per_node_patches[0].operation_mode.as_deref(),
            Some("db-lookup-extension")
        );
        assert_eq!(plan.per_node_patches[0].extra_flags, "-profile-mode true");
        assert_eq!(
            plan.per_node_patches[1].operation_mode.as_deref(),
            Some("snapshotless-observer")
        );
    }

    #[test]
    fn plan_imports_node_display_names_from_prefs_toml() {
        let custom_home = bash_fixture("", 2, "mainnet");
        write_prefs(custom_home.path(), 0, "Backup");
        write_prefs(custom_home.path(), 1, "Primary");
        let systemd_dir = tempfile::tempdir().unwrap();

        let plan = build_migration_plan(
            custom_home.path(),
            None,
            systemd_dir.path(),
            &MxnodeFile::default(),
        )
        .unwrap();

        assert_eq!(plan.state.nodes[0].display_name, "Backup");
        assert_eq!(plan.state.nodes[1].display_name, "Primary");
    }

    #[test]
    fn display_name_extraction_tolerates_invalid_later_prefs_toml() {
        let body = r#"
[Preferences]
   DestinationShardAsObserver = "2"
   NodeDisplayName = "Backup"

   OverridableConfigTomlValues = [
    { File = "external.toml", Path = "HostDriversConfig", Value = [
         {
        URL = "ws://notifier.xoxno.com",
        Enabled = true,
     }
    ] },
    ]

[BlockProcessingCutoff]
   Enabled = false
"#;
        assert!(body.parse::<toml::Value>().is_err());
        assert_eq!(
            extract_node_display_name_from_prefs(body)
                .unwrap()
                .as_deref(),
            Some("Backup")
        );
    }

    #[test]
    fn plan_skips_per_node_when_index_outside_state() {
        let custom_home = bash_fixture("Observers Squad", 4, "mainnet");
        let scripts_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(scripts_dir.path().join("config")).unwrap();
        fs::write(scripts_dir.path().join("config/variables.cfg"), "").unwrap();
        let systemd_dir = tempfile::tempdir().unwrap();
        // Node 99: not in sentinels (squad is 4 nodes, idx 0..=3)
        write_service(
            systemd_dir.path(),
            99,
            "/wd/node -use-log-view -log-logger-name -log-correlation -log-level *:DEBUG -rest-api-interface localhost:8099 -display-name orphan",
        );

        let plan = build_migration_plan(
            custom_home.path(),
            Some(scripts_dir.path()),
            systemd_dir.path(),
            &MxnodeFile::default(),
        )
        .unwrap();

        // Orphan service file does not produce an override; warning surfaced instead.
        assert!(plan.per_node_patches.is_empty());
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("elrond-node-99.service: no matching node")));
    }

    #[test]
    fn plan_warns_on_missing_variables_cfg() {
        let custom_home = bash_fixture("", 1, "devnet");
        let scripts_dir = tempfile::tempdir().unwrap();
        let systemd_dir = tempfile::tempdir().unwrap();
        let plan = build_migration_plan(
            custom_home.path(),
            Some(scripts_dir.path()),
            systemd_dir.path(),
            &MxnodeFile::default(),
        )
        .unwrap();
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("variables.cfg not found")));
    }

    #[test]
    fn plan_propagates_environment_to_network_section() {
        let custom_home = bash_fixture("Observers Squad", 4, "mainnet");
        let scripts_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(scripts_dir.path().join("config")).unwrap();
        fs::write(
            scripts_dir.path().join("config/variables.cfg"),
            r#"ENVIRONMENT="mainnet""#,
        )
        .unwrap();
        let systemd_dir = tempfile::tempdir().unwrap();
        let plan = build_migration_plan(
            custom_home.path(),
            Some(scripts_dir.path()),
            systemd_dir.path(),
            &MxnodeFile::default(),
        )
        .unwrap();
        let env_patch = plan
            .config_patches
            .iter()
            .find(|p| p.key == "network.environment")
            .expect("environment patch present");
        assert_eq!(env_patch.value, "mainnet");
    }

    #[test]
    fn token_mask_redacts_sensitive_chars() {
        assert_eq!(mask_token("ghp_abcdefghij"), "ghp_**********");
        assert_eq!(mask_token("abc"), "abc");
        assert_eq!(mask_token(""), "");
    }
}
