use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr};
use tracing::info;

const SERVICE_NAME: &str = "nexdesk";

fn service_file_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user")
        .join(format!("{}.service", SERVICE_NAME))
}

fn service_unit() -> String {
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("nexdesk"));

    format!(
        r#"[Unit]
Description=Nexdesk KVM Sharing Service
After=network.target

[Service]
Type=simple
ExecStart={exe} serve
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"#,
        exe = exe.display()
    )
}

pub fn install() -> Result<()> {
    let path = service_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err("Failed to create systemd user directory")?;
    }
    std::fs::write(&path, service_unit())
        .wrap_err("Failed to write systemd service file")?;

    info!("Installed systemd user service at {}", path.display());
    info!("Run: systemctl --user daemon-reload && systemctl --user enable --now {}", SERVICE_NAME);

    Ok(())
}
