use std::path::PathBuf;
use std::process::Command;

use color_eyre::eyre::{eyre, Result, WrapErr};
use tracing::info;

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
ExecStart={exe} {args}
{env_block}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"#,
        exe = exe.display(),
        args = args_str,
        env_block = env_block,
    )
}

pub fn install(args: &[&str]) -> Result<()> {
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
