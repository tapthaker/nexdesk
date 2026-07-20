use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use color_eyre::eyre::{eyre, Result};

use crate::ports::LocalSessionLockSource;
use crate::testing::ObservationLog;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionLockObservation {
    StateRead(bool),
    ReadFailed(String),
}

#[derive(Debug)]
enum LockAction {
    State(bool),
    Failure(String),
}

#[derive(Debug)]
struct LockState {
    current: bool,
    actions: VecDeque<LockAction>,
}

/// Stateful local-session lock source with FIFO state changes and failures.
#[derive(Clone, Debug)]
pub struct ScriptedSessionLockSource {
    state: Arc<Mutex<LockState>>,
    observations: ObservationLog<SessionLockObservation>,
}

impl ScriptedSessionLockSource {
    pub fn new(initially_locked: bool) -> Self {
        Self::with_log(initially_locked, ObservationLog::new())
    }

    pub fn with_log(
        initially_locked: bool,
        observations: ObservationLog<SessionLockObservation>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(LockState {
                current: initially_locked,
                actions: VecDeque::new(),
            })),
            observations,
        }
    }

    pub fn push_state(&self, locked: bool) {
        lock_recover(&self.state)
            .actions
            .push_back(LockAction::State(locked));
    }

    pub fn fail_next(&self, message: impl Into<String>) {
        lock_recover(&self.state)
            .actions
            .push_back(LockAction::Failure(message.into()));
    }

    pub fn current(&self) -> bool {
        lock_recover(&self.state).current
    }

    pub fn remaining_actions(&self) -> usize {
        lock_recover(&self.state).actions.len()
    }

    pub fn observations(&self) -> ObservationLog<SessionLockObservation> {
        self.observations.clone()
    }
}

impl Default for ScriptedSessionLockSource {
    fn default() -> Self {
        Self::new(false)
    }
}

impl LocalSessionLockSource for ScriptedSessionLockSource {
    fn is_locked(&self) -> Result<bool> {
        let result = {
            let mut state = lock_recover(&self.state);
            match state.actions.pop_front() {
                Some(LockAction::State(locked)) => {
                    state.current = locked;
                    Ok(locked)
                }
                Some(LockAction::Failure(message)) => Err(message),
                None => Ok(state.current),
            }
        };
        match result {
            Ok(locked) => {
                self.observations
                    .record(SessionLockObservation::StateRead(locked));
                Ok(locked)
            }
            Err(message) => {
                self.observations
                    .record(SessionLockObservation::ReadFailed(message.clone()));
                Err(eyre!(message))
            }
        }
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

    #[test]
    fn scripted_states_persist_and_failures_are_fifo() {
        let source = ScriptedSessionLockSource::new(false);
        source.push_state(true);
        source.fail_next("query failed");

        assert!(source.is_locked().unwrap());
        assert!(source
            .is_locked()
            .unwrap_err()
            .to_string()
            .contains("query failed"));
        assert!(source.is_locked().unwrap());
        assert_eq!(source.remaining_actions(), 0);
    }
}
