//! ClipSync — LAN clipboard synchronization.
//!
//! Architecture:
//! ```text
//! main.rs (Tauri setup + wiring)
//!   ├── config.rs       (load/save settings + allowlist)
//!   ├── clipboard.rs    (watch + read + write)
//!   ├── discovery.rs    (mDNS register + browse)
//!   ├── pairing.rs      (code, key derivation, encryption)
//!   ├── transport.rs    (encrypted WS server + client, swappable cipher)
//!   ├── sync.rs         (core engine: send/receive coordination)
//!   └── tray.rs         (system tray icon + menu)
//! ```

// Hide the console window on Windows release builds (tray-only app).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clipboard;
mod config;
mod discovery;
mod pairing;
mod sync;
mod transport;
mod tray;

use config::{ClipboardPriority, ConfigUpdate, FrontendConfig, FrontendPeer, SharedConfig};
use pairing::WireMessage;
use std::collections::HashSet;
use std::sync::Arc;
use tauri::Emitter;
use tauri_plugin_notification::NotificationExt;
use tokio::sync::{mpsc, RwLock};
use transport::{TransportEvent, TransportManager};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Global application state shared with Tauri commands.
pub struct AppState {
    pub config: SharedConfig,
    pub transport: Arc<TransportManager>,
}

/// A user-facing notification request (shown as an OS toast + UI event).
pub struct UserNotification {
    pub title: String,
    pub body: String,
}

fn main() {
    init_logging();
    tracing::info!("ClipSync v{} starting...", env!("CARGO_PKG_VERSION"));

    // Load configuration.
    let config = config::load_config();
    tracing::info!(
        "Instance ID: {}, payload limit: {} MB, debounce: {} ms, paired: {}",
        config.instance_id,
        config.payload_limit_mb,
        config.debounce_ms,
        config.pairing_code.is_some()
    );
    let instance_id = config.instance_id.clone();
    let shared_config: SharedConfig = Arc::new(RwLock::new(config));

    // Channels that bridge the transport layer and the sync engine.
    let (incoming_tx, incoming_rx) = mpsc::channel::<(String, WireMessage)>(256);
    let (event_tx, event_rx) = mpsc::channel::<TransportEvent>(64);

    let transport = Arc::new(TransportManager::new(instance_id, incoming_tx, event_tx));

    let app_state = AppState {
        config: shared_config.clone(),
        transport: transport.clone(),
    };

    // Move the receiver ends into the setup closure (FnOnce).
    let mut incoming_rx = Some(incoming_rx);
    let mut event_rx = Some(event_rx);

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .manage(app_state)
        .setup(move |app| {
            // Invisible, never-closing window keeps the event loop alive so the
            // app survives with only the tray (Tauri v2 exits when all windows
            // close).
            let hidden = tauri::WebviewWindowBuilder::new(
                app,
                "hidden",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("ClipSync")
            .visible(false)
            .skip_taskbar(true)
            .build()?;
            let hidden_clone = hidden.clone();
            hidden.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = hidden_clone.hide();
                }
            });

            tray::setup_tray(&app.handle().clone())?;

            let cfg = shared_config.clone();
            let transport = transport.clone();
            let app_handle = app.handle().clone();
            let incoming_rx = incoming_rx.take().expect("setup runs once");
            let event_rx = event_rx.take().expect("setup runs once");

            tauri::async_runtime::spawn(async move {
                start_background_services(cfg, transport, app_handle, incoming_rx, event_rx).await;
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            update_config,
            get_peers,
            pair_with_code,
            generate_code,
            get_status,
            get_version,
        ])
        .run(tauri::generate_context!())
        .expect("Failed to launch ClipSync");
}

/// Initialize logging — console (INFO) + rotating JSON file (DEBUG).
fn init_logging() {
    let log_dir = directories::ProjectDirs::from("com", "clipsync", "ClipSync")
        .map(|d| d.data_local_dir().join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));

    let file_appender = tracing_appender::rolling::hourly(&log_dir, "clipsync.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    // Keep the writer guard alive for the process lifetime.
    std::mem::forget(guard);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(file_writer)
        .with_filter(env_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();
}

/// Start clipboard watcher, mDNS discovery, transport server, notifier, and the
/// sync engine. Never returns.
async fn start_background_services(
    config: SharedConfig,
    transport: Arc<TransportManager>,
    app: tauri::AppHandle,
    incoming_rx: mpsc::Receiver<(String, WireMessage)>,
    event_rx: mpsc::Receiver<TransportEvent>,
) {
    // Start the WebSocket server; the OS-assigned port is advertised via mDNS.
    let ws_port = match transport.start_server().await {
        Ok(port) => port,
        Err(e) => {
            tracing::error!("WebSocket server failed to start: {e}");
            return;
        }
    };

    let (instance_id, hostname, saved_code, paused) = {
        let cfg = config.read().await;
        (
            cfg.instance_id.clone(),
            hostname_string(),
            cfg.pairing_code.clone(),
            cfg.sync_paused,
        )
    };

    // Resume pairing across restarts: install the saved code's cipher.
    if let Some(code) = saved_code {
        transport.set_pairing_code(&code).await;
        tracing::info!("Resumed pairing from saved config.");
    }

    // Channels for this run.
    let (clipboard_tx, clipboard_rx) = mpsc::channel::<clipboard::ClipboardContent>(64);
    let (discovery_tx, discovery_rx) = mpsc::channel::<discovery::DiscoveredPeer>(32);
    let (notify_tx, mut notify_rx) = mpsc::channel::<UserNotification>(16);

    // Clipboard watcher.
    {
        let cfg = config.clone();
        tokio::spawn(async move {
            clipboard::start_watcher(clipboard_tx, notify_tx, cfg).await;
        });
    }

    // mDNS discovery — non-fatal if unavailable. Keep the daemon alive for the
    // process lifetime (dropping it would unregister the service).
    let _daemon = match discovery::start_discovery(instance_id, hostname, ws_port, discovery_tx) {
        Ok(d) => {
            tracing::info!("mDNS discovery started.");
            Some(d)
        }
        Err(e) => {
            tracing::warn!("mDNS unavailable (non-fatal): {e}. Pairing still works once peers are reachable.");
            None
        }
    };

    // OS notification + UI event pump.
    {
        let app = app.clone();
        tokio::spawn(async move {
            while let Some(n) = notify_rx.recv().await {
                if let Err(e) = app.notification().builder().title(&n.title).body(&n.body).show() {
                    tracing::warn!("OS notification failed: {e}");
                }
                let _ = app.emit(
                    "clipsync:notification",
                    serde_json::json!({ "title": n.title, "body": n.body }),
                );
            }
        });
    }

    // Sync engine.
    {
        let cfg = config.clone();
        let transport = transport.clone();
        let app = app.clone();
        tokio::spawn(async move {
            sync::run_engine(
                clipboard_rx,
                discovery_rx,
                incoming_rx,
                event_rx,
                cfg,
                transport,
                app,
            )
            .await;
        });
    }

    // Initial tray/status reflect the loaded state.
    tray::set_status(&app, 0, paused);
    let _ = app.emit(
        "clipsync:status-changed",
        serde_json::json!({ "peers": 0, "paused": paused }),
    );

    tracing::info!("Background services started.");
    // Hold `_daemon` (and this task) alive forever.
    std::future::pending::<()>().await;
    drop(_daemon);
}

fn hostname_string() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "clipsync".to_string())
}

