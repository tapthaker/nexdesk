//! Session state helpers used to avoid trapping input on a remote peer when
//! the local desktop becomes unavailable (for example, when the Linux server
//! locks while the pointer is on a macOS client).

#[cfg(target_os = "linux")]
const MAX_LOGINCTL_OUTPUT_BYTES: usize = crate::status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES;

#[cfg(any(target_os = "linux", test))]
fn command_stdout_limited(
    mut command: std::process::Command,
    name: &str,
    max_bytes: usize,
) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        child.kill().ok();
        child.wait().ok();
        return None;
    };

    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(n) => n,
            Err(_) => {
                child.kill().ok();
                child.wait().ok();
                return None;
            }
        };
        if n == 0 {
            break;
        }
        if bytes.len().saturating_add(n) > max_bytes {
            child.kill().ok();
            child.wait().ok();
            tracing::debug!(
                "{} output too large to inspect (max {} bytes)",
                name,
                max_bytes
            );
            return None;
        }
        bytes.extend_from_slice(&buf[..n]);
    }

    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(any(target_os = "linux", test))]
fn valid_loginctl_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
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
    let mut command = std::process::Command::new("loginctl");
    command.args(["list-sessions", "--no-legend"]);
    let Some(stdout) =
        command_stdout_limited(command, "loginctl list-sessions", MAX_LOGINCTL_OUTPUT_BYTES)
    else {
        return false;
    };
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
    if !valid_loginctl_session_id(session_id) {
        return None;
    }
    let mut command = std::process::Command::new("loginctl");
    command.args(["show-session", session_id, "-p", "LockedHint", "--value"]);
    let stdout =
        command_stdout_limited(command, "loginctl show-session", MAX_LOGINCTL_OUTPUT_BYTES)?;

    Some(stdout.trim() == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loginctl_session_ids_are_bounded_and_argument_safe() {
        assert!(valid_loginctl_session_id("2"));
        assert!(valid_loginctl_session_id("c1"));
        assert!(valid_loginctl_session_id("session_1-2.3"));
        assert!(!valid_loginctl_session_id(""));
        assert!(!valid_loginctl_session_id("two words"));
        assert!(!valid_loginctl_session_id("bad\nvalue"));
        assert!(!valid_loginctl_session_id(&"a".repeat(129)));
    }

    #[cfg(unix)]
    #[test]
    fn command_stdout_limited_enforces_output_bound() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf abcdef"]);
        assert_eq!(
            command_stdout_limited(command, "test-command", 6).as_deref(),
            Some("abcdef")
        );

        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf abcdef"]);
        assert!(command_stdout_limited(command, "test-command", 5).is_none());
    }
}
