//! Sync engine — the core coordination layer.
//!
//! # Responsibilities
//! 1. Receive clipboard changes from the local watcher.
//! 2. Apply dedup/loop-guard logic.
//! 3. Serialize and broadcast to all connected peers.
//! 4. Receive clipboard content from peers, write to local clipboard.
//! 5. Manage the pause/resume toggle.
//!
//! # Architecture
//! ```
//! ClipboardWatcher ──→ SyncEngine ──→ [Peer1, Peer2, ...]
//!                              ↑
//!                     (incoming from peers) ──→ local clipboard
//! ```
//!
//! # Loop guard (CRITICAL — see prompt line 29)
//! When we write remote content to the local clipboard, we record its hash
//! as `last_applied_hash`. On the next local watcher tick, if the clipboard
//! has the same hash, we SKIP the event — preventing an infinite echo loop.
//! The `last_applied_hash` is cleared after a short window (2 seconds) to
//! allow legitimate re-copies of the same content.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::sync::RwLock;

use crate::clipboard::{self, ClipboardContent};
use crate::config::SharedConfig;
use crate::discovery::DiscoveredPeer;
use crate::pairing::{Cipher, WireMessage};
use crate::transport::TransportManager;

/// How long to hold the loop guard after applying remote content.
const LOOP_GUARD_WINDOW_MS: u64 = 2000;

/// State for loop guard and dedup.
struct SyncGuard {
    /// Hash of the last content we applied from a remote peer.
    last_applied_hash: Option<String>,
    /// When the last_applied_hash was set (for window expiry).
    last_applied_at: Option<Instant>,
    /// Set of recently sent hashes (dedup — don't re-send).
    recently_sent_hashes: HashSet<String>,
}

impl SyncGuard {
    fn new() -> Self {
        Self {
            last_applied_hash: None,
            last_applied_at: None,
            recently_sent_hashes: HashSet::new(),
        }
    }

    /// Check if a hash matches the loop guard. If the guard is expired, clear it.
    fn is_loop_guard_active(&mut self, hash: &str) -> bool {
        if let Some(ref h) = self.last_applied_hash {
            if h == hash {
                // Check if window is still active
                if let Some(at) = self.last_applied_at {
                    if at.elapsed().as_millis() < LOOP_GUARD_WINDOW_MS as u128 {
                        return true;
                    }
                }
                // Window expired — clear the guard
                self.last_applied_hash = None;
                self.last_applied_at = None;
            }
        }
        false
    }

    /// Set the loop guard after applying remote content.
    fn set_loop_guard(&mut self, hash: &str) {
        self.last_applied_hash = Some(hash.to_string());
        self.last_applied_at = Some(Instant::now());
    }

    /// Check if we recently sent this hash (dedup).
    fn was_recently_sent(&self, hash: &str) -> bool {
        self.recently_sent_hashes.contains(hash)
    }

    /// Mark a hash as recently sent.
    fn mark_sent(&mut self, hash: &str) {
        self.recently_sent_hashes.insert(hash.to_string());
        // Limit the set size to prevent memory leaks
        if self.recently_sent_hashes.len() > 100 {
            self.recently_sent_hashes.clear();
        }
    }
}

