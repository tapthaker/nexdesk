use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info};

const SERVICE_TYPE: &str = "_nexdesk._udp.local.";
const MAX_DISCOVERY_NAME_BYTES: usize = crate::net::protocol::MAX_PEER_NAME_BYTES;
const MAX_DISCOVERY_PLATFORM_BYTES: usize = 64;
const MAX_MDNS_HOST_LABEL_BYTES: usize = 63;
const MAX_MDNS_TXT_VALUE_BYTES: usize = 200;

fn mdns_txt_value(value: &str) -> String {
    sanitize_nonempty_discovery_text(value, MAX_MDNS_TXT_VALUE_BYTES, "unknown")
}

fn validate_advertise_port(port: u16) -> Result<()> {
    if port == 0 {
        return Err(eyre!(
            "Cannot advertise nexdesk on port 0; choose a fixed UDP port"
        ));
    }
    Ok(())
}

fn is_usable_mdns_addr(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_loopback(),
        // mdns-sd exposes only IpAddr, not the interface index needed to dial
        // an IPv6 link-local address. Avoid returning unusable [fe80::]/10
        // socket addresses with scope_id=0.
        IpAddr::V6(v6) => !v6.is_unspecified() && !v6.is_loopback() && !v6.is_unicast_link_local(),
    }
}

fn sanitize_discovery_text(value: &str, max_bytes: usize) -> String {
    let mut sanitized = String::new();
    for ch in value.chars() {
        let ch = if ch.is_control() { '�' } else { ch };
        let len = ch.len_utf8();
        if sanitized.len().saturating_add(len) > max_bytes {
            break;
        }
        sanitized.push(ch);
    }
    sanitized
}

fn bounded_mdns_property(info: &ServiceInfo, key: &str, default: &str, max_bytes: usize) -> String {
    let value = info.get_property_val_str(key).unwrap_or(default);
    let value = sanitize_discovery_text(value, max_bytes);
    if value.is_empty() {
        default.to_string()
    } else {
        value
    }
}

struct LocalMdnsNames {
    display_name: String,
    instance_name: String,
    host_label: String,
    txt_hostname: String,
}

fn local_mdns_names() -> LocalMdnsNames {
    let raw = gethostname::gethostname().to_string_lossy().into_owned();
    let display_name = sanitize_nonempty_discovery_text(&raw, MAX_DISCOVERY_NAME_BYTES, "nexdesk");
    let instance_name = sanitize_mdns_host_label(&raw);
    let host_label = sanitize_mdns_host_label(&raw);
    let txt_hostname = sanitize_nonempty_discovery_text(&raw, MAX_MDNS_TXT_VALUE_BYTES, "nexdesk");
    LocalMdnsNames {
        display_name,
        instance_name,
        host_label,
        txt_hostname,
    }
}

fn sanitize_nonempty_discovery_text(value: &str, max_bytes: usize, default: &str) -> String {
    let sanitized = sanitize_discovery_text(value, max_bytes);
    if sanitized.is_empty() {
        default.to_string()
    } else {
        sanitized
    }
}

fn sanitize_mdns_host_label(value: &str) -> String {
    let mut label = String::new();
    let mut last_was_dash = false;
    for ch in value.chars() {
        let ch = if ch.is_ascii_alphanumeric() { ch } else { '-' };
        if ch == '-' && (label.is_empty() || last_was_dash) {
            continue;
        }
        if label.len() + ch.len_utf8() > MAX_MDNS_HOST_LABEL_BYTES {
            break;
        }
        label.push(ch);
        last_was_dash = ch == '-';
    }
    while label.ends_with('-') {
        label.pop();
    }
    if label.is_empty() {
        "nexdesk".to_string()
    } else {
        label
    }
}

fn preferred_service_addr(info: &ServiceInfo) -> Option<SocketAddr> {
    let port = info.get_port();
    if port == 0 {
        return None;
    }
    let addresses = info.get_addresses();

    let selected = addresses
        .iter()
        .copied()
        .find(|addr| matches!(addr, IpAddr::V4(v4) if !v4.is_link_local() && is_usable_mdns_addr(*addr)))
        .or_else(|| {
            addresses
                .iter()
                .copied()
                .find(|addr| matches!(addr, IpAddr::V6(_)) && is_usable_mdns_addr(*addr))
        })
        .or_else(|| {
            addresses
                .iter()
                .copied()
                .find(|addr| matches!(addr, IpAddr::V4(_)) && is_usable_mdns_addr(*addr))
        })?;

    Some(SocketAddr::new(selected, port))
}

