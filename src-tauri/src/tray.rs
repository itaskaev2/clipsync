//! System tray module — tray icon and right-click menu.
//!
//! Uses Tauri 2.x built-in tray API (`tauri::tray::TrayIconBuilder`).
//!
//! # Menu structure
//! ```
//! ClipSync (status indicator)
//! ──────────────
//! Peers
//!   ↳ Peer A (online)
//!   ↳ Peer B (online)
//! ──────────────
//! Pause Sync / Resume Sync
//! Open Settings
//! ──────────────
//! Exit
//! ```
//!
//! # Behavior
//! - The app has NO taskbar window. It lives only in the tray.
//! - Clicking "Open Settings" creates a Tauri webview window.
//! - Closing the settings window does NOT exit the app — it just hides.
//! - Exit is only via the tray "Exit" menu item.

use tauri::{
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Runtime,
};

/// Build and register the system tray icon.
pub fn setup_tray<R: Runtime>(app: &AppHandle<R>) -> Result<(), Box<dyn std::error::Error>> {
    // --- Build menu ---
    let show_settings = MenuItemBuilder::with_id("open_settings", "Open Settings").build(app)?;

    let pause_sync = MenuItemBuilder::with_id("pause_sync", "Pause Sync").build(app)?;

    let exit = MenuItemBuilder::with_id("exit", "Exit").build(app)?;

    let peers_menu = SubmenuBuilder::new(app, "Peers")
        .text("no-peers", "No peers connected")
        .build()?;

    let menu = MenuBuilder::new(app)
        .item(&show_settings)
        .separator()
        .item(&pause_sync)
        .item(&peers_menu)
        .separator()
        .item(&exit)
        .build()?;

    // --- Build tray icon ---
    // For MVP we use a built-in icon. In production, bundle a proper icon file.
    let tray = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("ClipSync — LAN clipboard sync")
        .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
            // Fallback: create a minimal 32x32 RGBA icon (purple dot)
            tauri::Icon::Rgba {
                rgba: vec![0; 32 * 32 * 4],
                width: 32,
                height: 32,
            }
        }))
        .on_menu_event(move |app_handle, event| match event.id().as_ref() {
            "open_settings" => {
                open_settings_window(app_handle);
            }
            "pause_sync" => {
                toggle_pause_sync(app_handle);
            }
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
                // Left-click opens settings
                open_settings_window(tray_handle.app_handle());
            }
        })
        .build(app)?;

    // Store tray handle in app state for later menu updates
    app.manage(tray);

    tracing::info!("System tray icon created.");

    Ok(())
}

/// Open the settings/paring UI window.
fn open_settings_window<R: Runtime>(app: &AppHandle<R>) {
    // Check if window already exists
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }

    // Create a new window
    match tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("ClipSync")
    .inner_size(420.0, 560.0)
    .resizable(false)
    .skip_taskbar(false) // Show in taskbar while open
    .center()
    .build()
    {
        Ok(window) => {
            // Don't exit app when settings window is closed
            let window_clone = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    let _ = window_clone.hide();
                }
            });
        }
        Err(e) => {
            tracing::error!("Failed to create settings window: {e}");
        }
    }
}

/// Toggle the pause/resume sync state.
fn toggle_pause_sync<R: Runtime>(app: &AppHandle<R>) {
    // We signal the frontend to update the config
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.emit("clipsync:toggle-pause", ());
    }
    // Also let the sync engine know via the config
    // (The actual toggle is handled by the frontend calling update_config)
}