/// Start the sync engine. This is the main event loop that coordinates
/// clipboard → network and network → clipboard flows.
pub async fn run_engine(
    mut clipboard_rx: mpsc::Receiver<ClipboardContent>,
    mut discovery_rx: mpsc::Receiver<DiscoveredPeer>,
    mut incoming_rx: mpsc::Receiver<(String, WireMessage)>,
    config: SharedConfig,
    transport: Arc<TransportManager>,
) {
    let mut guard = SyncGuard::new();

    tracing::info!("Sync engine started");

    loop {
        tokio::select! {
            // --- Local clipboard change ---
            Some(content) = clipboard_rx.recv() => {
                let cfg = config.read().await;

                // Check if paused
                if cfg.sync_paused {
                    tracing::debug!("Sync paused; skipping local clipboard event.");
                    continue;
                }

                // Loop guard: skip if this hash was just applied from remote
                if guard.is_loop_guard_active(&content.content_hash) {
                    tracing::debug!(
                        "Loop guard: skipping local event for hash {:.8}",
                        &content.content_hash
                    );
                    continue;
                }

                // Dedup: skip if we just sent this
                if guard.was_recently_sent(&content.content_hash) {
                    tracing::debug!(
                        "Dedup: skipping re-send of hash {:.8}",
                        &content.content_hash
                    );
                    continue;
                }

                // Serialize to MessagePack
                let payload = match rmp_serde::to_vec(&content) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!("Failed to serialize clipboard content: {e}");
                        continue;
                    }
                };

                let msg = WireMessage::new("clipboard", payload);

                // Mark as sent BEFORE broadcasting (prevents race with incoming)
                guard.mark_sent(&content.content_hash);

                // Broadcast to all connected peers
                let peer_ids = transport.connected_peer_ids().await;
                tracing::info!(
                    "Broadcasting clipboard ({:.8}, {} bytes) to {} peers",
                    &content.content_hash,
                    msg.payload.len(),
                    peer_ids.len()
                );

                for peer_id in &peer_ids {
                    if let Some(handle) = transport.get_peer(peer_id).await {
                        if let Err(e) = handle.send(msg.clone()).await {
                            tracing::warn!("Failed to send to peer {peer_id}: {e}");
                        }
                    }
                }
            }

            // --- New peer discovered via mDNS ---
            Some(peer) = discovery_rx.recv() => {
                let cfg = config.read().await;

                // Only connect to paired (allowlisted) peers
                if !crate::pairing::is_peer_allowed(&cfg.paired_peers, &peer.id) {
                    tracing::debug!(
                        "Ignoring non-paired peer: {} ({})",
                        peer.name,
                        peer.id
                    );
                    continue;
                }

                let peer_addr = std::net::SocketAddr::new(peer.ip, peer.port);
                tracing::info!(
                    "Discovered paired peer: {} at {} — connecting...",
                    peer.name,
                    peer_addr
                );

                // Connect to the peer (transport handles reconnects internally)
                transport.connect_to_peer(peer.id.clone(), peer_addr);
            }

            // --- Incoming message from a peer ---
            Some((peer_id, msg)) = incoming_rx.recv() => {
                handle_incoming_message(
                    &peer_id,
                    msg,
                    &mut guard,
                    &config,
                ).await;
            }

            // All channels closed — exit
            else => {
                tracing::info!("All sync engine channels closed; shutting down.");
                break;
            }
        }
    }
}

/// Handle an incoming message from a peer.
async fn handle_incoming_message(
    peer_id: &str,
    msg: WireMessage,
    guard: &mut SyncGuard,
    config: &SharedConfig,
) {
    match msg.msg_type.as_str() {
        "clipboard" => {
            let cfg = config.read().await;

            if cfg.sync_paused {
                tracing::debug!("Sync paused; ignoring clipboard from {peer_id}");
                return;
            }

            // Deserialize the clipboard content
            let content: ClipboardContent = match rmp_serde::from_slice(&msg.payload) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Failed to deserialize clipboard from {peer_id}: {e}");
                    return;
                }
            };

            // Check payload limit
            let size = content.text.as_ref().map(|t| t.len()).unwrap_or(0)
                + content.image_data.as_ref().map(|d| d.len()).unwrap_or(0);
            if size as u64 > (cfg.payload_limit_mb as u64 * 1024 * 1024) {
                tracing::warn!(
                    "Clipboard from {peer_id} exceeds limit ({} bytes); skipping.",
                    size
                );
                return;
            }

            // Loop guard: if we already have this exact content, skip
            if guard.is_loop_guard_active(&content.content_hash) {
                tracing::debug!(
                    "Loop guard: skipping incoming from {peer_id} ({:.8})",
                    &content.content_hash
                );
                return;
            }

            // Set the loop guard BEFORE writing to clipboard
            // (CRITICAL: this prevents the watcher from re-emitting)
            guard.set_loop_guard(&content.content_hash);

            // Write to local clipboard
            if let Err(e) = clipboard::write_to_clipboard(&content) {
                tracing::error!("Failed to write clipboard from {peer_id}: {e}");
                // Clear guard on failure so user can try again
                guard.last_applied_hash = None;
                guard.last_applied_at = None;
                return;
            }

            tracing::info!(
                "Applied clipboard from {peer_id}: type={}, size={} ({:.8})",
                content.content_type,
                size,
                &content.content_hash
            );
        }

        "ping" => {
            tracing::trace!("Ping from {peer_id}");
        }

        "hello" => {
            // Already handled during connection setup
            tracing::debug!("Unexpected hello from {peer_id} (already connected)");
        }

        other => {
            tracing::warn!("Unknown message type '{other}' from {peer_id}");
        }
    }
}
