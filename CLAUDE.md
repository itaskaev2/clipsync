# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

ClipSync is a LAN clipboard synchronization app — copy on one machine, paste on another. Built with Tauri 2 (Rust backend + vanilla JS frontend), packaged as a Windows system tray application.

## Commands

```bash
# Install JS dependencies
npm ci

# Development (hot reload)
npm run dev

# Type-check Rust without building
cd src-tauri && cargo check

# Run Rust tests
cd src-tauri && cargo test

# Build frontend only (copies src/ → dist/)
npm run build:frontend

# Full production build (frontend + Tauri NSIS installer)
npm run build

# Verbose logging during dev
$env:RUST_LOG="debug"; npm run dev
```

No JS tests exist. Rust tests are in the individual `src-tauri/src/*.rs` modules.

## Architecture

### Data flow

```
Clipboard change
      ↓
[clipboard.rs] — polls at debounce_ms, hashes content to detect changes
      ↓  (channel)
[sync.rs] — dedup, loop-guard, pause check, peer routing
      ↓  (channel)
[transport.rs] — ChaCha20-Poly1305 encrypted WebSocket server+client
      ↓  (network)
Peer's [transport.rs] → [sync.rs] → [clipboard.rs] writes to clipboard
```

mDNS discovery runs in parallel: `[discovery.rs]` listens for `_clipsync._tcp` services and sends `DiscoveredPeer` events into `[sync.rs]`, which triggers `[transport.rs]` to open a client connection. The WebSocket server binds an OS-assigned port (`start_server` returns it) which is then advertised via mDNS.

### Module responsibilities (`src-tauri/src/`)

| Module | Responsibility |
|--------|---------------|
| `main.rs` | Tauri setup, Tokio runtime, spawns all services, defines all `#[tauri::command]` IPC handlers |
| `config.rs` | Load/save `config.toml` (instance ID, pairing code, paired peers, settings) |
| `clipboard.rs` | Debounced clipboard polling via `arboard`; loop-guard hash to prevent echo |
| `sync.rs` | Coordination: receives from watcher + mDNS + transport, deduplicates, broadcasts |
| `transport.rs` | WebSocket server (inbound) + client manager (outbound); per-peer encrypted tunnels; swappable cipher + reconnect/backoff; emits `TransportEvent` connect/disconnect |
| `pairing.rs` | `generate_pairing_code()`, `Cipher::from_pairing_code()` (HKDF-SHA256 key derivation), `WireMessage` encrypt/decrypt |
| `discovery.rs` | mDNS registration (advertises LAN IPv4) and browsing; non-fatal if unavailable |
| `tray.rs` | System tray icon + menu (status line, open settings, pause/resume, exit); `set_status` updates it live |

### Key design decisions

**Loop guard** (`sync.rs` `SyncGuard`) — When remote content is applied locally, its hash is remembered for ~2 seconds; a local watcher event matching it is suppressed (prevents an infinite echo). Because the OS may re-encode images on the clipboard round-trip, the guard also records the hash of what actually lands on the clipboard after the write (via `clipboard::read_current_hash`). The window lets the user legitimately re-copy the same content afterward.

**Invisible persistent window** — Tauri 2 exits the event loop when all windows close. A hidden, non-closeable window is created at startup to keep the process alive for tray-only operation.

**Pairing / encryption (the security gate)** — The shared key is the boundary. The 6-digit code is stretched to a ChaCha20-Poly1305 key via HKDF-SHA256 (`pairing.rs`); a peer without the code cannot decrypt the handshake, so it's rejected. The cipher lives in a swappable slot in `transport.rs`: **no code ⇒ no cipher ⇒ no sync**. Setting/changing the code installs a new key and bumps a `watch` "reset" generation so live connections drop and re-handshake. Peers are added to the persisted allowlist on a successful handshake (the allowlist is informational; the key gates).

**mDNS non-fatal** — If `mdns-sd` fails to initialize (e.g., Windows without Bonjour), the app continues with manual pairing code fallback.

**Thread safety** — Config and transport state use `Arc<RwLock<>>` shared across async Tokio tasks.

### Frontend (`src/`)

Single-page app with three sections: pairing (enter/generate 6-digit code), peers list, settings. `main.js` calls Tauri commands via `invoke()` and listens for backend events (`clipsync:peer-joined`, `clipsync:peer-left`, etc.). Peer list is polled every 5 seconds.

The `build.mjs` script copies `src/{index.html,main.js,style.css}` to `dist/`.

## Configuration & logs

- Config (incl. pairing code + allowlist): `%APPDATA%/clipsync/ClipSync/config/config.toml`
- Logs: `%LOCALAPPDATA%/clipsync/ClipSync/data/logs/` (console INFO in dev + rotating JSON DEBUG)

## CI/CD

- `.github/workflows/ci.yml` — Linux self-hosted: `cargo check` + `cargo test`
- `.github/workflows/build.yml` — Windows: builds the NSIS installer, uploads it as the `clipsync-windows-installers` artifact
- (No release workflow currently — it was disabled.)

## Local build toolchain (Windows)

Tauri's blessed toolchain is `x86_64-pc-windows-msvc` (needs VS Build Tools). The
GNU toolchain (`x86_64-pc-windows-gnu`) also works but requires MinGW-w64 on
`PATH` (`gcc`/`dlltool`/`ld`); a portable WinLibs *MSVCRT* build works without
admin. Run `npm run build:frontend` before any `cargo` command so `dist/` exists
for `generate_context!`.

## Extending the app

**New Tauri command**: implement `async fn` in `main.rs`, add to `invoke_handler![]`, call from JS with `invoke('name', {args})`.

**New sync message type**: add variant to `WireMessage` enum in `pairing.rs`, handle in `sync.rs`.
