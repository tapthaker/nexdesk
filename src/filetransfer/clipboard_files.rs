use std::path::PathBuf;

use color_eyre::eyre::Result;
use tracing::{debug, warn};

/// Check if the clipboard contains file references and return their paths.
///
/// Returns `None` if the clipboard does not contain files.
pub fn get_clipboard_files() -> Option<Vec<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        get_clipboard_files_macos()
    }

    #[cfg(target_os = "linux")]
    {
        get_clipboard_files_linux()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Place file references on the clipboard so the user can paste them.
pub fn set_clipboard_files(paths: &[PathBuf]) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        set_clipboard_files_macos(paths)
    }

    #[cfg(target_os = "linux")]
    {
        set_clipboard_files_linux(paths)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = paths;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// macOS: use osascript with JXA to access NSPasteboard
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "macos", test))]
const MAX_MACOS_CLIPBOARD_SCRIPT_BYTES: usize = 4 * 1024 * 1024;
#[cfg(any(target_os = "macos", test))]
const MAX_MACOS_CLIPBOARD_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[cfg(any(target_os = "macos", test))]
fn read_child_stdout_limited(
    mut child: std::process::Child,
    name: &str,
    max_bytes: usize,
) -> Option<String> {
    use std::io::Read;

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
            warn!(
                "{} output is too large to inspect (max {} bytes)",
                name, max_bytes
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

#[cfg(target_os = "macos")]
fn run_osascript_limited(script: &str) -> Option<String> {
    use std::process::{Command, Stdio};

    let child = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    read_child_stdout_limited(
        child,
        "macOS clipboard file",
        MAX_MACOS_CLIPBOARD_OUTPUT_BYTES,
    )
}

#[cfg(target_os = "macos")]
fn get_clipboard_files_macos() -> Option<Vec<PathBuf>> {
    // JXA script that reads public.file-url items from the general pasteboard
    let script = r#"
ObjC.import('AppKit');
var pb = $.NSPasteboard.generalPasteboard;
var items = pb.pasteboardItems;
var paths = [];
for (var i = 0; i < items.count; i++) {
    var item = items.objectAtIndex(i);
    var urlStr = item.stringForType('public.file-url');
    if (urlStr && !urlStr.isNil()) {
        var url = $.NSURL.URLWithString(urlStr);
        if (url && !url.isNil()) {
            paths.push(ObjC.unwrap(url.path));
        }
    }
}
JSON.stringify(paths);
"#;

    let stdout = run_osascript_limited(script)?;
    let paths = parse_macos_clipboard_paths(&stdout)?;

    if paths.is_empty() {
        debug!("Clipboard file URLs found but no files exist at those paths");
        None
    } else {
        debug!("Clipboard contains {} file(s)", paths.len());
        Some(paths)
    }
}

#[cfg(any(target_os = "macos", test))]
fn run_stderr_limited(
    mut command: std::process::Command,
    name: &str,
    max_bytes: usize,
) -> Result<Option<String>> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let Some(mut stderr) = child.stderr.take() else {
        child.kill().ok();
        child.wait().ok();
        return Err(color_eyre::eyre::eyre!("{} stderr unavailable", name));
    };

    let mut stderr_bytes = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match stderr.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                child.kill().ok();
                child.wait().ok();
                return Err(color_eyre::eyre::eyre!("{} stderr read: {}", name, e));
            }
        };
        if n == 0 {
            break;
        }
        if stderr_bytes.len().saturating_add(n) > max_bytes {
            child.kill().ok();
            child.wait().ok();
            return Err(color_eyre::eyre::eyre!(
                "{} stderr too large: exceeds {} bytes",
                name,
                max_bytes
            ));
        }
        stderr_bytes.extend_from_slice(&buf[..n]);
    }

    let status = child.wait()?;
    if status.success() {
        Ok(None)
    } else {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        Ok(Some(crate::status::terminal_safe_multiline(
            &stderr, max_bytes,
        )))
    }
}

