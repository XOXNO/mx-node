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

use mxnode_rpc::{LogStream, LogStreamEvent};
use tokio::sync::Mutex;

use crate::metrics::{LogLevel, LogLine, NodeSnapshot, LOG_BUFFER_CAP};

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
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!("invalid url {url}"));
    };
    let Some((host, port)) = rest.rsplit_once(':') else {
        return Err(format!("invalid url {url}"));
    };
    let port = port.parse::<u16>().map_err(|e| e.to_string())?;
    let mut stream = LogStream::connect(host, port, scheme == "wss", None)
        .await
        .map_err(|e| e.to_string())?;
    push(
        snapshot,
        LogLevel::Other,
        format!("--- WS connected: {url} ---"),
    )
    .await;

    loop {
        match stream.next_event().await.map_err(|e| e.to_string())? {
            LogStreamEvent::Line(line) => {
                let level = level_from_int(line.log_level);
                push(snapshot, level, line.format_plain()).await;
            }
            LogStreamEvent::Text(s) => push(snapshot, LogLevel::Info, s).await,
            LogStreamEvent::Closed => return Ok(()),
        }
    }
}

async fn push(snapshot: &Arc<Mutex<NodeSnapshot>>, level: LogLevel, raw: String) {
    let mut snap = snapshot.lock().await;
    if snap.log_lines.len() == LOG_BUFFER_CAP {
        snap.log_lines.pop_front();
    }
    snap.log_lines.push_back(LogLine { level, raw });
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

#[cfg(test)]
mod tests {
    use mxnode_rpc::{LogCorrelationMessage, LogLineMessage};
    use prost::Message as _;

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
        let s = m.format_plain();
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
