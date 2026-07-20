use std::sync::{Arc, Mutex, MutexGuard};

use color_eyre::eyre::Result;

use crate::ports::StatusSink;
use crate::status::RuntimeStatus;

#[derive(Clone, Default)]
pub struct RecordingStatusSink {
    history: Arc<Mutex<Vec<RuntimeStatus>>>,
}

impl RecordingStatusSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn history(&self) -> Vec<RuntimeStatus> {
        lock_recover(&self.history).clone()
    }

    pub fn states(&self) -> Vec<String> {
        self.history()
            .into_iter()
            .map(|status| status.state)
            .collect()
    }
}

impl StatusSink for RecordingStatusSink {
    fn write(&self, status: RuntimeStatus) -> Result<()> {
        lock_recover(&self.history).push(status);
        Ok(())
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
