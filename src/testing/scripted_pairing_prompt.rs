use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use color_eyre::eyre::eyre;

use crate::ports::{PairingPrompt, PairingPromptFuture};
use crate::testing::ObservationLog;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PairingPromptObservation {
    Prompted { addr: SocketAddr },
    Returned,
    Failed { message: String },
}

/// FIFO-scripted pairing prompt for handshake scenarios.
#[derive(Clone, Default)]
pub struct ScriptedPairingPrompt {
    actions: Arc<Mutex<VecDeque<std::result::Result<String, String>>>>,
    observations: ObservationLog<PairingPromptObservation>,
}

impl ScriptedPairingPrompt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_code(&self, code: impl Into<String>) {
        lock_recover(&self.actions).push_back(Ok(code.into()));
    }

    pub fn fail_next(&self, message: impl Into<String>) {
        lock_recover(&self.actions).push_back(Err(message.into()));
    }

    pub fn remaining_actions(&self) -> usize {
        lock_recover(&self.actions).len()
    }

    pub fn observations(&self) -> ObservationLog<PairingPromptObservation> {
        self.observations.clone()
    }
}

impl PairingPrompt for ScriptedPairingPrompt {
    fn prompt(&self, addr: SocketAddr) -> PairingPromptFuture<'_> {
        Box::pin(async move {
            self.observations
                .record(PairingPromptObservation::Prompted { addr });
            match lock_recover(&self.actions).pop_front() {
                Some(Ok(code)) => {
                    self.observations.record(PairingPromptObservation::Returned);
                    Ok(code)
                }
                Some(Err(message)) => {
                    self.observations.record(PairingPromptObservation::Failed {
                        message: message.clone(),
                    });
                    Err(eyre!(message))
                }
                None => {
                    let message = "unexpected pairing prompt: no scripted action".to_string();
                    self.observations.record(PairingPromptObservation::Failed {
                        message: message.clone(),
                    });
                    Err(eyre!(message))
                }
            }
        })
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

    #[tokio::test]
    async fn prompt_actions_are_fifo_and_unconfigured_calls_fail() {
        let prompt = ScriptedPairingPrompt::new();
        let addr: SocketAddr = "127.0.0.1:4242".parse().unwrap();
        prompt.push_code("123456");
        prompt.fail_next("terminal closed");

        assert_eq!(prompt.prompt(addr).await.unwrap(), "123456");
        assert!(prompt
            .prompt(addr)
            .await
            .unwrap_err()
            .to_string()
            .contains("terminal closed"));
        assert!(prompt
            .prompt(addr)
            .await
            .unwrap_err()
            .to_string()
            .contains("unexpected"));
    }
}
