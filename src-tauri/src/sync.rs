//! Sync engine — the core coordination layer.
//!
//! # Responsibilities
//! 1. Receive clipboard changes from the local watcher and broadcast them to
//!    connected peers (with dedup + loop-guard).
//! 2. Receive clipboard content from peers and write it to the local clipboard.
//! 3. Track discovered peers (mDNS) and open client connections to them.
//! 4. React to transport connect/disconnect events: maintain the persisted
//!    allowlist, update the UI and the tray.
//! 5. Honour the pause/resume toggle.
//!
//! ```text
//! ClipboardWatcher ─┐                          ┌─→ [Peer1, Peer2, ...]
//!                   ├─→ SyncEngine ────────────┤
//! mDNS Discovery ───┘        ↑                 └─→ local clipboard (incoming)
//!                    TransportEvents (connect/disconnect)
//! ```
//!
//! # Loop guard (CRITICAL — see prompt line 29)
//! When we write remote content to the local clipboard, the watcher will fire
//! again and try to re-broadcast it — an infinite echo. We prevent this by
//! recording the hash(es) of content we just applied and skipping local events
//! that match within a short window.
//!
//! Images are the tricky case: the OS may re-encode an image on the round-trip
//! through the clipboard, so the hash the watcher computes after we write can
//! differ from the hash we received. We therefore record BOTH the received hash
//! and the hash of the clipboard *after* writing, so the echo is still caught.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

use crate::clipboard::{self, ClipboardContent};
use crate::config::{self, PeerInfo, SharedConfig};
use crate::discovery::DiscoveredPeer;
use crate::pairing::WireMessage;
use crate::transport::{TransportEvent, TransportManager};

/// How long an applied/sent hash is remembered (loop guard + dedup window).
const GUARD_WINDOW: Duration = Duration::from_millis(2000);

/// Loop-guard and dedup state.
struct SyncGuard {
    /// Hashes of content recently written from a remote peer (loop guard).
    applied: Vec<(String, Instant)>,
    /// Hashes of content we recently sent (dedup — don't re-send echoes).
    sent: Vec<(String, Instant)>,
}

impl SyncGuard {
    fn new() -> Self {
        Self {
            applied: Vec::new(),
            sent: Vec::new(),
        }
    }

    fn purge(&mut self) {
        let now = Instant::now();
        self.applied
            .retain(|(_, at)| now.duration_since(*at) < GUARD_WINDOW);
        self.sent
            .retain(|(_, at)| now.duration_since(*at) < GUARD_WINDOW);
    }

    /// True if `hash` was applied from a remote peer within the guard window.
    fn is_applied(&mut self, hash: &str) -> bool {
        self.purge();
        self.applied.iter().any(|(h, _)| h == hash)
    }

    /// Record one or more hashes as just-applied (loop guard).
    fn mark_applied(&mut self, hashes: &[String]) {
        let now = Instant::now();
        for h in hashes {
            if !h.is_empty() {
                self.applied.push((h.clone(), now));
            }
        }
    }

    /// Clear any guard entry for a hash (e.g. after a failed write).
    fn clear_applied(&mut self, hash: &str) {
        self.applied.retain(|(h, _)| h != hash);
    }

    fn was_recently_sent(&mut self, hash: &str) -> bool {
        self.purge();
        self.sent.iter().any(|(h, _)| h == hash)
    }

    fn mark_sent(&mut self, hash: &str) {
        self.sent.push((hash.to_string(), Instant::now()));
    }
}

/// Start the sync engine — the main coordination loop.
#[allow(clippy::too_many_arguments)]
pub async fn run_engine(
    mut clipboard_rx: mpsc::Receiver<ClipboardContent>,
    mut discovery_rx: mpsc::Receiver<DiscoveredPeer>,
    mut incoming_rx: mpsc::Receiver<(String, WireMessage)>,
    mut event_rx: mpsc::Receiver<TransportEvent>,
    config: SharedConfig,
    transport: Arc<TransportManager>,
    app: AppHandle,
) {
    let mut guard = SyncGuard::new();
    // Peers seen via mDNS (id -> info); used to name connections and to
    // (re)connect once the user pairs.
    let mut known_peers: HashMap<String, DiscoveredPeer> = HashMap::new();
    // Peer IDs with a live connection.
    let mut connected: std::collections::HashSet<String> = std::collections::HashSet::new();

    tracing::info!("Sync engine started");

    loop {
        tokio::select! {
            // --- Local clipboard change → broadcast ---
            Some(content) = clipboard_rx.recv() => {
                let paused = config.read().await.sync_paused;
                if paused {
                    tracing::debug!("Sync paused; skipping local clipboard event.");
                    continue;
                }
                if guard.is_applied(&content.content_hash) {
                    tracing::debug!("Loop guard: skipping local echo of {:.8}", content.content_hash);
                    continue;
                }
                if guard.was_recently_sent(&content.content_hash) {
                    tracing::debug!("Dedup: skipping re-send of {:.8}", content.content_hash);
                    continue;
                }

                let payload = match rmp_serde::to_vec(&content) {
                    Ok(p) => p,
                    Err(e) => { tracing::error!("Serialize clipboard failed: {e}"); continue; }
                };
                let msg = WireMessage::new("clipboard", payload);
                guard.mark_sent(&content.content_hash);

                let peer_ids = transport.connected_peer_ids().await;
                if peer_ids.is_empty() {
                    tracing::debug!("Clipboard change but no peers connected.");
                } else {
                    tracing::info!(
                        "Broadcasting clipboard ({:.8}, {} bytes) to {} peer(s)",
                        content.content_hash, msg.payload.len(), peer_ids.len()
                    );
                }
                for peer_id in &peer_ids {
                    if let Some(handle) = transport.get_peer(peer_id).await {
                        if let Err(e) = handle.send(msg.clone()).await {
                            tracing::warn!("Send to {peer_id} failed: {e}");
                        }
                    }
                }
            }

            // --- New peer discovered via mDNS ---
            Some(peer) = discovery_rx.recv() => {
                known_peers.insert(peer.id.clone(), peer.clone());
                let addr = std::net::SocketAddr::new(peer.ip, peer.port);
                tracing::info!("Discovered peer {} at {} — ensuring connection.", peer.name, addr);
                // The connect loop waits internally until we are paired, so it
                // is always safe to start it on discovery.
                transport.connect_to_peer(peer.id.clone(), addr).await;
            }

            // --- Incoming clipboard message from a peer ---
            Some((peer_id, msg)) = incoming_rx.recv() => {
                handle_incoming_message(&peer_id, msg, &mut guard, &config).await;
            }

            // --- Transport connection lifecycle ---
            Some(ev) = event_rx.recv() => {
                handle_transport_event(ev, &mut connected, &known_peers, &config, &transport, &app).await;
            }

            else => {
                tracing::info!("All sync engine channels closed; shutting down.");
                break;
            }
        }
    }
}

