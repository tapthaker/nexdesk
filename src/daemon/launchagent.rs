use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{eyre, Result, WrapErr};
use tracing::info;

use crate::status;

const LABEL: &str = "com.nexdesk.agent";
const MAX_LAUNCHAGENT_ARG_BYTES: usize = 4096;
const MAX_LAUNCHAGENT_PLIST_BYTES: usize = 64 * 1024;

#[cfg(unix)]
fn restrict_log_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).wrap_err_with(|| {
        format!(
            "Failed to restrict LaunchAgent log directory permissions: {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn restrict_log_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_plist_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).wrap_err_with(|| {
        format!(
            "Failed to restrict LaunchAgent plist permissions: {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn restrict_plist_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn plist_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/Shared".into());
    PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", LABEL))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn log_paths() -> Result<(PathBuf, PathBuf)> {
    let dir = crate::config::NexdeskConfig::config_dir()?.join("logs");
    std::fs::create_dir_all(&dir).wrap_err("Failed to create LaunchAgent log directory")?;
    restrict_log_dir_permissions(&dir)?;
    Ok((dir.join("nexdesk.out.log"), dir.join("nexdesk.err.log")))
}

fn plist_arg_entries(args: &[&str]) -> Result<String> {
    args.iter()
        .map(|arg| {
            if arg.len() > MAX_LAUNCHAGENT_ARG_BYTES {
                return Err(eyre!(
                    "LaunchAgent argument too large: {} bytes (max {})",
                    arg.len(),
                    MAX_LAUNCHAGENT_ARG_BYTES
                ));
            }
            Ok(format!("        <string>{}</string>", xml_escape(arg)))
        })
        .collect::<Result<Vec<_>>>()
        .map(|entries| entries.join("\n"))
}

fn plist_content(args: &[&str]) -> Result<String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nexdesk"));
    let exe = xml_escape(&exe.display().to_string());
    let (stdout_log, stderr_log) = log_paths()?;
    let stdout_log = xml_escape(&stdout_log.display().to_string());
    let stderr_log = xml_escape(&stderr_log.display().to_string());

    let arg_entries = plist_arg_entries(args)?;

    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
{arg_entries}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{stdout_log}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_log}</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = exe,
        arg_entries = arg_entries,
        stdout_log = stdout_log,
        stderr_log = stderr_log,
    );
    if contents.len() > MAX_LAUNCHAGENT_PLIST_BYTES {
        return Err(eyre!(
            "LaunchAgent plist too large: {} bytes (max {})",
            contents.len(),
            MAX_LAUNCHAGENT_PLIST_BYTES
        ));
    }
    Ok(contents)
}

pub fn print_status() -> Result<()> {
    let uid = current_uid();
    let service = format!("gui/{uid}/{LABEL}");
    let loaded = command_stdout("launchctl", &["print", &service]).is_ok();
    let process = nexdesk_process_summary();
    let port = status_port();
    let listener_cmd = format!("lsof -nP -iUDP:{port} 2>/dev/null || true");
    let listener = command_stdout("sh", &["-c", &listener_cmd]).unwrap_or_default();

    println!("nexdesk status");
    println!("Service : {}", if loaded { "loaded" } else { "not loaded" });
    println!(
        "Process : {}",
        status::terminal_safe(
            process.as_deref().unwrap_or("not running"),
            status::MAX_STATUS_DISPLAY_BYTES
        )
    );
    println!(
        "Listener: {}",
        if listener.trim().is_empty() {
            "not listening on configured UDP port"
        } else {
            "listening on configured UDP port"
        }
    );
    print_connection_summary();
    let (stdout_log, stderr_log) = log_paths()?;
    println!(
        "Logs    : {}, {}",
        stdout_log.display(),
        stderr_log.display()
    );
    println!("\nFor logs: nexdesk log");
    Ok(())
}

pub fn print_log() -> Result<()> {
    let uid = current_uid();
    let service = format!("gui/{uid}/{LABEL}");

    println!("nexdesk service log (LaunchAgent)\n");
    print_command("launchctl", &["print", &service])?;
    print_command("pgrep", &pgrep_nexdesk_args())?;
    let port = status_port();
    let listener_cmd = format!("lsof -nP -iUDP:{port} 2>/dev/null || true");
    print_command("sh", &["-c", &listener_cmd])?;
    let (stdout_log, stderr_log) = log_paths()?;
    print_command(
        "tail",
        &[
            "-n",
            "40",
            stdout_log.to_string_lossy().as_ref(),
            stderr_log.to_string_lossy().as_ref(),
        ],
    )?;
    Ok(())
}

