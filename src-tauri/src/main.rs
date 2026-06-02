//! ClipSync — LAN clipboard synchronization.
//!
//! Architecture:
//! ```
//! main.rs (Tauri setup + wiring)
//!   ├── config.rs       (load/save settings)
//!   ├── clipboard.rs    (watch + read + write)
//!   ├── discovery.rs    (mDNS register + browse)
//!   ├── pairing.rs      (code, key derivation, encryption)
//!   ├── transport.rs    (encrypted WS server + client)
//!   ├── sync.rs         (core engine: send/receive coordination)
//!   └── tray.rs         (system tray icon + menu)
//! ```

mod clipboard;
mod config;
mod discovery;
mod pairing;
mod sync;
mod transport;
mod tray;

use config::{AppConfig, ClipboardPriority, ConfigUpdate, FrontendConfig, FrontendPeer};
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::RwLock;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Global application state shared across all modules.
struct AppState {
    config: Arc<RwLock<AppConfig>>,
}

#[tokio::main]
async fn main() {
    // Initialize logging — console (stdout) + rotating file (logs/ dir)
    let log_dir = directories::ProjectDirs::from("com", "clipsync", "ClipSync")
        .map(|d| d.data_local_dir().join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));

    let file_appender = tracing_appender::rolling::hourly(&log_dir, "clipsync.log");
    let (file_writer, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug"));

    // Console layer: human-readable, INFO level
    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);

    // File layer: JSON for structured parsing, DEBUG level
    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(file_writer)
        .with_filter(env_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    // Keep the file writer guard alive for the lifetime of the app
    std::mem::forget(_guard);

    tracing::info!("ClipSync v{} starting...", env!("CARGO_PKG_VERSION"));

    // Load configuration
    let config = config::load_config();
    tracing::info!(
        "Instance ID: {}, payload limit: {} MB, debounce: {} ms",
        config.instance_id,
        config.payload_limit_mb,
        config.debounce_ms
    );

    let shared_config = Arc::new(RwLock::new(config));
    let app_state = AppState {
        config: shared_config.clone(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .manage(app_state)
        .setup(move |app| {
            // --- Create an invisible persistent window (keeps event loop alive) ---
            // Tauri v2 exits when all windows close — tray alone doesn't prevent exit.
            // This hidden window is never shown, never appears in taskbar, and never closes.
            let _hidden = tauri::WebviewWindowBuilder::new(
                app,
                "hidden",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("ClipSync")
            .visible(false)
            .skip_taskbar(true)
            .build()?;
            // Prevent the hidden window from being closed (would exit the app)
            let hidden_clone = _hidden.clone();
            _hidden.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    let _ = hidden_clone.hide();
                }
            });

            // --- Create system tray ---
            let handle = app.handle().clone();
            tray::setup_tray(&handle)?;

            // --- Start background services ---
            let config = shared_config.clone();
            let app_handle = app.handle().clone();

            tauri::async_runtime::spawn(async move {
                start_background_services(config, app_handle).await;
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
        ])
        .run(tauri::generate_context!())
        .expect("Failed to launch ClipSync");
}

/// Start all background services: clipboard watcher, mDNS, transport, sync engine.
async fn start_background_services(config: Arc<RwLock<AppConfig>>, app: tauri::AppHandle) {
    let cfg = config.read().await;

    // --- Determine port ---
    let ws_port = find_available_port().await.unwrap_or(19876);
    tracing::info!("Using WebSocket port: {ws_port}");

    // --- Cipher (derived from pairing code if set, otherwise default) ---
    let pairing_code = cfg
        .pairing_code
        .clone()
        .unwrap_or_else(|| "000000".to_string());
    let cipher = Arc::new(pairing::Cipher::from_pairing_code(&pairing_code));

    let instance_id = cfg.instance_id.clone();
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "clipsync".to_string());

    // --- Channels ---
    // Clipboard watcher → sync engine
    let (clipboard_tx, clipboard_rx) =
        tokio::sync::mpsc::channel::<clipboard::ClipboardContent>(64);

    // mDNS discovery → sync engine
    let (discovery_tx, discovery_rx) = tokio::sync::mpsc::channel::<discovery::DiscoveredPeer>(32);

    // Transport (incoming from peers) → sync engine
    let (incoming_tx, incoming_rx) =
        tokio::sync::mpsc::channel::<(String, pairing::WireMessage)>(256);

    // --- Transport manager (receives the incoming sender) ---
    let transport = Arc::new(transport::TransportManager::new(
        cipher.clone(),
        instance_id.clone(),
        incoming_tx,
    ));

    // --- Start WebSocket server ---
    let transport_server = transport.clone();
    tokio::spawn(async move {
        if let Err(e) = transport_server.start_server(ws_port).await {
            tracing::error!("WebSocket server error: {e}");
        }
    });

    // --- Start clipboard watcher ---
    let debounce = cfg.debounce_ms;
    let priority = cfg.clipboard_priority.clone();
    let limit = (cfg.payload_limit_mb as u64) * 1024 * 1024;
    drop(cfg); // Release the read lock before spawning

    tokio::spawn(async move {
        clipboard::start_watcher(clipboard_tx, debounce, priority, limit).await;
    });

    // --- Start mDNS discovery (non-fatal if it fails) ---
    let instance_id2 = instance_id.clone();
    let hostname2 = hostname.clone();
    match discovery::start_discovery(instance_id2, hostname2, ws_port, discovery_tx) {
        Ok(_d) => {
            tracing::info!("mDNS discovery started");
        }
        Err(e) => {
            tracing::warn!("mDNS discovery unavailable (non-fatal): {e}");
            tracing::warn!("ClipSync will work without auto-discovery. Use manual pairing.");
        }
    };

    // --- Start sync engine ---
    let sync_config = config.clone();
    let sync_transport = transport.clone();

    tokio::spawn(async move {
        sync::run_engine(
            clipboard_rx,
            discovery_rx,
            incoming_rx,
            sync_config,
            sync_transport,
        )
        .await;
    });

    // Notify the frontend that we're running
    let _ = app.emit(
        "clipsync:status-changed",
        serde_json::json!({
            "connected": true,
            "paused": false,
        }),
    );

    // Keep the background task alive forever
    tracing::info!("Background services started. Waiting for exit...");
    std::future::pending::<()>().await;
}

/// Find an available TCP port.
async fn find_available_port() -> Option<u16> {
    use tokio::net::TcpListener;
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 0));
    TcpListener::bind(&addr)
        .await
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

