use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use ring::digest;
use tracing::{debug, info, warn};

use crate::net::protocol::{ClipboardContent, Message, MAX_CLIPBOARD_TEXT_BYTES};

fn clipboard_text_size_ok(text: &str) -> bool {
    text.len() <= MAX_CLIPBOARD_TEXT_BYTES
}

/// Monitors the local clipboard for changes and produces protocol messages.
pub struct ClipboardSync {
    last_hash: Option<Vec<u8>>,
}

impl ClipboardSync {
    pub fn new() -> Self {
        info!("Clipboard sync initialized");
        Self { last_hash: None }
    }

    /// Check if the clipboard has changed. Returns a message if it has.
    pub fn poll_change(&mut self) -> Result<Option<Message>> {
        let text = match read_clipboard() {
            Ok(t) => t,
            Err(e) => {
                debug!("Clipboard text unavailable: {}", e);
                return Ok(None);
            }
        };

        let hash = digest::digest(&digest::SHA256, text.as_bytes());
        let hash_bytes = hash.as_ref().to_vec();

        if self.last_hash.as_ref() == Some(&hash_bytes) {
            return Ok(None);
        }

        self.last_hash = Some(hash_bytes);
        if !clipboard_text_size_ok(&text) {
            warn!(
                "Clipboard changed but is too large to sync ({} bytes, max {})",
                text.len(),
                MAX_CLIPBOARD_TEXT_BYTES
            );
            return Ok(None);
        }
        info!("Clipboard changed ({} bytes), sending to peer", text.len());

        Ok(Some(Message::ClipboardUpdate {
            content: ClipboardContent::Text(text),
        }))
    }

    /// Apply a clipboard update from a peer.
    pub fn apply_update(&mut self, content: &ClipboardContent) -> Result<()> {
        match content {
            ClipboardContent::Text(text) => {
                if !clipboard_text_size_ok(text) {
                    return Err(eyre!(
                        "Clipboard update too large: {} bytes (max {})",
                        text.len(),
                        MAX_CLIPBOARD_TEXT_BYTES
                    ));
                }
                write_clipboard(text)?;
                // Update hash so we don't echo it back
                let hash = digest::digest(&digest::SHA256, text.as_bytes());
                self.last_hash = Some(hash.as_ref().to_vec());
                info!("Applied clipboard update from peer ({} bytes)", text.len());
            }
        }
        Ok(())
    }

    /// Suggested polling interval.
    pub fn poll_interval() -> Duration {
        Duration::from_secs(1)
    }
}

// ---------------------------------------------------------------------------
// Platform-specific clipboard access
//
// On macOS: use pbcopy/pbpaste (reliable from any process context)
// On Linux: try wl-paste/wl-copy first, fall back to xclip
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn read_text_from_command(
    mut command: std::process::Command,
    name: &str,
    max_bytes: usize,
) -> Result<String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| eyre!("{}: {}", name, e))?;

    let Some(mut stdout) = child.stdout.take() else {
        child.kill().ok();
        child.wait().ok();
        return Err(eyre!("{} stdout unavailable", name));
    };

    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                child.kill().ok();
                child.wait().ok();
                return Err(eyre!("{} read: {}", name, e));
            }
        };
        if n == 0 {
            break;
        }
        if bytes.len().saturating_add(n) > max_bytes {
            child.kill().ok();
            child.wait().ok();
            return Err(eyre!(
                "{} output too large: exceeds {} bytes",
                name,
                max_bytes
            ));
        }
        bytes.extend_from_slice(&buf[..n]);
    }

    let status = child.wait().map_err(|e| eyre!("{} wait: {}", name, e))?;
    if !status.success() {
        return Err(eyre!("{} exited with {}", name, status));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn write_text_to_command(mut command: std::process::Command, text: &str, name: &str) -> Result<()> {
    use std::io::{Read, Write};
    use std::process::Stdio;

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| eyre!("{}: {}", name, e))?;

    let write_result = match child.stdin.take() {
        Some(mut stdin) => stdin
            .write_all(text.as_bytes())
            .map_err(|e| eyre!("{} write: {}", name, e)),
        None => Err(eyre!("{} stdin unavailable", name)),
    };

    if let Err(err) = write_result {
        child.kill().ok();
        child.wait().ok();
        return Err(err);
    }

    let Some(mut stderr) = child.stderr.take() else {
        child.kill().ok();
        child.wait().ok();
        return Err(eyre!("{} stderr unavailable", name));
    };
    let mut stderr_bytes = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match stderr.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                child.kill().ok();
                child.wait().ok();
                return Err(eyre!("{} stderr read: {}", name, e));
            }
        };
        if n == 0 {
            break;
        }
        if stderr_bytes.len().saturating_add(n) > crate::status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES {
            child.kill().ok();
            child.wait().ok();
            return Err(eyre!(
                "{} stderr too large: exceeds {} bytes",
                name,
                crate::status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES
            ));
        }
        stderr_bytes.extend_from_slice(&buf[..n]);
    }

    let status = child.wait().map_err(|e| eyre!("{} wait: {}", name, e))?;
    if status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let stderr = crate::status::terminal_safe_multiline(
            &stderr,
            crate::status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES,
        );
        Err(eyre!("{} exited with {}: {}", name, status, stderr.trim()))
    }
}

