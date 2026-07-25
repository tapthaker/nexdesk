use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info, warn};

pub use crate::ports::DiscoveredPeer;
use crate::ports::{DiscoveryBrowse, DiscoveryEvent, DiscoveryFuture, PeerDiscovery};

const SERVICE_TYPE: &str = "_nexdesk._udp.local.";
const DISCOVERY_TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Get the primary local IPv4 address using the routing table (no packets sent).
fn local_ipv4() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("1.1.1.1:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) => Some(v4.to_string()),
        _ => None,
    }
}

fn preferred_addr(addresses: impl IntoIterator<Item = IpAddr>, port: u16) -> Option<SocketAddr> {
    let addresses = addresses.into_iter().collect::<Vec<_>>();
    let selected = addresses
        .iter()
        .copied()
        .find(|addr| matches!(addr, IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local()))
        .or_else(|| {
            addresses.iter().copied().find(|addr| {
                matches!(addr, IpAddr::V6(v6) if !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local())
            })
        })
        .or_else(|| addresses.first().copied())?;
    Some(SocketAddr::new(selected, port))
}

fn preferred_service_addr(info: &ServiceInfo) -> Option<SocketAddr> {
    preferred_addr(info.get_addresses().iter().copied(), info.get_port())
}

fn local_certificate_fingerprint() -> Result<String> {
    let (certificate, _) = crate::net::tls::load_or_generate_certs()?;
    Ok(crate::net::tls::fingerprint(&certificate))
}

fn discovered_peer(info: &ServiceInfo) -> Option<DiscoveredPeer> {
    Some(DiscoveredPeer {
        name: info
            .get_property_val_str("hostname")
            .unwrap_or("unknown")
            .to_string(),
        platform: info
            .get_property_val_str("platform")
            .unwrap_or("unknown")
            .to_string(),
        addr: preferred_service_addr(info)?,
        fingerprint: info.get_property_val_str("fingerprint")?.to_uppercase(),
    })
}

/// Advertise this machine on the local network via mDNS.
pub async fn advertise(port: u16) -> Result<()> {
    let mdns = ServiceDaemon::new()?;

    let hostname = gethostname::gethostname().to_string_lossy().into_owned();

    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    };

    let instance_name = hostname.to_string();
    let ip = local_ipv4().unwrap_or_default();
    let fingerprint = local_certificate_fingerprint()?;
    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &instance_name,
        &format!("{hostname}.local."),
        &ip,
        port,
        [
            ("hostname", hostname.as_str()),
            ("platform", platform),
            ("version", crate::net::protocol::BUILD_VERSION),
            ("fingerprint", fingerprint.as_str()),
        ]
        .as_slice(),
    )?;

    mdns.register(service)?;

    info!(
        "Advertising as '{}' on port {} (platform: {}, ip: {})",
        hostname, port, platform, ip
    );
    info!("Press Ctrl+C to stop");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;

    info!("Shutting down mDNS advertisement...");
    mdns.shutdown()?;

    Ok(())
}

/// Discover peers on the local network via mDNS.
pub async fn discover() -> Result<()> {
    let mut browse = MdnsDiscovery.browse().await?;
    info!("Browsing for nexdesk peers on the network...");
    info!("Press Ctrl+C to stop\n");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("\nShutting down discovery...");
                break;
            }
            event = browse.next_event() => match event? {
                Some(DiscoveryEvent::Found(peer)) => println!(
                    "  Found peer: {} ({})\n    Selected: {}\n    Fingerprint: {}\n",
                    peer.name,
                    peer.platform,
                    peer.addr,
                    peer.fingerprint,
                ),
                Some(DiscoveryEvent::Removed(name)) => println!("  Peer left: {}\n", name),
                None => break,
            }
        }
    }
    Ok(())
}

/// Handle to a running mDNS advertisement. Shuts down on drop.
pub struct AdvertiseHandle {
    mdns: Option<ServiceDaemon>,
}

impl Drop for AdvertiseHandle {
    fn drop(&mut self) {
        self.mdns.take().map(|m| m.shutdown().ok());
    }
}

