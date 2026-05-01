use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::NodeIndex;

/// All filesystem locations mxnode reads or writes. Defaults match the bash
/// today (`/home/ubuntu`), but every field is overridable via config.
///
/// Paths are stored as `PathBuf` after `{custom_home}` / `{XDG_*}`
/// interpolation has happened in `mxnode-config`. The core type only sees
/// resolved absolute paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paths {
    /// `$CUSTOM_HOME` in the bash. The operator's home directory and the root
    /// for `elrond-nodes/`, `elrond-utils/`, `elrond-proxy/`, key archives.
    pub custom_home: PathBuf,
    /// systemd `User=` for every node and proxy unit we render.
    pub custom_user: String,
    /// Directory containing `node-{INDEX}.zip` archives the operator drops
    /// before `install`.
    pub node_keys: PathBuf,
    /// Versioned binary store; replaces today's "cp into the node dir" flow.
    pub binaries: PathBuf,
    /// `state.toml`, `state.toml.lock`, `inflight.toml` live here.
    pub state: PathBuf,
    /// `upgrade.lock` PID-file, future IPC sockets.
    pub runtime: PathBuf,
}

impl Paths {
    /// Working directory for one node, matching the bash
    /// `$CUSTOM_HOME/elrond-nodes/node-$INDEX` convention.
    pub fn node_workdir(&self, index: NodeIndex) -> PathBuf {
        self.elrond_nodes_root()
            .join(format!("node-{}", index.get()))
    }

    pub fn elrond_nodes_root(&self) -> PathBuf {
        self.custom_home.join("elrond-nodes")
    }

    pub fn elrond_utils_root(&self) -> PathBuf {
        self.custom_home.join("elrond-utils")
    }

    pub fn elrond_proxy_root(&self) -> PathBuf {
        self.custom_home.join("elrond-proxy")
    }

    /// Versioned binary location for a given artifact and tag, e.g.
    /// `{binaries}/node/v1.7.13/node`. The artifact-name segment is the same
    /// segment used as the symlink basename inside `node-{i}/`.
    pub fn binary_path(&self, artifact: &str, tag: &str) -> PathBuf {
        self.binaries.join(artifact).join(tag).join(artifact)
    }

    pub fn state_file(&self) -> PathBuf {
        self.state.join("state.toml")
    }

    pub fn state_lock_file(&self) -> PathBuf {
        self.state.join("state.toml.lock")
    }

    pub fn inflight_file(&self) -> PathBuf {
        self.state.join("inflight.toml")
    }

    pub fn upgrade_lock_file(&self) -> PathBuf {
        self.runtime.join("upgrade.lock")
    }

    /// Legacy bash dotfiles. We read these during `migrate-from-bash` /
    /// `adopt`, never write them.
    pub fn legacy_dotfile(&self, name: &str) -> PathBuf {
        self.custom_home.join(name)
    }

    /// systemd unit file written under `/etc/systemd/system/`. Returns the
    /// canonical name `elrond-node-{INDEX}.service` (D4: keep existing names).
    pub fn node_unit_name(index: NodeIndex) -> String {
        format!("elrond-node-{}.service", index.get())
    }

    pub fn proxy_unit_name() -> &'static str {
        "elrond-proxy.service"
    }

    /// `/etc/systemd/system` is the only path mxnode writes outside the
    /// operator's home; surfaced here so callers don't hardcode it.
    pub fn systemd_unit_dir() -> &'static Path {
        Path::new("/etc/systemd/system")
    }
}

impl Default for Paths {
    /// Defaults match the bash variables.cfg (`CUSTOM_HOME=/home/ubuntu`,
    /// `CUSTOM_USER=ubuntu`). XDG dirs are filled in by `mxnode-config` at
    /// load time using `dirs`; this default is the static fallback.
    ///
    /// `runtime` defaults to a path under `state` rather than `/run/mxnode`
    /// because mxnode is an unprivileged operator CLI — it cannot reliably
    /// write under `/run` without root. The config layer prefers
    /// `$XDG_RUNTIME_DIR/mxnode` when that env var is set.
    fn default() -> Self {
        let home = PathBuf::from("/home/ubuntu");
        let state = home.join(".local/state/mxnode");
        Self {
            custom_home: home.clone(),
            custom_user: "ubuntu".to_string(),
            node_keys: home.join("VALIDATOR_KEYS"),
            binaries: home.join("mxnode/binaries"),
            state: state.clone(),
            runtime: state.join("run"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_paths_match_bash_layout() {
        let p = Paths::default();
        assert_eq!(p.custom_home, PathBuf::from("/home/ubuntu"));
        assert_eq!(p.custom_user, "ubuntu");
        assert_eq!(p.node_keys, PathBuf::from("/home/ubuntu/VALIDATOR_KEYS"));
        assert_eq!(
            p.elrond_nodes_root(),
            PathBuf::from("/home/ubuntu/elrond-nodes")
        );
    }

    #[test]
    fn node_workdir_matches_bash_convention() {
        let p = Paths::default();
        let wd = p.node_workdir(NodeIndex::new(3));
        assert_eq!(wd, PathBuf::from("/home/ubuntu/elrond-nodes/node-3"));
    }

    #[test]
    fn unit_names_preserved() {
        assert_eq!(
            Paths::node_unit_name(NodeIndex::new(0)),
            "elrond-node-0.service"
        );
        assert_eq!(
            Paths::node_unit_name(NodeIndex::new(7)),
            "elrond-node-7.service"
        );
        assert_eq!(Paths::proxy_unit_name(), "elrond-proxy.service");
    }

    #[test]
    fn binary_path_is_versioned() {
        let p = Paths::default();
        let bp = p.binary_path("node", "v1.7.13");
        assert_eq!(
            bp,
            PathBuf::from("/home/ubuntu/mxnode/binaries/node/v1.7.13/node")
        );
    }
}
