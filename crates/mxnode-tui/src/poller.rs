//! Per-node REST poller.
//!
//! One tokio task per node. Each task owns a [`NodeClient`] and writes
//! into a shared [`Arc<Mutex<NodeSnapshot>>`] every poll cycle. The App
//! reads a cloned snapshot under a very brief lock during each redraw —
//! the mutex is uncontended in practice because the poller and the UI
//! never overlap on the same lock for more than a microsecond.
//!
//! Retry-on-error is built in: a failed poll updates `last_error` and
//! the next tick tries again. The poller never gives up; the operator
//! sees the failure surface in the UI as `unreachable`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mxnode_rpc::{GatewayClient, NodeClient, RpcError};
use tokio::sync::Mutex;
use tokio::time;

use crate::metrics::{NodeSnapshot, SyncState};

pub struct Poller {
    pub client: NodeClient,
    pub gateway: Option<GatewayClient>,
    pub snapshot: Arc<Mutex<NodeSnapshot>>,
    pub interval: Duration,
}

impl Poller {
    /// Spawn the poller's tokio task. Returns the `JoinHandle` so the
    /// caller can abort on shutdown.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(self) {
        loop {
            let cycle_start = Instant::now();
            self.poll_once().await;
            // Sleep the *remainder* of the interval so a slow REST
            // call doesn't shift the cadence forward.
            let elapsed = cycle_start.elapsed();
            if elapsed < self.interval {
                time::sleep(self.interval - elapsed).await;
            }
        }
    }

    async fn poll_once(&self) {
        let status_res = self.client.status_raw().await;
        let bootstrap_res = self.client.bootstrap_status_raw().await;
        // Multikey nodes manage many validator keys via a single
        // observer process. The count rarely changes (only when the
        // operator updates the keys file) but it's the headline
        // number on the dashboard, so we re-pull it every cycle —
        // the request body is one integer.
        let managed_res = self.client.managed_keys_count().await;

        // Decide whether to opportunistically hit the gateway for
        // trie-statistics. Two sources can tell us our shard id:
        //   1. /node/status (preferred — populated once the node has
        //      finished bootstrapping)
        //   2. /node/bootstrapstatus (works during trie sync, before
        //      /node/status starts answering)
        // We only fetch once per (node, shard) pair; the gateway
        // result is cached in `snap.trie_total_nodes`.
        let mut shard_for_gateway: Option<u32> = None;
        {
            let snap = self.snapshot.lock().await;
            if snap.trie_total_nodes.is_none() {
                let from_status = status_res
                    .as_ref()
                    .ok()
                    .and_then(|m| m.get_u64("erd_shard_id"));
                let from_bootstrap = bootstrap_res
                    .as_ref()
                    .ok()
                    .and_then(|b| b.get_u64("erd_shard_id"));
                if let Some(s) = from_status.or(from_bootstrap) {
                    // u32::MAX = metachain. The metachain has no user
                    // accounts trie so the gateway doesn't return a
                    // meaningful total there; skip.
                    if s != u32::MAX as u64 {
                        shard_for_gateway = Some(s as u32);
                    }
                }
            }
        }

        let mut snap = self.snapshot.lock().await;
        // Bootstrap data lands in the snapshot regardless of whether
        // /node/status succeeded — that's the whole reason the
        // bootstrap endpoint exists. Trie sync metrics live there.
        if let Ok(b) = &bootstrap_res {
            snap.bootstrap = b.clone();
        }
        // Managed-keys count: `Ok(None)` = endpoint not supported
        // (404 — older / non-multikey node). We DON'T overwrite a
        // previously-cached count with `None` because that loses
        // info; only update on success.
        if let Ok(Some(n)) = managed_res {
            snap.managed_keys_count = Some(n);
        } else if matches!(managed_res, Ok(None)) && snap.managed_keys_count.is_none() {
            // Endpoint genuinely unsupported on this node — leave None.
        }
        match status_res {
            Ok(metrics) => {
                snap.metrics = metrics;
                snap.last_update = Some(Instant::now());
                snap.last_error = None;
                push_history(&mut snap);
                snap.recompute_state();
            }
            Err(RpcError::Status { status }) if status == 500 || status == 503 => {
                // Node returned `node is starting`. Bootstrap may
                // still carry trie-sync data — `recompute_state`
                // detects that and surfaces TrieSync, otherwise it
                // falls through to Starting.
                snap.last_update = Some(Instant::now());
                snap.last_error = None;
                snap.recompute_state();
            }
            Err(e) => {
                snap.state = Some(SyncState::Unreachable);
                snap.last_error = Some(e.to_string());
            }
        }
        drop(snap);

        if let (Some(shard), Some(gw)) = (shard_for_gateway, self.gateway.as_ref()) {
            // Network call without holding the snapshot lock — gateway
            // calls cross the internet and we never want to block the
            // renderer waiting for one. Recompute the state after we
            // store the total so the trie-sync percentage shows up
            // retroactively.
            if let Ok(total) = gw.trie_stats(shard).await {
                let mut snap = self.snapshot.lock().await;
                snap.trie_total_nodes = Some(total);
                snap.recompute_state();
            }
        }
    }
}

/// Push the latest sample into each ring buffer. Some metrics are exposed
/// via different keys depending on the chain release; we accept the most
/// common name plus one or two aliases so we don't break across
/// node-binary upgrades.
fn push_history(snap: &mut NodeSnapshot) {
    let nonce = snap.metrics.get_u64("erd_nonce").unwrap_or(0);
    snap.nonce_hist.push(nonce);

    let peers = snap.metrics.get_u64("erd_num_connected_peers").unwrap_or(0);
    snap.peers_hist.push(peers);

    let cpu = snap.metrics.get_u64("erd_cpu_load_percent").unwrap_or(0);
    snap.cpu_hist.push(cpu);

    let mem = snap.metrics.get_u64("erd_mem_load_percent").unwrap_or(0);
    snap.mem_hist.push(mem);

    let netin = snap.metrics.get_u64("erd_network_recv_bps").unwrap_or(0);
    snap.netin_hist.push(netin);

    let netout = snap.metrics.get_u64("erd_network_sent_bps").unwrap_or(0);
    snap.netout_hist.push(netout);
}
