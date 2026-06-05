//! Transport module — encrypted WebSocket server and client.
//!
//! # Architecture
//! Each ClipSync instance runs:
//! - A **WebSocket server** (tokio-tungstenite) listening on an OS-assigned
//!   port, accepting connections from peers.
//! - **WebSocket client** connections to each discovered peer, with automatic
//!   reconnect and exponential backoff.
//!
//! # Encryption / pairing
//! Messages are encrypted at the application layer with ChaCha20-Poly1305
//! (see `pairing::Cipher`). The key is derived from the pairing code, so a peer
//! can only complete the handshake (and exchange data) if it shares the same
//! code. This is the security boundary: **a peer without the code cannot decrypt
//! the handshake, so it is rejected.**
//!
//! The cipher is held in a swappable slot (`Option<Arc<Cipher>>`):
//! - `None` — not paired yet; no connection attempts succeed and the server
//!   rejects incoming connections.
//! - `Some` — paired; the derived key gates all traffic.
//!
//! When the pairing code changes at runtime we install a new cipher and bump a
//! `watch` "reset" generation; every live connection observes the change, drops,
//! and the client side reconnects with the new key.
//!
//! # Framing
//! Each WebSocket binary frame is `[12-byte nonce][ciphertext + 16-byte tag]`,
//! where the plaintext is a MessagePack-encoded `WireMessage`.
//!
//! # Reconnect
//! On connection drop, clients reconnect with exponential backoff:
//!   1s → 2s → 4s → 8s → ... → max 60s.

use crate::pairing::{Cipher, WireMessage};
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, RwLock};
use tokio_tungstenite::tungstenite::Message as WsMessage;

type Peers = Arc<RwLock<HashMap<String, PeerHandle>>>;
type CipherSlot = Arc<RwLock<Option<Arc<Cipher>>>>;

/// Monotonic per-connection token. A connection only removes its *own* entry
/// from the peer map on teardown (token match), so a reconnect that installs a
/// newer connection under the same peer id is never evicted by the old one.
static CONN_SEQ: AtomicU64 = AtomicU64::new(0);

/// WebSocket size limits. Clipboard images are raw RGBA and easily exceed
/// tungstenite's 16 MiB default frame size (a 2048x2048 image is ~16.8 MiB),
/// which would otherwise be rejected — the frame is dropped and the connection
/// resets, so images silently fail to sync. Cap well above the maximum
/// configurable payload (100 MB).
fn ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    let limit = 128 * 1024 * 1024; // 128 MiB
    let mut cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    cfg.max_message_size = Some(limit);
    cfg.max_frame_size = Some(limit);
    cfg
}

/// A connected peer handle — can send messages to this peer.
#[derive(Clone)]
pub struct PeerHandle {
    pub peer_id: String,
    tx: mpsc::Sender<WireMessage>,
    /// Identifies which physical connection currently owns the map entry.
    token: u64,
}

impl PeerHandle {
    /// Send a message to this peer.
    pub async fn send(&self, msg: WireMessage) -> Result<(), String> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| "Peer channel closed".to_string())
    }
}

/// Connection lifecycle events, consumed by the sync engine to update the
/// allowlist, the UI, and the tray.
#[derive(Debug, Clone)]
pub enum TransportEvent {
    Connected { peer_id: String, addr: SocketAddr },
    Disconnected { peer_id: String },
}

/// Manages peer connections — server and client sides.
pub struct TransportManager {
    /// Live peer handles, keyed by peer ID.
    peers: Peers,
    /// Swappable cipher slot (None = not paired).
    cipher: CipherSlot,
    /// Our instance ID.
    instance_id: String,
    /// Sender for incoming messages (to the sync engine).
    incoming_tx: mpsc::Sender<(String, WireMessage)>,
    /// Sender for connection lifecycle events (to the sync engine).
    event_tx: mpsc::Sender<TransportEvent>,
    /// "Reset" generation; bumped when the pairing code changes so live
    /// connections drop and clients reconnect with the new key.
    reset_tx: watch::Sender<u64>,
    /// Peer IDs that already have a client connect loop running (dedup).
    connecting: Arc<RwLock<HashSet<String>>>,
}

