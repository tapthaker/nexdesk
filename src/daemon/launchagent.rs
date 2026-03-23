use std::path::PathBuf;
use std::process::Command;

use color_eyre::eyre::{Result, WrapErr};
use tracing::info;

const LABEL: &str = "com.nexdesk.agent";

fn plist_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/Shared".into());
    PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", LABEL))
}

fn plist_content(args: &[&str]) -> String {
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("nexdesk"));

    let arg_entries: String = args
        .iter()
        .map(|a| format!("        <string>{}</string>", a))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
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
    <string>/tmp/nexdesk.out.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/nexdesk.err.log</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = exe.display(),
        arg_entries = arg_entries,
    )
}

pub fn install(args: &[&str]) -> Result<()> {
    let path = plist_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err("Failed to create LaunchAgents directory")?;
    }

    // Unload existing service first (ignore errors if not loaded)
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}", uid()), LABEL])
        .output();

    std::fs::write(&path, plist_content(args))
        .wrap_err("Failed to write LaunchAgent plist")?;

    info!("Installed LaunchAgent at {}", path.display());

    // Load the service into launchd
    let domain_target = format!("gui/{}", uid());
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain_target, &path.display().to_string()])
        .output()
        .wrap_err("Failed to run launchctl bootstrap")?;

    if status.status.success() {
        info!("LaunchAgent loaded successfully");
    } else {
        let stderr = String::from_utf8_lossy(&status.stderr);
        // Fall back to legacy `launchctl load` if bootstrap fails
        info!("bootstrap failed ({}), trying legacy load", stderr.trim());
        let status = Command::new("launchctl")
            .args(["load", "-w", &path.display().to_string()])
            .output()
            .wrap_err("Failed to run launchctl load")?;
        if status.status.success() {
            info!("LaunchAgent loaded successfully (legacy)");
        } else {
            let stderr = String::from_utf8_lossy(&status.stderr);
            tracing::warn!("Failed to load LaunchAgent: {}", stderr.trim());
        }
    }

    Ok(())
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}
