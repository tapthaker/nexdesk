use std::future::Future;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

pub const MAX_CONCURRENT_TRANSFERS: usize = 4;

/// Owns and bounds the background tasks for one connection's file transfers.
pub struct TransferTaskSet {
    permits: Arc<Semaphore>,
    tasks: JoinSet<()>,
}

impl Default for TransferTaskSet {
    fn default() -> Self {
        Self::new(MAX_CONCURRENT_TRANSFERS)
    }
}

impl TransferTaskSet {
    pub fn new(limit: usize) -> Self {
        assert!(limit > 0, "file transfer limit must be positive");
        Self {
            permits: Arc::new(Semaphore::new(limit)),
            tasks: JoinSet::new(),
        }
    }

    /// Starts a transfer if capacity is available. A rejected future is
    /// dropped without being polled.
    pub fn try_spawn<F>(&mut self, transfer: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            return false;
        };
        self.tasks.spawn(run_with_permit(permit, transfer));
        true
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub async fn join_next(&mut self) {
        self.tasks.join_next().await;
    }

    /// Aborts and joins every owned transfer task before returning.
    pub async fn shutdown(mut self) {
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }
}

async fn run_with_permit<F>(permit: OwnedSemaphorePermit, transfer: F)
where
    F: Future<Output = ()>,
{
    let _permit = permit;
    transfer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropCount(Arc<AtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn concurrent_limit_rejects_excess_and_recovers_capacity() {
        let mut tasks = TransferTaskSet::new(2);
        let first = Arc::new(tokio::sync::Notify::new());
        let second = Arc::new(tokio::sync::Notify::new());

        let first_wait = first.clone();
        assert!(tasks.try_spawn(async move { first_wait.notified().await }));
        let second_wait = second.clone();
        assert!(tasks.try_spawn(async move { second_wait.notified().await }));
        assert!(!tasks.try_spawn(async {}));

        first.notify_one();
        tasks.join_next().await;
        assert!(tasks.try_spawn(async {}));

        second.notify_one();
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_aborts_joins_and_drops_every_transfer_task() {
        let mut tasks = TransferTaskSet::new(3);
        let drops = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let guard = DropCount(drops.clone());
            assert!(tasks.try_spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            }));
        }
        tokio::task::yield_now().await;

        tasks.shutdown().await;
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }
}