impl TransportManager {
    /// Create a new TransportManager (initially unpaired — no cipher).
    pub fn new(
        instance_id: String,
        incoming_tx: mpsc::Sender<(String, WireMessage)>,
        event_tx: mpsc::Sender<TransportEvent>,
    ) -> Self {
        let (reset_tx, _reset_rx) = watch::channel(0u64);
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            cipher: Arc::new(RwLock::new(None)),
            instance_id,
            incoming_tx,
            event_tx,
            reset_tx,
            connecting: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Install (or replace) the pairing code. Builds a fresh cipher and bumps
    /// the reset generation so any live connection drops and reconnects using
    /// the new key. Passing the same code again still forces a clean reconnect.
    pub async fn set_pairing_code(&self, code: &str) {
        let cipher = Arc::new(Cipher::from_pairing_code(code));
        *self.cipher.write().await = Some(cipher);
        self.reset_tx.send_modify(|g| *g = g.wrapping_add(1));
        tracing::info!("Transport cipher installed; connections will (re)handshake.");
    }

    /// Snapshot the current cipher (None until paired).
    pub async fn current_cipher(&self) -> Option<Arc<Cipher>> {
        self.cipher.read().await.clone()
    }

    /// Start the WebSocket server on an OS-assigned port. Returns the bound
    /// port so the caller can advertise it via mDNS (avoids a bind/advertise
    /// race on a pre-chosen port).
    pub async fn start_server(&self) -> Result<u16, String> {
        let addr = SocketAddr::from(([0, 0, 0, 0], 0));
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("Failed to bind WebSocket server: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("Failed to read server port: {e}"))?
            .port();

        tracing::info!("WebSocket server listening on 0.0.0.0:{port}");

        let peers = self.peers.clone();
        let cipher = self.cipher.clone();
        let incoming_tx = self.incoming_tx.clone();
        let event_tx = self.event_tx.clone();
        let reset_tx = self.reset_tx.clone();
        let our_id = self.instance_id.clone();

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        tracing::info!("Incoming connection from {peer_addr}");
                        let p = peers.clone();
                        let c = cipher.clone();
                        let inc = incoming_tx.clone();
                        let ev = event_tx.clone();
                        let reset_rx = reset_tx.subscribe();
                        let id = our_id.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_incoming_connection(stream, peer_addr, p, c, inc, ev, reset_rx, id)
                                    .await
                            {
                                tracing::warn!("Connection from {peer_addr} ended: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::error!("Accept error: {e}"),
                }
            }
        });

        Ok(port)
    }

