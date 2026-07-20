use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::config::NexdeskConfig;
use crate::ports::StatusSink;

pub const MAX_STATUS_DISPLAY_BYTES: usize = 1024;
pub const MAX_COMMAND_OUTPUT_DISPLAY_BYTES: usize = 64 * 1024;

pub fn terminal_safe(value: &str, max_bytes: usize) -> String {
    terminal_safe_with(value, max_bytes, |_| false)
}

pub fn terminal_safe_multiline(value: &str, max_bytes: usize) -> String {
    terminal_safe_with(value, max_bytes, |ch| matches!(ch, '\n' | '\t'))
}

fn terminal_safe_with(
    value: &str,
    max_bytes: usize,
    preserve_control: impl Fn(char) -> bool,
) -> String {
    let mut sanitized = String::new();
    for ch in value.chars() {
        let ch = if ch.is_control() && !preserve_control(ch) {
            '�'
        } else {
            ch
        };
        if sanitized.len().saturating_add(ch.len_utf8()) > max_bytes {
            break;
        }
        sanitized.push(ch);
    }
    sanitized
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub role: String,
    pub state: String,
    pub pid: u32,
    pub updated_at: u64,
    pub local_addr: Option<String>,
    pub peer_addr: Option<String>,
    pub peer_name: Option<String>,
    pub peer_screen: Option<String>,
    pub peer_build: Option<String>,
}

impl RuntimeStatus {
    pub fn new(role: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            state: state.into(),
            pid: std::process::id(),
            updated_at: now_secs(),
            local_addr: None,
            peer_addr: None,
            peer_name: None,
            peer_screen: None,
            peer_build: None,
        }
    }
}

pub fn status_path() -> Result<PathBuf> {
    Ok(NexdeskConfig::config_dir()?.join("runtime-status.json"))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FileStatusSink;

impl StatusSink for FileStatusSink {
    fn write(&self, status: RuntimeStatus) -> Result<()> {
        write_status(status)
    }
}

pub fn write_status(mut status: RuntimeStatus) -> Result<()> {
    status.updated_at = now_secs();
    let path = status_path()?;
    let bytes = serde_json::to_vec_pretty(&status).wrap_err("Failed to encode runtime status")?;
    std::fs::write(&path, bytes)
        .wrap_err_with(|| format!("Failed to write runtime status: {}", path.display()))?;
    Ok(())
}

pub fn load_status() -> Result<Option<RuntimeStatus>> {
    let path = status_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&path)
        .wrap_err_with(|| format!("Failed to read runtime status: {}", path.display()))?;
    let status = serde_json::from_str(&contents).wrap_err("Failed to parse runtime status")?;
    Ok(Some(status))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
