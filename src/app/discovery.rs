use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};

use crate::app::CancellationToken;
use crate::ports::{DiscoveredPeer, DiscoveryEvent, PeerDiscovery};

pub async fn resolve_peer_with_retry(
    discovery: &dyn PeerDiscovery,
    timeout: Duration,
    attempts: u32,
    cancellation: &CancellationToken,
) -> Result<SocketAddr> {
    if attempts == 0 {
        return Err(eyre!("Discovery requires at least one attempt"));
    }
    let per_attempt = timeout
        .checked_div(attempts)
        .filter(|duration| !duration.is_zero())
        .unwrap_or(timeout);
    let mut last_error = None;

    for _ in 0..attempts {
        let result = tokio::select! {
            _ = cancellation.cancelled() => return Err(eyre!("Discovery cancelled")),
            result = tokio::time::timeout(per_attempt, discovery.resolve_one(per_attempt)) => result,
        };
        match result {
            Ok(Ok(addr)) => return Ok(addr),
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => {
                last_error = Some(eyre!("Discovery attempt timed out after {:?}", per_attempt))
            }
        }
    }

    Err(last_error.unwrap_or_else(|| eyre!("Discovery failed")))
}

#[derive(Debug, Default)]
pub struct DiscoveredPeerSet {
    peers: BTreeMap<SocketAddr, DiscoveredPeer>,
}

impl DiscoveredPeerSet {
    pub fn apply(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::Found(peer) => {
                self.peers.insert(peer.addr, peer);
            }
            DiscoveryEvent::Removed(name) => {
                self.peers.retain(|_, peer| peer.name != name);
            }
        }
    }

    pub fn peers(&self) -> Vec<DiscoveredPeer> {
        self.peers.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScriptedDiscovery;

    #[tokio::test(start_paused = true)]
    async fn resolution_retries_failures_then_accepts_delayed_peer() {
        let discovery = ScriptedDiscovery::new();
        let addr = "192.0.2.10:4242".parse().unwrap();
        discovery.fail_resolve("browse failed");
        discovery.fail_resolve("channel closed");
        discovery.resolve_after(Duration::from_secs(1), addr);
        let cancellation = CancellationToken::new();

        let resolution =
            resolve_peer_with_retry(&discovery, Duration::from_secs(6), 3, &cancellation);
        tokio::pin!(resolution);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;

        assert_eq!(resolution.await.unwrap(), addr);
        assert_eq!(discovery.remaining_resolves(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn resolution_timeout_is_enforced_with_virtual_time() {
        let discovery = ScriptedDiscovery::new();
        discovery.resolve_after(Duration::from_secs(30), "192.0.2.10:4242".parse().unwrap());
        let cancellation = CancellationToken::new();
        let resolution =
            resolve_peer_with_retry(&discovery, Duration::from_secs(3), 1, &cancellation);
        tokio::pin!(resolution);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;

        assert!(resolution
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
    }

    #[tokio::test]
    async fn resolution_is_cancellable() {
        let discovery = ScriptedDiscovery::new();
        discovery.resolve_after(Duration::from_secs(30), "192.0.2.10:4242".parse().unwrap());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(
            resolve_peer_with_retry(&discovery, Duration::from_secs(30), 1, &cancellation,)
                .await
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
    }

    #[test]
    fn peer_set_deduplicates_addresses_and_removes_names() {
        let addr = "192.0.2.10:4242".parse().unwrap();
        let mut peers = DiscoveredPeerSet::default();
        for platform in ["linux", "macos"] {
            peers.apply(DiscoveryEvent::Found(DiscoveredPeer {
                name: "desk".to_string(),
                platform: platform.to_string(),
                addr,
                fingerprint: "AA:BB".to_string(),
            }));
        }
        assert_eq!(peers.peers().len(), 1);
        assert_eq!(peers.peers()[0].platform, "macos");

        peers.apply(DiscoveryEvent::Removed("desk".to_string()));
        assert!(peers.peers().is_empty());
    }
}
