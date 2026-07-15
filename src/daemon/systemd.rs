use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;

use color_eyre::eyre::{eyre, Result, WrapErr};
use tracing::{info, warn};

use crate::status;

const SERVICE_NAME: &str = "nexdesk";
const MAX_SYSTEMD_ENV_VALUE_BYTES: usize = 4096;
const MAX_SYSTEMD_ARG_BYTES: usize = 4096;
const MAX_SYSTEMD_UNIT_BYTES: usize = 64 * 1024;
const SESSION_ENV_VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XAUTHORITY",
];

fn service_file_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user")
        .join(format!("{}.service", SERVICE_NAME))
}

#[cfg(unix)]
fn restrict_service_file_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).wrap_err_with(|| {
        format!(
            "Failed to restrict systemd service file permissions: {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn restrict_service_file_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn systemd_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match *byte {
            b'%' => escaped.push_str("%%"),
            b'\\' => escaped.push_str(r"\\"),
            b'"' => escaped.push_str(r#"\""#),
            0x20..=0x7e => escaped.push(*byte as char),
            other => escaped.push_str(&format!(r"\x{other:02x}")),
        }
    }
    escaped
}

fn systemd_exec_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'/'
            | b'.'
            | b'_'
            | b'-'
            | b':'
            | b'['
            | b']'
            | b'@'
            | b','
            | b'=' => escaped.push(*byte as char),
            other => escaped.push_str(&format!(r"\x{other:02x}")),
        }
    }
    if escaped.is_empty() {
        r#""""#.to_string()
    } else {
        escaped
    }
}

fn service_environment_assignment(key: &str, value: &str) -> Option<String> {
    if value.len() > MAX_SYSTEMD_ENV_VALUE_BYTES {
        return None;
    }
    Some(format!("Environment=\"{}={}\"", key, systemd_escape(value)))
}

fn service_environment() -> String {
    SESSION_ENV_VARS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .and_then(|value| service_environment_assignment(key, &value))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn service_args(args: &[&str]) -> Result<String> {
    args.iter()
        .map(|arg| {
            if arg.len() > MAX_SYSTEMD_ARG_BYTES {
                return Err(eyre!(
                    "systemd service argument too large: {} bytes (max {})",
                    arg.len(),
                    MAX_SYSTEMD_ARG_BYTES
                ));
            }
            Ok(systemd_exec_escape(arg))
        })
        .collect::<Result<Vec<_>>>()
        .map(|args| args.join(" "))
}

fn service_unit(args: &[&str]) -> Result<String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nexdesk"));

    let args_str = service_args(args)?;
    let exe = systemd_exec_escape(&exe.display().to_string());
    let env_lines = service_environment();
    let env_block = if env_lines.is_empty() {
        String::new()
    } else {
        format!("{env_lines}\n")
    };

    let unit = format!(
        r#"[Unit]
Description=Nexdesk KVM Sharing Service
After=network.target

[Service]
Type=simple
ExecStart={exe} {args}
{env_block}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"#,
        exe = exe,
        args = args_str,
        env_block = env_block,
    );
    if unit.len() > MAX_SYSTEMD_UNIT_BYTES {
        return Err(eyre!(
            "systemd service unit too large: {} bytes (max {})",
            unit.len(),
            MAX_SYSTEMD_UNIT_BYTES
        ));
    }
    Ok(unit)
}

pub fn print_status() -> Result<()> {
    let active = command_stdout("systemctl", &["--user", "is-active", "nexdesk.service"])
        .unwrap_or_else(|_| "unknown".into());
    let enabled = command_stdout("systemctl", &["--user", "is-enabled", "nexdesk.service"])
        .unwrap_or_else(|_| "unknown".into());
    let process = nexdesk_process_summary();
    let port = status_port();
    let listener_cmd = format!("ss -lunp 2>/dev/null | grep -E ':{port}\\b' || true");
    let listener = command_stdout("sh", &["-c", &listener_cmd]).unwrap_or_default();

    println!("nexdesk daemon status");
    println!(
        "Service : {}",
        if active == "active" {
            "running"
        } else {
            active.as_str()
        }
    );
    println!("Startup : {}", enabled);
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
    println!("\nFor logs: nexdesk log (or nexdesk daemon log)");
    Ok(())
}

