use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};

use crate::app::CancellationToken;
use crate::ports::{DiscoveredPeer, DiscoveryEvent, PeerDiscovery};

pub async fn resolve_peer_with_retry(
    discovery: &dyn PeerDiscovery,
    expected_fingerprint: &str,
    attempt_timeout: Duration,
    attempts: u32,
    cancellation: &CancellationToken,
) -> Result<SocketAddr> {
    if attempts == 0 {
        return Err(eyre!("Discovery requires at least one attempt"));
    }
    let mut last_error = None;

    for _ in 0..attempts {
        let result = tokio::select! {
            _ = cancellation.cancelled() => return Err(eyre!("Discovery cancelled")),
            result = tokio::time::timeout(
                attempt_timeout,
                discovery.resolve_one(expected_fingerprint, attempt_timeout),
            ) => result,
        };
        match result {
            Ok(Ok(addr)) => return Ok(addr),
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => {
                last_error = Some(eyre!(
                    "Discovery attempt timed out after {:?}",
                    attempt_timeout
                ))
            }
        }
    }

    Err(last_error.unwrap_or_else(|| eyre!("Discovery failed")))
}

#[derive(Debug, Default)]
pub struct DiscoveredPeerSet {
    peers: BTreeMap<String, DiscoveredPeer>,
}

impl DiscoveredPeerSet {
    pub fn apply(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::Found(peer) => {
                self.peers.insert(peer.fingerprint.clone(), peer);
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
        discovery.resolve_to("CC:DD", "192.0.2.99:4242".parse().unwrap());
        discovery.fail_resolve("browse channel refreshed");
        discovery.resolve_after(Duration::from_secs(1), "aa:bb", addr);
        let cancellation = CancellationToken::new();

        let resolution = resolve_peer_with_retry(
            &discovery,
            "AA:BB",
            Duration::from_secs(6),
            3,
            &cancellation,
        );
        tokio::pin!(resolution);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;

        assert_eq!(resolution.await.unwrap(), addr);
        assert_eq!(discovery.remaining_resolves(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_applies_to_each_discovery_attempt() {
        let discovery = ScriptedDiscovery::new();
        let addr = "192.0.2.10:4242".parse().unwrap();
        discovery.resolve_after(Duration::from_secs(4), "AA:BB", addr);
        let cancellation = CancellationToken::new();

        let resolution = resolve_peer_with_retry(
            &discovery,
            "AA:BB",
            Duration::from_secs(6),
            3,
            &cancellation,
        );
        tokio::pin!(resolution);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(4)).await;

        assert_eq!(resolution.await.unwrap(), addr);
        assert_eq!(discovery.remaining_resolves(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn resolution_timeout_is_enforced_with_virtual_time() {
        let discovery = ScriptedDiscovery::new();
        discovery.resolve_after(
            Duration::from_secs(30),
            "AA:BB",
            "192.0.2.10:4242".parse().unwrap(),
        );
        let cancellation = CancellationToken::new();
        let resolution = resolve_peer_with_retry(
            &discovery,
            "AA:BB",
            Duration::from_secs(3),
            1,
            &cancellation,
        );
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
        discovery.resolve_after(
            Duration::from_secs(30),
            "AA:BB",
            "192.0.2.10:4242".parse().unwrap(),
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(resolve_peer_with_retry(
            &discovery,
            "AA:BB",
            Duration::from_secs(30),
            1,
            &cancellation,
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("cancelled"));
    }

    #[test]
    fn peer_set_tracks_identity_across_address_changes_and_removes_names() {
        let mut peers = DiscoveredPeerSet::default();
        for addr in ["192.0.2.10:4242", "192.0.2.20:4242"] {
            peers.apply(DiscoveryEvent::Found(DiscoveredPeer {
                name: "desk".to_string(),
                platform: "linux".to_string(),
                addr: addr.parse().unwrap(),
                fingerprint: "AA:BB".to_string(),
            }));
        }
        assert_eq!(peers.peers().len(), 1);
        assert_eq!(peers.peers()[0].addr, "192.0.2.20:4242".parse().unwrap());

        peers.apply(DiscoveryEvent::Removed("desk".to_string()));
        assert!(peers.peers().is_empty());
    }
}
