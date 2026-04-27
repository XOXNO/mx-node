//! Per-node journald tailer (Linux only).
//!
//! Spawns `journalctl --unit <unit> -n 200 --follow --output=cat` as a
//! child process and streams its stdout into the snapshot's log ring
//! buffer. `--output=cat` strips the journald metadata prefix
//! (`<host> <unit>[<pid>]:`) and leaves the raw node-emitted line, so
//! downstream `detect_level` sees the same `INFO`/`WARN`/`ERROR` token
//! the file-tail path sees.
//!
//! Used in place of [`crate::log_tail`] on hosts with journald — the
//! systemd unit pipes stdout to the journal (`StandardOutput=journal`,
//! no `-log-save` on the node), so `<workdir>/logs/*.log` is empty
//! there. The CLI's `mxnode logs` already shells out to `journalctl`;
//! this module brings the dashboard in line.
//!
//! Restart behaviour: if journalctl exits (permission denied, unit
//! removed, SIGPIPE), we surface a marker line, sleep a beat, and
//! respawn. Aborting the JoinHandle on shutdown kills the child.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time;

use crate::metrics::{LogLevel, LogLine, NodeSnapshot, LOG_BUFFER_CAP};

/// Spawn a per-unit journalctl follower.
pub fn spawn(unit: String, snapshot: Arc<Mutex<NodeSnapshot>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(unit, snapshot).await;
    })
}

async fn run(unit: String, snapshot: Arc<Mutex<NodeSnapshot>>) {
    // Backoff so a misconfigured unit (e.g. journalctl returning
    // immediately because the unit doesn't exist) doesn't pin a CPU
    // core. Capped at 5s; a healthy journalctl --follow runs forever.
    let mut backoff = Duration::from_millis(500);
    loop {
        match try_run(&unit, &snapshot).await {
            Ok(()) => {
                push_marker(&snapshot, format!("--- journalctl exited cleanly for {unit}; respawning ---")).await;
            }
            Err(e) => {
                push_marker(&snapshot, format!("--- journalctl error for {unit}: {e} ---")).await;
            }
        }
        time::sleep(backoff).await;
        if backoff < Duration::from_secs(5) {
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
}

async fn try_run(
    unit: &str,
    snapshot: &Arc<Mutex<NodeSnapshot>>,
) -> std::io::Result<()> {
    let mut child = Command::new("journalctl")
        .arg("--unit")
        .arg(unit)
        .arg("-n")
        .arg("200")
        .arg("--follow")
        .arg("--output=cat")
        .arg("--no-pager")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("journalctl child returned no stdout"))?;

    push_marker(snapshot, format!("--- tailing journal for {unit} ---")).await;

    let mut reader = BufReader::new(stdout).lines();
    while let Some(line) = reader.next_line().await? {
        let cleaned = strip_ansi(&line);
        let mut snap = snapshot.lock().await;
        push_line(&mut snap, cleaned);
    }
    let _ = child.wait().await;
    Ok(())
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
    // Same heuristic as log_tail: the node logger writes the level as
    // the first whitespace-delimited token. journald `--output=cat`
    // preserves that exactly.
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

fn strip_ansi(line: &str) -> String {
    // Same plain CSI stripper as log_tail::strip_ansi but operating on
    // a `&str` already split per-line by BufReader.
    let bytes = line.as_bytes();
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

#[cfg(test)]
mod tests {
    use super::{detect_level, strip_ansi};
    use crate::metrics::LogLevel;

    #[test]
    fn detect_level_picks_first_token() {
        assert_eq!(detect_level("INFO [2026-04-26] hello"), LogLevel::Info);
        assert_eq!(detect_level("WARN  something"), LogLevel::Warn);
        assert_eq!(detect_level("ERROR boom"), LogLevel::Error);
        assert_eq!(detect_level("DEBUG x"), LogLevel::Debug);
        assert_eq!(detect_level("plain stdout"), LogLevel::Other);
    }

    #[test]
    fn strip_ansi_removes_csi() {
        assert_eq!(strip_ansi("\x1b[31mERR\x1b[0m hello"), "ERR hello");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
