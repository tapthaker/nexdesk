use std::time::Duration;

use color_eyre::eyre::Result;
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
        let clipboard = arboard::Clipboard::new();
        let mut clipboard = match clipboard {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to access clipboard: {}", e);
                return Ok(None);
            }
        };

        let text = match clipboard.get_text() {
            Ok(t) => t,
            Err(_) => return Ok(None),
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
        let mut clipboard = arboard::Clipboard::new()?;
        match content {
            ClipboardContent::Text(text) => {
                clipboard.set_text(text)?;
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
