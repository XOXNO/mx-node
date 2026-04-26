//! `mxnode migrate`: end-to-end migration from a bash-installed setup.
//! One command handles every step — operators don't have to chain
//! `migrate-from-bash` → `adopt` → `rebuild-state` manually any more.
//!
//! Sequence:
//!   1. Locate the legacy `mx-chain-scripts` repo (`--legacy-dir` or
//!      auto-detect under `$HOME/GitHub/`, `$HOME/`).
//!   2. Parse `config/variables.cfg` (whitelisted keys only) +
//!      `$CUSTOM_HOME/.numberofnodes` and friends.
//!   3. Walk every `<custom_home>/elrond-nodes/node-{i}/config/` and
//!      detect TOML keys whose values are *identical* across all
//!      nodes — those become a single `[overrides.prefs]` /
//!      `[overrides.config]` section, replacing the old per-node
//!      sed-managed copies.
//!   4. Write `~/.config/mxnode/config.toml`.
//!   5. Adopt the existing units into `state.toml`. The running
//!      processes are not touched: the new mxnode binary uses the
//!      same `elrond-node-{i}.service` / `com.multiversx.elrond-…`
//!      units the bash already installed.
//!
//! No databases are deleted, no logs are wiped, no services are
//! restarted as part of `migrate`. Nodes don't even know it happened —
//! the operator just gets a working `mxnode status` afterwards.

use std::path::{Path, PathBuf};

use mxnode_config::user_config_path_or_default as resolve_user_config_path;
use mxnode_core::{ArtifactSource, Environment};
use serde::Serialize;

use crate::cli::{GlobalArgs, MigrateFromBashArgs};
use crate::errors::CliError;
use crate::orchestrator::common_settings::{detect as detect_common_settings, CommonSettings};
use crate::orchestrator::legacy::{parse_variables_cfg, read_dotfiles, LegacyVariables};
use crate::orchestrator::runtime::CliErrorExt;