/// Advertise this machine on the local network via mDNS.
pub async fn advertise(port: u16) -> Result<()> {
    validate_advertise_port(port)?;
    let mdns = ServiceDaemon::new()?;

    let names = local_mdns_names();

    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    };
    let version = mdns_txt_value(crate::net::protocol::BUILD_VERSION);

    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &names.instance_name,
        &format!("{}.local.", names.host_label),
        (),
        port,
        [
            ("hostname", names.txt_hostname.as_str()),
            ("platform", platform),
            ("version", version.as_str()),
        ]
        .as_slice(),
    )?
    .enable_addr_auto();

    mdns.register(service)?;

    info!(
        "Advertising as '{}' on port {} (platform: {}, addresses: auto)",
        names.display_name, port, platform
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
    let mdns = ServiceDaemon::new()?;

    let receiver = mdns.browse(SERVICE_TYPE)?;

    info!("Browsing for nexdesk peers on the network...");
    info!("Press Ctrl+C to stop\n");

    let browse_handle = tokio::task::spawn_blocking(move || {
        while let Ok(event) = receiver.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let hostname = bounded_mdns_property(
                        &info,
                        "hostname",
                        "unknown",
                        MAX_DISCOVERY_NAME_BYTES,
                    );
                    let platform = bounded_mdns_property(
                        &info,
                        "platform",
                        "unknown",
                        MAX_DISCOVERY_PLATFORM_BYTES,
                    );
                    let addrs: Vec<String> =
                        info.get_addresses().iter().map(|a| a.to_string()).collect();
                    let selected = preferred_service_addr(&info)
                        .map(|addr| addr.to_string())
                        .unwrap_or_else(|| "none".to_string());

                    println!(
                        "  Found peer: {} ({})\n    Selected: {}\n    Addresses: {}\n    Port: {}\n",
                        hostname,
                        platform,
                        selected,
                        addrs.join(", "),
                        info.get_port(),
                    );
                }
                ServiceEvent::ServiceRemoved(_, full_name) => {
                    let full_name = sanitize_discovery_text(&full_name, MAX_DISCOVERY_NAME_BYTES);
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
    validate_advertise_port(port)?;
    let mdns = ServiceDaemon::new()?;

    let names = local_mdns_names();

    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    };
    let version = mdns_txt_value(crate::net::protocol::BUILD_VERSION);

    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &names.instance_name,
        &format!("{}.local.", names.host_label),
        (),
        port,
        [
            ("hostname", names.txt_hostname.as_str()),
            ("platform", platform),
            ("version", version.as_str()),
        ]
        .as_slice(),
    )?
    .enable_addr_auto();

    mdns.register(service)?;

    info!(
        "mDNS: advertising as '{}' on port {} (addresses: auto)",
        names.display_name, port
    );

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

fn send_discovered_peer(
    tx: &std::sync::mpsc::Sender<DiscoveredPeer>,
    peer: DiscoveredPeer,
) -> std::result::Result<(), std::sync::mpsc::SendError<DiscoveredPeer>> {
    tx.send(peer)
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
                let name =
                    bounded_mdns_property(&info, "hostname", "unknown", MAX_DISCOVERY_NAME_BYTES);
                let platform = bounded_mdns_property(
                    &info,
                    "platform",
                    "unknown",
                    MAX_DISCOVERY_PLATFORM_BYTES,
                );
                if let Some(addr) = preferred_service_addr(&info) {
                    let peer = DiscoveredPeer {
                        name,
                        platform,
                        addr,
                    };
                    if send_discovered_peer(&tx, peer).is_err() {
                        break;
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    Ok((rx, BrowseHandle { mdns: Some(mdns) }))
}

/// Discover the first nexdesk server on the LAN.
/// Returns its socket address or an error if none found within `timeout`.
pub async fn discover_one(timeout: Duration) -> Result<SocketAddr> {
    let attempts = 3;
    let per_attempt = timeout
        .checked_div(attempts)
        .filter(|duration| !duration.is_zero())
        .unwrap_or(timeout);

    for attempt in 1..=attempts {
        match discover_one_attempt(per_attempt).await {
            Ok(addr) => return Ok(addr),
            Err(e) if attempt < attempts => {
                debug!(
                    "mDNS discovery attempt {}/{} failed: {}. Restarting browse session...",
                    attempt, attempts, e
                );
            }
            Err(e) => return Err(e),
        }
    }

    Err(eyre!("No nexdesk server found within {:?}", timeout))
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
                        let hostname = bounded_mdns_property(
                            &info,
                            "hostname",
                            "unknown",
                            MAX_DISCOVERY_NAME_BYTES,
                        );
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

    fn service_info(ip: &str) -> ServiceInfo {
        ServiceInfo::new(
            SERVICE_TYPE,
            "test",
            "test.local.",
            ip,
            4242,
            [("hostname", "test")].as_slice(),
        )
        .unwrap()
    }

    #[test]
    fn browsing_sender_reports_dropped_receivers() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let peer = DiscoveredPeer {
            name: "test".into(),
            platform: "test".into(),
            addr: "127.0.0.1:4242".parse().unwrap(),
        };
        assert!(send_discovered_peer(&tx, peer).is_err());
    }

    #[test]
    fn discovery_text_is_bounded_and_terminal_safe() {
        assert_eq!(sanitize_discovery_text("abc\ndef", 32), "abc�def");
        assert_eq!(sanitize_discovery_text("abcdef", 3), "abc");
    }

    #[test]
    fn mdns_host_label_is_dns_safe() {
        assert_eq!(
            sanitize_mdns_host_label("host name.local\n"),
            "host-name-local"
        );
        assert_eq!(sanitize_mdns_host_label("---"), "nexdesk");
        assert_eq!(
            sanitize_mdns_host_label(&"a".repeat(80)).len(),
            MAX_MDNS_HOST_LABEL_BYTES
        );
    }

    #[test]
    fn mdns_txt_values_are_capped_to_dns_safe_size() {
        let value = sanitize_nonempty_discovery_text(
            &"a".repeat(MAX_MDNS_TXT_VALUE_BYTES + 1),
            MAX_MDNS_TXT_VALUE_BYTES,
            "nexdesk",
        );
        assert_eq!(value.len(), MAX_MDNS_TXT_VALUE_BYTES);
        assert_eq!(
            sanitize_nonempty_discovery_text("", MAX_MDNS_TXT_VALUE_BYTES, "nexdesk"),
            "nexdesk"
        );
        assert_eq!(mdns_txt_value("v1\n"), "v1�");
        assert_eq!(
            mdns_txt_value(&"v".repeat(MAX_MDNS_TXT_VALUE_BYTES + 1)).len(),
            MAX_MDNS_TXT_VALUE_BYTES
        );
    }

    #[test]
    fn removed_service_names_are_terminal_safe() {
        assert_eq!(sanitize_discovery_text("peer\x1b[31m", 64), "peer�[31m");
    }

    #[test]
    fn mdns_properties_are_bounded() {
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "test",
            "test.local.",
            "127.0.0.1",
            4242,
            [("hostname", "a\nb"), ("platform", &"x".repeat(128))].as_slice(),
        )
        .unwrap();
        assert_eq!(
            bounded_mdns_property(&info, "hostname", "unknown", MAX_DISCOVERY_NAME_BYTES),
            "a�b"
        );
        assert_eq!(
            bounded_mdns_property(&info, "platform", "unknown", MAX_DISCOVERY_PLATFORM_BYTES).len(),
            MAX_DISCOVERY_PLATFORM_BYTES
        );
    }

    #[test]
    fn advertise_port_rejects_zero() {
        assert!(validate_advertise_port(0).is_err());
        assert!(validate_advertise_port(4242).is_ok());
    }

    #[test]
    fn preferred_addr_rejects_remote_zero_port() {
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "test",
            "test.local.",
            "127.0.0.1",
            0,
            [("hostname", "test")].as_slice(),
        )
        .unwrap();
        assert!(preferred_service_addr(&info).is_none());
    }

    #[test]
    fn preferred_addr_rejects_unscoped_ipv6_link_local_only() {
        let info = service_info("fe80::1");
        assert!(preferred_service_addr(&info).is_none());
    }

    #[test]
    fn preferred_addr_uses_global_ipv6_over_unusable_link_local() {
        let info = service_info("fe80::1,2001:db8::1");
        assert_eq!(
            preferred_service_addr(&info).unwrap(),
            "[2001:db8::1]:4242".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn preferred_addr_keeps_ipv4_link_local_as_last_resort() {
        let info = service_info("169.254.1.2");
        assert_eq!(
            preferred_service_addr(&info).unwrap(),
            "169.254.1.2:4242".parse::<SocketAddr>().unwrap()
        );
    }
}
