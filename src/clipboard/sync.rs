use std::sync::Arc;
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use ring::digest;
use tracing::{debug, info, warn};

use crate::net::protocol::{ClipboardContent, Message, MAX_CLIPBOARD_TEXT_BYTES};
use crate::ports::Clipboard;

const CLIPBOARD_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

fn clipboard_text_size_ok(text: &str) -> bool {
    text.len() <= MAX_CLIPBOARD_TEXT_BYTES
}

/// Monitors the local clipboard for changes and produces protocol messages.
pub struct ClipboardSync {
    clipboard: Arc<dyn Clipboard>,
    last_hash: Option<Vec<u8>>,
}

impl ClipboardSync {
    pub fn new(clipboard: Arc<dyn Clipboard>) -> Self {
        info!("Clipboard sync initialized");
        Self {
            clipboard,
            last_hash: None,
        }
    }

    /// Check if the clipboard has changed. Returns a message if it has.
    pub fn poll_change(&mut self) -> Result<Option<Message>> {
        let text = match self.clipboard.read_text() {
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
                self.clipboard.write_text(text)?;
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_text_with_runner(
    runner: &dyn crate::ports::CommandRunner,
    program: &str,
    args: &[&str],
    max_bytes: usize,
) -> Result<String> {
    let mut request = crate::ports::CommandRequest::new(program).args(args.iter().copied());
    request.timeout = CLIPBOARD_COMMAND_TIMEOUT;
    request.max_stdout_bytes = max_bytes;
    let output = runner.run(&request)?;
    if output.stdout_truncated {
        return Err(eyre!(
            "{} output too large: exceeds {} bytes",
            program,
            max_bytes
        ));
    }
    if !output.success {
        return Err(eyre!("{} exited with {:?}", program, output.code));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "macos")]
fn write_text_with_runner(
    runner: &dyn crate::ports::CommandRunner,
    program: &str,
    args: &[&str],
    text: &str,
) -> Result<()> {
    write_text_with_runner_options(runner, program, args, text, false)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_text_with_runner_options(
    runner: &dyn crate::ports::CommandRunner,
    program: &str,
    args: &[&str],
    text: &str,
    discard_output: bool,
) -> Result<()> {
    let mut request = crate::ports::CommandRequest::new(program).args(args.iter().copied());
    request.stdin = text.as_bytes().to_vec();
    request.timeout = CLIPBOARD_COMMAND_TIMEOUT;
    request.discard_output = discard_output;
    let output = runner.run(&request)?;
    if output.success {
        Ok(())
    } else {
        Err(eyre!(
            "{} exited with {:?}: {}",
            program,
            output.code,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
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

#[cfg(test)]
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
pub(super) fn read_clipboard() -> Result<String> {
    read_text_with_runner(
        &crate::command::RealCommandRunner,
        "pbpaste",
        &[],
        MAX_CLIPBOARD_TEXT_BYTES,
    )
}

#[cfg(target_os = "macos")]
pub(super) fn write_clipboard(text: &str) -> Result<()> {
    write_text_with_runner(&crate::command::RealCommandRunner, "pbcopy", &[], text)
}

#[cfg(target_os = "linux")]
pub(super) fn read_clipboard() -> Result<String> {
    let runner = crate::command::RealCommandRunner;
    if let Ok(text) = read_text_with_runner(
        &runner,
        "wl-paste",
        &["--no-newline"],
        MAX_CLIPBOARD_TEXT_BYTES,
    ) {
        return Ok(text);
    }
    read_text_with_runner(
        &runner,
        "xclip",
        &["-selection", "clipboard", "-o"],
        MAX_CLIPBOARD_TEXT_BYTES,
    )
}

#[cfg(target_os = "linux")]
pub(super) fn write_clipboard(text: &str) -> Result<()> {
    let runner = crate::command::RealCommandRunner;
    // wl-copy and xclip may fork a long-lived clipboard owner. Capturing
    // output here would leave the runner waiting forever for EOF on descriptors
    // inherited by that owner after the short-lived parent exits.
    if write_text_with_runner_options(&runner, "wl-copy", &[], text, true).is_ok() {
        return Ok(());
    }
    write_text_with_runner_options(&runner, "xclip", &["-selection", "clipboard"], text, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_clipboard_supplies_local_changes() {
        let clipboard = Arc::new(crate::testing::MemoryClipboard::new());
        clipboard.set_text(Some("injected".to_string()));
        let mut sync = ClipboardSync::new(clipboard.clone());

        assert!(matches!(
            sync.poll_change().unwrap(),
            Some(Message::ClipboardUpdate {
                content: ClipboardContent::Text(text),
            }) if text == "injected"
        ));
        assert!(clipboard.observations().snapshot().iter().any(|entry| {
            matches!(entry.event, crate::testing::ClipboardObservation::ReadText)
        }));
    }

    #[test]
    fn peer_update_is_applied_without_echoing_it_back() {
        let clipboard = Arc::new(crate::testing::MemoryClipboard::new());
        let mut sync = ClipboardSync::new(clipboard.clone());
        let content = ClipboardContent::Text("from peer".to_string());

        sync.apply_update(&content).unwrap();

        assert!(sync.poll_change().unwrap().is_none());
        assert_eq!(
            clipboard.changes(),
            vec![crate::testing::ClipboardChange::Text(
                "from peer".to_string()
            )]
        );
    }

    #[test]
    fn oversized_local_and_peer_text_are_rejected() {
        let oversized = "a".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1);
        let clipboard = Arc::new(crate::testing::MemoryClipboard::new());
        clipboard.set_text(Some(oversized.clone()));
        let mut sync = ClipboardSync::new(clipboard.clone());

        assert!(sync.poll_change().unwrap().is_none());
        let error = sync
            .apply_update(&ClipboardContent::Text(oversized))
            .unwrap_err();

        assert!(error.to_string().contains("too large"));
        assert!(clipboard.changes().is_empty());
    }

    #[test]
    fn clipboard_read_and_write_failures_have_defined_behavior() {
        let clipboard = Arc::new(crate::testing::MemoryClipboard::new());
        clipboard.fail_next(
            crate::testing::ClipboardOperation::ReadText,
            "read unavailable",
        );
        let mut sync = ClipboardSync::new(clipboard.clone());

        // A temporarily unavailable local clipboard is not a session error.
        assert!(sync.poll_change().unwrap().is_none());

        clipboard.fail_next(
            crate::testing::ClipboardOperation::WriteText,
            "write unavailable",
        );
        let error = sync
            .apply_update(&ClipboardContent::Text("peer text".to_string()))
            .unwrap_err();
        assert!(error.to_string().contains("write unavailable"));
        assert!(clipboard.changes().is_empty());
    }

    #[test]
    fn blocked_clipboard_read_waits_for_release() {
        let clipboard = Arc::new(crate::testing::MemoryClipboard::new());
        clipboard.set_text(Some("eventual text".to_string()));
        let gate = clipboard.block_next(crate::testing::ClipboardOperation::ReadText);
        let worker_clipboard = clipboard.clone();
        let worker = std::thread::spawn(move || {
            let mut sync = ClipboardSync::new(worker_clipboard);
            sync.poll_change()
        });

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        assert!(!worker.is_finished());
        gate.release();
        assert!(matches!(
            worker.join().unwrap().unwrap(),
            Some(Message::ClipboardUpdate {
                content: ClipboardContent::Text(text),
            }) if text == "eventual text"
        ));
    }

    #[test]
    fn blocked_clipboard_write_waits_for_release() {
        let clipboard = Arc::new(crate::testing::MemoryClipboard::new());
        let gate = clipboard.block_next(crate::testing::ClipboardOperation::WriteText);
        let worker_clipboard = clipboard.clone();
        let worker = std::thread::spawn(move || {
            let mut sync = ClipboardSync::new(worker_clipboard);
            sync.apply_update(&ClipboardContent::Text("peer text".to_string()))
        });

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        assert!(!worker.is_finished());
        gate.release();
        worker.join().unwrap().unwrap();
        assert_eq!(
            clipboard.changes(),
            vec![crate::testing::ClipboardChange::Text(
                "peer text".to_string()
            )]
        );
    }

    #[test]
    fn blocked_clipboard_read_can_be_cancelled_for_shutdown() {
        let clipboard = Arc::new(crate::testing::MemoryClipboard::new());
        let gate = clipboard.block_next(crate::testing::ClipboardOperation::ReadText);
        let worker = std::thread::spawn(move || {
            let mut sync = ClipboardSync::new(clipboard);
            sync.poll_change()
        });

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        drop(gate);

        // ClipboardSync treats cancellation like any unavailable local read,
        // allowing its owner to join the worker during shutdown.
        assert!(worker.join().unwrap().unwrap().is_none());
    }

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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn platform_clipboard_commands_have_short_deadlines() {
        use crate::ports::CommandOutput;
        use crate::testing::{CommandObservation, ScriptedCommandRunner};

        let runner = ScriptedCommandRunner::new();
        for stdout in [b"clipboard".to_vec(), Vec::new()] {
            runner.push_output(CommandOutput {
                success: true,
                code: Some(0),
                signal: None,
                stdout,
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            });
        }

        assert_eq!(
            read_text_with_runner(&runner, "reader", &[], 1024).unwrap(),
            "clipboard"
        );
        write_text_with_runner_options(&runner, "writer", &[], "text", true).unwrap();

        let requests = runner
            .observations()
            .snapshot()
            .into_iter()
            .filter_map(|entry| match entry.event {
                CommandObservation::Run(request) => Some(request),
                CommandObservation::Failed(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 2);
        assert!(requests
            .iter()
            .all(|request| request.timeout == CLIPBOARD_COMMAND_TIMEOUT));
        assert!(!requests[0].discard_output);
        assert!(requests[1].discard_output);
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
