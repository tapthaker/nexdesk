use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result};

use crate::ports::{DisplaySessionControl, SleepInhibitor};
use crate::testing::ObservationLog;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DisplayOperation {
    InhibitIdleSleep,
    WakeDisplay,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplayObservation {
    InhibitRequested,
    InhibitorAcquired,
    InhibitorReleased,
    WakeRequested,
    WakeCompleted,
    Failed {
        operation: DisplayOperation,
        message: String,
    },
}

#[derive(Debug)]
struct GateState {
    entered: bool,
    outcome: Option<std::result::Result<(), String>>,
}

#[derive(Debug)]
struct GateInner {
    state: Mutex<GateState>,
    changed: Condvar,
}

/// Controller for one scripted blocking platform call.
#[derive(Debug)]
pub struct BlockingDisplayCall {
    inner: Arc<GateInner>,
}

impl BlockingDisplayCall {
    pub fn wait_until_entered(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock_gate(&self.inner.state);
        while !state.entered {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let wait = deadline.saturating_duration_since(now);
            let (next, result) = self
                .inner
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if result.timed_out() && !state.entered {
                return false;
            }
        }
        true
    }

    pub fn release(&self) {
        self.complete(Ok(()));
    }

    pub fn fail(&self, message: impl Into<String>) {
        self.complete(Err(message.into()));
    }

    fn complete(&self, outcome: std::result::Result<(), String>) {
        let mut state = lock_gate(&self.inner.state);
        if state.outcome.is_none() {
            state.outcome = Some(outcome);
            self.inner.changed.notify_all();
        }
    }
}

impl Drop for BlockingDisplayCall {
    fn drop(&mut self) {
        self.complete(Err("blocking display call controller dropped".to_string()));
    }
}

#[derive(Debug)]
struct DisplayState {
    failures: BTreeMap<DisplayOperation, VecDeque<String>>,
    blocks: BTreeMap<DisplayOperation, VecDeque<Arc<GateInner>>>,
    active_inhibitors: usize,
}

/// Stateful fake for display wake and idle-sleep control.
#[derive(Clone, Debug)]
pub struct FakeDisplaySessionControl {
    state: Arc<Mutex<DisplayState>>,
    observations: ObservationLog<DisplayObservation>,
}

impl FakeDisplaySessionControl {
    pub fn new() -> Self {
        Self::with_log(ObservationLog::new())
    }

    pub fn with_log(observations: ObservationLog<DisplayObservation>) -> Self {
        Self {
            state: Arc::new(Mutex::new(DisplayState {
                failures: BTreeMap::new(),
                blocks: BTreeMap::new(),
                active_inhibitors: 0,
            })),
            observations,
        }
    }

    pub fn fail_next(&self, operation: DisplayOperation, message: impl Into<String>) {
        lock_state(&self.state)
            .failures
            .entry(operation)
            .or_default()
            .push_back(message.into());
    }

    pub fn block_next(&self, operation: DisplayOperation) -> BlockingDisplayCall {
        let inner = Arc::new(GateInner {
            state: Mutex::new(GateState {
                entered: false,
                outcome: None,
            }),
            changed: Condvar::new(),
        });
        lock_state(&self.state)
            .blocks
            .entry(operation)
            .or_default()
            .push_back(inner.clone());
        BlockingDisplayCall { inner }
    }

    pub fn active_inhibitors(&self) -> usize {
        lock_state(&self.state).active_inhibitors
    }

    pub fn observations(&self) -> ObservationLog<DisplayObservation> {
        self.observations.clone()
    }

    fn run_scripts(&self, operation: DisplayOperation) -> Result<()> {
        if let Some(message) = pop_queue(&mut lock_state(&self.state).failures, operation) {
            self.observations.record(DisplayObservation::Failed {
                operation,
                message: message.clone(),
            });
            return Err(eyre!(message));
        }

        let gate = pop_queue(&mut lock_state(&self.state).blocks, operation);
        let Some(gate) = gate else {
            return Ok(());
        };
        let outcome = {
            let mut state = lock_gate(&gate.state);
            state.entered = true;
            gate.changed.notify_all();
            while state.outcome.is_none() {
                state = gate
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            state.outcome.clone().expect("gate outcome was checked")
        };
        outcome.map_err(|message| {
            self.observations.record(DisplayObservation::Failed {
                operation,
                message: message.clone(),
            });
            eyre!(message)
        })
    }
}

impl Default for FakeDisplaySessionControl {
    fn default() -> Self {
        Self::new()
    }
}

impl DisplaySessionControl for FakeDisplaySessionControl {
    fn inhibit_idle_sleep(&self) -> Result<Box<dyn SleepInhibitor>> {
        self.observations
            .record(DisplayObservation::InhibitRequested);
        self.run_scripts(DisplayOperation::InhibitIdleSleep)?;
        lock_state(&self.state).active_inhibitors += 1;
        self.observations
            .record(DisplayObservation::InhibitorAcquired);
        Ok(Box::new(FakeSleepGuard {
            state: self.state.clone(),
            observations: self.observations.clone(),
        }))
    }

    fn wake_display(&self) -> Result<()> {
        self.observations.record(DisplayObservation::WakeRequested);
        self.run_scripts(DisplayOperation::WakeDisplay)?;
        self.observations.record(DisplayObservation::WakeCompleted);
        Ok(())
    }
}

struct FakeSleepGuard {
    state: Arc<Mutex<DisplayState>>,
    observations: ObservationLog<DisplayObservation>,
}

impl Drop for FakeSleepGuard {
    fn drop(&mut self) {
        let mut state = lock_state(&self.state);
        state.active_inhibitors = state.active_inhibitors.saturating_sub(1);
        drop(state);
        self.observations
            .record(DisplayObservation::InhibitorReleased);
    }
}

fn pop_queue<K: Ord + Copy, V>(queues: &mut BTreeMap<K, VecDeque<V>>, key: K) -> Option<V> {
    let queue = queues.get_mut(&key)?;
    let value = queue.pop_front();
    if queue.is_empty() {
        queues.remove(&key);
    }
    value
}

fn lock_state(mutex: &Mutex<DisplayState>) -> MutexGuard<'_, DisplayState> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_gate(mutex: &Mutex<GateState>) -> MutexGuard<'_, GateState> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inhibitor_guard_records_its_full_lifetime() {
        let control = FakeDisplaySessionControl::new();
        let guard = control.inhibit_idle_sleep().unwrap();
        assert_eq!(control.active_inhibitors(), 1);
        drop(guard);
        assert_eq!(control.active_inhibitors(), 0);
        assert_eq!(
            control
                .observations()
                .snapshot()
                .into_iter()
                .map(|entry| entry.event)
                .collect::<Vec<_>>(),
            vec![
                DisplayObservation::InhibitRequested,
                DisplayObservation::InhibitorAcquired,
                DisplayObservation::InhibitorReleased,
            ]
        );
    }

    #[test]
    fn wake_failures_are_scripted_and_consumed() {
        let control = FakeDisplaySessionControl::new();
        control.fail_next(DisplayOperation::WakeDisplay, "display unavailable");
        assert_eq!(
            control.wake_display().unwrap_err().to_string(),
            "display unavailable"
        );
        control.wake_display().unwrap();
    }

    #[test]
    fn blocked_wake_waits_for_explicit_release() {
        let control = FakeDisplaySessionControl::new();
        let gate = control.block_next(DisplayOperation::WakeDisplay);
        let worker = control.clone();
        let thread = std::thread::spawn(move || worker.wake_display());

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        assert!(!thread.is_finished());
        gate.release();
        thread.join().unwrap().unwrap();
    }

    #[test]
    fn dropping_block_controller_unblocks_with_error() {
        let control = FakeDisplaySessionControl::new();
        let gate = control.block_next(DisplayOperation::WakeDisplay);
        let worker = control.clone();
        let thread = std::thread::spawn(move || worker.wake_display());

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        drop(gate);
        let error = thread.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("controller dropped"));
    }
}
