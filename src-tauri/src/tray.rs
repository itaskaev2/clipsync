//! System tray module — tray icon and right-click menu.
//!
//! Uses the Tauri 2.x built-in tray API.
//!
//! ```text
//! Status: connected (1)     (live, non-clickable)
//! ──────────────
//! Open Settings
//! Pause Sync / Resume Sync
//! ──────────────
//! Exit
//! ```
//!
//! # Behavior
//! - The app has no taskbar window; it lives in the tray.
//! - Left-click or "Open Settings" shows the settings/pairing window.
//! - Closing the settings window only hides it — the app keeps syncing.
//! - "Pause Sync" toggles syncing directly in the backend config (works even
//!   when no window is open) and flips its own label.
//! - Exit is only via the tray "Exit" item.

use tauri::{
    menu::{MenuBuilder, MenuItem, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, Runtime,
};

/// Handles needed to update the tray after construction.
pub struct TrayHandles<R: Runtime> {
    status_item: MenuItem<R>,
    pause_item: MenuItem<R>,
    tray: TrayIcon<R>,
}

/// Build and register the system tray icon.
pub fn setup_tray<R: Runtime>(app: &AppHandle<R>) -> Result<(), Box<dyn std::error::Error>> {
    let status_item = MenuItemBuilder::with_id("status", "Status: starting…")
        .enabled(false)
        .build(app)?;
    let show_settings = MenuItemBuilder::with_id("open_settings", "Open Settings").build(app)?;
    let pause_item = MenuItemBuilder::with_id("pause_sync", "Pause Sync").build(app)?;
    let exit = MenuItemBuilder::with_id("exit", "Exit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&status_item)
        .separator()
        .item(&show_settings)
        .item(&pause_item)
        .separator()
        .item(&exit)
        .build()?;

    // Prefer the bundled window icon, copied into an owned ('static) image so
    // no borrow of `app` escapes; otherwise fall back to a generated icon.
    let icon = app
        .default_window_icon()
        .map(|img| {
            tauri::image::Image::new_owned(img.rgba().to_vec(), img.width(), img.height())
        })
        .unwrap_or_else(fallback_icon);

    let tray = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("ClipSync — LAN clipboard sync")
        .icon(icon)
        .on_menu_event(move |app_handle, event| match event.id().as_ref() {
            "open_settings" => open_settings_window(app_handle),
            "pause_sync" => toggle_pause_sync(app_handle),
            "exit" => {
                tracing::info!("Exit requested via tray menu.");
                app_handle.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray_handle, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                open_settings_window(tray_handle.app_handle());
            }
        })
        .build(app)?;

    app.manage(TrayHandles {
        status_item,
        pause_item,
        tray,
    });

    tracing::info!("System tray icon created.");
    Ok(())
}

/// Update the tray's status line, pause label, and tooltip.
pub fn set_status<R: Runtime>(app: &AppHandle<R>, connected: usize, paused: bool) {
    let Some(h) = app.try_state::<TrayHandles<R>>() else {
        return;
    };
    let (status, tip) = if paused {
        ("Status: paused".to_string(), "ClipSync — paused")
    } else if connected == 0 {
        (
            "Status: waiting for peer".to_string(),
            "ClipSync — waiting for peer",
        )
    } else {
        (
            format!("Status: connected ({connected})"),
            "ClipSync — connected",
        )
    };
    let _ = h.status_item.set_text(status);
    let _ = h
        .pause_item
        .set_text(if paused { "Resume Sync" } else { "Pause Sync" });
    let _ = h.tray.set_tooltip(Some(tip));
}

/// Open (or reveal) the settings/pairing window.
fn open_settings_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }

    match tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("ClipSync")
    .inner_size(420.0, 620.0)
    .resizable(false)
    .skip_taskbar(false)
    .center()
    .build()
    {
        Ok(window) => {
            // Closing the settings window hides it instead of exiting the app.
            let clone = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = clone.hide();
                }
            });
        }
        Err(e) => tracing::error!("Failed to create settings window: {e}"),
    }
}

/// Toggle pause/resume directly in the backend config so it works even with no
/// window open, then refresh the tray and notify the UI.
fn toggle_pause_sync<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<crate::AppState>();
        let paused = {
            let mut cfg = state.config.write().await;
            cfg.sync_paused = !cfg.sync_paused;
            crate::config::save_config(&cfg);
            cfg.sync_paused
        };
        let connected = state.transport.connected_peer_ids().await.len();
        tracing::info!("Sync {} via tray.", if paused { "paused" } else { "resumed" });
        set_status(&app, connected, paused);
        let _ = app.emit(
            "clipsync:status-changed",
            serde_json::json!({ "peers": connected, "paused": paused }),
        );
    });
}

/// A visible 32×32 purple fallback icon (so the tray shows even without a
/// bundled icon, e.g. in `cargo run`).
fn fallback_icon() -> tauri::image::Image<'static> {
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    let r = (size as f32 / 2.5).powi(2);
    for y in 0..size {
        for x in 0..size {
            let idx = (y * size + x) * 4;
            let dx = x as f32 - size as f32 / 2.0;
            let dy = y as f32 - size as f32 / 2.0;
            if dx * dx + dy * dy < r {
                rgba[idx] = 99;
                rgba[idx + 1] = 102;
                rgba[idx + 2] = 241;
                rgba[idx + 3] = 255;
            }
        }
    }
    tauri::image::Image::new_owned(rgba, size as u32, size as u32)
}
