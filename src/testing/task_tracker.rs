use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningTask {
    pub id: u64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningTasksError {
    pub tasks: Vec<RunningTask>,
}

impl fmt::Display for RunningTasksError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} task(s) still running", self.tasks.len())?;
        for task in &self.tasks {
            write!(formatter, ": {}#{}", task.name, task.id)?;
        }
        Ok(())
    }
}

impl std::error::Error for RunningTasksError {}

#[derive(Debug)]
struct TrackerState {
    next_id: u64,
    generation: u64,
    running: BTreeMap<u64, String>,
}

/// Tracks the lifetime of background tasks owned by a deterministic scenario.
///
/// A task remains visible until its [`TaskGuard`] is dropped. Scenario tests
/// can fail with [`TaskTracker::ensure_idle`] instead of silently leaking work
/// into later tests.
#[derive(Clone, Debug)]
pub struct TaskTracker {
    state: Arc<Mutex<TrackerState>>,
    changed: watch::Sender<u64>,
}

impl TaskTracker {
    pub fn new() -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            state: Arc::new(Mutex::new(TrackerState {
                next_id: 0,
                generation: 0,
                running: BTreeMap::new(),
            })),
            changed,
        }
    }

    /// Register a task until the returned guard is dropped.
    pub fn start(&self, name: impl Into<String>) -> TaskGuard {
        let mut state = lock_recover(&self.state);
        let id = state.next_id;
        state.next_id = state.next_id.checked_add(1).expect("task id exhausted");
        state.running.insert(id, name.into());
        publish_change(&mut state, &self.changed);
        drop(state);

        TaskGuard {
            id: Some(id),
            tracker: self.clone(),
        }
    }

    /// Run a future while tracking it as a named task.
    pub async fn run<F>(&self, name: impl Into<String>, future: F) -> F::Output
    where
        F: Future,
    {
        let _guard = self.start(name);
        future.await
    }

    pub fn running_tasks(&self) -> Vec<RunningTask> {
        lock_recover(&self.state)
            .running
            .iter()
            .map(|(&id, name)| RunningTask {
                id,
                name: name.clone(),
            })
            .collect()
    }

    pub fn is_idle(&self) -> bool {
        lock_recover(&self.state).running.is_empty()
    }

    pub fn ensure_idle(&self) -> Result<(), RunningTasksError> {
        let tasks = self.running_tasks();
        if tasks.is_empty() {
            Ok(())
        } else {
            Err(RunningTasksError { tasks })
        }
    }

    /// Wait until all currently and concurrently tracked tasks finish.
    pub async fn wait_for_idle(&self) {
        let mut changed = self.changed.subscribe();
        loop {
            if self.is_idle() {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    fn finish(&self, id: u64) {
        let mut state = lock_recover(&self.state);
        if state.running.remove(&id).is_some() {
            publish_change(&mut state, &self.changed);
        }
    }
}

impl Default for TaskTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII registration for one tracked task.
#[derive(Debug)]
pub struct TaskGuard {
    id: Option<u64>,
    tracker: TaskTracker,
}

impl TaskGuard {
    /// Mark the task complete before the guard's lexical scope ends.
    pub fn finish(mut self) {
        if let Some(id) = self.id.take() {
            self.tracker.finish(id);
        }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.tracker.finish(id);
        }
    }
}

fn publish_change(state: &mut TrackerState, changed: &watch::Sender<u64>) {
    state.generation = state
        .generation
        .checked_add(1)
        .expect("task tracker generation exhausted");
    changed.send_replace(state.generation);
}

fn lock_recover(mutex: &Mutex<TrackerState>) -> MutexGuard<'_, TrackerState> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn reports_tasks_until_their_guards_finish() {
        let tracker = TaskTracker::new();
        let first = tracker.start("clipboard-poll");
        let second = tracker.start("input-reader");

        let error = tracker.ensure_idle().unwrap_err();
        assert_eq!(
            error.tasks,
            vec![
                RunningTask {
                    id: 0,
                    name: "clipboard-poll".to_string(),
                },
                RunningTask {
                    id: 1,
                    name: "input-reader".to_string(),
                },
            ]
        );
        assert!(error.to_string().contains("clipboard-poll#0"));

        first.finish();
        assert_eq!(tracker.running_tasks().len(), 1);
        drop(second);
        tracker.ensure_idle().unwrap();
    }

    #[tokio::test]
    async fn waits_for_tracked_futures_to_finish() {
        let tracker = TaskTracker::new();
        let runner = tracker.clone();
        let (release, released) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            runner
                .run("short-task", async {
                    released.await.expect("test should release task");
                    42
                })
                .await
        });

        tokio::task::yield_now().await;
        assert!(!tracker.is_idle());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), tracker.wait_for_idle())
            .await
            .expect("tracker should observe task completion");
        assert_eq!(task.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn idle_tracker_returns_without_waiting() {
        let tracker = TaskTracker::new();
        tokio::time::timeout(Duration::from_millis(10), tracker.wait_for_idle())
            .await
            .expect("idle tracker should return immediately");
    }
}
