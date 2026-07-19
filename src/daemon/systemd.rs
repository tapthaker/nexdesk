use std::path::PathBuf;
use std::process::Command;

use color_eyre::eyre::{eyre, Result, WrapErr};
use tracing::{info, warn};

use crate::status;

const SERVICE_NAME: &str = "nexdesk";
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

fn systemd_escape(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', r#"\""#)
}

fn service_environment() -> String {
    SESSION_ENV_VARS
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (*key, value)))
        .map(|(key, value)| format!("Environment=\"{}={}\"", key, systemd_escape(&value)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn service_unit(args: &[&str]) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nexdesk"));

    let args_str = args.join(" ");
    let env_lines = service_environment();
    let env_block = if env_lines.is_empty() {
        String::new()
    } else {
        format!("{env_lines}\n")
    };

    format!(
        r#"[Unit]
Description=Nexdesk KVM Sharing Service
After=network.target

[Service]
Type=simple
# Wait until the machine has a routable IPv4 address before starting.
# If nexdesk starts before Wi-Fi is up, mDNS can advertise an empty/unreachable IP.
ExecStartPre=/bin/sh -c 'for i in $(seq 1 60); do ip route get 1.1.1.1 2>/dev/null | grep -q " src " && exit 0; sleep 1; done; exit 1'
ExecStart={exe} {args}
{env_block}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"#,
        exe = exe.display(),
        args = args_str,
        env_block = env_block,
    )
}

pub fn start() -> Result<()> {
    if !service_file_path().is_file() {
        return Err(eyre!(
            "nexdesk daemon is not installed; run `nexdesk setup` first"
        ));
    }

    run_systemctl(&["--user", "daemon-reload"]).wrap_err("Failed to reload systemd user units")?;
    run_systemctl(&["--user", "start", "nexdesk.service"])
        .wrap_err("Failed to start nexdesk daemon")?;
    println!("nexdesk daemon started");
    Ok(())
}

pub fn stop() -> Result<()> {
    if !service_file_path().is_file() {
        return Err(eyre!(
            "nexdesk daemon is not installed; run `nexdesk setup` first"
        ));
    }

    run_systemctl(&["--user", "stop", "nexdesk.service"])
        .wrap_err("Failed to stop nexdesk daemon")?;
    println!("nexdesk daemon stopped");
    Ok(())
}

pub fn print_status() -> Result<()> {
    let active = command_stdout("systemctl", &["--user", "is-active", "nexdesk.service"])
        .unwrap_or_else(|_| "unknown".into());
    let enabled = command_stdout("systemctl", &["--user", "is-enabled", "nexdesk.service"])
        .unwrap_or_else(|_| "unknown".into());
    let process = command_stdout("pgrep", &["-a", "nexdesk"]).ok();
    let listener = command_stdout(
        "sh",
        &[
            "-c",
            "ss -lunp 2>/dev/null | grep -E '(:4242\\b|nexdesk)' || true",
        ],
    )
    .unwrap_or_default();

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
        process
            .as_deref()
            .and_then(|p| p.lines().next())
            .unwrap_or("not running")
    );
    println!(
        "Listener: {}",
        if listener.trim().is_empty() {
            "not listening on UDP 4242"
        } else {
            "listening on UDP 4242"
        }
    );
    print_connection_summary();
    println!("\nFor logs: nexdesk daemon logs");
    Ok(())
}

pub fn print_logs() -> Result<()> {
    println!("nexdesk daemon logs (systemd user)\n");
    print_command("systemctl", &["--user", "is-active", "nexdesk.service"])?;
    print_command("systemctl", &["--user", "is-enabled", "nexdesk.service"])?;
    print_command(
        "systemctl",
        &["--user", "status", "nexdesk.service", "--no-pager"],
    )?;
    print_command("pgrep", &["-a", "nexdesk"])?;
    print_command(
        "sh",
        &[
            "-c",
            "ss -lunp 2>/dev/null | grep -E '(:4242\\b|nexdesk)' || true",
        ],
    )?;
    print_command(
        "journalctl",
        &["--user", "-u", "nexdesk.service", "-n", "40", "--no-pager"],
    )?;
    Ok(())
}

pub fn install(args: &[&str]) -> Result<()> {
    configure_firewall(args);

    let path = service_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).wrap_err("Failed to create systemd user directory")?;
    }
    std::fs::write(&path, service_unit(args)).wrap_err("Failed to write systemd service file")?;

    info!("Installed systemd user service at {}", path.display());
    for key in SESSION_ENV_VARS {
        if let Ok(value) = std::env::var(key) {
            info!("Captured session env {}={}", key, value);
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

fn service_port(args: &[&str]) -> u16 {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match *arg {
            "-p" | "--port" => {
                if let Some(value) = iter.next() {
                    if let Ok(port) = value.parse() {
                        return port;
                    }
                }
            }
            _ => {}
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

fn run_sudo_ufw(args: &[&str]) -> Result<()> {
    let output = Command::new("sudo")
        .arg("-n")
        .arg("ufw")
        .args(args)
        .output()
        .wrap_err_with(|| format!("Failed to run sudo -n ufw {}", args.join(" ")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
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

fn print_connection_summary() {
    match status::load_status().ok().flatten() {
        Some(s) => {
            let peer = s
                .peer_name
                .as_deref()
                .or(s.peer_addr.as_deref())
                .unwrap_or("unknown");
            let addr = s.peer_addr.as_deref().unwrap_or("unknown");
            let screen = s.peer_screen.as_deref().unwrap_or("unknown screen");
            match s.state.as_str() {
                "connected" => println!("Connected: {} ({}) — {}", peer, addr, screen),
                "connecting" => println!("Connected: connecting to {}", addr),
                "listening" => println!("Connected: no client connected"),
                "disconnected" => println!("Connected: disconnected from {}", addr),
                other => println!("Connected: {}", other),
            }
        }
        None => println!("Connected: unknown"),
    }
}

fn command_stdout(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .wrap_err_with(|| format!("Failed to run {} {}", program, args.join(" ")))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(eyre!(
        "{} {} exited with {}: {}",
        program,
        args.join(" "),
        output.status,
        stderr.trim()
    ))
}

fn print_command(program: &str, args: &[&str]) -> Result<()> {
    println!("\n$ {} {}", program, args.join(" "));
    let output = Command::new(program)
        .args(args)
        .output()
        .wrap_err_with(|| format!("Failed to run {} {}", program, args.join(" ")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        print!("{}", stdout);
        if !stdout.ends_with('\n') {
            println!();
        }
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
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
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .wrap_err_with(|| format!("Failed to run systemctl {}", args.join(" ")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
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