    /// Ensure a client connect loop exists for this peer. Idempotent: repeated
    /// calls for the same peer are no-ops. The loop waits until a cipher is
    /// available (i.e. until the user pairs), then connects and auto-reconnects.
    pub async fn connect_to_peer(&self, peer_id: String, addr: SocketAddr) {
        // Deterministic tie-break: only the peer whose instance id sorts first
        // dials out; the other waits to accept. Both peers evaluate the same
        // comparison (operands swapped), so exactly one TCP connection is ever
        // established between a pair. Without this, both sides dial and the two
        // connections (inbound + outbound) evict each other from the peer map,
        // producing a connect/disconnect storm.
        if self.instance_id >= peer_id {
            tracing::debug!("Awaiting inbound connection from {peer_id} (it dials us).");
            return;
        }

        {
            let mut connecting = self.connecting.write().await;
            if !connecting.insert(peer_id.clone()) {
                return; // a loop already runs for this peer
            }
        }

        let peers = self.peers.clone();
        let cipher_slot = self.cipher.clone();
        let incoming_tx = self.incoming_tx.clone();
        let event_tx = self.event_tx.clone();
        let our_id = self.instance_id.clone();
        let mut reset_rx = self.reset_tx.subscribe();

        tokio::spawn(async move {
            let mut backoff = 1u64;
            loop {
                // Wait until we have a cipher (i.e. the user has paired).
                let cipher = loop {
                    if let Some(c) = cipher_slot.read().await.clone() {
                        break c;
                    }
                    tokio::select! {
                        _ = reset_rx.changed() => {}
                        _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {}
                    }
                };

                tracing::info!("Connecting to peer {peer_id} at {addr}...");
                match connect_and_handshake(&addr, &cipher, &our_id).await {
                    Ok(ws) => {
                        backoff = 1;
                        run_connection(
                            ws,
                            peer_id.clone(),
                            cipher,
                            peers.clone(),
                            incoming_tx.clone(),
                            event_tx.clone(),
                            reset_rx.clone(),
                            addr,
                        )
                        .await;
                        tracing::warn!("Disconnected from peer {peer_id}; will reconnect.");
                    }
                    Err(e) => tracing::warn!("Connect to {peer_id} failed: {e}"),
                }

                // Backoff before retrying — interruptible by a pairing reset.
                tokio::select! {
                    _ = reset_rx.changed() => backoff = 1,
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(backoff)) => {
                        backoff = (backoff * 2).min(60);
                    }
                }
            }
        });
    }

    /// Get a peer handle by ID.
    pub async fn get_peer(&self, peer_id: &str) -> Option<PeerHandle> {
        self.peers.read().await.get(peer_id).cloned()
    }

    /// List all live (connected) peer IDs.
    pub async fn connected_peer_ids(&self) -> Vec<String> {
        self.peers.read().await.keys().cloned().collect()
    }
}