pub fn run(args: MigrateFromBashArgs, global: &GlobalArgs) -> Result<(), CliError> {
    let legacy_dir = pick_legacy_dir(args.legacy_dir.as_deref())?;
    let variables_path = legacy_dir.join("config").join("variables.cfg");
    if !variables_path.exists() {
        return Err(CliError::new(
            "could not find legacy variables.cfg",
            format!("expected {}", variables_path.display()),
            "pass --legacy-dir <PATH> pointing at the bash repo root",
        )
        .json_if(global.json));
    }
    let parsed = parse_variables_cfg(&variables_path).map_err(|e| {
        CliError::new(
            "failed to parse legacy variables.cfg",
            e.to_string(),
            "the file may use bashisms beyond the whitelisted KEY=value shape; review manually",
        )
        .json_if(global.json)
    })?;

    // Decide $CUSTOM_HOME for dotfile reads. Honour the legacy file's
    // CUSTOM_HOME or fall back to the operator's actual home — never assume
    // /home/ubuntu when something more specific is available.
    let custom_home = parsed
        .custom_home
        .clone()
        .or_else(|| dirs::home_dir())
        .ok_or_else(|| {
            CliError::new(
                "could not determine CUSTOM_HOME",
                "neither variables.cfg nor $HOME yielded a usable path",
                "pass --legacy-dir <PATH> with a variables.cfg that sets CUSTOM_HOME",
            )
            .json_if(global.json)
        })?;
    let dotfiles = read_dotfiles(&custom_home);

    // Walk each node's working directory and find TOML keys whose
    // values are identical across the whole fleet. Those collapse
    // into [overrides.prefs] / [overrides.config] in the new config.
    let workdirs = candidate_workdirs(&custom_home, dotfiles.number_of_nodes.unwrap_or(0));
    let common = detect_common_settings(&workdirs);

    // Build a sparse mxnode config.toml. Only fields actually carried over
    // from variables.cfg / dotfiles / fleet-wide overrides end up in the
    // output — the rest stays implicit so the operator can later edit
    // the file without seeing the entire schema.
    let toml_text = render_sparse_config(&parsed, &dotfiles, &common)?;
    let target = resolve_user_config_path().map_err(|e| {
        CliError::new(
            "could not determine where to write config.toml",
            e.to_string(),
            "set $XDG_CONFIG_HOME or $HOME so mxnode can place the file under <home>/.config/mxnode/",
        )
        .json_if(global.json)
    })?;
    if target.exists() {
        return Err(CliError::new(
            "refusing to overwrite existing config",
            format!("{} already exists", target.display()),
            "back up the file first, then re-run; mxnode never overwrites a hand-edited config",
        )
        .json_if(global.json));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CliError::new(
                "failed to create config directory",
                e.to_string(),
                "ensure the parent directory is writable",
            )
            .json_if(global.json)
        })?;
    }
    std::fs::write(&target, toml_text).map_err(|e| {
        CliError::new(
            "failed to write config.toml",
            e.to_string(),
            "ensure the parent directory is writable",
        )
        .json_if(global.json)
    })?;

    if !global.json {
        println!("✓ config.toml  →  {}", target.display());
        println!("  ↳ source: {}", variables_path.display());
        if let Some(n) = dotfiles.number_of_nodes {
            println!("  ↳ legacy reported {n} node(s)");
        }
        if common.nodes_scanned > 0 && !common.prefs.is_empty() {
            println!(
                "  ↳ rolled up {} prefs override(s) from {} node(s)",
                common.prefs.len(),
                common.nodes_scanned,
            );
        }
        if common.differing_prefs_keys > 0 {
            println!(
                "  ↳ {} prefs key(s) differ across nodes — left in place per-node",
                common.differing_prefs_keys
            );
        }
        if !parsed.unknown_keys.is_empty() {
            println!(
                "  ↳ {} unknown variables.cfg key(s) skipped (review manually):",
                parsed.unknown_keys.len()
            );
            for k in &parsed.unknown_keys {
                println!("       {k}");
            }
        }
    }

    // Auto-adopt — pull the existing systemd / launchd units into
    // state.toml so future `mxnode start/stop/restart/upgrade` commands
    // can drive them. The running processes don't notice — we never
    // re-render the unit file or restart the unit during adopt.
    let adopt_outcome = run_inline_adopt(global);
    let adopt_summary = adopt_outcome
        .as_ref()
        .ok()
        .map(|o| (o.nodes_adopted, o.proxy_adopted));
    if let Err(e) = &adopt_outcome {
        if !global.json {
            println!();
            println!("! adopt could not finish: {}", e.summary);
            println!("  Run `mxnode adopt` manually to retry.");
        }
    }

    if global.json {
        let report = MigrateReport {
            ok: adopt_outcome.is_ok(),
            wrote: target.display().to_string(),
            from: variables_path.display().to_string(),
            unknown_keys: parsed.unknown_keys.clone(),
            number_of_nodes: dotfiles.number_of_nodes,
            installed_env: dotfiles.installed_env.clone(),
            squad_install: dotfiles.squad_install.clone(),
            common_prefs_overrides: common.prefs.len() as u32,
            common_config_overrides: 0,
            differing_keys: common.differing_prefs_keys as u32,
            nodes_adopted: adopt_summary.map(|(n, _)| n as u32).unwrap_or(0),
            proxy_adopted: adopt_summary.map(|(_, p)| p).unwrap_or(false),
        };
        println!("{}", serde_json::to_string(&report).unwrap_or_default());
        return Ok(());
    }

    if let Some((n, proxy)) = adopt_summary {
        println!();
        println!(
            "✓ state.toml  →  {} node(s){}",
            n,
            if proxy { " + proxy" } else { "" }
        );
        println!();
        println!("Migration complete. Your bash-installed units kept running.");
        println!();
        println!("Next:");
        println!("  mxnode status              # confirm the fleet is healthy");
        println!("  mxnode dashboard           # live multi-node view");
        println!("  mxnode upgrade --binary-tag <T>   # version bumps go through mxnode now");
    }
    Ok(())
}