pub fn install(args: &[&str]) -> Result<()> {
    let path = plist_path();
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("Invalid LaunchAgent path: {}", path.display()))?;
    std::fs::create_dir_all(parent).wrap_err("Failed to create LaunchAgents directory")?;

    let contents = plist_content(args)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!("{}.plist.", LABEL))
        .tempfile_in(parent)
        .wrap_err_with(|| {
            format!(
                "Failed to create temporary LaunchAgent plist in {}",
                parent.display()
            )
        })?;
    restrict_plist_file_permissions(tmp.path())?;
    tmp.write_all(contents.as_bytes())
        .wrap_err("Failed to write temporary LaunchAgent plist")?;
    tmp.as_file_mut()
        .sync_all()
        .wrap_err("Failed to sync temporary LaunchAgent plist")?;
    tmp.persist(&path)
        .map_err(|e| e.error)
        .wrap_err_with(|| format!("Failed to replace LaunchAgent plist: {}", path.display()))?;
    restrict_plist_file_permissions(&path)?;
    sync_directory(parent).wrap_err_with(|| {
        format!(
            "Failed to sync LaunchAgent directory after install: {}",
            parent.display()
        )
    })?;

    info!("Installed LaunchAgent at {}", path.display());

    let uid = current_uid();
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{LABEL}");

    let _ = run_launchctl(&["bootout", &domain, path.to_string_lossy().as_ref()]);
    run_launchctl(&["bootstrap", &domain, path.to_string_lossy().as_ref()])
        .wrap_err("Failed to bootstrap LaunchAgent")?;
    run_launchctl(&["enable", &service]).wrap_err("Failed to enable LaunchAgent")?;
    run_launchctl(&["kickstart", "-k", &service]).wrap_err("Failed to start LaunchAgent")?;

    info!("Loaded and started LaunchAgent {}", LABEL);

    Ok(())
}

fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn status_port() -> u16 {
    status::load_live_status()
        .ok()
        .flatten()
        .and_then(|s| s.local_addr)
        .and_then(|addr| addr.parse::<std::net::SocketAddr>().ok())
        .map(|addr| addr.port())
        .or_else(|| {
            crate::config::NexdeskConfig::load()
                .ok()
                .map(|config| config.port)
        })
        .unwrap_or(4242)
}

fn pgrep_nexdesk_args() -> [&'static str; 2] {
    ["-x", "nexdesk"]
}

fn process_command_line(pid: u32) -> Option<String> {
    command_stdout(
        "ps",
        &["-p", &pid.to_string(), "-o", "pid=", "-o", "command="],
    )
    .ok()
    .filter(|line| !line.trim().is_empty())
}

fn nexdesk_process_summary() -> Option<String> {
    let current_pid = std::process::id();
    let output = command_stdout("pgrep", &pgrep_nexdesk_args()).ok()?;
    output.lines().find_map(|line| {
        let pid = line.trim().parse::<u32>().ok()?;
        if pid == current_pid {
            None
        } else {
            process_command_line(pid).or_else(|| Some(pid.to_string()))
        }
    })
}

fn print_connection_summary() {
    match status::load_live_status().ok().flatten() {
        Some(s) => {
            let safe = |value: &str| status::terminal_safe(value, status::MAX_STATUS_DISPLAY_BYTES);
            let peer = safe(
                s.peer_name
                    .as_deref()
                    .or(s.peer_addr.as_deref())
                    .unwrap_or("unknown"),
            );
            let addr = safe(s.peer_addr.as_deref().unwrap_or("unknown"));
            let screen = safe(s.peer_screen.as_deref().unwrap_or("unknown screen"));
            match s.state.as_str() {
                "connected" => println!("Connected: {} ({}) — {}", peer, addr, screen),
                "connecting" => println!("Connected: connecting to {}", addr),
                "listening" => println!("Connected: no client connected"),
                "disconnected" => println!("Connected: disconnected from {}", addr),
                other => println!("Connected: {}", safe(other)),
            }
        }
        None => println!("Connected: unknown (no live runtime status)"),
    }
}

fn read_pipe_limited<R: Read>(
    mut reader: R,
    max_bytes: usize,
    too_large: &std::sync::atomic::AtomicBool,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if bytes.len().saturating_add(n) > max_bytes {
            too_large.store(true, std::sync::atomic::Ordering::Relaxed);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("command output exceeds {max_bytes} bytes"),
            ));
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    Ok(bytes)
}

