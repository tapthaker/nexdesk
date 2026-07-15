use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::config::NexdeskConfig;

const MAX_RUNTIME_STATUS_BYTES: usize = 64 * 1024;
pub const MAX_STATUS_DISPLAY_BYTES: usize = 1024;
pub const MAX_COMMAND_OUTPUT_DISPLAY_BYTES: usize = 64 * 1024;

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

#[cfg(unix)]
fn restrict_status_file_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).wrap_err_with(|| {
        format!(
            "Failed to restrict runtime status permissions: {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn restrict_status_file_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn read_bounded_to_string(path: &std::path::Path, max_bytes: usize) -> Result<String> {
    let file = std::fs::File::open(path)
        .wrap_err_with(|| format!("Failed to open runtime status: {}", path.display()))?;
    let mut limited = file.take(max_bytes as u64 + 1);
    let mut contents = String::new();
    limited
        .read_to_string(&mut contents)
        .wrap_err_with(|| format!("Failed to read runtime status: {}", path.display()))?;
    if contents.len() > max_bytes {
        return Err(color_eyre::eyre::eyre!(
            "Runtime status file too large: {} bytes (max {})",
            contents.len(),
            max_bytes
        ));
    }
    Ok(contents)
}

pub fn write_status(mut status: RuntimeStatus) -> Result<()> {
    status.updated_at = now_secs();
    let path = status_path()?;
    let bytes = serde_json::to_vec_pretty(&status).wrap_err("Failed to encode runtime status")?;
    if bytes.len() > MAX_RUNTIME_STATUS_BYTES {
        return Err(color_eyre::eyre::eyre!(
            "Runtime status payload too large: {} bytes (max {})",
            bytes.len(),
            MAX_RUNTIME_STATUS_BYTES
        ));
    }
    let dir = NexdeskConfig::config_dir()?;
    let mut tmp_file = tempfile::Builder::new()
        .prefix("runtime-status.json.")
        .tempfile_in(&dir)
        .wrap_err_with(|| {
            format!(
                "Failed to create temporary runtime status in {}",
                dir.display()
            )
        })?;
    restrict_status_file_permissions(tmp_file.path())?;
    tmp_file.write_all(&bytes).wrap_err_with(|| {
        format!(
            "Failed to write temporary runtime status: {}",
            tmp_file.path().display()
        )
    })?;
    tmp_file
        .as_file_mut()
        .sync_all()
        .wrap_err("Failed to sync temporary runtime status")?;
    tmp_file
        .persist(&path)
        .map_err(|e| e.error)
        .wrap_err_with(|| format!("Failed to replace runtime status: {}", path.display()))?;
    restrict_status_file_permissions(&path)?;
    sync_directory(&dir).wrap_err_with(|| {
        format!(
            "Failed to sync runtime status directory after write: {}",
            dir.display()
        )
    })?;
    Ok(())
}

pub fn load_status() -> Result<Option<RuntimeStatus>> {
    let path = status_path()?;
    if !path.exists() {
        return Ok(None);
    }
    restrict_status_file_permissions(&path)?;
    let contents = read_bounded_to_string(&path, MAX_RUNTIME_STATUS_BYTES)?;
    let status = serde_json::from_str(&contents).wrap_err("Failed to parse runtime status")?;
    Ok(Some(status))
}

pub fn load_live_status() -> Result<Option<RuntimeStatus>> {
    let Some(status) = load_status()? else {
        return Ok(None);
    };
    if is_process_running_for_status(&status) {
        Ok(Some(status))
    } else {
        Ok(None)
    }
}

#[cfg(unix)]
fn is_process_running_for_status(status: &RuntimeStatus) -> bool {
    is_process_running(status.pid) && process_command_matches_role(status.pid, &status.role)
}

#[cfg(not(unix))]
fn is_process_running_for_status(_status: &RuntimeStatus) -> bool {
    true
}

#[cfg(any(unix, test))]
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

#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    let alive = result == 0
        || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied;
    if !alive {
        return false;
    }

    // Avoid reporting stale runtime-status.json for an unrelated process that
    // reused the old daemon PID after a crash/reboot. `comm` is just the
    // executable name on both Linux procps and macOS/BSD ps.
    let mut command = std::process::Command::new("ps");
    command.args(["-p", &pid.to_string(), "-o", "comm="]);
    match command_stdout_limited(command, "ps comm", MAX_COMMAND_OUTPUT_DISPLAY_BYTES) {
        Some(comm) => {
            let Some(name) = std::path::Path::new(comm.trim()).file_name() else {
                return false;
            };
            is_nexdesk_process_name(&name.to_string_lossy())
        }
        None => true,
    }
}

#[cfg(unix)]
fn status_role_subcommand(role: &str) -> Option<&'static str> {
    match role {
        "server" => Some("serve"),
        "client" => Some("connect"),
        _ => None,
    }
}