// ─── Tauri Commands ────────────────────────────────────────────

/// Get the current frontend-visible config (includes the pairing code).
#[tauri::command]
async fn get_config(state: tauri::State<'_, AppState>) -> Result<FrontendConfig, String> {
    let cfg = state.config.read().await;
    Ok(FrontendConfig::from(&*cfg))
}

/// Update config from the settings form.
#[tauri::command]
async fn update_config(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    update: ConfigUpdate,
) -> Result<(), String> {
    let paused = {
        let mut cfg = state.config.write().await;
        if let Some(limit) = update.payload_limit_mb {
            cfg.payload_limit_mb = limit;
        }
        if let Some(debounce) = update.debounce_ms {
            cfg.debounce_ms = debounce;
        }
        if let Some(ref priority) = update.clipboard_priority {
            cfg.clipboard_priority = match priority.as_str() {
                "image_first" => ClipboardPriority::ImageFirst,
                "text_first" => ClipboardPriority::TextFirst,
                "text_only" => ClipboardPriority::TextOnly,
                "image_only" => ClipboardPriority::ImageOnly,
                other => return Err(format!("Unknown clipboard priority: {other}")),
            };
        }
        if let Some(p) = update.sync_paused {
            cfg.sync_paused = p;
        }
        config::save_config(&cfg);
        cfg.sync_paused
    };

    let connected = state.transport.connected_peer_ids().await.len();
    tray::set_status(&app, connected, paused);
    let _ = app.emit(
        "clipsync:status-changed",
        serde_json::json!({ "peers": connected, "paused": paused }),
    );
    tracing::info!("Config updated and saved.");
    Ok(())
}

/// List paired peers with live connection status.
#[tauri::command]
async fn get_peers(state: tauri::State<'_, AppState>) -> Result<Vec<FrontendPeer>, String> {
    let connected: HashSet<String> = state
        .transport
        .connected_peer_ids()
        .await
        .into_iter()
        .collect();
    let cfg = state.config.read().await;
    Ok(cfg
        .paired_peers
        .iter()
        .map(|p| FrontendPeer {
            name: p.name.clone(),
            id: p.id.clone(),
            connected: connected.contains(&p.id),
        })
        .collect())
}

/// Pair using a code shown on the other machine.
#[tauri::command]
async fn pair_with_code(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    code: String,
) -> Result<(), String> {
    if !pairing::is_valid_pairing_code(&code) {
        return Err("Invalid pairing code: must be 6 digits.".to_string());
    }
    {
        let mut cfg = state.config.write().await;
        cfg.pairing_code = Some(code.clone());
        config::save_config(&cfg);
    }
    // Install the derived key; existing/connecting peers (re)handshake with it.
    state.transport.set_pairing_code(&code).await;

    let _ = app.emit(
        "clipsync:notification",
        serde_json::json!({
            "title": "Pairing",
            "body": format!("Code {code} accepted. Connecting to peers with the same code…"),
        }),
    );
    tracing::info!("Pairing code accepted via UI.");
    Ok(())
}

/// Generate a new random 6-digit pairing code and start using it.
#[tauri::command]
async fn generate_code(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let code = pairing::generate_pairing_code();
    {
        let mut cfg = state.config.write().await;
        cfg.pairing_code = Some(code.clone());
        config::save_config(&cfg);
    }
    state.transport.set_pairing_code(&code).await;
    tracing::info!("Generated new pairing code.");
    Ok(code)
}

/// App version string, baked in at build time from Cargo.toml. Lets the UI
/// show exactly which build is running (both peers must match).
#[tauri::command]
fn get_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Current sync status.
#[tauri::command]
async fn get_status(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let connected = state.transport.connected_peer_ids().await.len();
    let cfg = state.config.read().await;
    Ok(serde_json::json!({
        "instance_id": cfg.instance_id,
        "sync_paused": cfg.sync_paused,
        "paired_peers_count": cfg.paired_peers.len(),
        "connected_peers": connected,
    }))
}
