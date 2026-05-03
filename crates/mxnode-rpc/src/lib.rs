//! Typed REST client for the local MultiversX node and proxy.
//!
//! `mxnode status` and `mxnode logs --save-archive` consume the small
//! typed [`NodeMetrics`] subset. The dashboard TUI consumes the full
//! [`RawMetrics`] flat map (every `erd_*` key the node exposes) so we
//! don't have to enumerate every metric in the type system — the metric
//! surface evolves with chain releases and most fields are
//! display-only.

use std::collections::BTreeMap;

use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use serde::Deserialize;
use thiserror::Error;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("node returned {status}")]
    Status { status: u16 },

    #[error("malformed response: {0}")]
    Malformed(String),

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeStatus {
    pub data: NodeStatusData,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeStatusData {
    pub metrics: NodeMetrics,
}

/// Subset of `/node/status` metrics we consume.
///
/// MultiversX prefixes every metric with `erd_`. Most are returned as numeric
/// strings; we keep them as `String` here and parse at the call site to keep
/// this layer dumb.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NodeMetrics {
    #[serde(default, rename = "erd_nonce")]
    pub erd_nonce: Option<u64>,
    #[serde(default, rename = "erd_is_syncing")]
    pub erd_is_syncing: Option<u64>,
    #[serde(default, rename = "erd_app_version")]
    pub erd_app_version: Option<String>,
    #[serde(default, rename = "erd_public_key_block_sign")]
    pub erd_public_key_block_sign: Option<String>,
    #[serde(default, rename = "erd_shard_id")]
    pub erd_shard_id: Option<u32>,
    #[serde(default, rename = "erd_consensus_state")]
    pub erd_consensus_state: Option<String>,
}

impl NodeMetrics {
    /// 12-character hex prefix used by the bash `get_logs` to name log files.
    /// Returns `None` if the field is missing or shorter than 12 chars.
    pub fn pubkey_prefix(&self) -> Option<&str> {
        self.erd_public_key_block_sign.as_deref().and_then(|p| {
            if p.len() >= 12 {
                Some(&p[..12])
            } else {
                None
            }
        })
    }
}

pub struct NodeClient {
    base_url: String,
    http: reqwest::Client,
}

/// Identifier sent by the upstream logviewer/termui when it wants the node's
/// current default log profile instead of a custom JSON profile.
pub const DEFAULT_LOG_PROFILE_IDENTIFIER: &str = "[default log profile]";

/// Runtime log profile accepted by the node's `/log` WebSocket.
///
/// Field names intentionally match `mx-chain-logger-go/logger.Profile` JSON.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogProfile {
    #[serde(rename = "LogLevelPatterns")]
    pub log_level_patterns: String,
    #[serde(rename = "WithCorrelation")]
    pub with_correlation: bool,
    #[serde(rename = "WithLoggerName")]
    pub with_logger_name: bool,
}

impl LogProfile {
    pub fn new(
        log_level_patterns: impl Into<String>,
        with_correlation: bool,
        with_logger_name: bool,
    ) -> Self {
        Self {
            log_level_patterns: log_level_patterns.into(),
            with_correlation,
            with_logger_name,
        }
    }
}