/// Apply an incoming clipboard message to the local clipboard.
async fn handle_incoming_message(
    peer_id: &str,
    msg: WireMessage,
    guard: &mut SyncGuard,
    config: &SharedConfig,
) {
    match msg.msg_type.as_str() {
        "clipboard" => {
            let (paused, limit_bytes, priority) = {
                let cfg = config.read().await;
                (
                    cfg.sync_paused,
                    cfg.payload_limit_mb as u64 * 1024 * 1024,
                    cfg.clipboard_priority.clone(),
                )
            };
            if paused {
                tracing::debug!("Sync paused; ignoring clipboard from {peer_id}");
                return;
            }

            let content: ClipboardContent = match rmp_serde::from_slice(&msg.payload) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Deserialize clipboard from {peer_id} failed: {e}");
                    return;
                }
            };

            let size = content.text.as_ref().map(|t| t.len()).unwrap_or(0)
                + content.image_data.as_ref().map(|d| d.len()).unwrap_or(0);
            if size as u64 > limit_bytes {
                tracing::warn!("Clipboard from {peer_id} exceeds limit ({size} bytes); skipping.");
                return;
            }

            // Already have it (e.g. duplicate frame) — don't re-apply.
            if guard.is_applied(&content.content_hash) {
                tracing::debug!("Already applied {:.8} from {peer_id}; skipping.", content.content_hash);
                return;
            }

            // Guard the received hash BEFORE writing so the watcher tick that
            // fires from our own write is suppressed even if it races us.
            guard.mark_applied(&[content.content_hash.clone()]);

            if let Err(e) = clipboard::write_to_clipboard(&content) {
                tracing::error!("Write clipboard from {peer_id} failed: {e}");
                guard.clear_applied(&content.content_hash);
                return;
            }

            // Also guard the hash of what actually landed on the clipboard: the
            // OS may re-encode images, so the watcher's recomputed hash can
            // differ from the one we received.
            if let Some(rt) = clipboard::read_current_hash(&priority) {
                if rt != content.content_hash {
                    guard.mark_applied(&[rt]);
                }
            }

            tracing::info!(
                "Applied clipboard from {peer_id}: type={}, {} bytes ({:.8})",
                content.content_type, size, content.content_hash
            );
        }
        "hello" => tracing::debug!("Unexpected post-handshake hello from {peer_id}"),
        "ping" => tracing::trace!("Ping from {peer_id}"),
        other => tracing::warn!("Unknown message type '{other}' from {peer_id}"),
    }
}

/// React to a transport connect/disconnect event.
async fn handle_transport_event(
    ev: TransportEvent,
    connected: &mut std::collections::HashSet<String>,
    known_peers: &HashMap<String, DiscoveredPeer>,
    config: &SharedConfig,
    transport: &Arc<TransportManager>,
    app: &AppHandle,
) {
    match ev {
        TransportEvent::Connected { peer_id, addr } => {
            connected.insert(peer_id.clone());

            // Name comes from mDNS discovery when available; otherwise fall back
            // to a short form of the peer id.
            let (name, port) = known_peers
                .get(&peer_id)
                .map(|p| (p.name.clone(), p.port))
                .unwrap_or_else(|| (short_id(&peer_id), addr.port()));

            {
                let mut cfg = config.write().await;
                let changed = config::upsert_peer(
                    &mut cfg,
                    PeerInfo {
                        name: name.clone(),
                        id: peer_id.clone(),
                        address: addr.ip().to_string(),
                        port,
                        last_seen: config::now_unix(),
                    },
                );
                if changed {
                    config::save_config(&cfg);
                }
            }

            tracing::info!("Peer connected: {name} ({})", short_id(&peer_id));
            let _ = app.emit(
                "clipsync:peer-joined",
                serde_json::json!({ "id": peer_id, "name": name }),
            );
        }
        TransportEvent::Disconnected { peer_id } => {
            connected.remove(&peer_id);
            tracing::info!("Peer disconnected: {}", short_id(&peer_id));
            let _ = app.emit(
                "clipsync:peer-left",
                serde_json::json!({ "id": peer_id }),
            );
        }
    }

    let _ = transport; // reserved for future live-status queries
    let paused = config.read().await.sync_paused;
    crate::tray::set_status(app, connected.len(), paused);
    let _ = app.emit(
        "clipsync:status-changed",
        serde_json::json!({ "peers": connected.len(), "paused": paused }),
    );
}

/// Short, human-readable form of a peer UUID.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}
