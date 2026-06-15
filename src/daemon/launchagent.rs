use std::path::PathBuf;
use std::process::Command;

use color_eyre::eyre::{eyre, Result, WrapErr};
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
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nexdesk"));

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

pub fn print_status() -> Result<()> {
    let uid = current_uid();
    let service = format!("gui/{uid}/{LABEL}");

    println!("nexdesk service status (LaunchAgent)\n");
    print_command("launchctl", &["print", &service])?;
    print_command("pgrep", &["-a", "nexdesk"])?;
    print_command("sh", &["-c", "lsof -nP -iUDP:4242 2>/dev/null || true"])?;
    print_command(
        "sh",
        &[
            "-c",
            "tail -n 40 /tmp/nexdesk.out.log /tmp/nexdesk.err.log 2>/dev/null || true",
        ],
    )?;
    Ok(())
}

pub fn install(args: &[&str]) -> Result<()> {
    let path = plist_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).wrap_err("Failed to create LaunchAgents directory")?;
    }
    std::fs::write(&path, plist_content(args)).wrap_err("Failed to write LaunchAgent plist")?;

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

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
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

fn run_launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .wrap_err_with(|| format!("Failed to run launchctl {}", args.join(" ")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
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
