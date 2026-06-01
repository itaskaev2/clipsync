# ClipSync — LAN Clipboard Synchronization

Copy on one machine, paste on another. ClipSync syncs your system clipboard
(text and images) between Windows machines on the same local network —
automatically, encrypted, and without any cloud.

## How It Works

```
Machine A                          Machine B
  Copy "hello"                       (idle)
       │                                │
       ▼                                │
  Clipboard watcher detects change      │
       │                                │
       ▼                                │
  Encrypt + send via WebSocket ────────►│
                                        ▼
                                  Decrypt + write to clipboard
                                        │
                                        ▼
                                  "hello" ready for Ctrl+V
```

1. A clipboard watcher polls for changes every ~200ms (configurable).
2. When content changes, it reads the highest-priority format (image > text by default).
3. The content is hashed (SHA-256), serialized with MessagePack, encrypted with
   ChaCha20-Poly1305, and sent to all paired peers over WebSocket.
4. Peers receive, decrypt, and write the content to their local clipboard.
5. A **loop guard** (hash-based, 2-second window) prevents echo loops.
6. A **dedup** check prevents re-sending recently sent content.

## Security

- **Pairing required**: machines must exchange a 6-digit code before syncing.
  Unpaired machines in the same LAN cannot read your clipboard.
- **Encrypted transport**: all clipboard data is encrypted with ChaCha20-Poly1305
  using a key derived from the pairing code via HKDF-SHA256.
- **No cloud**: data never leaves your local network.
- **No history on disk**: clipboard is never persisted to disk by default.

## Requirements (for building)

- Rust 1.75+ (stable)
- Node.js 20+ with npm
- Windows 10/11 (MVP target; macOS/Linux support planned for Phase 2)

## Quick Start

```bash
# Install dependencies
npm install

# Run in development mode
npm run tauri dev

# Build for production (creates Windows installers)
npm run tauri build
```

Built installers are placed in:
- `src-tauri/target/release/bundle/nsis/*.exe` (NSIS installer)
- `src-tauri/target/release/bundle/msi/*.msi` (MSI installer)

## Usage

1. Install ClipSync on two Windows machines on the same LAN.
2. The app starts silently in the system tray — no window, no taskbar entry.
3. Left-click the tray icon to open settings.
4. On one machine, click "Pair" and enter a 6-digit code.
5. On the other machine, enter the same code.
6. Machines discover each other via mDNS and establish an encrypted connection.
7. Copy text or an image on either machine — it appears on the other's clipboard.

## Configuration

Settings are stored in `%APPDATA%/ClipSync/config.toml`:

| Setting | Default | Description |
|---|---|---|
| `payload_limit_mb` | 25 | Max clipboard payload size in MB |
| `debounce_ms` | 200 | Clipboard poll interval in ms |
| `clipboard_priority` | `image_first` | Format priority: `image_first`, `text_first`, `text_only`, `image_only` |
| `sync_paused` | false | Pause/resume sync |

## Project Structure

```
clipsync/
├── src/                   # Web UI (pairing/settings page)
│   ├── index.html
│   ├── main.js
│   └── style.css
├── src-tauri/             # Rust backend
│   ├── src/
│   │   ├── main.rs        # App entry, Tauri setup, channel wiring
│   │   ├── config.rs      # Config load/save (TOML)
│   │   ├── clipboard.rs   # Clipboard watch/read/write (arboard)
│   │   ├── discovery.rs   # mDNS service registration + peer browsing
│   │   ├── pairing.rs     # Pairing code, HKDF key derivation, ChaCha20-Poly1305 cipher
│   │   ├── transport.rs   # Encrypted WebSocket server + client with reconnect
│   │   ├── sync.rs        # Core engine: send/receive, loop guard, dedup
│   │   └── tray.rs        # System tray icon + right-click menu
│   ├── Cargo.toml
│   └── tauri.conf.json
├── .github/
│   └── workflows/
│       └── build.yml      # CI: build Windows installers, upload artifacts
├── package.json
├── build.mjs
└── README.md
```

## CI/CD

GitHub Actions builds installers on every push to `main`, every PR, and on
manual trigger (`workflow_dispatch`). Artifacts are downloadable from the
Actions tab.

```yaml
# .github/workflows/build.yml
# Triggers: push (main), pull_request, workflow_dispatch
# Runner: windows-latest
# Output: clipsync-windows-installers (artifact)
```

## Technology Stack

| Component | Technology |
|---|---|
| Framework | Tauri 2.x |
| Backend | Rust (tokio async runtime) |
| Frontend | Vanilla HTML/CSS/JS |
| Clipboard | arboard |
| Discovery | mdns-sd (mDNS/DNS-SD) |
| Transport | WebSocket (tokio-tungstenite) |
| Encryption | ChaCha20-Poly1305 + HKDF-SHA256 |
| Serialization | MessagePack (rmp-serde) |
| Config | TOML |
| Build | GitHub Actions (windows-latest) |

## Roadmap

- **Phase 0 (MVP)** — Windows, text + image sync, 2 machines, tray-only, pairing, encryption. ✓
- **Phase 1** — File sync, sync history UI, autostart, >2 peers (mesh).
- **Phase 2** — macOS (NSPasteboard) and Linux (X11 + Wayland).
- **Phase 3** — Optional relay backend in Kubernetes for cross-subnet sync.

## Known Limitations (MVP)

- Only Windows 10/11 is supported (macOS/Linux in Phase 2).
- Installers are unsigned — Windows SmartScreen will show a warning. Click "More info" → "Run anyway".
- Clipboard watcher uses polling (200ms interval). Native clipboard listeners
  (AddClipboardFormatListener) planned for efficiency.
- Pairing code has ~20 bits of entropy (10^6 combinations). Sufficient for LAN;
  consider longer codes or Diffie-Hellman for higher security.
- mDNS only works within the same L2 broadcast domain (same subnet/VLAN).
- Image sync uses raw RGBA container format — PNG re-encoding planned.

## License

Proprietary (MVP phase).