/// Re-run adopt in-process so the operator gets a single output. We
/// bypass the dispatch through `commands::adopt::run` to keep the
/// summary tidy — adopt has its own JSON contract that would clobber
/// migrate's. `force_adopt` defaults to false; if there's drift the
/// operator can re-run `mxnode adopt --force-adopt` themselves.
struct AdoptOutcome {
    nodes_adopted: usize,
    proxy_adopted: bool,
}

fn run_inline_adopt(global: &GlobalArgs) -> Result<AdoptOutcome, CliError> {
    use crate::orchestrator::adopt::{analyze, AdoptInputs};
    use crate::orchestrator::runtime::Runtime;
    use crate::orchestrator::supervisor::unit_dir_for_platform;
    use mxnode_core::Platform;
    use mxnode_state::StateStore;

    let runtime = Runtime::from_global(global)?;
    let env = runtime
        .loaded
        .config
        .network
        .environment
        .ok_or_else(|| {
            CliError::new(
                "network.environment is missing after migrate",
                "the legacy ENVIRONMENT variable did not resolve to mainnet|testnet|devnet",
                "edit ~/.config/mxnode/config.toml and set [network] environment = \"...\"",
            )
            .json_if(global.json)
        })?;
    let store = StateStore::new(&runtime.paths.state);
    if store.exists() {
        // state.toml already there — adopt would be a no-op. Treat as
        // success with the existing counts so migrate's summary still
        // shows operator-meaningful numbers.
        let existing = store.load().ok().flatten();
        let count = existing.as_ref().map(|s| s.nodes.len()).unwrap_or(0);
        let proxy = existing.as_ref().and_then(|s| s.proxy.clone()).is_some();
        return Ok(AdoptOutcome { nodes_adopted: count, proxy_adopted: proxy });
    }
    let dir = unit_dir_for_platform(Platform::current()).ok_or_else(|| {
        CliError::new(
            "no supervisor directory for this platform",
            "mxnode supports linux + macos only",
            "run on a supported host",
        )
        .json_if(global.json)
    })?;
    let inputs = AdoptInputs {
        paths: &runtime.paths,
        environment: env,
        github_org: &runtime.loaded.config.network.github_org,
        log_level: &runtime.loaded.config.node.log_level,
        limit_nofile: runtime.loaded.config.node.limit_nofile,
        restart_sec: runtime.loaded.config.node.restart_sec,
        api_port_base: runtime.loaded.config.node.api_port_base,
        extra_flags: &runtime.loaded.config.node.extra_flags,
    };
    let outcome = analyze(&dir, &inputs, "mxnode-migrate").map_err(|e| {
        CliError::new(
            "adopt scan failed",
            e.to_string(),
            "rerun `mxnode adopt` for a detailed diagnosis",
        )
        .json_if(global.json)
    })?;

    // Migrate is a forgiving entry point — if there's drift between
    // what mxnode would render and what's installed, we still adopt
    // (the bash and mxnode unit shapes are byte-identical for the
    // happy path; any drift is operator-customised flags we'd preserve
    // verbatim under `unit_override` regardless). Keeps the migration
    // single-shot.
    let mut state = outcome.state;
    state.written_at = time::OffsetDateTime::now_utc();
    let nodes_adopted = state.nodes.len();
    let proxy_adopted = state.proxy.is_some();
    let guard = store.lock().map_err(|e| {
        CliError::new(
            "failed to lock state.toml",
            e.to_string(),
            "another mxnode op may be running",
        )
        .json_if(global.json)
    })?;
    store.save(&state, &guard).map_err(|e| {
        CliError::new(
            "failed to write state.toml",
            e.to_string(),
            "ensure the state directory is writable",
        )
        .json_if(global.json)
    })?;
    Ok(AdoptOutcome { nodes_adopted, proxy_adopted })
}