pub fn print_log() -> Result<()> {
    println!("nexdesk service log (systemd user)\n");
    print_command("systemctl", &["--user", "is-active", "nexdesk.service"])?;
    print_command("systemctl", &["--user", "is-enabled", "nexdesk.service"])?;
    print_command(
        "systemctl",
        &["--user", "status", "nexdesk.service", "--no-pager"],
    )?;
    print_command("pgrep", &pgrep_nexdesk_args())?;
    let port = status_port();
    let listener_cmd = format!("ss -lunp 2>/dev/null | grep -E ':{port}\\b' || true");
    print_command("sh", &["-c", &listener_cmd])?;
    print_command(
        "journalctl",
        &["--user", "-u", "nexdesk.service", "-n", "40", "--no-pager"],
    )?;
    Ok(())
}

pub fn start() -> Result<()> {
    if !service_file_path().is_file() {
        return Err(eyre!(
            "Nexdesk background service is not installed. Run `nexdesk daemon setup` first."
        ));
    }

    run_systemctl(&["--user", "daemon-reload"]).wrap_err("Failed to reload systemd user units")?;
    run_systemctl(&["--user", "start", &format!("{SERVICE_NAME}.service")])
        .wrap_err("Failed to start systemd user service")?;

    println!("Nexdesk background service started.");
    Ok(())
}

pub fn stop() -> Result<()> {
    if !service_file_path().is_file() {
        return Err(eyre!(
            "Nexdesk background service is not installed. Run `nexdesk daemon setup` first."
        ));
    }

    run_systemctl(&["--user", "stop", &format!("{SERVICE_NAME}.service")])
        .wrap_err("Failed to stop systemd user service")?;
    println!("Nexdesk background service stopped.");
    Ok(())
}

pub fn install(args: &[&str]) -> Result<()> {
    // Validate and render before side effects such as firewall changes. If the
    // service arguments are invalid/oversized, installation should fail without
    // modifying UFW rules.
    let contents = service_unit(args)?;

    configure_firewall(args);

    let path = service_file_path();
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("Invalid systemd service path: {}", path.display()))?;
    std::fs::create_dir_all(parent).wrap_err("Failed to create systemd user directory")?;

    let mut tmp = tempfile::Builder::new()
        .prefix(&format!("{SERVICE_NAME}.service."))
        .tempfile_in(parent)
        .wrap_err_with(|| {
            format!(
                "Failed to create temporary systemd service file in {}",
                parent.display()
            )
        })?;
    restrict_service_file_permissions(tmp.path())?;
    tmp.write_all(contents.as_bytes())
        .wrap_err("Failed to write temporary systemd service file")?;
    tmp.as_file_mut()
        .sync_all()
        .wrap_err("Failed to sync temporary systemd service file")?;
    tmp.persist(&path)
        .map_err(|e| e.error)
        .wrap_err_with(|| format!("Failed to replace systemd service file: {}", path.display()))?;
    restrict_service_file_permissions(&path)?;
    sync_directory(parent).wrap_err_with(|| {
        format!(
            "Failed to sync systemd service directory after install: {}",
            parent.display()
        )
    })?;

    info!("Installed systemd user service at {}", path.display());
    for key in SESSION_ENV_VARS {
        if let Ok(value) = std::env::var(key) {
            if value.len() > MAX_SYSTEMD_ENV_VALUE_BYTES {
                warn!(
                    "Skipped session env {} because it is too large ({} bytes, max {})",
                    key,
                    value.len(),
                    MAX_SYSTEMD_ENV_VALUE_BYTES
                );
            } else {
                info!("Captured session env {}={}", key, systemd_escape(&value));
            }
        }
    }

    run_systemctl(&["--user", "daemon-reload"]).wrap_err("Failed to reload systemd user units")?;
    run_systemctl(&[
        "--user",
        "enable",
        "--now",
        &format!("{SERVICE_NAME}.service"),
    ])
    .wrap_err("Failed to enable and start systemd user service")?;

    info!("Enabled and started systemd user service {}", SERVICE_NAME);

    Ok(())
}

