//! Configuration management — load/save ClipSync settings.
//!
//! Persists to the OS app config directory:
//!   Windows: %APPDATA%/ClipSync/config.toml
//!   macOS:   ~/Library/Application Support/ClipSync/config.toml
//!   Linux:   ~/.config/ClipSync/config.toml

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Priority order for clipboard format selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardPriority {
    /// Prefer image, fall back to text.
    ImageFirst,
    /// Prefer text, fall back to image.
    TextFirst,
    /// Only sync text.
    TextOnly,
    /// Only sync images.
    ImageOnly,
}

impl Default for ClipboardPriority {
    fn default() -> Self {
        Self::ImageFirst
    }
}

/// Information about a paired peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Human-readable name (from mDNS).
    pub name: String,
    /// Unique peer ID (UUID).
    pub id: String,
    /// IP address from last discovery.
    pub address: String,
    /// Port from last discovery.
    pub port: u16,
    /// When this peer was last seen (Unix timestamp).
    pub last_seen: u64,
}

/// Main application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Maximum payload size in megabytes (default 25).
    pub payload_limit_mb: u32,
    /// Debounce window in milliseconds (default 200).
    pub debounce_ms: u64,
    /// Clipboard format priority.
    pub clipboard_priority: ClipboardPriority,
    /// Whether sync is paused.
    pub sync_paused: bool,
    /// List of paired (allowlisted) peers.
    pub paired_peers: Vec<PeerInfo>,
    /// Our unique instance ID.
    pub instance_id: String,
    /// Pairing code (6-digit, stored while pairing is in progress).
    #[serde(skip_serializing)]
    pub pairing_code: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            payload_limit_mb: 25,
            debounce_ms: 200,
            clipboard_priority: ClipboardPriority::default(),
            sync_paused: false,
            paired_peers: Vec::new(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            pairing_code: None,
        }
    }
}

/// Wraps config in Arc<RwLock> for sharing across Tauri state.
pub type SharedConfig = Arc<RwLock<AppConfig>>;

/// Return the path to config.toml in the OS config directory.
pub fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("com", "clipsync", "ClipSync").map(|dirs| {
        let path = dirs.config_dir().to_path_buf();
        path.join("config.toml")
    })
}

/// Load config from disk, or return defaults if no saved config exists.
pub fn load_config() -> AppConfig {
    let path = match config_path() {
        Some(p) => p,
        None => {
            tracing::warn!("Cannot determine config directory; using defaults.");
            return AppConfig::default();
        }
    };

    match fs::read_to_string(&path) {
        Ok(contents) => {
            match toml::from_str::<AppConfig>(&contents) {
                Ok(mut cfg) => {
                    // Ensure instance_id is set (backward compat)
                    if cfg.instance_id.is_empty() {
                        cfg.instance_id = uuid::Uuid::new_v4().to_string();
                    }
                    tracing::info!("Config loaded from {}", path.display());
                    cfg
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to parse config at {}: {e}; using defaults.",
                        path.display()
                    );
                    let mut cfg = AppConfig::default();
                    // Try to preserve the old instance_id if it was a UUID
                    // (best-effort; if parsing failed completely, generate new)
                    if let Some(parent) = path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    save_config(&cfg);
                    cfg
                }
            }
        }
        Err(_) => {
            tracing::info!("No config found at {}; creating default.", path.display());
            let cfg = AppConfig::default();
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            save_config(&cfg);
            cfg
        }
    }
}

/// Save config to disk.
pub fn save_config(config: &AppConfig) {
    let path = match config_path() {
        Some(p) => p,
        None => {
            tracing::error!("Cannot determine config directory; config not saved.");
            return;
        }
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            tracing::error!(
                "Failed to create config directory {}: {e}",
                parent.display()
            );
            return;
        }
    }

    match toml::to_string_pretty(config) {
        Ok(contents) => {
            if let Err(e) = fs::write(&path, &contents) {
                tracing::error!("Failed to write config to {}: {e}", path.display());
            } else {
                tracing::info!("Config saved to {}", path.display());
            }
        }
        Err(e) => {
            tracing::error!("Failed to serialize config: {e}");
        }
    }
}

/// Serializable subset of config for the frontend (no secrets).
#[derive(Debug, Clone, Serialize)]
pub struct FrontendConfig {
    pub payload_limit_mb: u32,
    pub debounce_ms: u64,
    pub clipboard_priority: String,
    pub sync_paused: bool,
    pub instance_id: String,
}

impl From<&AppConfig> for FrontendConfig {
    fn from(cfg: &AppConfig) -> Self {
        Self {
            payload_limit_mb: cfg.payload_limit_mb,
            debounce_ms: cfg.debounce_ms,
            clipboard_priority: format!("{:?}", cfg.clipboard_priority).to_lowercase(),
            sync_paused: cfg.sync_paused,
            instance_id: cfg.instance_id.clone(),
        }
    }
}

/// Serializable peer info for the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct FrontendPeer {
    pub name: String,
    pub id: String,
    pub connected: bool,
}

/// Deserializable config update from the frontend.
#[derive(Debug, Deserialize)]
pub struct ConfigUpdate {
    pub payload_limit_mb: Option<u32>,
    pub debounce_ms: Option<u64>,
    pub clipboard_priority: Option<String>,
    pub sync_paused: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.payload_limit_mb, 25);
        assert_eq!(cfg.debounce_ms, 200);
        assert!(!cfg.instance_id.is_empty());
    }
}
