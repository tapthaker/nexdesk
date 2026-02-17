use std::path::PathBuf;

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

fn plist_content() -> String {
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("nexdesk"));

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
        <string>serve</string>
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
        exe = exe.display()
    )
}

pub fn install() -> Result<()> {
    let path = plist_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err("Failed to create LaunchAgents directory")?;
    }
    std::fs::write(&path, plist_content())
        .wrap_err("Failed to write LaunchAgent plist")?;

    info!("Installed LaunchAgent at {}", path.display());
    info!("Run: launchctl load {}", path.display());

    Ok(())
}
