//! systemd unit rendering and systemctl wrappers.

mod ctl;
mod discovery;
mod plist;
mod render;
mod tomledit;

pub use ctl::{ActiveState, Ctl, CtlError, LaunchdCtl, SystemctlCtl};
pub use discovery::{
    parse_unit_text, scan_launchd_dir, scan_supervisor_dir, scan_systemd_dir, Discovered,
    DiscoveredKind, DiscoveryError, ParseError, UnitView,
};
pub use plist::{
    launchd_filename, launchd_label, render_canonical_node_plist, user_launch_agent_path,
    user_launch_agents_dir, LAUNCH_AGENT_PREFIX,
};
pub use render::{
    render_canonical_node_unit, render_canonical_proxy_unit, NodeUnitSpec, ProxyUnitSpec,
};
pub use tomledit::{
    apply_overrides, clear_cpu_flags, enable_db_lookup_extensions, rewrite_proxy_config,
    set_destination_shard, set_node_display_name, set_redundancy_level, ObserverEntry,
    TomlEditError,
};

#[cfg(test)]
pub use ctl::testing as ctl_testing;