#[cfg(target_os = "macos")]
fn read_clipboard() -> Result<String> {
    read_text_from_command(
        std::process::Command::new("pbpaste"),
        "pbpaste",
        MAX_CLIPBOARD_TEXT_BYTES,
    )
}

#[cfg(target_os = "macos")]
fn write_clipboard(text: &str) -> Result<()> {
    write_text_to_command(std::process::Command::new("pbcopy"), text, "pbcopy")
}

#[cfg(target_os = "linux")]
fn read_clipboard() -> Result<String> {
    use std::process::Command;

    // Try wl-paste first (Wayland), fall back to xclip (X11). Read stdout with
    // the same limit enforced on outgoing clipboard sync so a huge local
    // clipboard cannot be buffered unboundedly before the size check.
    let mut wl_paste = Command::new("wl-paste");
    wl_paste.args(["--no-newline"]);
    if let Ok(text) = read_text_from_command(wl_paste, "wl-paste", MAX_CLIPBOARD_TEXT_BYTES) {
        return Ok(text);
    }

    let mut xclip = Command::new("xclip");
    xclip.args(["-selection", "clipboard", "-o"]);
    read_text_from_command(xclip, "xclip", MAX_CLIPBOARD_TEXT_BYTES)
}

#[cfg(target_os = "linux")]
fn write_clipboard(text: &str) -> Result<()> {
    use std::process::Command;

    // Try wl-copy first (Wayland), fall back to xclip (X11). Both commands
    // consume stdin until EOF, so take/drop the pipe before waiting; otherwise
    // failures can hang the clipboard sync task or be reported as success.
    let wl_copy = Command::new("wl-copy");
    if write_text_to_command(wl_copy, text, "wl-copy").is_ok() {
        return Ok(());
    }

    let mut xclip = Command::new("xclip");
    xclip.args(["-selection", "clipboard"]);
    write_text_to_command(xclip, text, "xclip")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_size_limit_accepts_boundary() {
        let text = "a".repeat(MAX_CLIPBOARD_TEXT_BYTES);
        assert!(clipboard_text_size_ok(&text));
    }

    #[test]
    fn clipboard_size_limit_rejects_oversize() {
        let text = "a".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1);
        assert!(!clipboard_text_size_ok(&text));
    }

    #[cfg(unix)]
    #[test]
    fn read_text_from_command_enforces_output_limit() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf abcdef"]);
        assert_eq!(
            read_text_from_command(command, "test-reader", 6).unwrap(),
            "abcdef"
        );

        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf abcdef"]);
        let err = read_text_from_command(command, "test-reader", 5).unwrap_err();
        assert!(err.to_string().contains("test-reader output too large"));
    }

    #[cfg(unix)]
    #[test]
    fn write_text_to_command_reports_child_failure() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "cat >/dev/null; echo failed >&2; exit 7"]);
        let err = write_text_to_command(command, "hello", "test-clipboard").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("test-clipboard exited"));
        assert!(message.contains("failed"));
    }

    #[cfg(unix)]
    #[test]
    fn write_text_to_command_bounds_and_sanitizes_stderr() {
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "cat >/dev/null; printf '\\033]0;bad\\007failed' >&2; exit 7",
        ]);
        let err = write_text_to_command(command, "hello", "test-clipboard").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("test-clipboard exited"));
        assert!(message.contains("failed"));
        assert!(!message.contains('\u{1b}'));

        let mut command = std::process::Command::new("sh");
        command.args(["-c", "cat >/dev/null; yes x >&2"]);
        let err = write_text_to_command(command, "hello", "test-clipboard").unwrap_err();
        assert!(err.to_string().contains("stderr too large"));
    }
}
