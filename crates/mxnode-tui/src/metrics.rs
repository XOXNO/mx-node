//! Per-node snapshot consumed by the renderer.
//!
//! The poller writes one of these every poll cycle; the App reads a
//! cloned copy under a brief lock. History ring buffers carry the most
//! recent N samples for sparklines (nonce, peer count, CPU, memory, net
//! in/out).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use mxnode_rpc::RawMetrics;

const HISTORY_LEN: usize = 120;
pub const LOG_BUFFER_CAP: usize = 1000;

/// Coarse log level used to colour the bottom log panel. Detection
/// happens off the raw text by looking at the first whitespace token —
/// the node logger emits `INFO [...]`, `DEBUG [...]`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    /// Couldn't classify the line (banner separator, table border, etc.).
    Other,
}

impl LogLevel {
    /// Ordering for the panel filter — higher number = more severe.
    /// `Other` is mapped to whatever its inherited classification is at
    /// render time, so it shouldn't normally be passed here directly.
    pub fn severity(self) -> u8 {
        match self {
            LogLevel::Trace => 0,
            LogLevel::Debug => 1,
            LogLevel::Info => 2,
            LogLevel::Warn => 3,
            LogLevel::Error => 4,
            LogLevel::Other => 0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Other => "?",
        }
    }

    /// Step the threshold up (more strict).
    pub fn step_up(self) -> Self {
        match self {
            LogLevel::Trace => LogLevel::Debug,
            LogLevel::Debug => LogLevel::Info,
            LogLevel::Info => LogLevel::Warn,
            LogLevel::Warn => LogLevel::Error,
            LogLevel::Error | LogLevel::Other => LogLevel::Error,
        }
    }

