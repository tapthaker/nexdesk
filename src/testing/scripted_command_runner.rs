use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use color_eyre::eyre::{eyre, Result};

use crate::ports::{CommandOutput, CommandRequest, CommandRunner};
use crate::testing::ObservationLog;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandObservation {
    Run(CommandRequest),
    Failed(String),
}

enum CommandAction {
    Output(CommandOutput),
    Failure(String),
    Block(Arc<BlockState>),
}

struct BlockState {
    state: Mutex<BlockedCommandState>,
    changed: Condvar,
}

struct BlockedCommandState {
    entered: bool,
    outcome: Option<std::result::Result<CommandOutput, String>>,
}

#[derive(Clone, Default)]
pub struct ScriptedCommandRunner {
    actions: Arc<Mutex<VecDeque<CommandAction>>>,
    observations: ObservationLog<CommandObservation>,
}

impl ScriptedCommandRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_output(&self, output: CommandOutput) {
        lock_recover(&self.actions).push_back(CommandAction::Output(output));
    }

    pub fn fail_next(&self, message: impl Into<String>) {
        lock_recover(&self.actions).push_back(CommandAction::Failure(message.into()));
    }

    pub fn block_next(&self) -> BlockingCommand {
        let state = Arc::new(BlockState {
            state: Mutex::new(BlockedCommandState {
                entered: false,
                outcome: None,
            }),
            changed: Condvar::new(),
        });
        lock_recover(&self.actions).push_back(CommandAction::Block(state.clone()));
        BlockingCommand { state }
    }

    pub fn observations(&self) -> ObservationLog<CommandObservation> {
        self.observations.clone()
    }

    pub fn remaining_actions(&self) -> usize {
        lock_recover(&self.actions).len()
    }
}

impl CommandRunner for ScriptedCommandRunner {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput> {
        self.observations
            .record(CommandObservation::Run(request.clone()));
        let action = lock_recover(&self.actions)
            .pop_front()
            .ok_or_else(|| eyre!("ScriptedCommandRunner unexpected command"))?;
        let result = match action {
            CommandAction::Output(output) => Ok(output),
            CommandAction::Failure(message) => Err(message),
            CommandAction::Block(state) => {
                let mut blocked = lock_recover(&state.state);
                blocked.entered = true;
                state.changed.notify_all();
                while blocked.outcome.is_none() {
                    blocked = state
                        .changed
                        .wait(blocked)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                blocked.outcome.take().unwrap()
            }
        };
        result.map_err(|message| {
            self.observations
                .record(CommandObservation::Failed(message.clone()));
            eyre!(message)
        })
    }
}

pub struct BlockingCommand {
    state: Arc<BlockState>,
}

impl BlockingCommand {
    pub fn wait_until_entered(&self) {
        let mut blocked = lock_recover(&self.state.state);
        while !blocked.entered {
            blocked = self
                .state
                .changed
                .wait(blocked)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub fn complete(&self, output: CommandOutput) {
        lock_recover(&self.state.state).outcome = Some(Ok(output));
        self.state.changed.notify_all();
    }

    pub fn fail(&self, message: impl Into<String>) {
        lock_recover(&self.state.state).outcome = Some(Err(message.into()));
        self.state.changed.notify_all();
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success() -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            signal: None,
            stdout: b"ok".to_vec(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn scripted_runner_supports_outputs_failures_and_hangs() {
        let runner = ScriptedCommandRunner::new();
        runner.push_output(success());
        runner.fail_next("spawn failed");
        assert!(runner.run(&CommandRequest::new("one")).unwrap().success);
        assert!(runner
            .run(&CommandRequest::new("two"))
            .unwrap_err()
            .to_string()
            .contains("spawn failed"));

        let gate = runner.block_next();
        let worker = {
            let runner = runner.clone();
            std::thread::spawn(move || runner.run(&CommandRequest::new("hung")))
        };
        gate.wait_until_entered();
        gate.complete(success());
        assert!(worker.join().unwrap().unwrap().success);
    }
}
