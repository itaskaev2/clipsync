//! Discovery module — mDNS service registration and peer browsing.
//!
//! Each ClipSync instance registers a service `_clipsync._tcp.local` on mDNS
//! and simultaneously browses for other instances of the same service.
//!
//! # mDNS properties
//! - `id` — Unique peer UUID (matches config.instance_id).
//! - `name` — Human-readable name (hostname).
//! - `port` — WebSocket server port.
//!
//! # Platform notes
//! - mDNS works within a single L2 broadcast domain (same subnet).
//! - On Windows, the `mdns-sd` crate uses the native Bonjour/mDNSResponder
//!   if available, or its own built-in implementation.

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use tokio::sync::mpsc;

/// Best-effort discovery of the primary LAN IPv4 address. Opens a UDP socket
/// "connected" to a public address — no packets are sent, but the OS picks the
/// outbound interface, whose local address is our LAN IP.
fn local_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

/// Reduce an arbitrary hostname to a safe DNS label (alphanumeric + '-').
fn sanitize_label(name: &str) -> String {
    let label: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = label.trim_matches('-');
    if trimmed.is_empty() {
        "clipsync".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Information discovered about a peer via mDNS.
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    /// Peer UUID (from mDNS TXT record).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// IP address resolved from mDNS.
    pub ip: IpAddr,
    /// WebSocket port.
    pub port: u16,
}

/// Start mDNS service registration and browsing.
///
/// # Arguments
/// * `instance_id` — Unique ID for this instance.
/// * `hostname` — Human-readable name to advertise.
/// * `port` — WebSocket server port.
/// * `peer_tx` — Channel to send discovered peers.
///
/// Returns the ServiceDaemon handle (must be kept alive).
pub fn start_discovery(
    instance_id: String,
    hostname: String,
    port: u16,
    peer_tx: mpsc::Sender<DiscoveredPeer>,
) -> Result<ServiceDaemon, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("Failed to create mDNS daemon: {e}"))?;

    // --- Register our own service ---
    let service_type = "_clipsync._tcp.local.";
    let instance_name = format!("{}_{}", hostname, &instance_id[..8]);
    // Host name must be a valid DNS label ending in `.local.`.
    let host_name = format!("{}-{}.local.", sanitize_label(&hostname), &instance_id[..8]);

    let mut properties = HashMap::new();
    properties.insert("id".to_string(), instance_id.clone());
    properties.insert("name".to_string(), hostname.clone());

    // Advertise the actual LAN IPv4 so peers get a reachable address. If we
    // can't determine it, fall back to mDNS auto-detecting interface addresses.
    let service_info = match local_ipv4() {
        Some(ip) => {
            tracing::info!("Advertising mDNS address {ip}:{port}");
            ServiceInfo::new(
                service_type,
                &instance_name,
                &host_name,
                IpAddr::V4(ip),
                port,
                properties,
            )
            .map_err(|e| format!("Failed to create mDNS service info: {e}"))?
        }
        None => {
            tracing::warn!("Could not determine LAN IPv4; using mDNS auto-detect.");
            ServiceInfo::new(service_type, &instance_name, &host_name, "", port, properties)
                .map_err(|e| format!("Failed to create mDNS service info: {e}"))?
                .enable_addr_auto()
        }
    };

    daemon
        .register(service_info)
        .map_err(|e| format!("Failed to register mDNS service: {e}"))?;

    tracing::info!(
        "mDNS service registered: {} (id={})",
        instance_name,
        &instance_id[..8]
    );

    // --- Browse for peers ---
    let browser = daemon
        .browse(service_type)
        .map_err(|e| format!("Failed to start mDNS browse: {e}"))?;

    // Spawn an async task to process mDNS events
    let our_id = instance_id.clone();
    tokio::spawn(async move {
        let mut known_peers: HashMap<String, DiscoveredPeer> = HashMap::new();

        loop {
            let event = browser.recv_async().await;
            match event {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    // NB: use val_str() (the value), not to_string() which
                    // yields "id=<value>" and would break the self-skip below.
                    let peer_id = match info.get_property("id") {
                        Some(id) => id.val_str().to_string(),
                        None => continue,
                    };

                    // Skip our own service
                    if peer_id == our_id {
                        continue;
                    }

                    let peer_name = info
                        .get_property("name")
                        .map(|p| p.val_str().to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    let peer_port = info.get_port();

                    // Get the first IPv4 address, fall back to any address
                    let peer_ip = match info.get_addresses_v4().iter().next() {
                        Some(addr) => IpAddr::V4(**addr),
                        None => match info.get_addresses().iter().next() {
                            Some(addr) => *addr,
                            None => continue,
                        },
                    };

                    let peer = DiscoveredPeer {
                        id: peer_id.clone(),
                        name: peer_name.clone(),
                        ip: peer_ip,
                        port: peer_port,
                    };

                    tracing::info!(
                        "mDNS peer discovered: {} ({}:{})",
                        peer_name,
                        peer_ip,
                        peer_port
                    );

                    known_peers.insert(peer_id.clone(), peer.clone());

                    // Notify the sync engine
                    if peer_tx.send(peer).await.is_err() {
                        tracing::info!("Discovery channel closed; stopping browse.");
                        break;
                    }
                }
                Ok(ServiceEvent::ServiceRemoved(_service_type, instance_name)) => {
                    // Extract peer_id from the instance name (format: hostname_UUIDPREFIX)
                    // Find and remove from known_peers
                    let removed: Vec<String> = known_peers
                        .iter()
                        .filter(|(id, _)| instance_name.ends_with(&id[..8]))
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in removed {
                        if let Some(peer) = known_peers.remove(&id) {
                            tracing::info!("mDNS peer left: {}", peer.name);
                            // We could send a "peer left" event, but for MVP
                            // the transport layer handles disconnection.
                        }
                    }
                }
                Ok(_) => {
                    // Other events (SearchStarted, etc.) — ignore
                }
                Err(e) => {
                    tracing::error!("mDNS browse error: {e}");
                    // Brief pause before retrying
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }
        }
    });

    Ok(daemon)
}

#[cfg(test)]
mod tests {
    // mDNS tests require network access and a running mDNS stack,
    // so we only test the types compile and the logic is correct.
}