/// Build the list of plausible `node-{i}` working directories under
/// the legacy `<custom_home>/elrond-nodes/`. We trust `.numberofnodes`
/// when present; otherwise enumerate up to 16 (max squad size we've
/// ever seen in practice) and let `read_to_string` filter the ones
/// that don't actually exist.
fn candidate_workdirs(custom_home: &Path, number_of_nodes: u16) -> Vec<PathBuf> {
    let n = if number_of_nodes == 0 { 16 } else { number_of_nodes };
    let root = custom_home.join("elrond-nodes");
    (0..n)
        .map(|i| root.join(format!("node-{i}")))
        .filter(|p| p.exists())
        .collect()
}

fn pick_legacy_dir(arg: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(p) = arg {
        return Ok(p.to_path_buf());
    }
    // Default: assume the bash repo lives next to mx-node under the same
    // GitHub directory. This is a heuristic that matches the user's
    // observed layout (~/GitHub/mx-chain-scripts).
    if let Some(home) = dirs::home_dir() {
        for candidate in [
            home.join("GitHub/mx-chain-scripts"),
            home.join("github/mx-chain-scripts"),
            home.join("mx-chain-scripts"),
        ] {
            if candidate.join("config").join("variables.cfg").exists() {
                return Ok(candidate);
            }
        }
    }
    Err(CliError::new(
        "could not locate the bash repo",
        "no `mx-chain-scripts/config/variables.cfg` under any standard location",
        "pass --legacy-dir <PATH> pointing at the bash repo root",
    ))
}

#[derive(Debug, Serialize)]
struct MigrateReport {
    ok: bool,
    wrote: String,
    from: String,
    unknown_keys: Vec<String>,
    number_of_nodes: Option<u16>,
    installed_env: Option<String>,
    squad_install: Option<String>,
    common_prefs_overrides: u32,
    common_config_overrides: u32,
    differing_keys: u32,
    nodes_adopted: u32,
    proxy_adopted: bool,
}

