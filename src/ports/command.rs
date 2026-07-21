use std::time::Duration;

use color_eyre::eyre::Result;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl CommandRequest {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            stdin: Vec::new(),
            timeout: Duration::from_secs(10),
            max_stdout_bytes: 64 * 1024,
            max_stderr_bytes: 64 * 1024,
        }
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Bounded process execution boundary shared by command-backed adapters.
pub trait CommandRunner: Send + Sync {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SuccessRunner;

    impl CommandRunner for SuccessRunner {
        fn run(&self, _request: &CommandRequest) -> Result<CommandOutput> {
            Ok(CommandOutput {
                success: true,
                code: Some(0),
                signal: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }

    #[test]
    fn command_runner_is_object_safe_and_requests_are_bounded() {
        let runner: &dyn CommandRunner = &SuccessRunner;
        let request = CommandRequest::new("true");
        assert_eq!(request.timeout, Duration::from_secs(10));
        assert_eq!(request.max_stdout_bytes, 64 * 1024);
        assert!(runner.run(&request).unwrap().success);
    }
}
