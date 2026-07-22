use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};

use crate::ports::{
    DiscoveredPeer, DiscoveryBrowse, DiscoveryEvent, DiscoveryFuture, PeerDiscovery,
};
use crate::testing::ObservationLog;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryObservation {
    BrowseStarted,
    EventDelivered(DiscoveryEvent),
    BrowseClosed,
    Failed(String),
    ResolveStarted(Duration),
    Resolved(SocketAddr),
}

enum BrowseAction {
    Event(DiscoveryEvent),
    Delayed(Duration, DiscoveryEvent),
    Failure(String),
    Close,
}

enum ResolveAction {
    Peer(String, SocketAddr),
    Delayed(Duration, String, SocketAddr),
    Failure(String),
}

#[derive(Default)]
struct DiscoveryScripts {
    browse_sessions: VecDeque<VecDeque<BrowseAction>>,
    current_browse: VecDeque<BrowseAction>,
    resolves: VecDeque<ResolveAction>,
}

/// Deterministic discovery fake with FIFO browse sessions and resolutions.
#[derive(Clone, Default)]
pub struct ScriptedDiscovery {
    scripts: Arc<Mutex<DiscoveryScripts>>,
    observations: ObservationLog<DiscoveryObservation>,
}

impl ScriptedDiscovery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_peer(&self, peer: DiscoveredPeer) {
        self.push_browse_action(BrowseAction::Event(DiscoveryEvent::Found(peer)));
    }

    pub fn push_delayed_peer(&self, delay: Duration, peer: DiscoveredPeer) {
        self.push_browse_action(BrowseAction::Delayed(delay, DiscoveryEvent::Found(peer)));
    }

    pub fn push_removed(&self, name: impl Into<String>) {
        self.push_browse_action(BrowseAction::Event(DiscoveryEvent::Removed(name.into())));
    }

    pub fn push_malformed_peer(&self, description: impl Into<String>) {
        self.push_browse_action(BrowseAction::Failure(format!(
            "Malformed discovered peer: {}",
            description.into()
        )));
    }

    pub fn fail_browse(&self, message: impl Into<String>) {
        self.push_browse_action(BrowseAction::Failure(message.into()));
    }

    pub fn close_browse(&self) {
        self.push_browse_action(BrowseAction::Close);
    }

    /// Finish the current browse script so the next calls use a new session.
    pub fn finish_browse_session(&self) {
        let mut scripts = lock_recover(&self.scripts);
        let session = std::mem::take(&mut scripts.current_browse);
        scripts.browse_sessions.push_back(session);
    }

    pub fn resolve_to(&self, fingerprint: impl Into<String>, addr: SocketAddr) {
        lock_recover(&self.scripts)
            .resolves
            .push_back(ResolveAction::Peer(fingerprint.into(), addr));
    }

    pub fn resolve_after(&self, delay: Duration, fingerprint: impl Into<String>, addr: SocketAddr) {
        lock_recover(&self.scripts)
            .resolves
            .push_back(ResolveAction::Delayed(delay, fingerprint.into(), addr));
    }

    pub fn fail_resolve(&self, message: impl Into<String>) {
        lock_recover(&self.scripts)
            .resolves
            .push_back(ResolveAction::Failure(message.into()));
    }

    pub fn observations(&self) -> ObservationLog<DiscoveryObservation> {
        self.observations.clone()
    }

    pub fn remaining_resolves(&self) -> usize {
        lock_recover(&self.scripts).resolves.len()
    }

    fn push_browse_action(&self, action: BrowseAction) {
        lock_recover(&self.scripts).current_browse.push_back(action);
    }
}

struct ScriptedBrowse {
    actions: VecDeque<BrowseAction>,
    observations: ObservationLog<DiscoveryObservation>,
}

impl DiscoveryBrowse for ScriptedBrowse {
    fn next_event(&mut self) -> DiscoveryFuture<'_, Result<Option<DiscoveryEvent>>> {
        Box::pin(async move {
            let action = self.actions.pop_front().ok_or_else(|| {
                eyre!("ScriptedDiscovery unexpected browse read: script is empty")
            })?;
            let event = match action {
                BrowseAction::Event(event) => event,
                BrowseAction::Delayed(delay, event) => {
                    tokio::time::sleep(delay).await;
                    event
                }
                BrowseAction::Failure(message) => {
                    self.observations
                        .record(DiscoveryObservation::Failed(message.clone()));
                    return Err(eyre!(message));
                }
                BrowseAction::Close => {
                    self.observations.record(DiscoveryObservation::BrowseClosed);
                    return Ok(None);
                }
            };
            self.observations
                .record(DiscoveryObservation::EventDelivered(event.clone()));
            Ok(Some(event))
        })
    }
}

