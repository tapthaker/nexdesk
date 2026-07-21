/// Session state helpers used to avoid trapping input on a remote peer when
/// the local desktop becomes unavailable (for example, when the Linux server
/// locks while the pointer is on a macOS client).
use color_eyre::eyre::Result;

#[cfg(target_os = "linux")]
use crate::command::{run_command, RealCommandRunner};
#[cfg(target_os = "linux")]
use crate::ports::CommandRunner;
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
    linux_session_locked_with(&RealCommandRunner)
}

#[cfg(target_os = "linux")]
fn linux_session_locked_with(runner: &dyn CommandRunner) -> bool {
    if let Ok(session_id) = std::env::var("XDG_SESSION_ID") {
        if session_locked_hint(runner, &session_id).unwrap_or(false) {
            return true;
        }
    }

    // User services often do not preserve XDG_SESSION_ID. Fall back to checking
    // logind sessions owned by this uid and treat any locked session as locked.
    let uid = unsafe { libc::geteuid() }.to_string();
    let Ok(output) = run_command(runner, "loginctl", &["list-sessions", "--no-legend"]) else {
        return false;
    };

    if !output.success {
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
        session_uid == uid && session_locked_hint(runner, session_id).unwrap_or(false)
    })
}

#[cfg(target_os = "linux")]
fn session_locked_hint(runner: &dyn CommandRunner, session_id: &str) -> Option<bool> {
    let output = run_command(
        runner,
        "loginctl",
        &["show-session", session_id, "-p", "LockedHint", "--value"],
    )
    .ok()?;

    if !output.success {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).trim() == "yes")
}
