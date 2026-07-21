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
        let mut child = Command::new(&request.program)
            .args(&request.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
                child.kill().ok();
                child.wait().ok();
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
        let mut request = CommandRequest::new("sh").args(["-c", "sleep 10"]);
        request.timeout = Duration::from_millis(50);
        let error = RealCommandRunner.run(&request).unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }
}