impl PeerDiscovery for ScriptedDiscovery {
    fn browse(&self) -> DiscoveryFuture<'_, Result<Box<dyn DiscoveryBrowse>>> {
        Box::pin(async move {
            let actions = {
                let mut scripts = lock_recover(&self.scripts);
                if !scripts.current_browse.is_empty() {
                    let session = std::mem::take(&mut scripts.current_browse);
                    scripts.browse_sessions.push_back(session);
                }
                scripts.browse_sessions.pop_front()
            }
            .ok_or_else(|| eyre!("ScriptedDiscovery unexpected browse: no scripted session"))?;
            self.observations
                .record(DiscoveryObservation::BrowseStarted);
            Ok(Box::new(ScriptedBrowse {
                actions,
                observations: self.observations.clone(),
            }) as Box<dyn DiscoveryBrowse>)
        })
    }

    fn resolve_one(
        &self,
        expected_fingerprint: &str,
        timeout: Duration,
    ) -> DiscoveryFuture<'_, Result<SocketAddr>> {
        let expected_fingerprint = expected_fingerprint.to_uppercase();
        Box::pin(async move {
            self.observations
                .record(DiscoveryObservation::ResolveStarted(timeout));
            let action = lock_recover(&self.scripts)
                .resolves
                .pop_front()
                .ok_or_else(|| eyre!("ScriptedDiscovery unexpected resolve: no scripted action"))?;
            let result = match action {
                ResolveAction::Peer(fingerprint, addr) => {
                    if fingerprint.to_uppercase() == expected_fingerprint {
                        Ok(addr)
                    } else {
                        Err(eyre!("no peer matched fingerprint {expected_fingerprint}"))
                    }
                }
                ResolveAction::Delayed(delay, fingerprint, addr) => {
                    tokio::time::sleep(delay).await;
                    if fingerprint.to_uppercase() == expected_fingerprint {
                        Ok(addr)
                    } else {
                        Err(eyre!("no peer matched fingerprint {expected_fingerprint}"))
                    }
                }
                ResolveAction::Failure(message) => Err(eyre!(message)),
            };
            match &result {
                Ok(addr) => self
                    .observations
                    .record(DiscoveryObservation::Resolved(*addr)),
                Err(error) => self
                    .observations
                    .record(DiscoveryObservation::Failed(error.to_string())),
            };
            result
        })
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> DiscoveredPeer {
        DiscoveredPeer {
            name: "desk".to_string(),
            platform: "linux".to_string(),
            addr: "192.0.2.1:4242".parse().unwrap(),
            fingerprint: "AA:BB".to_string(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn browse_scripts_delay_malformed_failure_and_closure() {
        let discovery = ScriptedDiscovery::new();
        discovery.push_delayed_peer(Duration::from_secs(2), peer());
        discovery.push_malformed_peer("missing address");
        discovery.close_browse();
        let mut browse = discovery.browse().await.unwrap();

        let events = tokio::spawn(async move {
            let found = browse.next_event().await.unwrap();
            let malformed = browse.next_event().await.unwrap_err().to_string();
            let closed = browse.next_event().await.unwrap();
            (found, malformed, closed)
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let (found, malformed, closed) = events.await.unwrap();
        assert!(matches!(found, Some(DiscoveryEvent::Found(_))));
        assert!(malformed.contains("Malformed discovered peer"));
        assert_eq!(closed, None);
    }

    #[tokio::test(start_paused = true)]
    async fn resolution_scripts_delays_and_failures() {
        let discovery = ScriptedDiscovery::new();
        let addr = "192.0.2.2:4242".parse().unwrap();
        discovery.resolve_after(Duration::from_secs(1), "AA:BB", addr);
        discovery.fail_resolve("resolver unavailable");

        let resolved = discovery.resolve_one("AA:BB", Duration::from_secs(5));
        tokio::pin!(resolved);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(resolved.await.unwrap(), addr);
        assert!(discovery
            .resolve_one("AA:BB", Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string()
            .contains("resolver unavailable"));
    }
}
