use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result};

use crate::ports::Clipboard;
use crate::testing::ObservationLog;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ClipboardOperation {
    ReadText,
    WriteText,
    ReadFiles,
    WriteFiles,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardChange {
    Text(String),
    Files(Vec<PathBuf>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardObservation {
    ReadText,
    WriteText {
        bytes: usize,
    },
    ReadFiles,
    WriteFiles {
        count: usize,
    },
    Failed {
        operation: ClipboardOperation,
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

/// Controller for one blocked clipboard operation.
pub struct BlockingClipboardCall {
    inner: Arc<GateInner>,
}

impl BlockingClipboardCall {
    pub fn wait_until_entered(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock_recover(&self.inner.state);
        while !state.entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, result) = self
                .inner
                .changed
                .wait_timeout(state, remaining)
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
        let mut state = lock_recover(&self.inner.state);
        if state.outcome.is_none() {
            state.outcome = Some(outcome);
            self.inner.changed.notify_all();
        }
    }
}

impl Drop for BlockingClipboardCall {
    fn drop(&mut self) {
        self.complete(Err("blocking clipboard call controller dropped".to_string()));
    }
}

#[derive(Default)]
struct ClipboardState {
    text: Option<String>,
    files: Vec<PathBuf>,
    failures: BTreeMap<ClipboardOperation, VecDeque<String>>,
    blocks: BTreeMap<ClipboardOperation, VecDeque<Arc<GateInner>>>,
    changes: Vec<ClipboardChange>,
}

/// Stateful in-memory clipboard with failure scripts, blocking gates, and
/// successful write history shared across clones.
#[derive(Clone)]
pub struct MemoryClipboard {
    state: Arc<Mutex<ClipboardState>>,
    observations: ObservationLog<ClipboardObservation>,
}

impl MemoryClipboard {
    pub fn new() -> Self {
        Self::with_log(ObservationLog::new())
    }

    pub fn with_log(observations: ObservationLog<ClipboardObservation>) -> Self {
        Self {
            state: Arc::new(Mutex::new(ClipboardState::default())),
            observations,
        }
    }

    pub fn set_text(&self, text: Option<String>) {
        lock_recover(&self.state).text = text;
    }

    pub fn set_files(&self, files: Vec<PathBuf>) {
        lock_recover(&self.state).files = files;
    }

    pub fn fail_next(&self, operation: ClipboardOperation, message: impl Into<String>) {
        lock_recover(&self.state)
            .failures
            .entry(operation)
            .or_default()
            .push_back(message.into());
    }

    pub fn block_next(&self, operation: ClipboardOperation) -> BlockingClipboardCall {
        let inner = Arc::new(GateInner {
            state: Mutex::new(GateState {
                entered: false,
                outcome: None,
            }),
            changed: Condvar::new(),
        });
        lock_recover(&self.state)
            .blocks
            .entry(operation)
            .or_default()
            .push_back(inner.clone());
        BlockingClipboardCall { inner }
    }

    pub fn changes(&self) -> Vec<ClipboardChange> {
        lock_recover(&self.state).changes.clone()
    }

    pub fn observations(&self) -> ObservationLog<ClipboardObservation> {
        self.observations.clone()
    }

    fn run_script(&self, operation: ClipboardOperation) -> Result<()> {
        if let Some(message) = pop_queue(&mut lock_recover(&self.state).failures, operation) {
            return Err(self.record_failure(operation, message));
        }
        let Some(gate) = pop_queue(&mut lock_recover(&self.state).blocks, operation) else {
            return Ok(());
        };
        let outcome = {
            let mut state = lock_recover(&gate.state);
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
        outcome.map_err(|message| self.record_failure(operation, message))
    }

    fn record_failure(
        &self,
        operation: ClipboardOperation,
        message: String,
    ) -> color_eyre::eyre::Report {
        self.observations.record(ClipboardObservation::Failed {
            operation,
            message: message.clone(),
        });
        eyre!(message)
    }
}

impl Default for MemoryClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for MemoryClipboard {
    fn read_text(&self) -> Result<Option<String>> {
        self.observations.record(ClipboardObservation::ReadText);
        self.run_script(ClipboardOperation::ReadText)?;
        Ok(lock_recover(&self.state).text.clone())
    }

    fn write_text(&self, text: &str) -> Result<()> {
        self.observations
            .record(ClipboardObservation::WriteText { bytes: text.len() });
        self.run_script(ClipboardOperation::WriteText)?;
        let mut state = lock_recover(&self.state);
        state.text = Some(text.to_string());
        state.changes.push(ClipboardChange::Text(text.to_string()));
        Ok(())
    }

    fn read_files(&self) -> Result<Vec<PathBuf>> {
        self.observations.record(ClipboardObservation::ReadFiles);
        self.run_script(ClipboardOperation::ReadFiles)?;
        Ok(lock_recover(&self.state).files.clone())
    }

    fn write_files(&self, paths: &[PathBuf]) -> Result<()> {
        self.observations
            .record(ClipboardObservation::WriteFiles { count: paths.len() });
        self.run_script(ClipboardOperation::WriteFiles)?;
        let mut state = lock_recover(&self.state);
        state.files = paths.to_vec();
        state.changes.push(ClipboardChange::Files(paths.to_vec()));
        Ok(())
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

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_writes_and_history_are_shared_across_clones() {
        let clipboard = MemoryClipboard::new();
        let clone = clipboard.clone();
        clipboard.set_text(Some("first".to_string()));
        assert_eq!(clone.read_text().unwrap().as_deref(), Some("first"));

        clone.write_text("second").unwrap();
        let files = vec![PathBuf::from("one.txt"), PathBuf::from("two.txt")];
        clipboard.write_files(&files).unwrap();

        assert_eq!(clipboard.read_text().unwrap().as_deref(), Some("second"));
        assert_eq!(clone.read_files().unwrap(), files);
        assert_eq!(clipboard.changes().len(), 2);
    }

    #[test]
    fn scripted_failure_is_consumed_without_mutating_content() {
        let clipboard = MemoryClipboard::new();
        clipboard.set_text(Some("old".to_string()));
        clipboard.fail_next(ClipboardOperation::WriteText, "clipboard busy");

        assert_eq!(
            clipboard.write_text("new").unwrap_err().to_string(),
            "clipboard busy"
        );
        assert_eq!(clipboard.read_text().unwrap().as_deref(), Some("old"));
        clipboard.write_text("new").unwrap();
        assert_eq!(clipboard.read_text().unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn blocked_read_waits_for_explicit_release() {
        let clipboard = MemoryClipboard::new();
        clipboard.set_text(Some("ready".to_string()));
        let gate = clipboard.block_next(ClipboardOperation::ReadText);
        let worker = clipboard.clone();
        let thread = std::thread::spawn(move || worker.read_text());

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        assert!(!thread.is_finished());
        gate.release();
        assert_eq!(thread.join().unwrap().unwrap().as_deref(), Some("ready"));
    }
}
