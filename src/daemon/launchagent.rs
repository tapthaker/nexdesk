use std::path::PathBuf;
use std::process::Command;

use color_eyre::eyre::{eyre, Result, WrapErr};
use tracing::info;

use crate::status;

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

pub fn start() -> Result<()> {
    let path = plist_path();
    if !path.is_file() {
        return Err(eyre!(
            "nexdesk daemon is not installed; run `nexdesk setup` first"
        ));
    }

    let uid = current_uid();
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{LABEL}");

    if command_stdout("launchctl", &["print", &service]).is_err() {
        run_launchctl(&["bootstrap", &domain, path.to_string_lossy().as_ref()])
            .wrap_err("Failed to load nexdesk daemon")?;
    }
    run_launchctl(&["enable", &service]).wrap_err("Failed to enable nexdesk daemon")?;
    run_launchctl(&["kickstart", &service]).wrap_err("Failed to start nexdesk daemon")?;

    println!("nexdesk daemon started");
    Ok(())
}

pub fn stop() -> Result<()> {
    if !plist_path().is_file() {
        return Err(eyre!(
            "nexdesk daemon is not installed; run `nexdesk setup` first"
        ));
    }

    let uid = current_uid();
    let service = format!("gui/{uid}/{LABEL}");

    if command_stdout("launchctl", &["print", &service]).is_err() {
        println!("nexdesk daemon is already stopped");
        return Ok(());
    }

    run_launchctl(&["bootout", &service]).wrap_err("Failed to stop nexdesk daemon")?;
    println!("nexdesk daemon stopped");
    Ok(())
}

pub fn print_status() -> Result<()> {
    let uid = current_uid();
    let service = format!("gui/{uid}/{LABEL}");
    let loaded = command_stdout("launchctl", &["print", &service]).is_ok();
    let process = command_stdout("pgrep", &["-a", "nexdesk"]).ok();
    let listener = command_stdout("sh", &["-c", "lsof -nP -iUDP:4242 2>/dev/null || true"])
        .unwrap_or_default();

    println!("nexdesk daemon status");
    println!("Service : {}", if loaded { "loaded" } else { "not loaded" });
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
    println!("Logs    : /tmp/nexdesk.out.log, /tmp/nexdesk.err.log");
    println!("\nFor logs: nexdesk daemon logs");
    Ok(())
}

pub fn print_logs() -> Result<()> {
    let uid = current_uid();
    let service = format!("gui/{uid}/{LABEL}");

    println!("nexdesk daemon logs (LaunchAgent)\n");
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
