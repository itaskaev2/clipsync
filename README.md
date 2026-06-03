# ClipSync

LAN clipboard synchronization — copy on one machine, paste on another. ClipSync
lives in the system tray, discovers peers on your local network via mDNS, and
syncs text and images over an **encrypted** peer-to-peer connection after a
one-time code pairing.

[![CI](https://github.com/itaskaev-hbs/clipsync/actions/workflows/ci.yml/badge.svg)](https://github.com/itaskaev-hbs/clipsync/actions/workflows/ci.yml)

> **Status:** Phase 0 (MVP), Windows 10/11 only. Text + image sync, mDNS
> discovery, code pairing, encryption, tray control, payload limit. macOS/Linux
> and file sync are on the roadmap (not yet implemented).

## Quick start

1. Download the installer from the latest CI run (Actions → **build** →
   `clipsync-windows-installers`) or from [Releases](https://github.com/itaskaev-hbs/clipsync/releases), and install on **both** machines.
2. ClipSync starts in the system tray (no taskbar window).
3. **Pair the two machines:**
   - On machine **A**, open the window (left-click the tray icon) and click
     **Generate** — a 6-digit code appears.
   - On machine **B**, type that same code into *peer code* and click **Pair**.
   - Both machines now share the same code (and therefore the same encryption
     key) and will connect automatically once they see each other on the LAN.
4. Copy text or a screenshot on one machine — it's ready to paste (Ctrl+V) on
   the other within ~1 second.

The installer is unsigned, so Windows SmartScreen will warn on first run
("More info" → "Run anyway"). Code signing is a later task.

## How it works

```text
Clipboard change
   │  clipboard.rs   poll @ debounce, hash content, enforce size limit
   ▼
sync.rs             dedup + loop-guard, honour pause, route to peers
   │
   ▼
transport.rs        ChaCha20-Poly1305 over WebSocket (server + client)
   │  (LAN)
   ▼
peer transport.rs ─► peer sync.rs ─► peer clipboard.rs writes the clipboard
```

- **Discovery** — each instance advertises `_clipsync._tcp` over mDNS with its
  LAN IPv4 and WebSocket port, and browses for peers.
- **Pairing / security** — the 6-digit code is stretched into a 256-bit key via
  HKDF-SHA256. All traffic (including the identity handshake) is encrypted with
  ChaCha20-Poly1305, so a machine without the code cannot complete the handshake
  or read clipboard data. No code set = no connections.
- **Loop guard** — when remote content is written locally, the watcher would
  normally re-broadcast it (an echo). ClipSync remembers the hash it just
  applied — including the hash of what actually landed on the clipboard, since
  the OS may re-encode images — and suppresses the echo for a short window.
- **Reconnect** — client connections retry with exponential backoff (1s → 60s),
  and re-handshake automatically when the pairing code changes.
- **Privacy** — clipboard history is never written to disk.

## Development

### Prerequisites

- **Node.js** 20+ (for the frontend build and the Tauri CLI).
- **Rust** stable with a working Windows linker. Two options:
  - *Recommended (matches CI):* `rustup` with the **MSVC** toolchain
    (`x86_64-pc-windows-msvc`) + Visual Studio Build Tools (C++ workload).
  - *GNU toolchain:* if you use `x86_64-pc-windows-gnu`, you need MinGW-w64 on
    your `PATH` (it provides `gcc`/`dlltool`/`ld`). A portable build from
    [WinLibs](https://winlibs.com/) (the *MSVCRT* variant) works without admin.
- A [WebView2 runtime](https://developer.microsoft.com/microsoft-edge/webview2/)
  (preinstalled on current Windows 11).

### Commands

```bash
npm ci                       # install JS deps (Tauri CLI + API)
npm run dev                  # hot-reload dev build (tauri dev)

npm run build:frontend       # copy src/ → dist/ (needed before any cargo build)
npm run build                # frontend + tauri release bundle (NSIS installer)

# Rust-only (run from the repo root):
cargo test  --manifest-path src-tauri/Cargo.toml
cargo check --manifest-path src-tauri/Cargo.toml
```

> `cargo build`/`cargo check` embed the frontend via `generate_context!`, so run
> `npm run build:frontend` at least once first (so `dist/` exists).

### Layout

```
src-tauri/src/   Rust backend (one module per concern — see main.rs header)
src-tauri/capabilities/  Tauri ACL (window permissions for the UI)
src/             Minimal HTML/JS/CSS settings + pairing UI
build.mjs        Frontend "build" (copies src → dist)
.github/workflows/  CI (Linux check/test) + build (Windows installers)
```

### Config & logs

- Config (incl. pairing code + paired-peer allowlist):
  `%APPDATA%\clipsync\ClipSync\config\config.toml`
- Logs (rotating JSON, plus INFO to console in dev):
  `%LOCALAPPDATA%\clipsync\ClipSync\data\logs\`
- Increase verbosity with `RUST_LOG=debug`.

## Roadmap

- **Phase 1** — file sync, sync history window, autostart at login, >2 peers (mesh).
- **Phase 2** — macOS + Linux (incl. Wayland) support and packaging.
- **Phase 3** — optional Kubernetes relay backend for cross-subnet use.