fn command_output_limited(program: &str, args: &[&str]) -> Result<std::process::Output> {
    use std::process::Stdio;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err_with(|| format!("Failed to run {} {}", program, args.join(" ")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("{} stdout unavailable", program))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| eyre!("{} stderr unavailable", program))?;
    let too_large = Arc::new(AtomicBool::new(false));
    let stdout_too_large = Arc::clone(&too_large);
    let stderr_too_large = Arc::clone(&too_large);
    let max = status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES;
    let stdout_reader =
        std::thread::spawn(move || read_pipe_limited(stdout, max, &stdout_too_large));
    let stderr_reader =
        std::thread::spawn(move || read_pipe_limited(stderr, max, &stderr_too_large));

    let exit_status = loop {
        if too_large.load(Ordering::Relaxed) {
            child.kill().ok();
        }
        if let Some(status) = child
            .try_wait()
            .wrap_err_with(|| format!("Failed to wait for {} {}", program, args.join(" ")))?
        {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| eyre!("{} stdout reader panicked", program))?
        .wrap_err_with(|| format!("{} {} produced too much stdout", program, args.join(" ")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| eyre!("{} stderr reader panicked", program))?
        .wrap_err_with(|| format!("{} {} produced too much stderr", program, args.join(" ")))?;

    Ok(std::process::Output {
        status: exit_status,
        stdout,
        stderr,
    })
}

fn command_stdout(program: &str, args: &[&str]) -> Result<String> {
    let output = command_output_limited(program, args)?;

    if output.stdout.len() > status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES {
        return Err(eyre!(
            "{} {} produced too much stdout: {} bytes (max {})",
            program,
            args.join(" "),
            output.stdout.len(),
            status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES
        ));
    }
    if output.stderr.len() > status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES {
        return Err(eyre!(
            "{} {} produced too much stderr: {} bytes (max {})",
            program,
            args.join(" "),
            output.stderr.len(),
            status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES
        ));
    }

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = status::terminal_safe_multiline(&stderr, status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES);
    Err(eyre!(
        "{} {} exited with {}: {}",
        program,
        args.join(" "),
        output.status,
        stderr.trim()
    ))
}

fn print_command(program: &str, args: &[&str]) -> Result<()> {
    let command_display = status::terminal_safe(
        &format!("{} {}", program, args.join(" ")),
        status::MAX_STATUS_DISPLAY_BYTES,
    );
    println!("\n$ {}", command_display);
    let output = command_output_limited(program, args)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        let stdout =
            status::terminal_safe_multiline(&stdout, status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES);
        print!("{}", stdout);
        if !stdout.ends_with('\n') {
            println!();
        }
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        let stderr =
            status::terminal_safe_multiline(&stderr, status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES);
        eprint!("{}", stderr);
        if !stderr.ends_with('\n') {
            eprintln!();
        }
    }

    if !output.status.success() {
        println!("(exit status: {})", output.status);
    }

    Ok(())
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let output = command_output_limited("launchctl", args)?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = status::terminal_safe_multiline(
        &String::from_utf8_lossy(&output.stderr),
        status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES,
    );
    let stdout = status::terminal_safe_multiline(
        &String::from_utf8_lossy(&output.stdout),
        status::MAX_COMMAND_OUTPUT_DISPLAY_BYTES,
    );
    Err(eyre!(
        "launchctl {} exited with {}{}\n{}",
        args.join(" "),
        output.status,
        if stdout.trim().is_empty() {
            String::new()
        } else {
            format!("\n{}", stdout.trim())
        },
        stderr.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_summary_uses_exact_pgrep_name_match() {
        assert_eq!(pgrep_nexdesk_args(), ["-x", "nexdesk"]);
    }

    #[test]
    fn plist_arguments_are_bounded_and_escaped() {
        let entries = plist_arg_entries(&["serve", "--edge", "left&right"]).unwrap();
        assert!(entries.contains("left&amp;right"));
        assert!(plist_arg_entries(&[&"x".repeat(MAX_LAUNCHAGENT_ARG_BYTES)]).is_ok());
        assert!(plist_arg_entries(&[&"x".repeat(MAX_LAUNCHAGENT_ARG_BYTES + 1)]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn command_output_limited_enforces_output_bounds() {
        let output = command_output_limited("sh", &["-c", "printf abcdef"]).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "abcdef");

        let err = command_output_limited("sh", &["-c", "yes x"]).unwrap_err();
        assert!(err.to_string().contains("produced too much stdout"));
    }

    #[cfg(unix)]
    #[test]
    fn launchagent_log_dir_permissions_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        restrict_log_dir_permissions(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn launchagent_plist_permissions_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("com.nexdesk.agent.plist");
        std::fs::write(&path, b"<plist/>").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        restrict_plist_file_permissions(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
