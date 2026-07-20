use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use color_eyre::eyre::{eyre, Result};

use crate::ports::TrustStore;
use crate::testing::ObservationLog;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustOperation {
    Read,
    Write,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustObservation {
    Read {
        fingerprint: String,
    },
    ReadResult {
        fingerprint: String,
        trusted: bool,
    },
    Write {
        fingerprint: String,
    },
    Failed {
        operation: TrustOperation,
        message: String,
    },
}

#[derive(Debug, Default)]
struct TrustState {
    trusted: BTreeSet<String>,
    read_failures: VecDeque<String>,
    write_failures: VecDeque<String>,
}

/// In-memory trust store with observable reads/writes and scripted failures.
#[derive(Clone, Debug)]
pub struct MemoryTrustStore {
    state: Arc<Mutex<TrustState>>,
    observations: ObservationLog<TrustObservation>,
}

impl MemoryTrustStore {
    pub fn new() -> Self {
        Self::with_log(ObservationLog::new())
    }

    pub fn with_log(observations: ObservationLog<TrustObservation>) -> Self {
        Self {
            state: Arc::new(Mutex::new(TrustState::default())),
            observations,
        }
    }

    pub fn with_trusted(fingerprints: impl IntoIterator<Item = String>) -> Self {
        let store = Self::new();
        lock_recover(&store.state).trusted.extend(fingerprints);
        store
    }

    pub fn fail_next_read(&self, message: impl Into<String>) {
        lock_recover(&self.state)
            .read_failures
            .push_back(message.into());
    }

    pub fn fail_next_write(&self, message: impl Into<String>) {
        lock_recover(&self.state)
            .write_failures
            .push_back(message.into());
    }

    pub fn trusted_fingerprints(&self) -> BTreeSet<String> {
        lock_recover(&self.state).trusted.clone()
    }

    pub fn observations(&self) -> ObservationLog<TrustObservation> {
        self.observations.clone()
    }

    fn take_failure(&self, operation: TrustOperation) -> Option<String> {
        let mut state = lock_recover(&self.state);
        match operation {
            TrustOperation::Read => state.read_failures.pop_front(),
            TrustOperation::Write => state.write_failures.pop_front(),
        }
    }

    fn fail_if_scripted(&self, operation: TrustOperation) -> Result<()> {
        let Some(message) = self.take_failure(operation) else {
            return Ok(());
        };
        self.observations.record(TrustObservation::Failed {
            operation,
            message: message.clone(),
        });
        Err(eyre!(message))
    }
}

impl Default for MemoryTrustStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TrustStore for MemoryTrustStore {
    fn is_trusted(&self, fingerprint: &str) -> Result<bool> {
        self.observations.record(TrustObservation::Read {
            fingerprint: fingerprint.to_string(),
        });
        self.fail_if_scripted(TrustOperation::Read)?;
        let trusted = lock_recover(&self.state).trusted.contains(fingerprint);
        self.observations.record(TrustObservation::ReadResult {
            fingerprint: fingerprint.to_string(),
            trusted,
        });
        Ok(trusted)
    }

    fn trust(&self, fingerprint: &str) -> Result<()> {
        self.observations.record(TrustObservation::Write {
            fingerprint: fingerprint.to_string(),
        });
        self.fail_if_scripted(TrustOperation::Write)?;
        lock_recover(&self.state)
            .trusted
            .insert(fingerprint.to_string());
        Ok(())
    }
}

fn lock_recover(mutex: &Mutex<TrustState>) -> MutexGuard<'_, TrustState> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINGERPRINT: &str =
        "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF";

    #[test]
    fn writes_are_idempotent_and_visible_to_clones() {
        let store = MemoryTrustStore::new();
        let second_handle = store.clone();

        store.trust(FINGERPRINT).unwrap();
        store.trust(FINGERPRINT).unwrap();

        assert!(second_handle.is_trusted(FINGERPRINT).unwrap());
        assert_eq!(store.trusted_fingerprints().len(), 1);
    }

    #[test]
    fn read_failure_is_consumed_without_changing_state() {
        let store = MemoryTrustStore::with_trusted([FINGERPRINT.to_string()]);
        store.fail_next_read("config unavailable");

        assert_eq!(
            store.is_trusted(FINGERPRINT).unwrap_err().to_string(),
            "config unavailable"
        );
        assert!(store.is_trusted(FINGERPRINT).unwrap());
    }

    #[test]
    fn write_failure_is_consumed_without_persisting_trust() {
        let store = MemoryTrustStore::new();
        store.fail_next_write("disk full");

        assert_eq!(
            store.trust(FINGERPRINT).unwrap_err().to_string(),
            "disk full"
        );
        assert!(!store.is_trusted(FINGERPRINT).unwrap());
        store.trust(FINGERPRINT).unwrap();
        assert!(store.is_trusted(FINGERPRINT).unwrap());
    }

    #[test]
    fn records_typed_read_and_write_observations() {
        let store = MemoryTrustStore::new();
        store.trust(FINGERPRINT).unwrap();
        store.is_trusted(FINGERPRINT).unwrap();

        let observations = store.observations().snapshot();
        assert!(matches!(
            observations[0].event,
            TrustObservation::Write { .. }
        ));
        assert!(matches!(
            observations[1].event,
            TrustObservation::Read { .. }
        ));
        assert!(matches!(
            observations[2].event,
            TrustObservation::ReadResult { trusted: true, .. }
        ));
    }
}