/// Run a fully-established (post-handshake) connection until it drops or a
/// pairing reset fires. Generic over the stream type so the client
/// (`MaybeTlsStream<TcpStream>`) and server (`TcpStream`) sides share one path.
#[allow(clippy::too_many_arguments)]
async fn run_connection<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    peer_id: String,
    cipher: Arc<Cipher>,
    peers: Peers,
    incoming_tx: mpsc::Sender<(String, WireMessage)>,
    event_tx: mpsc::Sender<TransportEvent>,
    mut reset_rx: watch::Receiver<u64>,
    addr: SocketAddr,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let conn_token = CONN_SEQ.fetch_add(1, Ordering::Relaxed);
    let (mut ws_write, mut ws_read) = ws.split();
    let (peer_tx, mut peer_rx) = mpsc::channel::<WireMessage>(64);

    peers.write().await.insert(
        peer_id.clone(),
        PeerHandle {
            peer_id: peer_id.clone(),
            tx: peer_tx,
            token: conn_token,
        },
    );
    let _ = event_tx
        .send(TransportEvent::Connected {
            peer_id: peer_id.clone(),
            addr,
        })
        .await;

    // Write task: drains the per-peer channel, encrypts, writes to the socket.
    let write_cipher = cipher.clone();
    let write_id = peer_id.clone();
    let write_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            match msg.to_encrypted(&write_cipher) {
                Ok(data) => {
                    if ws_write.send(WsMessage::Binary(data.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => tracing::error!("Encrypt error for {write_id}: {e}"),
            }
        }
        let _ = ws_write.close().await;
    });

    // Read loop: decrypt and forward to the sync engine; break on reset.
    loop {
        tokio::select! {
            // Pairing changed (or app shutting down) — drop this connection.
            res = reset_rx.changed() => {
                if res.is_ok() {
                    tracing::info!("Pairing reset; dropping connection to {peer_id}");
                }
                break;
            }
            frame = ws_read.next() => {
                match frame {
                    Some(Ok(WsMessage::Binary(data))) => {
                        match WireMessage::from_encrypted(&data, &cipher) {
                            Ok(msg) => {
                                if incoming_tx.send((peer_id.clone(), msg)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => tracing::warn!("Decrypt error from {peer_id}: {e}"),
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Err(e)) => {
                        tracing::warn!("WS read error from {peer_id}: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    write_task.abort();
    // Only tear down the shared peer entry if it is still *ours*. A reconnect
    // may have installed a newer connection under the same id during an overlap;
    // in that case we must not evict it or emit a spurious disconnect.
    let still_ours = {
        let mut map = peers.write().await;
        match map.get(&peer_id) {
            Some(h) if h.token == conn_token => {
                map.remove(&peer_id);
                true
            }
            _ => false,
        }
    };
    if still_ours {
        let _ = event_tx
            .send(TransportEvent::Disconnected {
                peer_id: peer_id.clone(),
            })
            .await;
    }
}

/// Handle an incoming WebSocket connection (server side): upgrade, exchange
/// the encrypted handshake to learn the peer's identity, then run it.
#[allow(clippy::too_many_arguments)]
async fn handle_incoming_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    peers: Peers,
    cipher_slot: CipherSlot,
    incoming_tx: mpsc::Sender<(String, WireMessage)>,
    event_tx: mpsc::Sender<TransportEvent>,
    reset_rx: watch::Receiver<u64>,
    our_id: String,
) -> Result<(), String> {
    // Reject if we are not paired yet — there is no key to authenticate with.
    let cipher = match cipher_slot.read().await.clone() {
        Some(c) => c,
        None => return Err("not paired (no cipher); rejecting".to_string()),
    };

    let mut ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config()))
        .await
        .map_err(|e| format!("WebSocket upgrade error: {e}"))?;

    // Read the peer's encrypted "hello" — only a peer with our key can produce
    // a frame we can decrypt, which is what enforces pairing.
    let peer_id = match ws.next().await {
        Some(Ok(WsMessage::Binary(data))) => {
            let msg = WireMessage::from_encrypted(&data, &cipher)
                .map_err(|e| format!("hello decrypt error (wrong pairing code?): {e}"))?;
            if msg.msg_type != "hello" {
                return Err(format!("expected 'hello', got '{}'", msg.msg_type));
            }
            String::from_utf8(msg.payload).map_err(|e| format!("invalid peer id: {e}"))?
        }
        other => return Err(format!("no hello received: {other:?}")),
    };

    // Reply with our own hello so the client learns our identity.
    let hello = WireMessage::new("hello", our_id.into_bytes());
    let encrypted = hello.to_encrypted(&cipher)?;
    ws.send(WsMessage::Binary(encrypted.into()))
        .await
        .map_err(|e| format!("hello send error: {e}"))?;

    tracing::info!("Peer {peer_id} connected (incoming) from {peer_addr}");
    run_connection(
        ws,
        peer_id,
        cipher,
        peers,
        incoming_tx,
        event_tx,
        reset_rx,
        peer_addr,
    )
    .await;
    Ok(())
}

/// Connect to a peer and perform the encrypted handshake (client side).
async fn connect_and_handshake(
    addr: &SocketAddr,
    cipher: &Cipher,
    our_id: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>, String>
{
    let url = format!("ws://{addr}");
    let (mut ws, _resp) = tokio_tungstenite::connect_async_with_config(&url, Some(ws_config()), false)
        .await
        .map_err(|e| format!("WebSocket connect error: {e}"))?;

    // Send our hello.
    let hello = WireMessage::new("hello", our_id.as_bytes().to_vec());
    let encrypted = hello.to_encrypted(cipher)?;
    ws.send(WsMessage::Binary(encrypted.into()))
        .await
        .map_err(|e| format!("hello send error: {e}"))?;

    // Await the peer's hello (confirms it shares our pairing code).
    match ws.next().await {
        Some(Ok(WsMessage::Binary(data))) => {
            let msg = WireMessage::from_encrypted(&data, cipher)
                .map_err(|e| format!("hello decrypt error (wrong pairing code?): {e}"))?;
            if msg.msg_type != "hello" {
                return Err(format!("expected 'hello', got '{}'", msg.msg_type));
            }
            let peer_id =
                String::from_utf8(msg.payload).map_err(|e| format!("invalid peer id: {e}"))?;
            tracing::info!("Handshake complete with peer {peer_id}");
        }
        other => return Err(format!("no hello response: {other:?}")),
    }

    Ok(ws)
}
