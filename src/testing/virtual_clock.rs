use std::time::Duration;

/// Facade over Tokio's paused clock used by deterministic rigs.
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtualClock;

impl VirtualClock {
    pub fn now(&self) -> tokio::time::Instant {
        tokio::time::Instant::now()
    }

    pub async fn advance(&self, duration: Duration) {
        tokio::time::advance(duration).await;
    }
}
