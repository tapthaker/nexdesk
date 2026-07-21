use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use color_eyre::eyre::Result;

pub type DiscoveryFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredPeer {
    pub name: String,
    pub platform: String,
    pub addr: SocketAddr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryEvent {
    Found(DiscoveredPeer),
    Removed(String),
}

/// One asynchronous browse session. End-of-stream is distinct from failure.
pub trait DiscoveryBrowse: Send {
    fn next_event(&mut self) -> DiscoveryFuture<'_, Result<Option<DiscoveryEvent>>>;
}

/// Peer discovery boundary supporting browse streams and bounded resolution.
pub trait PeerDiscovery: Send + Sync {
    fn browse(&self) -> DiscoveryFuture<'_, Result<Box<dyn DiscoveryBrowse>>>;

    fn resolve_one(&self, timeout: Duration) -> DiscoveryFuture<'_, Result<SocketAddr>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EmptyBrowse;

    impl DiscoveryBrowse for EmptyBrowse {
        fn next_event(&mut self) -> DiscoveryFuture<'_, Result<Option<DiscoveryEvent>>> {
            Box::pin(async { Ok(None) })
        }
    }

    struct EmptyDiscovery;

    impl PeerDiscovery for EmptyDiscovery {
        fn browse(&self) -> DiscoveryFuture<'_, Result<Box<dyn DiscoveryBrowse>>> {
            Box::pin(async { Ok(Box::new(EmptyBrowse) as Box<dyn DiscoveryBrowse>) })
        }

        fn resolve_one(&self, _timeout: Duration) -> DiscoveryFuture<'_, Result<SocketAddr>> {
            Box::pin(async { Err(color_eyre::eyre::eyre!("no peer")) })
        }
    }

    #[tokio::test]
    async fn discovery_ports_are_object_safe() {
        let discovery: &dyn PeerDiscovery = &EmptyDiscovery;
        let mut browse = discovery.browse().await.unwrap();
        assert_eq!(browse.next_event().await.unwrap(), None);
        assert!(discovery.resolve_one(Duration::from_secs(1)).await.is_err());
    }
}
