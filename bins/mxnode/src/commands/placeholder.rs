use crate::cli::Command;
use crate::errors::CliError;

/// Routes the few remaining unimplemented commands through a structured
/// error. Phases 0–3 have shipped; today only `self-update` remains
/// (Phase 3b — depends on shipping signed mxnode releases first).
pub fn not_implemented(cmd: Command, json: bool) -> Result<(), CliError> {
    let (name, phase) = classify(&cmd);
    let err = CliError::new(
        format!("`{name}` is not yet implemented"),
        format!("scheduled for Phase {phase}b — depends on the signed-release distribution pipeline"),
        format!("for now: replace the binary manually via the install one-liner; full self-update lands once mxnode releases ship via cargo-dist + minisign"),
    );
    Err(if json { err.json() } else { err })
}

fn classify(cmd: &Command) -> (&'static str, u8) {
    match cmd {
        // Phase 0 read paths
        Command::Init(_)             => ("init", 0),
        // Adopt removed — folded into `migrate`.
        Command::Migrate(_)          => ("migrate", 0),
        Command::RebuildState        => ("rebuild-state", 0),
        Command::Unlock { .. }       => ("unlock", 0),
        Command::Status(_)           => ("status", 0),
        Command::Logs(_)             => ("logs", 0),
        Command::Doctor              => ("doctor", 0),

        // Phase 1 — lifecycle + observability
        Command::Start(_)            => ("start", 1),
        Command::Stop(_)             => ("stop", 1),
        Command::Restart(_)          => ("restart", 1),
        Command::Db { .. }           => ("db", 1),
        Command::Benchmark           => ("benchmark", 1),
        Command::Keygen(_)           => ("keygen", 1),
        Command::Keys { .. }         => ("keys", 1),
        Command::Cleanup(_)          => ("cleanup", 1),
        Command::Metrics(_)          => ("metrics", 1),
        Command::ReapplyConfig(_)    => ("reapply-config", 3),
        Command::Dashboard(_)        => ("dashboard", 1),

        // Phase 3b — distribution (signed releases)
        Command::SelfUpdate(_)       => ("self-update", 3),

        // Already shipped — placeholder kept for clarity if a future
        // refactor reintroduces routing through here.
        Command::Upgrade(_)          => ("upgrade", 2),
        Command::Rollback(_)         => ("rollback", 2),
        Command::Install(_)          => ("install", 3),
        Command::AddNodes(_)         => ("add-nodes", 3),
        Command::Observers { .. }    => ("observers", 3),
        Command::Multikey { .. }     => ("multikey", 3),

        // Always-on
        Command::Version             => ("version", 0),
        Command::Config { .. }       => ("config", 0),
    }
}