#[cfg(target_os = "macos")]
fn set_clipboard_files_macos(paths: &[PathBuf]) -> Result<()> {
    use std::process::Command;

    let script = build_macos_clipboard_script(paths)?;

    let mut command = Command::new("osascript");
    command.args(["-l", "JavaScript", "-e", &script]);
    if let Some(stderr) = run_stderr_limited(
        command,
        "macOS clipboard file writer",
        crate::status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES,
    )? {
        warn!("Failed to set clipboard files: {}", stderr.trim());
    } else {
        debug!("Set {} file(s) on clipboard", paths.len());
    }

    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_clipboard_paths(output: &str) -> Option<Vec<PathBuf>> {
    let decoded: Vec<String> = serde_json::from_str(output.trim()).ok()?;
    Some(
        decoded
            .into_iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect(),
    )
}

#[cfg(any(target_os = "macos", test))]
fn build_macos_clipboard_script(paths: &[PathBuf]) -> Result<String> {
    // Build a JXA script that writes NSURL objects to the pasteboard.
    // NSURL conforms to NSPasteboardWriting, so Finder's Paste command works.
    let mut script = String::from(
        "ObjC.import('AppKit');\n\
         var pb = $.NSPasteboard.generalPasteboard;\n\
         pb.clearContents;\n\
         var urls = $.NSMutableArray.alloc.init;\n",
    );

    for path in paths {
        let literal = js_string_literal(&path.display().to_string())?;
        let line = format!("urls.addObject($.NSURL.fileURLWithPath({}));\n", literal);
        if script.len().saturating_add(line.len()).saturating_add(25)
            > MAX_MACOS_CLIPBOARD_SCRIPT_BYTES
        {
            return Err(color_eyre::eyre::eyre!(
                "Clipboard file JXA script too large: exceeds {} bytes",
                MAX_MACOS_CLIPBOARD_SCRIPT_BYTES
            ));
        }
        script.push_str(&line);
    }

    script.push_str("pb.writeObjects(urls);\n'ok';\n");
    Ok(script)
}

#[cfg(any(target_os = "macos", test))]
fn js_string_literal(value: &str) -> Result<String> {
    serde_json::to_string(value).map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Linux: use xclip / wl-paste / wl-copy for text/uri-list
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", test))]
const MAX_URI_LIST_BYTES: usize = 4 * 1024 * 1024;

#[cfg(any(target_os = "linux", test))]
fn read_command_limited(
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
            warn!(
                "{} output is too large to inspect (max {} bytes)",
                name, max_bytes
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

#[cfg(target_os = "linux")]
fn read_uri_list_command(command: std::process::Command) -> Option<String> {
    read_command_limited(command, "Clipboard text/uri-list", MAX_URI_LIST_BYTES)
}

#[cfg(any(target_os = "linux", test))]
fn write_stdin_to_command(
    mut command: std::process::Command,
    stdin_bytes: &[u8],
    name: &str,
) -> Result<()> {
    use color_eyre::eyre::eyre;
    use std::io::{Read, Write};
    use std::process::Stdio;

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    let write_result = match child.stdin.take() {
        Some(mut stdin) => stdin
            .write_all(stdin_bytes)
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

    let status = child.wait()?;
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

#[cfg(target_os = "linux")]
fn get_clipboard_files_linux() -> Option<Vec<PathBuf>> {
    use std::process::Command;

    let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();

    let stdout = if is_wayland {
        let mut command = Command::new("wl-paste");
        command.args(["-t", "text/uri-list"]);
        read_uri_list_command(command)?
    } else {
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard", "-t", "text/uri-list", "-o"]);
        read_uri_list_command(command)?
    };

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    let paths: Vec<PathBuf> = trimmed
        .lines()
        .filter(|line| !line.starts_with('#')) // uri-list comments
        .filter_map(|line| {
            let line = line.trim();
            if let Some(path) = line.strip_prefix("file://") {
                parse_file_uri_path(path)
            } else {
                None
            }
        })
        .filter(|p| p.exists())
        .collect();

    if paths.is_empty() {
        None
    } else {
        debug!("Clipboard contains {} file(s)", paths.len());
        Some(paths)
    }
}

#[cfg(target_os = "linux")]
fn set_clipboard_files_linux(paths: &[PathBuf]) -> Result<()> {
    use std::process::Command;

    let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
    let uri_list = build_uri_list(paths)?;

    if is_wayland {
        let mut wl_copy = Command::new("wl-copy");
        wl_copy.args(["-t", "text/uri-list"]);
        if write_stdin_to_command(wl_copy, uri_list.as_bytes(), "wl-copy").is_ok() {
            debug!("Set {} file(s) on clipboard", paths.len());
            return Ok(());
        }
    }

    let mut xclip = Command::new("xclip");
    xclip.args(["-selection", "clipboard", "-t", "text/uri-list", "-i"]);
    write_stdin_to_command(xclip, uri_list.as_bytes(), "xclip")?;

    debug!("Set {} file(s) on clipboard", paths.len());
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn build_uri_list(paths: &[PathBuf]) -> Result<String> {
    let mut uri_list = String::new();
    for path in paths {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let uri = path_to_file_uri(&abs);
        if uri_list.len().saturating_add(uri.len()).saturating_add(2) > MAX_URI_LIST_BYTES {
            return Err(color_eyre::eyre::eyre!(
                "Clipboard file URI list too large: exceeds {} bytes",
                MAX_URI_LIST_BYTES
            ));
        }
        uri_list.push_str(&uri);
        uri_list.push_str("\r\n");
    }
    Ok(uri_list)
}

#[cfg(any(target_os = "linux", test))]
fn parse_file_uri_path(rest: &str) -> Option<PathBuf> {
    let path_part = if rest.starts_with('/') {
        rest.to_string()
    } else {
        let (authority, path) = rest.split_once('/')?;
        if !authority.eq_ignore_ascii_case("localhost") {
            return None;
        }
        format!("/{path}")
    };

    let decoded = percent_decode(&path_part)?;
    let path = PathBuf::from(decoded);
    if path.is_absolute() {
        Some(path)
    } else {
        None
    }
}

#[cfg(any(target_os = "linux", test))]
fn path_to_file_uri(path: &std::path::Path) -> String {
    format!("file://{}", percent_encode_path(path))
}

#[cfg(any(target_os = "linux", test))]
fn percent_encode_path(path: &std::path::Path) -> String {
    let path = path.to_string_lossy();
    let mut encoded = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(any(target_os = "linux", test))]
fn percent_decode(s: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().and_then(hex_val)?;
            let lo = chars.next().and_then(hex_val)?;
            bytes.push(hi << 4 | lo);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(any(target_os = "linux", test))]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_file_uri_paths() {
        assert_eq!(
            parse_file_uri_path("/tmp/a%20b.txt").unwrap(),
            PathBuf::from("/tmp/a b.txt")
        );
        assert_eq!(
            parse_file_uri_path("localhost/tmp/a.txt").unwrap(),
            PathBuf::from("/tmp/a.txt")
        );
    }

    #[test]
    fn rejects_remote_or_invalid_file_uri_paths() {
        assert!(parse_file_uri_path("other-host/tmp/a.txt").is_none());
        assert!(parse_file_uri_path("relative/path.txt").is_none());
        assert!(parse_file_uri_path("/tmp/%ZZ").is_none());
    }

    #[test]
    fn encodes_file_uri_paths() {
        assert_eq!(
            path_to_file_uri(std::path::Path::new("/tmp/a b#.txt")),
            "file:///tmp/a%20b%23.txt"
        );
    }

    #[test]
    fn build_uri_list_enforces_size_limit() {
        let long_name = "a".repeat(MAX_URI_LIST_BYTES);
        let path = PathBuf::from(format!("/tmp/{long_name}"));
        assert!(build_uri_list(&[path]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_command_limited_enforces_output_limit() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf abcdef"]);
        assert_eq!(
            read_command_limited(command, "test-reader", 6).unwrap(),
            "abcdef"
        );

        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf abcdef"]);
        assert!(read_command_limited(command, "test-reader", 5).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn write_stdin_to_command_reports_child_failure() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "cat >/dev/null; echo failed >&2; exit 7"]);
        let err = write_stdin_to_command(command, b"file:///tmp/a\r\n", "test-writer").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("test-writer exited"));
        assert!(message.contains("failed"));
    }

    #[cfg(unix)]
    #[test]
    fn write_stdin_to_command_bounds_and_sanitizes_stderr() {
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "cat >/dev/null; printf '\\033]0;bad\\007failed' >&2; exit 7",
        ]);
        let err = write_stdin_to_command(command, b"file:///tmp/a\r\n", "test-writer").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("test-writer exited"));
        assert!(message.contains("failed"));
        assert!(!message.contains('\u{1b}'));

        let mut command = std::process::Command::new("sh");
        command.args(["-c", "cat >/dev/null; yes x >&2"]);
        let err = write_stdin_to_command(command, b"file:///tmp/a\r\n", "test-writer").unwrap_err();
        assert!(err.to_string().contains("stderr too large"));
    }

    #[test]
    fn js_string_literal_escapes_script_delimiters_and_control_chars() {
        assert_eq!(
            js_string_literal("/tmp/a'b\\c\nfile.txt").unwrap(),
            r#""/tmp/a'b\\c\nfile.txt""#
        );
    }

    #[test]
    fn parses_macos_clipboard_paths_as_json_not_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a\nb.txt");
        std::fs::write(&path, b"x").unwrap();
        let json = serde_json::to_string(&vec![path.to_string_lossy().to_string()]).unwrap();
        assert_eq!(parse_macos_clipboard_paths(&json).unwrap(), vec![path]);
    }

    #[cfg(unix)]
    #[test]
    fn run_stderr_limited_bounds_and_sanitizes_output() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf '\\033]0;bad\\007failed' >&2; exit 7"]);
        let stderr = run_stderr_limited(command, "test-command", 1024)
            .unwrap()
            .unwrap();
        assert!(stderr.contains("failed"));
        assert!(!stderr.contains('\u{1b}'));

        let mut command = std::process::Command::new("sh");
        command.args(["-c", "yes x >&2"]);
        let err = run_stderr_limited(command, "test-command", 1024).unwrap_err();
        assert!(err.to_string().contains("stderr too large"));
    }

    #[test]
    fn build_macos_clipboard_script_enforces_size_limit() {
        let long_name = "a".repeat(MAX_MACOS_CLIPBOARD_SCRIPT_BYTES);
        let path = PathBuf::from(format!("/tmp/{long_name}"));
        assert!(build_macos_clipboard_script(&[path]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_child_stdout_limited_enforces_output_limit() {
        let child = std::process::Command::new("sh")
            .args(["-c", "printf abcdef"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(
            read_child_stdout_limited(child, "test-reader", 6).unwrap(),
            "abcdef"
        );

        let child = std::process::Command::new("sh")
            .args(["-c", "printf abcdef"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        assert!(read_child_stdout_limited(child, "test-reader", 5).is_none());
    }

    #[test]
    fn macos_clipboard_output_limit_matches_script_limit() {
        assert_eq!(
            MAX_MACOS_CLIPBOARD_OUTPUT_BYTES,
            MAX_MACOS_CLIPBOARD_SCRIPT_BYTES
        );
    }
}
