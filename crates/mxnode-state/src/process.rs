//! PID liveness checks and process birth-time capture.
//!
//! `Inflight` records both the PID and the process's birth time so PID reuse
//! after a crash doesn't fool the stale-vs-live classifier.

#[cfg(target_os = "linux")]
use std::path::Path;

/// Identity of the process that started an in-flight operation.
///
/// Birth time is the kernel's notion of when the process began. On Linux it
/// is field 22 (`starttime`) of `/proc/<pid>/stat`, expressed in jiffies
/// since boot. On macOS it is `pbi_start_tvsec` from `proc_pidinfo`.
///
/// Storing it alongside the PID lets us distinguish "the original mxnode
/// process is still running" from "a new process happens to have reused the
/// PID".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Opaque kernel-supplied birth-time token. Format differs per OS; we
    /// only ever compare by equality with a value we recorded earlier, never
    /// interpret it. `0` means "could not capture" — treat with suspicion.
    #[serde(default)]
    pub started_token: u64,
}

impl ProcessIdentity {
    /// Identity of the current process. Captures `getpid` + a best-effort
    /// birth-time token.
    pub fn current() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            started_token: birth_token(pid).unwrap_or(0),
        }
    }
}

/// Whether a process with the given identity is still running.
///
/// Returns `Live` only when both the PID exists *and* its current birth-time
/// token matches the one recorded. A non-zero stored token that no longer
/// matches a live PID is treated as PID reuse → `Stale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// PID is alive and its birth-time matches the stored token.
    Live,
    /// PID does not exist, or exists but with a different birth-time token.
    Stale,
    /// Could not determine — typically because the kernel denied access to
    /// either the kill probe or the birth-time source. Caller should treat
    /// this conservatively (refuse the op rather than stomp).
    Unknown,
}

pub fn classify(identity: &ProcessIdentity) -> Liveness {
    if identity.pid == 0 {
        return Liveness::Stale;
    }
    match pid_status(identity.pid) {
        PidStatus::Missing => Liveness::Stale,
        PidStatus::Unknown => Liveness::Unknown,
        PidStatus::Present => match birth_token(identity.pid) {
            // We have no recorded birth token (legacy state file) → fall
            // back to PID-only check. This is the previous behaviour.
            _ if identity.started_token == 0 => Liveness::Live,
            Some(now) if now == identity.started_token => Liveness::Live,
            // Different token → PID was reused.
            Some(_) => Liveness::Stale,
            // Could not read the token. Be conservative.
            None => Liveness::Unknown,
        },
    }
}

#[derive(Debug, Clone, Copy)]
enum PidStatus {
    Present,
    Missing,
    Unknown,
}

#[cfg(unix)]
fn pid_status(pid: u32) -> PidStatus {
    // SAFETY: `kill` with sig=0 has no side effects beyond the existence
    // check. Returns 0 when the process exists and we can signal it; -1 with
    // ESRCH when it doesn't; -1 with EPERM when it exists but we lack
    // permission.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return PidStatus::Present;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    match errno {
        libc::ESRCH => PidStatus::Missing,
        libc::EPERM => PidStatus::Present,
        _ => PidStatus::Unknown,
    }
}

#[cfg(not(unix))]
fn pid_status(_pid: u32) -> PidStatus {
    // Non-Unix is not a target platform for mxnode; refuse to act if the
    // file claims an inflight op so we don't silently stomp.
    PidStatus::Unknown
}

/// Capture an opaque birth-time token for `pid`. Returns `None` if the
/// platform-specific source is unreadable.
fn birth_token(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        return linux_starttime(pid);
    }
    #[cfg(target_os = "macos")]
    {
        return macos_starttime(pid);
    }
    #[allow(unreachable_code)]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "linux")]
fn linux_starttime(pid: u32) -> Option<u64> {
    // Field 22 of /proc/<pid>/stat (1-indexed) is `starttime` in jiffies
    // since boot. The line ends with the comm field potentially containing
    // spaces, but everything after the closing `)` of the comm is
    // space-separated.
    let path = format!("/proc/{pid}/stat");
    let raw = std::fs::read_to_string(Path::new(&path)).ok()?;
    let close = raw.rfind(')')?;
    let after = raw.get(close + 1..)?.trim_start();
    let mut fields = after.split_whitespace();
    // Field 3 is state, field 4 is ppid, ... starttime is field 22 from the
    // start, i.e. field 22 - 2 = 20 after the comm closing paren (because
    // pid + comm consume the first two fields).
    fields.nth(19).and_then(|s| s.parse::<u64>().ok())
}

#[cfg(target_os = "macos")]
fn macos_starttime(pid: u32) -> Option<u64> {
    // `proc_pidinfo` with `PROC_PIDTBSDINFO` returns `pbi_start_tvsec` and
    // `pbi_start_tvusec`. We pack them into a single u64 token: high 32 =
    // sec, low 32 = usec. Equality check is all we need.
    use std::mem::MaybeUninit;

    const PROC_PIDTBSDINFO: i32 = 3;
    #[repr(C)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [libc::c_char; 16],
        pbi_name: [libc::c_char; 32],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut std::ffi::c_void,
            buffersize: i32,
        ) -> i32;
    }

    let mut info = MaybeUninit::<ProcBsdInfo>::uninit();
    let size = std::mem::size_of::<ProcBsdInfo>() as i32;
    // SAFETY: `proc_pidinfo` is documented to fill `buffersize` bytes when
    // it returns a positive value equal to the requested size; we discard
    // the value otherwise.
    let rc = unsafe {
        proc_pidinfo(
            pid as i32,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr() as *mut std::ffi::c_void,
            size,
        )
    };
    if rc != size {
        return None;
    }
    // SAFETY: rc == size so the buffer is initialised.
    let info = unsafe { info.assume_init() };
    Some((info.pbi_start_tvsec << 32) | (info.pbi_start_tvusec & 0xFFFF_FFFF))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_pid_is_live_with_matching_token() {
        let me = ProcessIdentity::current();
        assert_eq!(classify(&me), Liveness::Live);
    }

    #[test]
    fn nonexistent_pid_is_stale() {
        // u32::MAX is not a valid Linux/macOS pid; pid_max defaults to
        // 4_194_304 on Linux and 99_999 on macOS.
        let ghost = ProcessIdentity {
            pid: u32::MAX - 1,
            started_token: 0,
        };
        let result = classify(&ghost);
        // We cannot guarantee Missing on every CI runner — accept Stale or
        // Unknown but not Live.
        assert_ne!(result, Liveness::Live, "got {result:?}");
    }

    #[test]
    fn pid_zero_is_stale() {
        let zero = ProcessIdentity {
            pid: 0,
            started_token: 0,
        };
        assert_eq!(classify(&zero), Liveness::Stale);
    }

    #[test]
    fn current_pid_with_wrong_token_is_stale() {
        let mut me = ProcessIdentity::current();
        // Only meaningful when we successfully captured a real token.
        if me.started_token == 0 {
            return;
        }
        me.started_token = me.started_token.wrapping_add(1);
        assert_eq!(
            classify(&me),
            Liveness::Stale,
            "tampered token should classify as PID reuse",
        );
    }
}