/// Build the sparse migration output via `toml::Value::Table` so values
/// containing `"`, `\`, or newlines are escaped correctly. The `_` binding
/// for `ArtifactSource::Source` is kept as a tripwire: if the bash file
/// ever starts dictating `artifact_source`, this is the line to update.
fn render_sparse_config(
    parsed: &LegacyVariables,
    dotfiles: &crate::orchestrator::legacy::LegacyDotfiles,
    common: &CommonSettings,
) -> Result<String, CliError> {
    use toml::map::Map;
    use toml::Value;

    let mut root: Map<String, Value> = Map::new();
    root.insert("schema_version".to_string(), Value::Integer(1));

    let mut network: Map<String, Value> = Map::new();
    let env = parsed
        .environment
        .clone()
        .or_else(|| dotfiles.installed_env.clone());
    if let Some(env_str) = env.as_deref() {
        if env_str.parse::<Environment>().is_err() {
            return Err(CliError::new(
                "legacy ENVIRONMENT was not mainnet|testnet|devnet",
                format!("got {env_str:?}"),
                "fix variables.cfg or .installedenv, then re-run migrate-from-bash",
            ));
        }
        network.insert(
            "environment".to_string(),
            Value::String(env_str.to_string()),
        );
    }
    if let Some(org) = parsed.github_org.as_deref() {
        network.insert("github_org".to_string(), Value::String(org.to_string()));
    }
    if !network.is_empty() {
        root.insert("network".to_string(), Value::Table(network));
    }

    if parsed.custom_home.is_some()
        || parsed.custom_user.is_some()
        || parsed.node_keys_location.is_some()
    {
        let mut paths: Map<String, Value> = Map::new();
        if let Some(p) = parsed.custom_home.as_ref() {
            paths.insert(
                "custom_home".to_string(),
                Value::String(p.display().to_string()),
            );
        }
        if let Some(u) = parsed.custom_user.as_deref() {
            paths.insert("custom_user".to_string(), Value::String(u.to_string()));
        }
        if let Some(k) = parsed.node_keys_location.as_deref() {
            let normalised = k.replace("$CUSTOM_HOME", "{custom_home}");
            paths.insert("node_keys".to_string(), Value::String(normalised));
        }
        root.insert("paths".to_string(), Value::Table(paths));
    }

    if let Some(flags) = parsed.node_extra_flags.as_deref() {
        let mut node: Map<String, Value> = Map::new();
        node.insert("extra_flags".to_string(), Value::String(flags.to_string()));
        root.insert("node".to_string(), Value::Table(node));
    }

    let has_legacy_overrides =
        parsed.override_configver.is_some() || parsed.override_proxyver.is_some();
    let has_common_prefs = !common.prefs.is_empty();
    if has_legacy_overrides || has_common_prefs {
        let mut overrides: Map<String, Value> = Map::new();
        if let Some(v) = parsed.override_configver.as_deref() {
            overrides.insert("configver".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = parsed.override_proxyver.as_deref() {
            overrides.insert("proxyver".to_string(), Value::String(v.to_string()));
        }
        // Fleet-wide prefs.toml tweaks that all bash-managed nodes
        // had identical values for collapse into a single override
        // section. We deliberately don't roll up `config.toml`:
        // every node clones the same upstream config repo so 99% of
        // its keys would show as "common" and end up as a 600-line
        // dump of upstream defaults that freezes the operator on a
        // specific upstream snapshot. See `common_settings` module
        // docstring for the full reasoning.
        if has_common_prefs {
            let mut prefs_table = Map::new();
            for (k, v) in &common.prefs {
                prefs_table.insert(k.clone(), v.clone());
            }
            overrides.insert("prefs".to_string(), Value::Table(prefs_table));
        }
        root.insert("overrides".to_string(), Value::Table(overrides));
    }

    // Source-build remains the default per D2.
    let _ = ArtifactSource::Source;

    let body = toml::to_string_pretty(&Value::Table(root))
        .expect("toml::Value serialisation cannot fail for known-shape values");

    let mut out = String::new();
    out.push_str("# mxnode config — generated by `mxnode migrate`.\n");
    out.push_str("# Edit freely; mxnode preserves unknown keys on round-trip.\n");
    if common.nodes_scanned > 0 && !common.prefs.is_empty() {
        out.push_str(&format!(
            "# [overrides.prefs] auto-derived from settings identical across all\n# {} bash-managed node(s) — applied to every node on install/upgrade.\n",
            common.nodes_scanned
        ));
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::legacy::{LegacyDotfiles, LegacyVariables};

    fn legacy_with_quote() -> LegacyVariables {
        let mut l = LegacyVariables::default();
        l.environment = Some("mainnet".to_string());
        l.custom_user = Some("name-with\"quote".to_string());
        l.node_extra_flags = Some("-display-name \"with quotes\"".to_string());
        l
    }

    #[test]
    fn render_escapes_quotes_via_toml_crate() {
        let body = render_sparse_config(&legacy_with_quote(), &LegacyDotfiles::default(), &CommonSettings::default()).unwrap();
        // Round-trip parse — proves no broken TOML.
        let parsed: toml::Value = toml::from_str(&body).expect("must parse back");
        assert_eq!(
            parsed["paths"]["custom_user"].as_str(),
            Some("name-with\"quote"),
        );
        assert_eq!(
            parsed["node"]["extra_flags"].as_str(),
            Some("-display-name \"with quotes\""),
        );
    }

    #[test]
    fn render_rejects_invalid_environment() {
        let mut legacy = LegacyVariables::default();
        legacy.environment = Some("not-a-network".to_string());
        let err = render_sparse_config(&legacy, &LegacyDotfiles::default(), &CommonSettings::default()).unwrap_err();
        assert!(err.summary.contains("ENVIRONMENT"));
    }

    #[test]
    fn render_omits_empty_sections() {
        let body = render_sparse_config(
            &LegacyVariables::default(),
            &LegacyDotfiles::default(),
            &CommonSettings::default(),
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&body).expect("must parse back");
        assert!(parsed.get("paths").is_none());
        assert!(parsed.get("node").is_none());
        assert!(parsed.get("overrides").is_none());
    }
}
