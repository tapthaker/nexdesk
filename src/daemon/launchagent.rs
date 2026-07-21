use color_eyre::eyre::{eyre, Result, WrapErr};
use std::path::PathBuf;
use tracing::info;

use crate::command::{run_checked, run_command, RealCommandRunner};
use crate::ports::CommandRunner;
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
    start_with_runner(&RealCommandRunner, &plist_path(), current_uid())
}

fn start_with_runner(runner: &dyn CommandRunner, path: &std::path::Path, uid: u32) -> Result<()> {
    if !path.is_file() {
        return Err(eyre!(
            "nexdesk daemon is not installed; run `nexdesk setup` first"
        ));
    }

    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{LABEL}");

    if command_stdout_with_runner(runner, "launchctl", &["print", &service]).is_err() {
        run_launchctl_with_runner(
            runner,
            &["bootstrap", &domain, path.to_string_lossy().as_ref()],
        )
        .wrap_err("Failed to load nexdesk daemon")?;
    }
    run_launchctl_with_runner(runner, &["enable", &service])
        .wrap_err("Failed to enable nexdesk daemon")?;
    run_launchctl_with_runner(runner, &["kickstart", &service])
        .wrap_err("Failed to start nexdesk daemon")?;

    println!("nexdesk daemon started");
    Ok(())
}

pub fn stop() -> Result<()> {
    stop_with_runner(&RealCommandRunner, &plist_path(), current_uid())
}

fn stop_with_runner(runner: &dyn CommandRunner, path: &std::path::Path, uid: u32) -> Result<()> {
    if !path.is_file() {
        return Err(eyre!(
            "nexdesk daemon is not installed; run `nexdesk setup` first"
        ));
    }

    let service = format!("gui/{uid}/{LABEL}");

    if command_stdout_with_runner(runner, "launchctl", &["print", &service]).is_err() {
        println!("nexdesk daemon is already stopped");
        return Ok(());
    }

    run_launchctl_with_runner(runner, &["bootout", &service])
        .wrap_err("Failed to stop nexdesk daemon")?;
    println!("nexdesk daemon stopped");
    Ok(())
}

pub fn print_status() -> Result<()> {
    let uid = current_uid();
    let loaded = service_loaded_with_runner(&RealCommandRunner, uid);
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
    install_with_runner(args, &plist_path(), current_uid(), &RealCommandRunner)
}

fn install_with_runner(
    args: &[&str],
    path: &std::path::Path,
    uid: u32,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let path = path.to_path_buf();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).wrap_err("Failed to create LaunchAgents directory")?;
    }
    std::fs::write(&path, plist_content(args)).wrap_err("Failed to write LaunchAgent plist")?;

    info!("Installed LaunchAgent at {}", path.display());

    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{LABEL}");

    let _ = run_launchctl_with_runner(
        runner,
        &["bootout", &domain, path.to_string_lossy().as_ref()],
    );
    run_launchctl_with_runner(
        runner,
        &["bootstrap", &domain, path.to_string_lossy().as_ref()],
    )
    .wrap_err("Failed to bootstrap LaunchAgent")?;
    run_launchctl_with_runner(runner, &["enable", &service])
        .wrap_err("Failed to enable LaunchAgent")?;
    run_launchctl_with_runner(runner, &["kickstart", "-k", &service])
        .wrap_err("Failed to start LaunchAgent")?;

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

fn service_loaded_with_runner(runner: &dyn CommandRunner, uid: u32) -> bool {
    let service = format!("gui/{uid}/{LABEL}");
    command_stdout_with_runner(runner, "launchctl", &["print", &service]).is_ok()
}

fn command_stdout(program: &str, args: &[&str]) -> Result<String> {
    command_stdout_with_runner(&RealCommandRunner, program, args)
}

fn command_stdout_with_runner(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[&str],
) -> Result<String> {
    let output = run_checked(runner, program, args)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn print_command(program: &str, args: &[&str]) -> Result<()> {
    println!("\n$ {} {}", program, args.join(" "));
    let output = run_command(&RealCommandRunner, program, args)?;

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

    if !output.success {
        println!("(exit status: {:?})", output.code);
    }

    Ok(())
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    run_launchctl_with_runner(&RealCommandRunner, args)
}

fn run_launchctl_with_runner(runner: &dyn CommandRunner, args: &[&str]) -> Result<()> {
    run_checked(runner, "launchctl", args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::CommandOutput;
    use crate::testing::{CommandObservation, ScriptedCommandRunner};

    fn success() -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            signal: None,
            stdout: b"loaded".to_vec(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn launchd_install_start_stop_and_status_use_only_scripted_commands() {
        let temp = tempfile::tempdir().unwrap();
        let plist = temp.path().join("agent.plist");
        let runner = ScriptedCommandRunner::new();
        for _ in 0..10 {
            runner.push_output(success());
        }

        install_with_runner(&["connect", "192.0.2.1:4242"], &plist, 501, &runner).unwrap();
        start_with_runner(&runner, &plist, 501).unwrap();
        stop_with_runner(&runner, &plist, 501).unwrap();
        assert!(service_loaded_with_runner(&runner, 501));

        assert_eq!(runner.remaining_actions(), 0);
        assert_eq!(
            runner
                .observations()
                .snapshot()
                .into_iter()
                .filter(|entry| matches!(entry.event, CommandObservation::Run(_)))
                .count(),
            10
        );
        assert!(plist.is_file());
    }
}
