use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result, WrapErr};

use crate::ports::{CommandOutput, CommandRequest, CommandRunner};

#[derive(Clone, Copy, Debug, Default)]
pub struct RealCommandRunner;

pub fn run_command(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[&str],
) -> Result<CommandOutput> {
    runner.run(&CommandRequest::new(program).args(args.iter().copied()))
}

pub fn run_checked(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[&str],
) -> Result<CommandOutput> {
    let output = run_command(runner, program, args)?;
    if output.success {
        Ok(output)
    } else {
        Err(eyre!(
            "{} {} exited with {:?}: {}",
            program,
            args.join(" "),
            output.code,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

impl CommandRunner for RealCommandRunner {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput> {
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .wrap_err_with(|| format!("Failed to start {}", request.program))?;

        let stdin = child.stdin.take();
        let input = request.stdin.clone();
        let input_task = std::thread::spawn(move || -> std::io::Result<()> {
            if let Some(mut stdin) = stdin {
                stdin.write_all(&input)?;
            }
            Ok(())
        });
        let stdout_task = drain_bounded(child.stdout.take(), request.max_stdout_bytes);
        let stderr_task = drain_bounded(child.stderr.take(), request.max_stderr_bytes);
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().wrap_err("Failed to poll child process")? {
                break status;
            }
            if started.elapsed() >= request.timeout {
                terminate_child_tree(&mut child);
                let _ = input_task.join();
                let _ = stdout_task.join();
                let _ = stderr_task.join();
                return Err(eyre!(
                    "Command {} timed out after {:?}",
                    request.program,
                    request.timeout
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        input_task
            .join()
            .map_err(|_| eyre!("Command stdin worker panicked"))??;
        let (stdout, stdout_truncated) = stdout_task
            .join()
            .map_err(|_| eyre!("Command stdout worker panicked"))??;
        let (stderr, stderr_truncated) = stderr_task
            .join()
            .map_err(|_| eyre!("Command stderr worker panicked"))??;

        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;

        Ok(CommandOutput {
            success: status.success(),
            code: status.code(),
            signal,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }
}

fn terminate_child_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    child.kill().ok();
    child.wait().ok();
}

fn drain_bounded(
    stream: Option<impl Read + Send + 'static>,
    limit: usize,
) -> std::thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>> {
    std::thread::spawn(move || {
        let mut retained = Vec::new();
        let mut truncated = false;
        let Some(mut stream) = stream else {
            return Ok((retained, truncated));
        };
        let mut buffer = [0u8; 8192];
        loop {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let available = limit.saturating_sub(retained.len());
            let keep = count.min(available);
            retained.extend_from_slice(&buffer[..keep]);
            truncated |= keep < count;
        }
        Ok((retained, truncated))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn real_runner_bounds_output_and_reports_exit_and_signal() {
        let mut request =
            CommandRequest::new("sh").args(["-c", "printf 123456; printf abcdef >&2; exit 7"]);
        request.max_stdout_bytes = 3;
        request.max_stderr_bytes = 4;
        let output = RealCommandRunner.run(&request).unwrap();
        assert!(!output.success);
        assert_eq!(output.code, Some(7));
        assert_eq!(output.stdout, b"123");
        assert_eq!(output.stderr, b"abcd");
        assert!(output.stdout_truncated && output.stderr_truncated);

        let signal = RealCommandRunner
            .run(&CommandRequest::new("sh").args(["-c", "kill -TERM $$"]))
            .unwrap();
        assert_eq!(signal.signal, Some(15));
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_terminates_hung_process_at_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let pid_path = temp.path().join("pid");
        let script = format!("echo $$ > {}; sleep 10", pid_path.display());
        let mut request = CommandRequest::new("sh").args(["-c", &script]);
        request.timeout = Duration::from_millis(100);

        let error = RealCommandRunner.run(&request).unwrap_err();

        assert!(error.to_string().contains("timed out"));
        let pid: i32 = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let process_exists = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!process_exists, "timed-out child process {pid} survived");
    }
}
