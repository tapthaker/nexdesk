use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::config::PersistenceRoots;
use crate::ports::{AtomicFileStore, RealAtomicFileStore, StatusSink};

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
    let roots = PersistenceRoots::production()?;
    roots.ensure_config_root()?;
    Ok(roots.status_path())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FileStatusSink;

impl StatusSink for FileStatusSink {
    fn write(&self, status: RuntimeStatus) -> Result<()> {
        write_status(status)
    }
}

pub fn write_status(status: RuntimeStatus) -> Result<()> {
    write_status_at(&status_path()?, status)
}

pub fn write_status_at(path: &std::path::Path, status: RuntimeStatus) -> Result<()> {
    write_status_with_store(path, status, &RealAtomicFileStore)
}

pub fn write_status_with_store(
    path: &std::path::Path,
    mut status: RuntimeStatus,
    store: &dyn AtomicFileStore,
) -> Result<()> {
    status.updated_at = now_secs();
    let bytes = serde_json::to_vec_pretty(&status).wrap_err("Failed to encode runtime status")?;
    store
        .replace(path, &bytes)
        .wrap_err_with(|| format!("Failed to write runtime status: {}", path.display()))
}

pub fn load_status() -> Result<Option<RuntimeStatus>> {
    load_status_at(&status_path()?)
}

pub fn load_status_at(path: &std::path::Path) -> Result<Option<RuntimeStatus>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_status_can_use_an_explicit_temporary_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join("status.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        write_status_at(&path, RuntimeStatus::new("server", "ready")).unwrap();
        let loaded = load_status_at(&path).unwrap().unwrap();

        assert_eq!(loaded.role, "server");
        assert_eq!(loaded.state, "ready");
    }
}
