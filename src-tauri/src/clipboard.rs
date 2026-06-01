//! Clipboard module — watch + read + write.
//!
//! Uses `arboard` for cross-platform clipboard access.
//! The watcher polls the clipboard at the configured debounce interval,
//! hashing contents to detect changes.
//!
//! # Loop guard (CRITICAL)
//! When we write remote content to the local clipboard, we record its hash
//! as `last_applied_hash`. On the next poll tick, if the clipboard content
//! hashes to the same value, we skip — preventing an echo loop.
//!
//! # Debounce
//! We poll at `debounce_ms` intervals. On each tick we read the clipboard,
//! hash it, and compare with the previous hash. Only true changes trigger
//! a sync event. This naturally handles multiple rapid clipboard events.

use arboard::Clipboard;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use tokio::sync::mpsc;

use crate::config::ClipboardPriority;

/// Represents clipboard content ready for sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardContent {
    /// Content type: "text" or "image".
    pub content_type: String,
    /// Text content (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Raw image data: [4-byte width BE][4-byte height BE][RGBA pixels...]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(with = "serde_bytes")]
    pub image_data: Option<Vec<u8>>,
    /// SHA-256 hash of the content (for dedup/loop guard).
    pub content_hash: String,
}

/// Internal clipboard snapshot for change detection.
struct ClipboardSnapshot {
    text: Option<String>,
    image_data: Option<Vec<u8>>,
    hash: String,
}

/// Start the clipboard watcher. Returns a receiver that yields ClipboardContent
/// when the local clipboard changes. Runs until the channel is closed.
pub async fn start_watcher(
    tx: mpsc::Sender<ClipboardContent>,
    debounce_ms: u64,
    priority: ClipboardPriority,
    payload_limit_bytes: u64,
) {
    let mut last_snapshot: Option<ClipboardSnapshot> = None;
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(debounce_ms));

    loop {
        interval.tick().await;

        // Read current clipboard state
        let current = match read_clipboard_snapshot(&priority) {
            Ok(snap) => snap,
            Err(e) => {
                tracing::debug!("Clipboard read error: {e}");
                continue;
            }
        };

        // If content hasn't changed, skip
        if let Some(ref prev) = last_snapshot {
            if prev.hash == current.hash {
                continue;
            }
        }

        // Build ClipboardContent for sync
        let content = ClipboardContent {
            content_type: if current.image_data.is_some() {
                "image".to_string()
            } else {
                "text".to_string()
            },
            text: current.text.clone(),
            image_data: current.image_data.clone(),
            content_hash: current.hash.clone(),
        };

        // Check payload size limit
        let size = content_size(&content);
        if size > payload_limit_bytes {
            tracing::warn!(
                "Clipboard payload {} bytes exceeds limit {} bytes; skipping.",
                size,
                payload_limit_bytes
            );
            last_snapshot = Some(current);
            continue;
        }

        tracing::debug!(
            "Clipboard change detected: type={}, hash={:.8}, size={}",
            content.content_type,
            &content.content_hash,
            size
        );

        // Send to sync engine
        if tx.send(content).await.is_err() {
            tracing::info!("Clipboard watcher channel closed; stopping.");
            break;
        }

        last_snapshot = Some(current);
    }
}

/// Read the current clipboard and return a snapshot.
fn read_clipboard_snapshot(
    priority: &ClipboardPriority,
) -> Result<ClipboardSnapshot, arboard::Error> {
    let mut clipboard = Clipboard::new()?;

    let text = clipboard.get_text().ok();
    let image = clipboard.get_image().ok();

    // Copy image data out before clipboard is dropped
    let image_data: Option<Vec<u8>> = image.and_then(|img| encode_image_data(&img));

    let (selected_text, selected_image, _selected_type) =
        select_format(&text, &image_data, priority);

    let hash = compute_hash(selected_text.as_deref(), selected_image.as_deref());

    Ok(ClipboardSnapshot {
        text: selected_text,
        image_data: selected_image,
        hash,
    })
}