/// Start advertising this machine on the local network via mDNS (non-blocking).
/// Returns a handle that stops advertising when dropped.
pub fn start_advertising(port: u16) -> Result<AdvertiseHandle> {
    let mdns = ServiceDaemon::new()?;

    let hostname = gethostname::gethostname().to_string_lossy().into_owned();

    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    };

    let instance_name = hostname.to_string();
    let ip = local_ipv4().unwrap_or_default();
    let fingerprint = local_certificate_fingerprint()?;
    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &instance_name,
        &format!("{hostname}.local."),
        &ip,
        port,
        [
            ("hostname", hostname.as_str()),
            ("platform", platform),
            ("version", crate::net::protocol::BUILD_VERSION),
            ("fingerprint", fingerprint.as_str()),
        ]
        .as_slice(),
    )?;

    mdns.register(service)?;

    info!(
        "mDNS: advertising as '{}' on port {} (ip: {})",
        hostname, port, ip
    );

    Ok(AdvertiseHandle { mdns: Some(mdns) })
}

/// Handle to a running mDNS browse. Shuts down on drop.
pub struct BrowseHandle {
    mdns: Option<ServiceDaemon>,
}

impl Drop for BrowseHandle {
    fn drop(&mut self) {
        if let Some(mdns) = self.mdns.take() {
            if let Ok(shutdown) = mdns.shutdown() {
                // Ensure the daemon releases its cache and multicast sockets before
                // setup starts a replacement browse session.
                shutdown.recv_timeout(Duration::from_millis(250)).ok();
            }
        }
    }
}

