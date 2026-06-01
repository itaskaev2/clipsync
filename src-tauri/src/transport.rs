//! Transport module — encrypted WebSocket server and client.
//!
//! # Architecture
//! Each ClipSync instance runs:
//! - A **WebSocket server** (tokio-tungstenite) listening on a random port,
//!   accepting connections from paired peers.
//! - **WebSocket client** connections to each paired peer, with automatic
//!   reconnect and exponential backoff.
//!
//! # Encryption
//! Messages are encrypted at the application layer using ChaCha20-Poly1305
//! (see `pairing::Cipher`). Each WebSocket binary frame contains:
//!   [12-byte nonce][ciphertext + 16-byte tag]
//!
//! # Framing
//! MessagePack (rmp-serde) serialization of `WireMessage` structs.
//! Message types:
//!   - "clipboard" — ClipboardContent payload
//!   - "ping" — Keepalive
//!   - "hello" — Initial handshake with peer identity
//!
//! # Reconnect
//! On connection drop, clients reconnect with exponential backoff:
//!   1s → 2s → 4s → 8s → ... → max 60s, then hold at 60s.

use crate::pairing::{Cipher, WireMessage};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::RwLock;

/// A connected peer handle — can send messages to this peer.
#[derive(Clone)]
pub struct PeerHandle {
    pub peer_id: String,
    tx: mpsc::Sender<WireMessage>,
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

/// Manages peer connections — server and client sides.
///
/// All received messages from all connections are forwarded to `incoming_tx`,
/// which the sync engine reads from.
pub struct TransportManager {
    /// Connected peer handles, keyed by peer ID.
    peers: Arc<RwLock<HashMap<String, PeerHandle>>>,
    /// Shared cipher for encryption/decryption.
    cipher: Arc<Cipher>,
    /// Our instance ID.
    instance_id: String,
    /// Sender for incoming messages (to sync engine).
    incoming_tx: mpsc::Sender<(String, WireMessage)>,
}

impl TransportManager {
    /// Create a new TransportManager.
    pub fn new(
        cipher: Arc<Cipher>,
        instance_id: String,
        incoming_tx: mpsc::Sender<(String, WireMessage)>,
    ) -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            cipher,
            instance_id,
            incoming_tx,
        }
    }

    /// Start the WebSocket server on the given port.
    /// Incoming connections are authenticated and added to the peer set.
    pub async fn start_server(&self, port: u16) -> Result<(), String> {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("Failed to bind to {addr}: {e}"))?;

        tracing::info!("WebSocket server listening on {addr}");

        let peers = self.peers.clone();
        let cipher = self.cipher.clone();
        let our_id = self.instance_id.clone();
        let incoming_tx = self.incoming_tx.clone();

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        tracing::info!("Incoming connection from {peer_addr}");

                        let p = peers.clone();
                        let c = cipher.clone();
                        let rx = incoming_tx.clone();
                        let id = our_id.clone();

                        tokio::spawn(async move {
                            if let Err(e) = handle_incoming_connection(stream, p, c, rx, id).await {
                                tracing::warn!("Connection from {peer_addr} error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Accept error: {e}");
                    }
                }
            }
        });

        Ok(())
    }

    /// Connect to a peer. Handles the client side: TCP → WS upgrade → handshake.
    /// Spawns a background task that reconnects on failure.
    pub fn connect_to_peer(&self, peer_id: String, addr: SocketAddr) {
        let peers = self.peers.clone();
        let cipher = self.cipher.clone();
        let our_id = self.instance_id.clone();
        let incoming_tx = self.incoming_tx.clone();

        tokio::spawn(async move {
            let mut backoff = 1u64;

            loop {
                tracing::info!("Connecting to peer {peer_id} at {addr}...");

                match connect_and_handshake(&addr, &cipher, &our_id).await {
                    Ok(ws_stream) => {
                        tracing::info!("Connected to peer {peer_id}");
                        backoff = 1; // Reset backoff on success

                        // Create a channel for sending to this peer
                        let (peer_tx, peer_rx) = mpsc::channel::<WireMessage>(64);

                        // Store the peer handle
                        {
                            let mut p = peers.write().await;
                            p.insert(
                                peer_id.clone(),
                                PeerHandle {
                                    peer_id: peer_id.clone(),
                                    tx: peer_tx,
                                },
                            );
                        }

                        // Split WS stream
                        let (mut ws_write, mut ws_read) = ws_stream.split();

                        // Spawn write task
                        let write_cipher = cipher.clone();
                        let write_peer_id = peer_id.clone();
                        tokio::spawn(async move {
                            let mut peer_rx2 = peer_rx;
                            while let Some(msg) = peer_rx2.recv().await {
                                match msg.to_encrypted(&write_cipher) {
                                    Ok(data) => {
                                        if let Err(e) = ws_write
                                            .send(tokio_tungstenite::tungstenite::Message::Binary(
                                                data.into(),
                                            ))
                                            .await
                                        {
                                            tracing::warn!(
                                                "Write to peer {write_peer_id} failed: {e}"
                                            );
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Encrypt error for {write_peer_id}: {e}");
                                    }
                                }
                            }
                        });

                        // Read loop
                        let read_cipher = cipher.clone();
                        let read_peer_id = peer_id.clone();
                        let read_tx = incoming_tx.clone();
                        loop {
                            match ws_read.next().await {
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
                                    match WireMessage::from_encrypted(&data, &read_cipher) {
                                        Ok(msg) => {
                                            if read_tx
                                                .send((read_peer_id.clone(), msg))
                                                .await
                                                .is_err()
                                            {
                                                tracing::info!(
                                                    "Incoming channel closed; stopping read."
                                                );
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "Decrypt error from {read_peer_id}: {e}"
                                            );
                                        }
                                    }
                                }
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_)))
                                | Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_))) => {
                                    // Handled by tungstenite
                                }
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                                    tracing::info!("Peer {read_peer_id} closed connection");
                                    break;
                                }
                                Some(Err(e)) => {
                                    tracing::warn!("WS read error from {read_peer_id}: {e}");
                                    break;
                                }
                                None => {
                                    tracing::info!("Peer {read_peer_id} stream ended");
                                    break;
                                }
                                _ => {}
                            }
                        }

                        // Connection lost — remove peer handle
                        {
                            let mut p = peers.write().await;
                            p.remove(&peer_id);
                        }
                        tracing::warn!("Disconnected from peer {peer_id}; reconnecting...");
                    }
                    Err(e) => {
                        tracing::warn!("Connect to {peer_id} failed: {e}");
                    }
                }

                // Exponential backoff
                tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        });
    }

    /// Get a peer handle by ID.
    pub async fn get_peer(&self, peer_id: &str) -> Option<PeerHandle> {
        let peers = self.peers.read().await;
        peers.get(peer_id).cloned()
    }

    /// List all connected peer IDs.
    pub async fn connected_peer_ids(&self) -> Vec<String> {
        let peers = self.peers.read().await;
        peers.keys().cloned().collect()
    }
}

