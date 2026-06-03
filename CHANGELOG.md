# Changelog

All notable changes to ClipSync.

## [Unreleased] — MVP alignment

End-to-end sync was previously non-functional; these changes make the Phase 0
acceptance criteria actually pass.

### Fixed
- **Pairing now connects.** The allowlist was never populated, so the
  discovery→connect path was dead and no peer ever connected. Connections are
  now gated by the shared key (derived from the pairing code); peers are added
  to the persisted allowlist on a successful handshake.
- **Runtime pairing.** The transport cipher was built once at startup from the
  saved code (defaulting to `000000`), so pairing from the UI had no effect.
  The cipher is now swappable: setting/changing the code installs a new key and
  forces affected connections to re-handshake. With no code set, no sync occurs.
- **Pairing code is persisted** (was `skip_serializing`), so pairing survives
  restarts — as the spec requires.
- **Tray Pause/Resume works without an open window** (it mutated config only via
  the settings window before) and its label now reflects state.
- **Settings save fixed** — the frontend sent the wrong argument name to the
  `update_config` command.
- **Frontend events work** — added the missing Tauri capabilities file (ACL was
  empty, so `event.listen`/`emit` were denied).
- **mDNS advertises the real LAN IPv4** instead of `0.0.0.0`.
- **Image echo loop** hardened: the loop guard also records the hash of what
  actually lands on the clipboard after a write (the OS may re-encode images).

### Added
- "Generate code" button + visible "Your code" in the UI (there was no way to
  obtain a code to share before).
- OS toast + tray notification when a clipboard item exceeds the size limit.
- Live connection status in the tray and a per-peer online/offline indicator in
  the UI; settings (debounce/limit/priority) now apply without a restart.

## [0.1.1] - 2026-06-02

### Fixed
- App no longer exits silently when mDNS is unavailable (e.g. Windows without Bonjour)
- Tray icon now has a visible purple circle fallback instead of transparent (invisible)
- Background services stay alive regardless of mDNS status

## [0.1.0] - 2026-06-02

### Added
- LAN clipboard sync via encrypted WebSocket
- mDNS peer discovery (automatic, no config)
- 6-digit pairing code with HKDF key derivation
- System tray with pause/resume + settings UI
- ChaCha20-Poly1305 encrypted transport
- Self-hosted CI on Kubernetes (Linux) + GitHub Windows release builds
- GitHub Releases with auto-generated notes

[0.1.1]: https://github.com/itaskaev-hbs/clipsync/releases/tag/v0.1.1
[0.1.0]: https://github.com/itaskaev-hbs/clipsync/releases/tag/v0.1.0
