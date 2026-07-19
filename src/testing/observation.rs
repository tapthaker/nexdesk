use std::sync::{Arc, Mutex, MutexGuard};

/// A recorded test-harness event with a stable order within its shared log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation<E> {
    pub sequence: u64,
    pub event: E,
}

#[derive(Debug)]
struct LogState<E> {
    next_sequence: u64,
    entries: Vec<Observation<E>>,
}

/// A cloneable, thread-safe event log shared by test fakes.
///
/// Fakes using the same log preserve one total observation order, allowing
/// scenario tests to assert cross-boundary behavior such as releasing a local
/// input grab before attempting a network send.
#[derive(Debug)]
pub struct ObservationLog<E> {
    state: Arc<Mutex<LogState<E>>>,
}

impl<E> ObservationLog<E> {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(LogState {
                next_sequence: 0,
                entries: Vec::new(),
            })),
        }
    }

    /// Record an event and return its monotonically increasing sequence.
    pub fn record(&self, event: E) -> u64 {
        let mut state = lock_recover(&self.state);
        let sequence = state.next_sequence;
        state.next_sequence = state
            .next_sequence
            .checked_add(1)
            .expect("observation sequence exhausted");
        state.entries.push(Observation { sequence, event });
        sequence
    }

    pub fn len(&self) -> usize {
        lock_recover(&self.state).entries.len()
    }

    pub fn is_empty(&self) -> bool {
        lock_recover(&self.state).entries.is_empty()
    }

    /// Remove all entries while preserving sequence monotonicity for future events.
    pub fn drain(&self) -> Vec<Observation<E>> {
        std::mem::take(&mut lock_recover(&self.state).entries)
    }

    pub fn clear(&self) {
        lock_recover(&self.state).entries.clear();
    }
}

impl<E: Clone> ObservationLog<E> {
    /// Return a non-destructive snapshot of all currently recorded entries.
    pub fn snapshot(&self) -> Vec<Observation<E>> {
        lock_recover(&self.state).entries.clone()
    }
}

impl<E> Clone for ObservationLog<E> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<E> Default for ObservationLog<E> {
    fn default() -> Self {
        Self::new()
    }
}

fn lock_recover<E>(mutex: &Mutex<LogState<E>>) -> MutexGuard<'_, LogState<E>> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Event {
        GrabReleased,
        SendAttempted,
        Finished,
    }

    #[test]
    fn clones_share_one_ordered_log() {
        let log = ObservationLog::new();
        let second_fake = log.clone();

        assert_eq!(log.record(Event::GrabReleased), 0);
        assert_eq!(second_fake.record(Event::SendAttempted), 1);

        assert_eq!(
            log.snapshot(),
            vec![
                Observation {
                    sequence: 0,
                    event: Event::GrabReleased,
                },
                Observation {
                    sequence: 1,
                    event: Event::SendAttempted,
                },
            ]
        );
    }

    #[test]
    fn drain_preserves_sequence_monotonicity() {
        let log = ObservationLog::new();
        log.record(Event::GrabReleased);

        assert_eq!(log.drain().len(), 1);
        assert!(log.is_empty());
        assert_eq!(log.record(Event::Finished), 1);
    }

    #[test]
    fn clear_removes_entries_without_resetting_sequence() {
        let log = ObservationLog::new();
        log.record(Event::GrabReleased);
        log.clear();

        assert_eq!(log.len(), 0);
        assert_eq!(log.record(Event::Finished), 1);
    }
}
