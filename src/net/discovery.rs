use std::net::SocketAddr;
use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{info, debug};

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

/// Advertise this machine on the local network via mDNS.
pub async fn advertise(port: u16) -> Result<()> {
    let mdns = ServiceDaemon::new()?;

    let hostname = gethostname::gethostname()
        .to_string_lossy()
        .into_owned();

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

    info!("Advertising as '{}' on port {} (platform: {}, ip: {})", hostname, port, platform, ip);
    info!("Press Ctrl+C to stop");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;

    info!("Shutting down mDNS advertisement...");
    mdns.shutdown()?;

    Ok(())
}

/// Discover peers on the local network via mDNS.
pub async fn discover() -> Result<()> {
    let mdns = ServiceDaemon::new()?;

    let receiver = mdns.browse(SERVICE_TYPE)?;

    info!("Browsing for nexdesk peers on the network...");
    info!("Press Ctrl+C to stop\n");

    let browse_handle = tokio::task::spawn_blocking(move || {
        loop {
            match receiver.recv() {
                Ok(event) => {
                    match event {
                        ServiceEvent::ServiceResolved(info) => {
                            let hostname = info.get_property_val_str("hostname")
                                .unwrap_or("unknown");
                            let platform = info.get_property_val_str("platform")
                                .unwrap_or("unknown");
                            let addrs: Vec<String> = info.get_addresses()
                                .iter()
                                .map(|a| a.to_string())
                                .collect();

                            println!(
                                "  Found peer: {} ({})\n    Addresses: {}\n    Port: {}\n",
                                hostname,
                                platform,
                                addrs.join(", "),
                                info.get_port(),
                            );
                        }
                        ServiceEvent::ServiceRemoved(_, full_name) => {
                            println!("  Peer left: {}\n", full_name);
                        }
                        ServiceEvent::SearchStarted(_) => {
                            debug!("mDNS search started");
                        }
                        other => {
                            debug!("mDNS event: {:?}", other);
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("\nShutting down discovery...");
        }
        result = browse_handle => {
            result?;
        }
    }

    mdns.shutdown()?;

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

    let hostname = gethostname::gethostname()
        .to_string_lossy()
        .into_owned();

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

    info!("mDNS: advertising as '{}' on port {} (ip: {})", hostname, port, ip);

    Ok(AdvertiseHandle { mdns: Some(mdns) })
}

/// A peer discovered via mDNS browsing.
pub struct DiscoveredPeer {
    pub name: String,
    pub platform: String,
    pub addr: SocketAddr,
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

    std::thread::spawn(move || {
        loop {
            match receiver.recv() {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let name = info.get_property_val_str("hostname")
                        .unwrap_or("unknown").to_string();
                    let platform = info.get_property_val_str("platform")
                        .unwrap_or("unknown").to_string();
                    let port = info.get_port();
                    if let Some(ip) = info.get_addresses().iter().next() {
                        let _ = tx.send(DiscoveredPeer {
                            name,
                            platform,
                            addr: SocketAddr::new(*ip, port),
                        });
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    Ok((rx, BrowseHandle { mdns: Some(mdns) }))
}

/// Discover the first nexdesk server on the LAN.
/// Returns its socket address or an error if none found within `timeout`.
pub async fn discover_one(timeout: Duration) -> Result<SocketAddr> {
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(SERVICE_TYPE)?;

    info!("Searching for nexdesk server on the network...");

    let result = tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || {
        loop {
            match receiver.recv() {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if let Some(addr) = info.get_addresses().iter().next() {
                        let port = info.get_port();
                        let hostname = info.get_property_val_str("hostname")
                            .unwrap_or("unknown");
                        info!("Discovered server '{}' at {}:{}", hostname, addr, port);
                        return Ok(SocketAddr::new(*addr, port));
                    }
                }
                Ok(_) => {}
                Err(_) => return Err(eyre!("mDNS browse channel closed")),
            }
        }
    }))
    .await;

    mdns.shutdown().ok();

    match result {
        Ok(Ok(addr)) => addr,
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(eyre!("No nexdesk server found within {:?}", timeout)),
    }
}
