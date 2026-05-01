//! Per-node WebSocket `/log` streamer.
//!
//! The MultiversX node serves a binary WebSocket stream at `/log` that
//! emits gogo-protobuf-encoded `LogLineMessage` records. The wire
//! format is plain protobuf (the `stable_marshaler` option only affects
//! the encoder's determinism, not the bytes), so we can decode with
//! [`prost`] using the schema lifted from
//! `multiversx/mx-chain-logger-go/proto/logLineMessage.proto`.
//!
//! Connection model: send the profile identifier as the first message,
//! then loop reading binary frames. On disconnect: backoff 10s then
//! reconnect, mirroring the Go reference's `retryDuration`.
//!
//! Output is appended to the same per-node log ring buffer the file
//! tailer feeds. To avoid duplicate lines the operator opts in via
//! `--ws-logs`; when WS is on, file tail is suppressed by the caller.

use std::sync::Arc;
use std::time::Duration;

use futures_util::SinkExt;
use prost::Message as _;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

use crate::metrics::{LogLevel, LogLine, NodeSnapshot, LOG_BUFFER_CAP};

/// Default-profile identifier sent as a text message immediately after
/// the WS handshake. Mirrors `common.DefaultLogProfileIdentifier` in
/// the upstream Go code.
const DEFAULT_LOG_PROFILE_IDENTIFIER: &str = "default";

const RETRY_BACKOFF: Duration = Duration::from_secs(10);

/// Spawn a WS log streamer. Connects to `ws://<host>:<port>/log` and
/// keeps reconnecting forever.
pub fn spawn(
    host: String,
    port: u16,
    snapshot: Arc<Mutex<NodeSnapshot>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(host, port, snapshot).await;
    })
}

async fn run(host: String, port: u16, snapshot: Arc<Mutex<NodeSnapshot>>) {
    let url = format!("ws://{host}:{port}/log");
    loop {
        match connect_and_stream(&url, &snapshot).await {
            Ok(()) => {
                push(
                    &snapshot,
                    LogLevel::Other,
                    format!("--- {} closed ---", url),
                )
                .await;
            }
            Err(e) => {
                push(
                    &snapshot,
                    LogLevel::Warn,
                    format!(
                        "--- WS error: {e}; retrying in {}s ---",
                        RETRY_BACKOFF.as_secs()
                    ),
                )
                .await;
            }
        }
        tokio::time::sleep(RETRY_BACKOFF).await;
    }
}

async fn connect_and_stream(url: &str, snapshot: &Arc<Mutex<NodeSnapshot>>) -> Result<(), String> {
    let (mut stream, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| e.to_string())?;
    push(
        snapshot,
        LogLevel::Other,
        format!("--- WS connected: {url} ---"),
    )
    .await;

    // Send the profile identifier so the node knows which log
    // patterns to send. We use the default profile (matches Go's
    // sendDefaultProfileIdentifier).
    stream
        .send(Message::Text(DEFAULT_LOG_PROFILE_IDENTIFIER.to_string()))
        .await
        .map_err(|e| e.to_string())?;

    use futures_util::StreamExt as _;
    while let Some(msg) = stream.next().await {
        let msg = msg.map_err(|e| e.to_string())?;
        match msg {
            Message::Binary(bytes) => {
                if let Some(line) = decode_line(&bytes) {
                    let level = level_from_int(line.log_level);
                    push(snapshot, level, format_line(&line)).await;
                }
            }
            Message::Text(s) => {
                // Some chains send plain-text status messages; surface
                // them as info lines rather than failing on the decode.
                push(snapshot, LogLevel::Info, s.to_string()).await;
            }
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

async fn push(snapshot: &Arc<Mutex<NodeSnapshot>>, level: LogLevel, raw: String) {
    let mut snap = snapshot.lock().await;
    if snap.log_lines.len() == LOG_BUFFER_CAP {
        snap.log_lines.pop_front();
    }
    snap.log_lines.push_back(LogLine { level, raw });
}

fn decode_line(bytes: &[u8]) -> Option<LogLineMessage> {
    LogLineMessage::decode(bytes).ok()
}

/// Format a decoded record similarly to the Go `PlainFormatter`:
/// `LEVEL [timestamp] (loggerName) (correlation) message  arg = value …`.
fn format_line(line: &LogLineMessage) -> String {
    let level = level_label(line.log_level);
    let ts = format_ts(line.timestamp);
    let mut out = format!("{level} [{ts}]");
    if !line.logger_name.is_empty() {
        out.push_str(&format!(" [{}]", line.logger_name));
    }
    if let Some(c) = &line.correlation {
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
    out.push_str(&line.message);
    if !line.args.is_empty() {
        out.push(' ');
        let chunks = line.args.chunks(2);
        for pair in chunks {
            if let [k, v] = pair {
                out.push_str(&format!("{k} = {v} "));
            }
        }
    }
    out
}

fn level_label(level: i32) -> &'static str {
    // Matches mx-chain-logger-go's LogLevel enum:
    //   0 LogTrace, 1 LogDebug, 2 LogInfo, 3 LogWarning, 4 LogError, 5 LogNone
    match level {
        0 => "TRACE",
        1 => "DEBUG",
        2 => "INFO ",
        3 => "WARN ",
        4 => "ERROR",
        _ => "?    ",
    }
}

fn level_from_int(level: i32) -> LogLevel {
    match level {
        0 => LogLevel::Trace,
        1 => LogLevel::Debug,
        2 => LogLevel::Info,
        3 => LogLevel::Warn,
        4 => LogLevel::Error,
        _ => LogLevel::Other,
    }
}

fn format_ts(ts_secs: i64) -> String {
    let dt = time::OffsetDateTime::from_unix_timestamp(ts_secs).ok();
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    dt.and_then(|d| d.format(&fmt).ok())
        .unwrap_or_else(|| ts_secs.to_string())
}

// ── Protobuf schema (mx-chain-logger-go/proto/logLineMessage.proto) ──

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

#[cfg(test)]
mod tests {
    use super::*;

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
            timestamp: 1777158180,
            logger_name: "proofscache".to_string(),
            correlation: Some(LogCorrelationMessage {
                shard: "0".to_string(),
                epoch: 5739,
                round: 13858728,
                sub_round: String::new(),
            }),
        };
        let s = format_line(&m);
        assert!(s.starts_with("INFO "));
        assert!(s.contains("[proofscache]"));
        assert!(s.contains("shard=0"));
        assert!(s.contains("added proof to pool"));
        assert!(s.contains("header hash = abc123"));
    }

    #[test]
    fn round_trip_through_prost() {
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
