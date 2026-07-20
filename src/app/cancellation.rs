use tokio::sync::watch;

/// Cloneable cancellation signal for long-running application operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: watch::Sender<bool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        let (cancelled, _) = watch::channel(false);
        Self { cancelled }
    }

    pub fn cancel(&self) {
        self.cancelled.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.cancelled.subscribe();
        if *receiver.borrow_and_update() {
            return;
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow_and_update() {
                return;
            }
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn cancellation_is_shared_and_sticky() {
        let token = CancellationToken::new();
        let observer = token.clone();
        token.cancel();

        tokio::time::timeout(Duration::from_millis(10), observer.cancelled())
            .await
            .expect("cancelled token should resolve immediately");
        assert!(observer.is_cancelled());
    }
}
