use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use ring::digest;
use tracing::{debug, warn};

use crate::net::protocol::{ClipboardContent, Message};

/// Monitors the local clipboard for changes and produces protocol messages.
pub struct ClipboardSync {
    last_hash: Option<Vec<u8>>,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self { last_hash: None }
    }

    /// Check if the clipboard has changed. Returns a message if it has.
    pub fn poll_change(&mut self) -> Result<Option<Message>> {
        let text = match read_clipboard() {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to read clipboard: {}", e);
                return Ok(None);
            }
        };

        let hash = digest::digest(&digest::SHA256, text.as_bytes());
        let hash_bytes = hash.as_ref().to_vec();

        if self.last_hash.as_ref() == Some(&hash_bytes) {
            return Ok(None);
        }

        self.last_hash = Some(hash_bytes);
        debug!("Clipboard changed ({} bytes)", text.len());

        Ok(Some(Message::ClipboardUpdate {
            content: ClipboardContent::Text(text),
        }))
    }

    /// Apply a clipboard update from a peer.
    pub fn apply_update(&mut self, content: &ClipboardContent) -> Result<()> {
        match content {
            ClipboardContent::Text(text) => {
                write_clipboard(text)?;
                // Update hash so we don't echo it back
                let hash = digest::digest(&digest::SHA256, text.as_bytes());
                self.last_hash = Some(hash.as_ref().to_vec());
                debug!("Applied clipboard update ({} bytes)", text.len());
            }
        }
        Ok(())
    }

    /// Suggested polling interval.
    pub fn poll_interval() -> Duration {
        Duration::from_millis(250)
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
    let output = std::process::Command::new("pbpaste")
        .output()
        .map_err(|e| eyre!("pbpaste: {}", e))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "macos")]
fn write_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| eyre!("pbcopy: {}", e))?;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(text.as_bytes())
        .map_err(|e| eyre!("pbcopy write: {}", e))?;
    child.wait().map_err(|e| eyre!("pbcopy wait: {}", e))?;
    Ok(())
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

    let mut child = std::process::Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| eyre!("xclip: {}", e))?;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(text.as_bytes())
        .map_err(|e| eyre!("xclip write: {}", e))?;
    child.wait().map_err(|e| eyre!("xclip wait: {}", e))?;
    Ok(())
}