// ─── Tauri Commands ────────────────────────────────────────────

/// Get the current frontend-visible config.
#[tauri::command]
async fn get_config(state: tauri::State<'_, AppState>) -> Result<FrontendConfig, String> {
    let cfg = state.config.read().await;
    Ok(FrontendConfig::from(&*cfg))
}

/// Update config from the frontend settings form.
#[tauri::command]
async fn update_config(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    update: ConfigUpdate,
) -> Result<(), String> {
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
            _ => return Err(format!("Unknown clipboard priority: {priority}")),
        };
    }
    if let Some(paused) = update.sync_paused {
        cfg.sync_paused = paused;
    }

    // Persist to disk
    config::save_config(&cfg);

    // Notify frontend
    let _ = app.emit(
        "clipsync:status-changed",
        serde_json::json!({
            "connected": true,
            "paused": cfg.sync_paused,
        }),
    );

    tracing::info!("Config updated and saved.");
    Ok(())
}

/// Get the list of paired peers and their connection status.
#[tauri::command]
async fn get_peers(state: tauri::State<'_, AppState>) -> Result<Vec<FrontendPeer>, String> {
    let cfg = state.config.read().await;
    let peers: Vec<FrontendPeer> = cfg
        .paired_peers
        .iter()
        .map(|p| FrontendPeer {
            name: p.name.clone(),
            id: p.id.clone(),
            connected: false, // TODO: query transport layer for live status
        })
        .collect();
    Ok(peers)
}

/// Initiate pairing with a 6-digit code.
#[tauri::command]
async fn pair_with_code(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    code: String,
) -> Result<(), String> {
    if !pairing::is_valid_pairing_code(&code) {
        return Err("Invalid pairing code: must be 6 digits.".to_string());
    }

    let mut cfg = state.config.write().await;

    // Store the pairing code (used to derive the shared key)
    cfg.pairing_code = Some(code.clone());

    // Add a placeholder peer — the actual peer info will be filled
    // when mDNS discovers the paired peer.
    config::save_config(&cfg);

    // Notify the frontend
    let _ = app.emit(
        "clipsync:notification",
        serde_json::json!({
            "title": "Pairing",
            "body": format!("Pairing code {} accepted. Looking for peer...", code),
        }),
    );

    tracing::info!("Pairing code {} accepted.", code);
    Ok(())
}

/// Generate a new random 6-digit pairing code.
#[tauri::command]
async fn generate_code(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let code = pairing::generate_pairing_code();
    let mut cfg = state.config.write().await;
    cfg.pairing_code = Some(code.clone());
    config::save_config(&cfg);
    tracing::info!("Generated new pairing code: {code}");
    Ok(code)
}

/// Get current sync status.
#[tauri::command]
async fn get_status(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let cfg = state.config.read().await;
    Ok(serde_json::json!({
        "instance_id": cfg.instance_id,
        "sync_paused": cfg.sync_paused,
        "paired_peers_count": cfg.paired_peers.len(),
    }))
}