#[cfg(unix)]
fn process_command_matches_role(pid: u32, role: &str) -> bool {
    let Some(expected_subcommand) = status_role_subcommand(role) else {
        return false;
    };

    let mut command = std::process::Command::new("ps");
    command.args(["-p", &pid.to_string(), "-o", "command="]);
    match command_stdout_limited(command, "ps command", MAX_COMMAND_OUTPUT_DISPLAY_BYTES) {
        Some(command) => command_line_matches_role(&command, expected_subcommand),
        None => true,
    }
}

#[cfg(unix)]
fn command_line_matches_role(command: &str, expected_subcommand: &str) -> bool {
    let args: Vec<&str> = command.split_whitespace().collect();
    let Some(exe_index) = args.iter().position(|arg| {
        std::path::Path::new(arg)
            .file_name()
            .map(|name| is_nexdesk_process_name(&name.to_string_lossy()))
            .unwrap_or(false)
    }) else {
        return false;
    };

    first_role_subcommand(&args[exe_index + 1..]) == Some(expected_subcommand)
}

#[cfg(unix)]
fn first_role_subcommand<'a>(args: &'a [&str]) -> Option<&'a str> {
    for arg in args.iter().copied() {
        match arg {
            "-v" | "--verbose" => continue,
            _ if arg.starts_with("--verbose=") => continue,
            _ if arg.starts_with('-') => continue,
            other => return Some(other),
        }
    }
    None
}

fn is_nexdesk_process_name(name: &str) -> bool {
    matches!(name, "nexdesk" | "nexdesk.exe")
}

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
        let len = ch.len_utf8();
        if sanitized.len().saturating_add(len) > max_bytes {
            break;
        }
        sanitized.push(ch);
    }
    sanitized
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
    fn bounded_status_reader_rejects_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-status.json");
        std::fs::write(&path, vec![b'a'; 17]).unwrap();
        assert_eq!(read_bounded_to_string(&path, 17).unwrap().len(), 17);
        assert!(read_bounded_to_string(&path, 16).is_err());
    }

    #[test]
    fn write_status_rejects_oversized_payloads_before_persisting() {
        let mut status = RuntimeStatus::new("server", "listening");
        status.peer_name = Some("x".repeat(MAX_RUNTIME_STATUS_BYTES));
        let err = write_status(status).unwrap_err();
        assert!(err.to_string().contains("Runtime status payload too large"));
    }

    #[test]
    fn terminal_safe_status_text_is_bounded_and_control_free() {
        assert_eq!(terminal_safe("peer\x1b[31m", 64), "peer�[31m");
        assert_eq!(terminal_safe("abcdef", 3), "abc");
    }

    #[test]
    fn terminal_safe_multiline_preserves_log_format_without_escape_controls() {
        assert_eq!(
            terminal_safe_multiline("ok\npeer\x1b[31m\tend", 64),
            "ok\npeer�[31m\tend"
        );
        assert_eq!(terminal_safe_multiline("abcdef", 3), "abc");
    }

    #[cfg(unix)]
    #[test]
    fn status_file_permissions_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-status.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        restrict_status_file_permissions(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn process_name_match_is_exact() {
        assert!(is_nexdesk_process_name("nexdesk"));
        assert!(is_nexdesk_process_name("nexdesk.exe"));
        assert!(!is_nexdesk_process_name("nexdesk-helper"));
        assert!(!is_nexdesk_process_name("not-nexdesk"));
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

    #[cfg(unix)]
    #[test]
    fn status_roles_are_strictly_limited_to_runtime_roles() {
        assert_eq!(status_role_subcommand("server"), Some("serve"));
        assert_eq!(status_role_subcommand("client"), Some("connect"));
        assert_eq!(status_role_subcommand("setup"), None);
        assert_eq!(status_role_subcommand(""), None);
    }

    #[cfg(unix)]
    #[test]
    fn command_line_role_match_is_exact() {
        assert!(command_line_matches_role(
            "/Applications/Nexdesk/nexdesk serve --port 4242",
            "serve"
        ));
        assert!(command_line_matches_role(
            "/usr/local/bin/nexdesk connect 127.0.0.1:4242",
            "connect"
        ));
        assert!(!command_line_matches_role(
            "/usr/local/bin/nexdesk daemon status",
            "serve"
        ));
        assert!(!command_line_matches_role(
            "/usr/local/bin/nexdesk-helper serve",
            "connect"
        ));
        assert!(!command_line_matches_role(
            "/usr/local/bin/nexdesk daemon status serve",
            "serve"
        ));
        assert!(command_line_matches_role(
            "/usr/local/bin/nexdesk --verbose serve --port 4242",
            "serve"
        ));
        assert!(!command_line_matches_role(
            "/usr/local/bin/not-nexdesk serve",
            "serve"
        ));
    }
}
