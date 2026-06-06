//! End-to-end sync tests.
//!
//! These exercise the *real* transport stack over a loopback TCP socket: two
//! [`TransportManager`]s (one server, one client) complete the encrypted
//! ChaCha20-Poly1305 handshake, then a [`WireMessage`] carrying a serialized
//! [`ClipboardContent`] is sent and asserted to arrive — decrypted and intact —
//! on the peer's incoming channel. Nothing below the public transport API is
//! mocked, so this is the closest we get to "copy on machine A, paste on
//! machine B" without a second machine or the GUI.
//!
//! Why this lives in the crate rather than `tests/`: clipsync is a binary crate
//! with no library target, so an integration test in `tests/` cannot reach
//! `TransportManager`. A `#[cfg(test)]` module keeps full access to the internal
//! API while still running under `cargo test`.
#![cfg(test)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::clipboard::ClipboardContent;
use crate::pairing::WireMessage;
use crate::transport::{TransportEvent, TransportManager};

/// Initialize tracing once, honoring `RUST_LOG`, so transport warnings surface
/// under `cargo test -- --nocapture`. No-op if a subscriber is already set.
fn init_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("clipsync=debug")),
            )
            .try_init();
    });
}

/// A test peer: a [`TransportManager`] plus the receiving ends of its channels.
/// The receivers are held so their senders (owned by the manager) stay open for
/// the lifetime of the test.
struct TestPeer {
    manager: Arc<TransportManager>,
    incoming_rx: mpsc::Receiver<(String, WireMessage)>,
    // Kept alive so the transport's event sender doesn't close; not asserted on.
    _event_rx: mpsc::Receiver<TransportEvent>,
    id: String,
}

impl TestPeer {
    fn new(id: &str) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::channel(64);
        let (event_tx, event_rx) = mpsc::channel(64);
        let manager = Arc::new(TransportManager::new(id.to_string(), incoming_tx, event_tx));
        Self {
            manager,
            incoming_rx,
            _event_rx: event_rx,
            id: id.to_string(),
        }
    }
}

/// Poll until `manager` reports `peer_id` connected, or panic after `timeout`.
async fn wait_connected(manager: &TransportManager, peer_id: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if manager
            .connected_peer_ids()
            .await
            .iter()
            .any(|p| p == peer_id)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {peer_id} to connect"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Bring up a connected server+client pair sharing `code`. The client (lower
/// instance id) is the side that dials out — the transport's tie-break makes the
/// higher id wait to accept — so exactly one connection forms. Returns both
/// peers, each already reporting the other connected.
async fn connected_pair(code: &str) -> (TestPeer, TestPeer) {
    init_tracing();
    let server = TestPeer::new("zzzz-server");
    let client = TestPeer::new("aaaa-client");

    server.manager.set_pairing_code(code).await;
    client.manager.set_pairing_code(code).await;

    let port = server.manager.start_server().await.expect("server starts");
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();

    client.manager.connect_to_peer(server.id.clone(), addr).await;

    wait_connected(&client.manager, &server.id, Duration::from_secs(10)).await;
    wait_connected(&server.manager, &client.id, Duration::from_secs(10)).await;

    (server, client)
}

/// Send `content` from `client` to `server` and return what `server` receives.
async fn round_trip(server: &mut TestPeer, client: &TestPeer, content: &ClipboardContent) -> (String, ClipboardContent) {
    let payload = rmp_serde::to_vec(content).expect("serialize clipboard");
    client
        .manager
        .get_peer(&server.id)
        .await
        .expect("peer handle exists once connected")
        .send(WireMessage::new("clipboard", payload))
        .await
        .expect("send to peer");

    let (from, msg) = tokio::time::timeout(Duration::from_secs(15), server.incoming_rx.recv())
        .await
        .expect("message arrives before timeout")
        .expect("incoming channel still open");
    assert_eq!(msg.msg_type, "clipboard");
    let got: ClipboardContent = rmp_serde::from_slice(&msg.payload).expect("deserialize clipboard");
    (from, got)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_clipboard_syncs_end_to_end() {
    let (mut server, client) = connected_pair("123456").await;

    let content = ClipboardContent {
        content_type: "text".into(),
        text: Some("hello from the other side".into()),
        image_data: None,
        content_hash: "hash-text".into(),
    };

    let (from, got) = round_trip(&mut server, &client, &content).await;
    assert_eq!(from, client.id);
    assert_eq!(got.content_type, "text");
    assert_eq!(got.text.as_deref(), Some("hello from the other side"));
    assert!(got.image_data.is_none());
    assert_eq!(got.content_hash, "hash-text");
}

/// Regression test for the v0.1.4 image fix. Raw RGBA clipboard images exceed
/// tungstenite's default 16 MiB frame limit, so before `ws_config()` raised the
/// limit to 128 MiB the frame was rejected and the connection reset — images
/// silently failed to sync while small text worked. A >16 MiB image payload
/// must now transit the encrypted socket intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_image_clipboard_syncs_end_to_end() {
    let (mut server, client) = connected_pair("654321").await;

    // 2100x2100 RGBA ≈ 16.8 MiB, comfortably above the old 16 MiB default. The
    // container format is [4-byte width BE][4-byte height BE][RGBA pixels...].
    let (w, h) = (2100u32, 2100u32);
    let pixels = (w as usize) * (h as usize) * 4;
    let mut image_data = Vec::with_capacity(8 + pixels);
    image_data.extend_from_slice(&w.to_be_bytes());
    image_data.extend_from_slice(&h.to_be_bytes());
    image_data.resize(8 + pixels, 0xAB);
    assert!(
        image_data.len() > 16 * 1024 * 1024,
        "test image must exceed the old 16 MiB frame limit to be a valid regression"
    );

    let content = ClipboardContent {
        content_type: "image".into(),
        text: None,
        image_data: Some(image_data.clone()),
        content_hash: "hash-image".into(),
    };

    let (from, got) = round_trip(&mut server, &client, &content).await;
    assert_eq!(from, client.id);
    assert_eq!(got.content_type, "image");
    assert_eq!(got.image_data.as_deref(), Some(image_data.as_slice()));
    assert_eq!(got.content_hash, "hash-image");
}

/// The pairing code is the security boundary: a client with the wrong code
/// cannot complete the encrypted handshake, so no connection is established and
/// nothing syncs. (The server cannot decrypt the client's `hello`; the client
/// never gets a `hello` back.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_pairing_code_never_connects() {
    let server = TestPeer::new("zzzz-server");
    let client = TestPeer::new("aaaa-client");

    server.manager.set_pairing_code("111111").await;
    client.manager.set_pairing_code("222222").await; // mismatched code

    let port = server.manager.start_server().await.expect("server starts");
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    client.manager.connect_to_peer(server.id.clone(), addr).await;

    // Allow several failed handshake/backoff cycles to elapse.
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert!(
        client.manager.connected_peer_ids().await.is_empty(),
        "client must not connect with the wrong pairing code"
    );
    assert!(
        server.manager.connected_peer_ids().await.is_empty(),
        "server must not accept a peer with the wrong pairing code"
    );
}