/// Start browsing for nexdesk peers on the network (non-blocking).
/// Returns a receiver for discovered peers and a handle that stops browsing on drop.
pub fn start_browsing() -> Result<(std::sync::mpsc::Receiver<DiscoveredPeer>, BrowseHandle)> {
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(SERVICE_TYPE)?;
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || loop {
        match receiver.recv() {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(peer) = discovered_peer(&info) {
                    let _ = tx.send(peer);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    Ok((rx, BrowseHandle { mdns: Some(mdns) }))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MdnsDiscovery;

struct MdnsBrowse {
    _mdns: BrowseHandle,
    events: tokio::sync::mpsc::UnboundedReceiver<Result<DiscoveryEvent>>,
}

impl DiscoveryBrowse for MdnsBrowse {
    fn next_event(&mut self) -> DiscoveryFuture<'_, Result<Option<DiscoveryEvent>>> {
        Box::pin(async move {
            match self.events.recv().await {
                Some(Ok(event)) => Ok(Some(event)),
                Some(Err(error)) => Err(error),
                None => Ok(None),
            }
        })
    }
}

impl PeerDiscovery for MdnsDiscovery {
    fn browse(&self) -> DiscoveryFuture<'_, Result<Box<dyn DiscoveryBrowse>>> {
        Box::pin(async move {
            let mdns = ServiceDaemon::new()?;
            let receiver = mdns.browse(SERVICE_TYPE)?;
            let (send, events) = tokio::sync::mpsc::unbounded_channel();
            std::thread::spawn(move || loop {
                let event = match receiver.recv() {
                    Ok(ServiceEvent::ServiceResolved(info)) => {
                        let Some(peer) = discovered_peer(&info) else {
                            continue;
                        };
                        Ok(DiscoveryEvent::Found(peer))
                    }
                    Ok(ServiceEvent::ServiceRemoved(_, name)) => Ok(DiscoveryEvent::Removed(name)),
                    Ok(_) => continue,
                    Err(error) => Err(eyre!("mDNS browse channel closed: {}", error)),
                };
                let terminal = event.is_err();
                if send.send(event).is_err() || terminal {
                    break;
                }
            });
            Ok(Box::new(MdnsBrowse {
                _mdns: BrowseHandle { mdns: Some(mdns) },
                events,
            }) as Box<dyn DiscoveryBrowse>)
        })
    }

    fn resolve_one(
        &self,
        expected_fingerprint: &str,
        timeout: Duration,
    ) -> DiscoveryFuture<'_, Result<SocketAddr>> {
        let expected_fingerprint = expected_fingerprint.to_uppercase();
        Box::pin(async move { discover_one_attempt(&expected_fingerprint, timeout).await })
    }
}

async fn run_bounded_discovery_task<T, Shutdown>(
    mut task: tokio::task::JoinHandle<Result<T>>,
    timeout: Duration,
    shutdown: Shutdown,
) -> Result<Option<T>>
where
    T: Send + 'static,
    Shutdown: FnOnce() + Send + 'static,
{
    let completion = tokio::time::timeout(timeout, &mut task).await;

    // ServiceDaemon::shutdown is asynchronous. Wait for it on a blocking worker
    // so every retry releases its multicast sockets before another daemon starts.
    tokio::task::spawn_blocking(shutdown)
        .await
        .map_err(|error| eyre!("mDNS shutdown task failed: {}", error))?;

    match completion {
        Ok(result) => result
            .map_err(|error| eyre!("mDNS browse task failed: {}", error))?
            .map(Some),
        Err(_) => {
            // The blocking receiver exits when daemon shutdown closes its event
            // channel. Join it so timed-out retries cannot accumulate threads.
            if tokio::time::timeout(DISCOVERY_TASK_SHUTDOWN_TIMEOUT, &mut task)
                .await
                .is_err()
            {
                warn!("Timed out waiting for the mDNS browse task to stop");
            }
            Ok(None)
        }
    }
}

async fn discover_one_attempt(expected_fingerprint: &str, timeout: Duration) -> Result<SocketAddr> {
    let expected_fingerprint = expected_fingerprint.to_uppercase();
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(SERVICE_TYPE)?;
    let browse_handle = BrowseHandle { mdns: Some(mdns) };

    info!("Searching for nexdesk server on the network...");

    let browse_fingerprint = expected_fingerprint.clone();
    let task = tokio::task::spawn_blocking(move || loop {
        match receiver.recv() {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(peer) = discovered_peer(&info) {
                    if peer.fingerprint == browse_fingerprint {
                        info!(
                            "Discovered trusted server '{}' at {} ({})",
                            peer.name, peer.addr, peer.fingerprint
                        );
                        return Ok(peer.addr);
                    }
                    debug!(
                        "Ignoring server '{}' at {} with fingerprint {}",
                        peer.name, peer.addr, peer.fingerprint
                    );
                }
            }
            Ok(_) => {}
            Err(_) => return Err(eyre!("mDNS browse channel closed")),
        }
    });

    match run_bounded_discovery_task(task, timeout, move || drop(browse_handle)).await? {
        Some(addr) => Ok(addr),
        None => Err(eyre!(
            "No nexdesk server with fingerprint {} found within {:?}",
            expected_fingerprint,
            timeout
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_selection_prefers_routable_ipv4_then_ipv6_then_fallback() {
        let loopback = "127.0.0.1".parse().unwrap();
        let ipv6 = "2001:db8::1".parse().unwrap();
        let ipv4 = "192.0.2.1".parse().unwrap();
        assert_eq!(
            preferred_addr([loopback, ipv6, ipv4], 4242).unwrap(),
            "192.0.2.1:4242".parse().unwrap()
        );
        assert_eq!(
            preferred_addr([loopback, ipv6], 4242).unwrap(),
            "[2001:db8::1]:4242".parse().unwrap()
        );
        assert_eq!(
            preferred_addr([loopback], 4242).unwrap(),
            "127.0.0.1:4242".parse().unwrap()
        );
    }

    #[test]
    fn discovered_peer_requires_and_normalizes_certificate_fingerprint() {
        let properties = [
            ("hostname", "desk"),
            ("platform", "linux"),
            ("fingerprint", "aa:bb:cc"),
        ];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "desk",
            "desk.local.",
            "192.0.2.1",
            4242,
            properties.as_slice(),
        )
        .unwrap();
        let peer = discovered_peer(&info).unwrap();
        assert_eq!(peer.fingerprint, "AA:BB:CC");
        assert_eq!(peer.addr, "192.0.2.1:4242".parse().unwrap());

        let legacy = ServiceInfo::new(
            SERVICE_TYPE,
            "legacy",
            "legacy.local.",
            "192.0.2.2",
            4242,
            [("hostname", "legacy"), ("platform", "linux")].as_slice(),
        )
        .unwrap();
        assert!(discovered_peer(&legacy).is_none());
    }

    #[test]
    fn mdns_adapter_implements_discovery_port() {
        fn assert_adapter(_: &dyn PeerDiscovery) {}
        assert_adapter(&MdnsDiscovery);
    }

    #[tokio::test]
    async fn timed_out_discovery_shuts_down_and_joins_blocking_receiver() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let shutdown_called = Arc::new(AtomicBool::new(false));
        let receiver_exited = Arc::new(AtomicBool::new(false));
        let (shutdown_send, shutdown_recv) = std::sync::mpsc::channel();
        let exited = receiver_exited.clone();
        let task: tokio::task::JoinHandle<Result<()>> = tokio::task::spawn_blocking(move || {
            shutdown_recv.recv().ok();
            exited.store(true, Ordering::SeqCst);
            Ok(())
        });
        let called = shutdown_called.clone();

        let result = run_bounded_discovery_task(task, Duration::from_millis(10), move || {
            called.store(true, Ordering::SeqCst);
            shutdown_send.send(()).ok();
        })
        .await
        .unwrap();

        assert!(result.is_none());
        assert!(shutdown_called.load(Ordering::SeqCst));
        assert!(receiver_exited.load(Ordering::SeqCst));
    }
}