pub struct LogStream {
    inner: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogStreamEvent {
    Line(LogLineMessage),
    Text(String),
    Closed,
}

impl LogStream {
    pub async fn connect(
        host: &str,
        port: u16,
        use_wss: bool,
        profile: Option<LogProfile>,
    ) -> Result<Self, RpcError> {
        let scheme = if use_wss { "wss" } else { "ws" };
        let url = format!("{scheme}://{host}:{port}/log");
        let (mut inner, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(ws_error)?;
        let profile_message = match profile {
            Some(profile) => serde_json::to_string(&profile)?,
            None => DEFAULT_LOG_PROFILE_IDENTIFIER.to_string(),
        };
        inner
            .send(Message::Text(profile_message))
            .await
            .map_err(ws_error)?;
        Ok(Self { inner })
    }

    pub async fn next_event(&mut self) -> Result<LogStreamEvent, RpcError> {
        while let Some(msg) = self.inner.next().await {
            match msg.map_err(ws_error)? {
                Message::Binary(bytes) => {
                    let line = LogLineMessage::decode(&bytes[..])
                        .map_err(|e| RpcError::Malformed(e.to_string()))?;
                    return Ok(LogStreamEvent::Line(line));
                }
                Message::Text(text) => return Ok(LogStreamEvent::Text(text.to_string())),
                Message::Close(_) => return Ok(LogStreamEvent::Closed),
                _ => {}
            }
        }
        Ok(LogStreamEvent::Closed)
    }
}

fn ws_error(e: tokio_tungstenite::tungstenite::Error) -> RpcError {
    RpcError::WebSocket(e.to_string())
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LogLineMessage {
    #[prost(string, tag = "1")]
    pub message: String,
    #[prost(int32, tag = "2")]
    pub log_level: i32,
    #[prost(string, repeated, tag = "3")]
    pub args: ::prost::alloc::vec::Vec<String>,
    #[prost(int64, tag = "4")]
    pub timestamp: i64,
    #[prost(string, tag = "5")]
    pub logger_name: String,
    #[prost(message, optional, tag = "6")]
    pub correlation: Option<LogCorrelationMessage>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LogCorrelationMessage {
    #[prost(string, tag = "1")]
    pub shard: String,
    #[prost(uint32, tag = "2")]
    pub epoch: u32,
    #[prost(int64, tag = "3")]
    pub round: i64,
    #[prost(string, tag = "4")]
    pub sub_round: String,
}

impl LogLineMessage {
    pub fn level_label(&self) -> &'static str {
        match self.log_level {
            0 => "TRACE",
            1 => "DEBUG",
            2 => "INFO ",
            3 => "WARN ",
            4 => "ERROR",
            _ => "?    ",
        }
    }

    /// Format similarly to the upstream Go `PlainFormatter`.
    pub fn format_plain(&self) -> String {
        let mut out = format!("{} [{}]", self.level_label(), format_log_ts(self.timestamp));
        if !self.logger_name.is_empty() {
            out.push_str(&format!(" [{}]", self.logger_name));
        }
        if let Some(c) = &self.correlation {
            let mut bits = Vec::new();
            if !c.shard.is_empty() {
                bits.push(format!("shard={}", c.shard));
            }
            if c.epoch != 0 {
                bits.push(format!("epoch={}", c.epoch));
            }
            if c.round != 0 {
                bits.push(format!("round={}", c.round));
            }
            if !c.sub_round.is_empty() {
                bits.push(format!("sub={}", c.sub_round));
            }
            if !bits.is_empty() {
                out.push_str(&format!(" [{}]", bits.join(" ")));
            }
        }
        out.push(' ');
        out.push_str(&self.message);
        for pair in self.args.chunks(2) {
            if let [k, v] = pair {
                out.push_str(&format!(" {k} = {v}"));
            }
        }
        out
    }
}

fn format_log_ts(raw: i64) -> String {
    let dt = if raw.unsigned_abs() > 10_000_000_000 {
        time::OffsetDateTime::from_unix_timestamp_nanos(raw as i128).ok()
    } else {
        time::OffsetDateTime::from_unix_timestamp(raw).ok()
    };
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    dt.and_then(|d| d.format(&fmt).ok())
        .unwrap_or_else(|| raw.to_string())
}

impl NodeClient {
    /// `host` is typically `127.0.0.1`; `port` is `node.api_port_base + index`.
    pub fn new(host: &str, port: u16) -> Result<Self, RpcError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()?;
        Ok(Self {
            base_url: format!("http://{host}:{port}"),
            http,
        })
    }

    pub async fn status(&self) -> Result<NodeStatus, RpcError> {
        let url = format!("{}/node/status", self.base_url);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(RpcError::Status {
                status: resp.status().as_u16(),
            });
        }
        let status: NodeStatus = resp.json().await?;
        Ok(status)
    }

    /// Fetch the full `/node/status` metric map without losing any keys.
    /// Used by the dashboard TUI; the typed [`NodeMetrics`] subset is
    /// enough for `mxnode status`.
    pub async fn status_raw(&self) -> Result<RawMetrics, RpcError> {
        let url = format!("{}/node/status", self.base_url);
        self.fetch_metrics_map(&url).await
    }

    /// Fetch `/node/bootstrapstatus`. The bootstrap endpoint exposes
    /// trie-sync progress and a few keys that aren't in `/node/status`
    /// while the node is still bootstrapping.
    pub async fn bootstrap_status_raw(&self) -> Result<RawMetrics, RpcError> {
        let url = format!("{}/node/bootstrapstatus", self.base_url);
        self.fetch_metrics_map(&url).await
    }

    /// Number of validator keys this node manages (multikey nodes
    /// only). Returns `None` when the endpoint isn't supported (older
    /// nodes / non-multikey builds returning 404). Returns `Some(0)`
    /// for a multikey-capable node that currently manages no keys.
    pub async fn managed_keys_count(&self) -> Result<Option<u64>, RpcError> {
        let url = format!("{}/node/managed-keys/count", self.base_url);
        let resp = self.http.get(&url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(RpcError::Status {
                status: resp.status().as_u16(),
            });
        }
        let body: serde_json::Value = resp.json().await?;
        // Shape: { "data": { "count": N }, "error": "", "code": "successful" }
        let count = body
            .get("data")
            .and_then(|d| d.get("count"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| RpcError::Malformed("missing data.count".to_string()))?;
        Ok(Some(count))
    }

    async fn fetch_metrics_map(&self, url: &str) -> Result<RawMetrics, RpcError> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            // Bootstrap endpoint returns 500 with `node is starting` JSON
            // while the node is initialising. Surface as Status so the
            // caller can label it specifically without a parse attempt.
            return Err(RpcError::Status {
                status: status.as_u16(),
            });
        }
        let body: serde_json::Value = resp.json().await?;
        let metrics = body
            .get("data")
            .and_then(|d| d.get("metrics"))
            .ok_or_else(|| RpcError::Malformed("missing data.metrics".to_string()))?;
        let map = metrics
            .as_object()
            .ok_or_else(|| RpcError::Malformed("data.metrics is not an object".to_string()))?;
        let mut out = BTreeMap::new();
        for (k, v) in map {
            out.insert(k.clone(), v.clone());
        }
        Ok(RawMetrics(out))
    }
}

