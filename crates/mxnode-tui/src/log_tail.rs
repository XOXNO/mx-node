//! Per-node log file tailer.
//!
//! Picks the most-recently-modified `*.log` file under `<workdir>/logs/`
//! and streams new bytes into a capped ring buffer on the snapshot.
//! Detects rotation by polling the directory's newest log file every
//! poll cycle — when the newest file changes, we reopen.
//!
//! On macOS launchd writes the node's stdout to `<workdir>/logs/stdout.log`;
//! the node binary itself also writes structured logs as
//! `mx-chain-{pubkey}-{round}.log`. We just pick the newest, regardless
//! of which one — both contain the same level-tagged log lines.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::Mutex;
use tokio::time;

use crate::metrics::{LogLevel, LogLine, NodeSnapshot, LOG_BUFFER_CAP};

/// Spawn a tailer for `<workdir>/logs/`. The task runs forever; abort
/// on shutdown.
pub fn spawn(workdir: PathBuf, snapshot: Arc<Mutex<NodeSnapshot>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(workdir, snapshot).await;
    })
}

async fn run(workdir: PathBuf, snapshot: Arc<Mutex<NodeSnapshot>>) {
    let logs_dir = workdir.join("logs");
    let mut current: Option<TailState> = None;
    let mut tick = time::interval(Duration::from_millis(400));
    loop {
        tick.tick().await;

        // Decide which file to read. If the newest *.log differs from
        // what we're tailing, switch. Re-detection is cheap (one
        // read_dir + a few stat calls).
        let newest = newest_log(&logs_dir).await;
        if let Some(path) = newest {
            let same = current.as_ref().map(|t| t.path == path).unwrap_or(false);
            if !same {
                if let Ok(state) = TailState::open(&path).await {
                    current = Some(state);
                    push_marker(&snapshot, format!("--- tailing {} ---", path.display())).await;
                } else {
                    current = None;
                    continue;
                }
            }
        } else {
            // No log file yet — node may be starting. Try again next tick.
            continue;
        }

        let state = match &mut current {
            Some(s) => s,
            None => continue,
        };
        if let Err(e) = state.read_into(&snapshot).await {
            push_marker(&snapshot, format!("--- log tail error: {e} ---")).await;
            // Force re-detect on next tick.
            current = None;
        }
    }
}

struct TailState {
    path: PathBuf,
    file: File,
    leftover: Vec<u8>,
}

impl TailState {
    async fn open(path: &Path) -> std::io::Result<Self> {
        let mut file = OpenOptions::new().read(true).open(path).await?;
        file.seek(SeekFrom::End(0)).await?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            leftover: Vec::new(),
        })
    }

    async fn read_into(&mut self, snapshot: &Arc<Mutex<NodeSnapshot>>) -> std::io::Result<()> {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = self.file.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            self.leftover.extend_from_slice(&buf[..n]);
            // Split off complete lines (everything up to the last
            // newline) and leave the trailing partial in `leftover`.
            let mut lines: VecDeque<String> = VecDeque::new();
            let mut last_nl = 0usize;
            for (i, b) in self.leftover.iter().enumerate() {
                if *b == b'\n' {
                    let raw = &self.leftover[last_nl..i];
                    lines.push_back(strip_ansi(raw));
                    last_nl = i + 1;
                }
            }
            if last_nl > 0 {
                self.leftover.drain(..last_nl);
            }
            if lines.is_empty() {
                continue;
            }
            let mut snap = snapshot.lock().await;
            for line in lines {
                push_line(&mut snap, line);
            }
        }
    }
}

async fn push_marker(snapshot: &Arc<Mutex<NodeSnapshot>>, text: String) {
    let mut snap = snapshot.lock().await;
    push_line(&mut snap, text);
}

fn push_line(snap: &mut NodeSnapshot, raw: String) {
    let level = detect_level(&raw);
    if snap.log_lines.len() == LOG_BUFFER_CAP {
        snap.log_lines.pop_front();
    }
    snap.log_lines.push_back(LogLine { level, raw });
}

fn detect_level(line: &str) -> LogLevel {
    // The MultiversX node logger emits lines like
    //   `INFO [2026-04-26 ...]  ...`
    // or
    //   `DEBUG [...] ...`
    // — with the level as the first whitespace-delimited token. Some
    // logs (e.g. those routed via stdout) prefix with a level-coloured
    // ANSI escape we already stripped, so we just look for the literal
    // word in the first 12 chars.
    let head = &line[..line.len().min(12)];
    if head.contains("ERROR") {
        LogLevel::Error
    } else if head.contains("WARN") {
        LogLevel::Warn
    } else if head.contains("DEBUG") {
        LogLevel::Debug
    } else if head.contains("TRACE") {
        LogLevel::Trace
    } else if head.contains("INFO") {
        LogLevel::Info
    } else {
        LogLevel::Other
    }
}

fn strip_ansi(bytes: &[u8]) -> String {
    // Plain CSI stripper: drop `ESC [ ... <terminator>` sequences plus
    // the rare 2-byte escapes the node logger sometimes emits.
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'[' {
                let mut j = i + 2;
                while j < bytes.len() {
                    let c = bytes[j];
                    j += 1;
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
                i = j;
                continue;
            }
            // Two-byte escape (e.g. ESC ( B); skip both.
            i += 2;
            continue;
        }
        if b == b'\r' {
            i += 1;
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

async fn newest_log(dir: &Path) -> Option<PathBuf> {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return None,
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let mtime = match entry.metadata().await {
            Ok(m) => m.modified().ok(),
            Err(_) => None,
        };
        if let Some(t) = mtime {
            match &best {
                Some((bt, _)) if *bt >= t => {}
                _ => best = Some((t, path)),
            }
        }
    }
    best.map(|(_, p)| p)
}
