/// Session state helpers used to avoid trapping input on a remote peer when
/// the local desktop becomes unavailable (for example, when the Linux server
/// locks while the pointer is on a macOS client).

#[cfg(target_os = "linux")]
use std::process::Command;

use color_eyre::eyre::Result;

use crate::ports::LocalSessionLockSource;

#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformLocalSessionLockSource;

impl LocalSessionLockSource for PlatformLocalSessionLockSource {
    fn is_locked(&self) -> Result<bool> {
        Ok(is_session_locked())
    }
}

/// Returns true when the current graphical session appears to be locked.
///
/// This is intentionally best-effort: if we cannot query logind, return false
/// rather than interrupting an active sharing session spuriously.
pub fn is_session_locked() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_session_locked()
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn linux_session_locked() -> bool {
    if let Ok(session_id) = std::env::var("XDG_SESSION_ID") {
        if session_locked_hint(&session_id).unwrap_or(false) {
            return true;
        }
    }

    // User services often do not preserve XDG_SESSION_ID. Fall back to checking
    // logind sessions owned by this uid and treat any locked session as locked.
    let uid = unsafe { libc::geteuid() }.to_string();
    let Ok(output) = Command::new("loginctl")
        .args(["list-sessions", "--no-legend"])
        .output()
    else {
        return false;
    };

    if !output.status.success() {
        return false;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|line| {
        let mut parts = line.split_whitespace();
        let Some(session_id) = parts.next() else {
            return false;
        };
        let Some(session_uid) = parts.next() else {
            return false;
        };
        session_uid == uid && session_locked_hint(session_id).unwrap_or(false)
    })
}

#[cfg(target_os = "linux")]
fn session_locked_hint(session_id: &str) -> Option<bool> {
    let output = Command::new("loginctl")
        .args(["show-session", session_id, "-p", "LockedHint", "--value"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).trim() == "yes")
}