/// Talks to a public MultiversX gateway (e.g. gateway.multiversx.com).
/// Used by the dashboard for `/network/trie-statistics/<shard>` —
/// observers can't compute the totals locally so we ask a gateway.
pub struct GatewayClient {
    base_url: String,
    http: reqwest::Client,
}

impl GatewayClient {
    pub fn new(base_url: &str) -> Result<Self, RpcError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Fetch the user-accounts-snapshot trie node count for `shard`.
    /// The endpoint returns the same shape as `/node/status`:
    /// `{ "data": { "accounts-snapshot-num-nodes": N }, "error": "", "code": "" }`.
    pub async fn trie_stats(&self, shard: u32) -> Result<u64, RpcError> {
        let url = format!("{}/network/trie-statistics/{}", self.base_url, shard);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(RpcError::Status {
                status: resp.status().as_u16(),
            });
        }
        let body: serde_json::Value = resp.json().await?;
        let n = body
            .get("data")
            .and_then(|d| d.get("accounts-snapshot-num-nodes"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                RpcError::Malformed("missing data.accounts-snapshot-num-nodes".to_string())
            })?;
        Ok(n)
    }
}

/// Flat metric map keyed by the node's `erd_*` names. Provides typed
/// accessors for the common shapes (uint64, signed int, string, bool).
#[derive(Debug, Clone, Default)]
pub struct RawMetrics(pub BTreeMap<String, serde_json::Value>);

impl RawMetrics {
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.0.get(key)
    }

    /// Many `/node/status` numerics are returned as JSON numbers; the
    /// node's status handler also serialises a few as strings (for very
    /// large hashes for instance). We accept both forms — falling back
    /// to `parse()` on strings — so callers never have to care.
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        match self.0.get(key)? {
            serde_json::Value::Number(n) => n.as_u64(),
            serde_json::Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        match self.0.get(key)? {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        match self.0.get(key)? {
            serde_json::Value::Number(n) => n.as_f64(),
            serde_json::Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.as_str())
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.0.get(key)? {
            serde_json::Value::Bool(b) => Some(*b),
            serde_json::Value::Number(n) => n.as_u64().map(|v| v != 0),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pubkey_prefix_truncates_to_twelve() {
        let metrics = NodeMetrics {
            erd_public_key_block_sign: Some("abcdef0123456789abcdef".to_string()),
            ..Default::default()
        };
        assert_eq!(metrics.pubkey_prefix(), Some("abcdef012345"));
    }

    #[test]
    fn pubkey_prefix_returns_none_when_short() {
        let metrics = NodeMetrics {
            erd_public_key_block_sign: Some("abc".to_string()),
            ..Default::default()
        };
        assert_eq!(metrics.pubkey_prefix(), None);
    }

    #[test]
    fn pubkey_prefix_returns_none_when_missing() {
        let metrics = NodeMetrics::default();
        assert_eq!(metrics.pubkey_prefix(), None);
    }

    #[test]
    fn log_profile_serializes_with_go_field_names() {
        let profile = LogProfile::new("*:DEBUG,api:INFO", true, true);
        let json = serde_json::to_value(profile).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "LogLevelPatterns": "*:DEBUG,api:INFO",
                "WithCorrelation": true,
                "WithLoggerName": true,
            })
        );
    }

    #[test]
    fn default_log_profile_identifier_matches_upstream() {
        assert_eq!(DEFAULT_LOG_PROFILE_IDENTIFIER, "[default log profile]");
    }

    #[test]
    fn formats_decoded_log_line() {
        let m = LogLineMessage {
            message: "added proof to pool".to_string(),
            log_level: 2,
            args: vec![
                "header hash".to_string(),
                "abc123".to_string(),
                "epoch".to_string(),
                "5739".to_string(),
            ],
            timestamp: 1_777_158_180_000_000_000,
            logger_name: "proofscache".to_string(),
            correlation: Some(LogCorrelationMessage {
                shard: "0".to_string(),
                epoch: 5739,
                round: 13858728,
                sub_round: String::new(),
            }),
        };
        let s = m.format_plain();
        assert!(s.starts_with("INFO "));
        assert!(s.contains("[proofscache]"));
        assert!(s.contains("shard=0"));
        assert!(s.contains("added proof to pool"));
        assert!(s.contains("header hash = abc123"));
    }

    #[test]
    fn log_line_round_trips_through_prost() {
        let m = LogLineMessage {
            message: "hi".to_string(),
            log_level: 3,
            args: vec!["k".to_string(), "v".to_string()],
            timestamp: 42,
            logger_name: "lg".to_string(),
            correlation: None,
        };
        let bytes = m.encode_to_vec();
        let back = LogLineMessage::decode(&bytes[..]).unwrap();
        assert_eq!(back.message, "hi");
        assert_eq!(back.log_level, 3);
        assert_eq!(back.args, vec!["k", "v"]);
    }
}