/// Handle an incoming WebSocket connection (server side).
async fn handle_incoming_connection(
    stream: TcpStream,
    peers: Arc<RwLock<HashMap<String, PeerHandle>>>,
    cipher: Arc<Cipher>,
    incoming_tx: mpsc::Sender<(String, WireMessage)>,
    our_id: String,
) -> Result<(), String> {
    let ws_stream = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| format!("WebSocket upgrade error: {e}"))?;

    let (mut ws_write, mut ws_read) = ws_stream.split();

    // Wait for "hello" message to identify the peer
    let peer_id = match ws_read.next().await {
        Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
            let msg = WireMessage::from_encrypted(&data, &cipher)
                .map_err(|e| format!("Hello decrypt error: {e}"))?;
            if msg.msg_type != "hello" {
                return Err(format!("Expected 'hello', got '{}'", msg.msg_type));
            }
            String::from_utf8(msg.payload).map_err(|e| format!("Invalid peer ID: {e}"))?
        }
        other => {
            return Err(format!("No hello message received: {other:?}"));
        }
    };

    tracing::info!("Peer {peer_id} connected (incoming)");

    // Send our own hello back
    let hello_msg = WireMessage::new("hello", our_id.clone().into_bytes());
    let encrypted = hello_msg.to_encrypted(&cipher)?;
    ws_write
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            encrypted.into(),
        ))
        .await
        .map_err(|e| format!("Hello send error: {e}"))?;

    // Create peer channel
    let (peer_tx, peer_rx) = mpsc::channel::<WireMessage>(64);

    // Store peer
    {
        let mut p = peers.write().await;
        p.insert(
            peer_id.clone(),
            PeerHandle {
                peer_id: peer_id.clone(),
                tx: peer_tx,
            },
        );
    }

    // Spawn write task
    let write_cipher = cipher.clone();
    let write_peer_id = peer_id.clone();
    tokio::spawn(async move {
        let mut peer_rx2 = peer_rx;
        while let Some(msg) = peer_rx2.recv().await {
            match msg.to_encrypted(&write_cipher) {
                Ok(data) => {
                    if let Err(e) = ws_write
                        .send(tokio_tungstenite::tungstenite::Message::Binary(data.into()))
                        .await
                    {
                        tracing::warn!("Write to peer {write_peer_id} failed: {e}");
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("Encrypt error for {write_peer_id}: {e}");
                }
            }
        }
    });

    // Read loop
    let read_peer_id = peer_id.clone();
    loop {
        match ws_read.next().await {
            Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
                match WireMessage::from_encrypted(&data, &cipher) {
                    Ok(msg) => {
                        if incoming_tx.send((read_peer_id.clone(), msg)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Decrypt error from {read_peer_id}: {e}");
                    }
                }
            }
            Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                tracing::info!("Peer {read_peer_id} disconnected (incoming)");
                break;
            }
            Some(Err(e)) => {
                tracing::warn!("WS read error from {read_peer_id}: {e}");
                break;
            }
            _ => {}
        }
    }

    // Remove peer on disconnect
    {
        let mut p = peers.write().await;
        p.remove(&peer_id);
    }

    Ok(())
}

/// Connect to a peer and perform the WebSocket handshake (client side).
/// Returns the full WebSocket stream on success.
async fn connect_and_handshake(
    addr: &SocketAddr,
    cipher: &Cipher,
    our_id: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>, String>
{
    let url = format!("ws://{addr}");
    let (mut ws_stream, _response) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| format!("WebSocket connect error: {e}"))?;

    // Send hello with our identity
    let hello_msg = WireMessage::new("hello", our_id.as_bytes().to_vec());
    let encrypted = hello_msg.to_encrypted(cipher)?;
    ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            encrypted.into(),
        ))
        .await
        .map_err(|e| format!("Hello send error: {e}"))?;

    // Wait for peer's hello
    match ws_stream.next().await {
        Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
            let msg = WireMessage::from_encrypted(&data, cipher)
                .map_err(|e| format!("Hello decrypt error: {e}"))?;
            if msg.msg_type != "hello" {
                return Err(format!("Expected 'hello', got '{}'", msg.msg_type));
            }
            let peer_id =
                String::from_utf8(msg.payload).map_err(|e| format!("Invalid peer ID: {e}"))?;
            tracing::info!("Handshake complete with peer {peer_id}");
        }
        other => {
            return Err(format!("No hello response: {other:?}"));
        }
    }

    Ok(ws_stream)
}