    /// Step the threshold down (more verbose).
    pub fn step_down(self) -> Self {
        match self {
            LogLevel::Error => LogLevel::Warn,
            LogLevel::Warn => LogLevel::Info,
            LogLevel::Info => LogLevel::Debug,
            LogLevel::Debug => LogLevel::Trace,
            LogLevel::Trace | LogLevel::Other => LogLevel::Trace,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: LogLevel,
    pub raw: String,
}

/// What state the node is in, derived from `/node/status` +
/// `/node/bootstrapstatus`. Drives the colour-coded sync header and the
/// "is this node healthy" decision in the tab list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    /// REST endpoint not reachable (connection refused / timeout / 5xx
    /// from a port that isn't even listening yet).
    Unreachable,
    /// `/node/status` returned `node is starting` AND we have no trie
    /// sync metrics yet — the very-early bootstrap phase before the
    /// node has decided what to do. Once trie sync kicks in we
    /// transition to [`SyncState::TrieSync`].
    Starting,
    /// Trie sync running. Renderer derives the progress percentage in
    /// this priority order:
    ///   1. `pct` — direct from `erd_trie_sync_processed_percentage`
    ///      when the node exposes it.
    ///   2. `processed / NodeSnapshot::trie_total_nodes` — when the
    ///      gateway responded to `/network/trie-statistics/<shard>`.
    ///   3. None — show a marquee / spinner instead of a fixed bar.
    TrieSync { processed: u64, pct: Option<u64> },
    /// Block sync — node is catching up to the network's tip.
    BlockSync { nonce: u64, target: u64 },
    /// Caught up to within K blocks of the tip.
    Synced { nonce: u64 },
}

impl SyncState {
    pub fn label(&self) -> &'static str {
        match self {
            SyncState::Unreachable => "unreachable",
            SyncState::Starting => "starting",
            SyncState::TrieSync { .. } => "trie sync",
            SyncState::BlockSync { .. } => "block sync",
            SyncState::Synced { .. } => "synchronized",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct History {
    samples: VecDeque<u64>,
}

impl History {
    pub fn push(&mut self, v: u64) {
        if self.samples.len() == HISTORY_LEN {
            self.samples.pop_front();
        }
        self.samples.push_back(v);
    }

    pub fn samples(&self) -> &VecDeque<u64> {
        &self.samples
    }

    pub fn last(&self) -> Option<u64> {
        self.samples.back().copied()
    }

    pub fn as_vec(&self) -> Vec<u64> {
        self.samples.iter().copied().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct NodeSnapshot {
    /// Wall-clock time of the most recent successful poll.
    pub last_update: Option<Instant>,
    /// Last error, if the most recent poll failed. Cleared on success.
    pub last_error: Option<String>,
    /// Derived sync state.
    pub state: Option<SyncState>,
    /// Full /node/status payload.
    pub metrics: RawMetrics,
    /// Full /node/bootstrapstatus payload (mostly trie sync details).
    pub bootstrap: RawMetrics,
    /// History rings.
    pub nonce_hist: History,
    pub peers_hist: History,
    pub cpu_hist: History,
    pub mem_hist: History,
    pub netin_hist: History,
    pub netout_hist: History,
    /// Total trie nodes (from the gateway). Cached after the first
    /// successful gateway lookup; `None` until then or if the gateway
    /// was unreachable.
    pub trie_total_nodes: Option<u64>,
    /// Number of validator keys this node manages, populated by the
    /// poller via `/node/managed-keys/count`. `None` means the
    /// endpoint isn't supported (older / non-multikey node);
    /// `Some(0)` means the node is multikey-capable but currently
    /// manages no keys; `Some(N)` for an actively-managing node.
    pub managed_keys_count: Option<u64>,
    /// Tail of the node's log file (newest line at the back).
    pub log_lines: VecDeque<LogLine>,
}

impl NodeSnapshot {
    /// Re-derive `state` from whatever metrics we currently hold.
    /// Cheap; called by the poller after every successful poll, and
    /// also after the gateway trie-stats lookup so the percentage
    /// shows up retroactively.
    pub fn recompute_state(&mut self) {
        // 1. Trie sync — `/node/bootstrapstatus` exposes the trie sync
        //    counters while the user-accounts trie is downloading.
        //    This branch fires even when `/node/status` is returning
        //    500 (`node is starting`) so the operator gets a real
        //    progress bar instead of a flat "starting" badge.
        if let Some(processed) = self
            .bootstrap
            .get_u64("erd_trie_sync_num_processed_nodes")
            .filter(|n| *n > 0)
        {
            let pct = self
                .bootstrap
                .get_u64("erd_trie_sync_processed_percentage")
                .filter(|p| *p > 0);
            self.state = Some(SyncState::TrieSync { processed, pct });
            return;
        }

        // 2. Block sync — node has metrics but is behind the network.
        let nonce = self.metrics.get_u64("erd_nonce").unwrap_or(0);
        let probable = self
            .metrics
            .get_u64("erd_probable_highest_nonce")
            .unwrap_or(nonce);
        if probable > nonce + 5 {
            self.state = Some(SyncState::BlockSync {
                nonce,
                target: probable,
            });
            return;
        }

        // 3. Synced (within 5 blocks of the network tip).
        if nonce > 0 {
            self.state = Some(SyncState::Synced { nonce });
            return;
        }

        // 4. Neither status nor bootstrap carries useful data yet →
        //    very early bootstrap.
        if self.metrics.is_empty() {
            self.state = Some(SyncState::Starting);
        }
    }

    /// Effective trie-sync percentage for the renderer. Combines the
    /// node-direct value (when reported) and the gateway-derived
    /// fraction (`processed / trie_total_nodes`). Returns `None` when
    /// neither source is available — the renderer should show a
    /// spinner or just textual progress in that case.
    pub fn trie_sync_pct(&self) -> Option<u64> {
        let SyncState::TrieSync { processed, pct } = self.state.as_ref()? else {
            return None;
        };
        if let Some(p) = *pct {
            return Some(p.min(100));
        }
        let total = self.trie_total_nodes?;
        if total == 0 {
            return None;
        }
        Some(((*processed) * 100 / total).min(100))
    }

    pub fn is_stale(&self, threshold: Duration) -> bool {
        self.last_update
            .map(|t| t.elapsed() > threshold)
            .unwrap_or(true)
    }
}