fn configure_firewall(args: &[&str]) {
    if args.first() != Some(&"serve") {
        return;
    }

    let port = service_port(args);
    if !ufw_is_active() {
        return;
    }

    match run_sudo_ufw(&[
        "allow",
        &format!("{port}/udp"),
        "comment",
        "nexdesk QUIC",
    ]) {
        Ok(()) => info!("Allowed nexdesk through UFW on UDP {}", port),
        Err(e) => warn!(
            "UFW is active but nexdesk could not add the UDP {} rule automatically: {}. Run: sudo ufw allow {}/udp comment 'nexdesk QUIC'",
            port, e, port
        ),
    }
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

fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn parse_service_port(value: &str) -> Option<u16> {
    value.parse::<u16>().ok().filter(|port| *port != 0)
}

fn service_port(args: &[&str]) -> u16 {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match *arg {
            "-p" | "--port" => {
                if let Some(value) = iter.next().and_then(|value| parse_service_port(value)) {
                    return value;
                }
            }
            _ => {
                if let Some(value) = arg
                    .strip_prefix("--port=")
                    .or_else(|| arg.strip_prefix("-p="))
                    .and_then(parse_service_port)
                {
                    return value;
                }
            }
        }
    }
    4242
}

fn ufw_is_active() -> bool {
    let output = Command::new("systemctl")
        .args(["is-active", "--quiet", "ufw"])
        .status();

    matches!(output, Ok(status) if status.success())
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

fn run_sudo_ufw(args: &[&str]) -> Result<()> {
    let mut sudo_args = vec!["-n", "ufw"];
    sudo_args.extend_from_slice(args);
    let output = command_output_limited("sudo", &sudo_args)
        .wrap_err_with(|| format!("Failed to run sudo -n ufw {}", args.join(" ")))?;

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
        "sudo -n ufw {} exited with {}{}\n{}",
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

fn run_systemctl(args: &[&str]) -> Result<()> {
    let output = command_output_limited("systemctl", args)?;

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
        "systemctl {} exited with {}{}\n{}",
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

    #[cfg(unix)]
    #[test]
    fn command_output_limited_enforces_output_bounds() {
        let output = command_output_limited("sh", &["-c", "printf abcdef"]).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "abcdef");

        let err = command_output_limited("sh", &["-c", "yes x"]).unwrap_err();
        assert!(err.to_string().contains("produced too much stdout"));
    }

    #[test]
    fn service_port_parses_supported_forms_and_ignores_invalid_zero() {
        assert_eq!(service_port(&["serve", "--port", "5555"]), 5555);
        assert_eq!(service_port(&["serve", "-p", "5556"]), 5556);
        assert_eq!(service_port(&["serve", "--port=5557"]), 5557);
        assert_eq!(service_port(&["serve", "-p=5558"]), 5558);
        assert_eq!(service_port(&["serve", "--port", "0"]), 4242);
        assert_eq!(service_port(&["serve", "--port=not-a-port"]), 4242);
    }

    #[test]
    fn exec_escape_handles_systemd_specifiers_and_whitespace() {
        assert_eq!(systemd_exec_escape("/tmp/a b/%n"), r"/tmp/a\x20b/\x25n");
    }

    #[test]
    fn exec_escape_preserves_common_socket_chars() {
        assert_eq!(systemd_exec_escape("[::1]:4242"), "[::1]:4242");
    }

    #[test]
    fn service_arguments_are_bounded_and_escaped() {
        assert_eq!(
            service_args(&["serve", "--edge", "left right"]).unwrap(),
            r"serve --edge left\x20right"
        );
        assert!(service_args(&[&"x".repeat(MAX_SYSTEMD_ARG_BYTES)]).is_ok());
        assert!(service_args(&[&"x".repeat(MAX_SYSTEMD_ARG_BYTES + 1)]).is_err());
    }

    #[test]
    fn environment_escape_prevents_unit_line_injection() {
        assert_eq!(
            systemd_escape("wayland-0\nExecStart=/bin/false\t%n"),
            r"wayland-0\x0aExecStart=/bin/false\x09%%n"
        );
    }

    #[test]
    fn environment_escape_quotes_backslash_and_quotes() {
        assert_eq!(systemd_escape(r#"a\"b"#), r#"a\\\"b"#);
    }

    #[test]
    fn service_environment_rejects_oversized_values() {
        assert!(service_environment_assignment(
            "DISPLAY",
            &"x".repeat(MAX_SYSTEMD_ENV_VALUE_BYTES)
        )
        .is_some());
        assert!(service_environment_assignment(
            "DISPLAY",
            &"x".repeat(MAX_SYSTEMD_ENV_VALUE_BYTES + 1)
        )
        .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn service_file_permissions_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nexdesk.service");
        std::fs::write(&path, b"[Service]\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        restrict_service_file_permissions(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
