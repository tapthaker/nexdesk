use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info};

pub use crate::ports::DiscoveredPeer;
use crate::ports::{DiscoveryBrowse, DiscoveryEvent, DiscoveryFuture, PeerDiscovery};

const SERVICE_TYPE: &str = "_nexdesk._udp.local.";

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

    let instance_name = format!("{hostname}");
    let ip = local_ipv4().unwrap_or_default();
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
                    "  Found peer: {} ({})\n    Selected: {}\n",
                    peer.name, peer.platform, peer.addr,
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

    let instance_name = format!("{hostname}");
    let ip = local_ipv4().unwrap_or_default();
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
            mdns.shutdown().ok();
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
                let name = info
                    .get_property_val_str("hostname")
                    .unwrap_or("unknown")
                    .to_string();
                let platform = info
                    .get_property_val_str("platform")
                    .unwrap_or("unknown")
                    .to_string();
                if let Some(addr) = preferred_service_addr(&info) {
                    let _ = tx.send(DiscoveredPeer {
                        name,
                        platform,
                        addr,
                    });
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
                        let Some(addr) = preferred_service_addr(&info) else {
                            continue;
                        };
                        Ok(DiscoveryEvent::Found(DiscoveredPeer {
                            name: info
                                .get_property_val_str("hostname")
                                .unwrap_or("unknown")
                                .to_string(),
                            platform: info
                                .get_property_val_str("platform")
                                .unwrap_or("unknown")
                                .to_string(),
                            addr,
                        }))
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

    fn resolve_one(&self, timeout: Duration) -> DiscoveryFuture<'_, Result<SocketAddr>> {
        Box::pin(async move { discover_one_attempt(timeout).await })
    }
}

/// Discover the first nexdesk server on the LAN.
/// Returns its socket address or an error if none found within `timeout`.
pub async fn discover_one(timeout: Duration) -> Result<SocketAddr> {
    crate::app::resolve_peer_with_retry(
        &MdnsDiscovery,
        timeout,
        3,
        &crate::app::CancellationToken::new(),
    )
    .await
}

async fn discover_one_attempt(timeout: Duration) -> Result<SocketAddr> {
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(SERVICE_TYPE)?;

    info!("Searching for nexdesk server on the network...");

    let result = tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || loop {
            match receiver.recv() {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if let Some(addr) = preferred_service_addr(&info) {
                        let hostname = info.get_property_val_str("hostname").unwrap_or("unknown");
                        info!("Discovered server '{}' at {}", hostname, addr);
                        return Ok(addr);
                    }
                }
                Ok(_) => {}
                Err(_) => return Err(eyre!("mDNS browse channel closed")),
            }
        }),
    )
    .await;

    mdns.shutdown().ok();

    match result {
        Ok(Ok(addr)) => addr,
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(eyre!("No nexdesk server found within {:?}", timeout)),
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
    fn mdns_adapter_implements_discovery_port() {
        fn assert_adapter(_: &dyn PeerDiscovery) {}
        assert_adapter(&MdnsDiscovery);
    }
}