/// Select the best format based on priority.
fn select_format(
    text: &Option<String>,
    image_data: &Option<Vec<u8>>,
    priority: &ClipboardPriority,
) -> (Option<String>, Option<Vec<u8>>, &'static str) {
    match priority {
        ClipboardPriority::ImageFirst => {
            if image_data.is_some() {
                return (None, image_data.clone(), "image");
            }
            if let Some(t) = text {
                return (Some(t.clone()), None, "text");
            }
            (None, None, "none")
        }
        ClipboardPriority::TextFirst => {
            if let Some(t) = text {
                return (Some(t.clone()), None, "text");
            }
            if image_data.is_some() {
                return (None, image_data.clone(), "image");
            }
            (None, None, "none")
        }
        ClipboardPriority::TextOnly => (text.clone(), None, "text"),
        ClipboardPriority::ImageOnly => {
            if image_data.is_some() {
                (None, image_data.clone(), "image")
            } else {
                (None, None, "none")
            }
        }
    }
}

/// Encode arboard ImageData to our container format:
/// [4-byte width BE][4-byte height BE][RGBA pixels...]
fn encode_image_data(image: &arboard::ImageData) -> Option<Vec<u8>> {
    let rgba = image.bytes.to_vec();
    if rgba.is_empty() {
        return None;
    }
    let mut data = Vec::with_capacity(8 + rgba.len());
    data.extend_from_slice(&(image.width as u32).to_be_bytes());
    data.extend_from_slice(&(image.height as u32).to_be_bytes());
    data.extend_from_slice(&rgba);
    Some(data)
}

/// Write content to the local clipboard.
///
/// # Loop guard
/// Callers (sync engine) must update `last_applied_hash` BEFORE calling this
/// to prevent the watcher from re-emitting the same content.
pub fn write_to_clipboard(content: &ClipboardContent) -> Result<(), String> {
    let mut clipboard = Clipboard::new().map_err(|e| format!("Clipboard open error: {e}"))?;

    match content.content_type.as_str() {
        "text" => {
            if let Some(ref text) = content.text {
                clipboard
                    .set_text(text)
                    .map_err(|e| format!("Clipboard set_text error: {e}"))?;
                tracing::debug!("Wrote text to clipboard ({:.8})", &content.content_hash);
            }
        }
        "image" => {
            if let Some(ref raw_data) = content.image_data {
                if raw_data.len() < 8 {
                    return Err("Image data too short".to_string());
                }
                let width = u32::from_be_bytes([raw_data[0], raw_data[1], raw_data[2], raw_data[3]])
                    as usize;
                let height =
                    u32::from_be_bytes([raw_data[4], raw_data[5], raw_data[6], raw_data[7]])
                        as usize;
                let rgba = &raw_data[8..];

                let img = arboard::ImageData {
                    width,
                    height,
                    bytes: Cow::Owned(rgba.to_vec()),
                };
                clipboard
                    .set_image(img)
                    .map_err(|e| format!("Clipboard set_image error: {e}"))?;
                tracing::debug!(
                    "Wrote image to clipboard {}x{} ({:.8})",
                    width,
                    height,
                    &content.content_hash
                );
            }
        }
        other => {
            return Err(format!("Unknown content type: {other}"));
        }
    }

    Ok(())
}

/// Compute SHA-256 hash of clipboard content for dedup.
fn compute_hash(text: Option<&str>, image_data: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    if let Some(t) = text {
        hasher.update(b"text:");
        hasher.update(t.as_bytes());
    }
    if let Some(img) = image_data {
        hasher.update(b"image:");
        hasher.update(img);
    }
    if text.is_none() && image_data.is_none() {
        hasher.update(b"empty");
    }
    hex::encode(hasher.finalize())
}

/// Estimate the serialized size of clipboard content.
fn content_size(content: &ClipboardContent) -> u64 {
    let mut size: u64 = 0;
    if let Some(ref t) = content.text {
        size += t.len() as u64;
    }
    if let Some(ref img) = content.image_data {
        size += img.len() as u64;
    }
    // Add overhead for serialization ~100 bytes
    size + 100
}

/// Check if two hashes are equal.
pub fn hashes_equal(a: &str, b: &str) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_hash_different_content() {
        let h1 = compute_hash(Some("hello"), None);
        let h2 = compute_hash(Some("world"), None);
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_compute_hash_same_content() {
        let h1 = compute_hash(Some("hello"), None);
        let h2 = compute_hash(Some("hello"), None);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_compute_hash_empty() {
        let h = compute_hash(None, None);
        assert!(!h.is_empty());
    }

    #[test]
    fn test_content_size() {
        let content = ClipboardContent {
            content_type: "text".into(),
            text: Some("hello".into()),
            image_data: None,
            content_hash: "abc".into(),
        };
        assert!(content_size(&content) > 5);
    }
}
