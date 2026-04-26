//! `mxnode multikey [--count 4]`: install N multikey observers, no proxy.
//!
//! Same orchestrator as `mxnode observers`, but `install_proxy = false`
//! and `kind = MultikeySquad`. Operators place
//! `allValidatorsKeys.pem` under each `node-{i}/config/` after install.

use mxnode_core::InstallKind;

use crate::cli::GlobalArgs;
use crate::errors::CliError;

#[tokio::main(flavor = "current_thread")]
pub async fn run(count: u16, global: &GlobalArgs) -> Result<(), CliError> {
    crate::commands::observers::drive(count, false, InstallKind::MultikeySquad, "multikey", global)
        .await
}
