use std::path::PathBuf;

use color_eyre::eyre::Result;
use tracing::debug;
#[cfg(target_os = "macos")]
use tracing::warn;

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

#[cfg(target_os = "macos")]
fn get_clipboard_files_macos() -> Option<Vec<PathBuf>> {
    use std::process::Command;

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
paths.join('\n');
"#;

    let output = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", script])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    let paths: Vec<PathBuf> = trimmed
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();

    if paths.is_empty() {
        debug!("Clipboard file URLs found but no files exist at those paths");
        None
    } else {
        debug!("Clipboard contains {} file(s)", paths.len());
        Some(paths)
    }
}

#[cfg(target_os = "macos")]
fn set_clipboard_files_macos(paths: &[PathBuf]) -> Result<()> {
    use std::process::Command;

    // Build a JXA script that writes NSURL objects to the pasteboard.
    // NSURL conforms to NSPasteboardWriting, so Finder's Paste command works.
    let mut script = String::from(
        "ObjC.import('AppKit');\n\
         var pb = $.NSPasteboard.generalPasteboard;\n\
         pb.clearContents;\n\
         var urls = $.NSMutableArray.alloc.init;\n",
    );

    for path in paths {
        let escaped = path
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('\'', "\\'");
        script.push_str(&format!(
            "urls.addObject($.NSURL.fileURLWithPath('{}'));\n",
            escaped
        ));
    }

    script.push_str("pb.writeObjects(urls);\n'ok';\n");

    let output = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("Failed to set clipboard files: {}", stderr);
    } else {
        debug!("Set {} file(s) on clipboard", paths.len());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Linux: use xclip / wl-paste / wl-copy for text/uri-list
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn get_clipboard_files_linux() -> Option<Vec<PathBuf>> {
    use std::process::Command;

    let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();

    let output = if is_wayland {
        Command::new("wl-paste")
            .args(["-t", "text/uri-list"])
            .output()
            .ok()?
    } else {
        Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "text/uri-list", "-o"])
            .output()
            .ok()?
    };

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
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
                // URL-decode the path
                let decoded = urldecode(path);
                Some(PathBuf::from(decoded))
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
    use std::io::Write;
    use std::process::{Command, Stdio};

    let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();

    // Build URI list
    let mut uri_list = String::new();
    for path in paths {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        uri_list.push_str(&format!("file://{}\r\n", abs.display()));
    }

    if is_wayland {
        let mut child = Command::new("wl-copy")
            .args(["-t", "text/uri-list"])
            .stdin(Stdio::piped())
            .spawn()?;
        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(uri_list.as_bytes())?;
        }
        child.wait()?;
    } else {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "text/uri-list", "-i"])
            .stdin(Stdio::piped())
            .spawn()?;
        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(uri_list.as_bytes())?;
        }
        child.wait()?;
    }

    debug!("Set {} file(s) on clipboard", paths.len());
    Ok(())
}

/// Simple percent-decoding for file:// URIs.
#[cfg(target_os = "linux")]
fn urldecode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().and_then(hex_val);
            let lo = chars.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                result.push((h << 4 | l) as char);
            } else {
                result.push('%');
            }
        } else {
            result.push(b as char);
        }
    }
    result
}

#[cfg(target_os = "linux")]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
