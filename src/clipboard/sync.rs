use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use ring::digest;
use tracing::{debug, info};

use crate::net::protocol::{ClipboardContent, Message, MAX_CLIPBOARD_TEXT_BYTES};
use crate::ports::Clipboard;

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
        let text = match super::PlatformClipboard.read_text() {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(None),
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
                super::PlatformClipboard.write_text(text)?;
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

#[cfg(target_os = "macos")]
fn read_clipboard() -> Result<String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| eyre!("clipboard init: {}", e))?;
    clipboard
        .get_text()
        .map_err(|e| eyre!("clipboard read: {}", e))
}

#[cfg(target_os = "macos")]
fn write_clipboard(text: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| eyre!("clipboard init: {}", e))?;
    clipboard
        .set_text(text.to_owned())
        .map_err(|e| eyre!("clipboard write: {}", e))
}

#[cfg(target_os = "linux")]
fn read_clipboard() -> Result<String> {
    // Try wl-paste first (Wayland), fall back to xclip (X11)
    if let Ok(output) = std::process::Command::new("wl-paste")
        .args(["--no-newline"])
        .output()
    {
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }

    let output = std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-o"])
        .output()
        .map_err(|e| eyre!("xclip: {}", e))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(eyre!(
            "xclip failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(target_os = "linux")]
fn write_clipboard(text: &str) -> Result<()> {
    use std::io::Write;

    // Try wl-copy first (Wayland), fall back to xclip (X11)
    if let Ok(mut child) = std::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() {
            if stdin.write_all(text.as_bytes()).is_ok() {
                if let Ok(status) = child.wait() {
                    if status.success() {
                        return Ok(());
                    }
                }
            }
        }
    }

    let status = child.wait().map_err(|e| eyre!("{} wait: {}", name, e))?;
    if status.success() {
        Ok(())
    } else {
        Err(eyre!("{} exited with {}", name, status))
    }
}

#[cfg(target_os = "macos")]
pub(super) fn read_clipboard() -> Result<String> {
    read_text_from_command(
        std::process::Command::new("pbpaste"),
        "pbpaste",
        MAX_CLIPBOARD_TEXT_BYTES,
    )
}

#[cfg(target_os = "macos")]
pub(super) fn write_clipboard(text: &str) -> Result<()> {
    write_text_to_command(std::process::Command::new("pbcopy"), text, "pbcopy")
}

#[cfg(target_os = "linux")]
pub(super) fn read_clipboard() -> Result<String> {
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
pub(super) fn write_clipboard(text: &str) -> Result<()> {
    use std::process::Command;

    // Try wl-copy first (Wayland), fall back to xclip (X11). Both may fork a
    // long-lived clipboard owner, so they must not inherit a stderr pipe that
    // this process waits to reach EOF.
    let wl_copy = Command::new("wl-copy");
    if write_text_to_daemonizing_command(wl_copy, text, "wl-copy").is_ok() {
        return Ok(());
    }

    let mut xclip = Command::new("xclip");
    xclip.args(["-selection", "clipboard"]);
    write_text_to_daemonizing_command(xclip, text, "xclip")
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
