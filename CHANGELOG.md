# Changelog

All notable changes to ClipSync.

## [Unreleased]

## [0.1.3] - 2026-06-04

### Added
- The window now shows the running **build version** (a chip next to the title
  and in the footer) plus the short device ID. This makes it easy to confirm
  both machines are on the same build — a version skew is the usual cause of the
  connect/disconnect loop, since the connection tie-break only works when both
  peers run it.

## [0.1.2] - 2026-06-03 — MVP alignment

End-to-end sync was previously non-functional; these changes make the Phase 0
acceptance criteria actually pass.

### Fixed
- **Connect/disconnect storm eliminated.** Both peers discovered each other and
  both dialed out, so two connections (inbound + outbound) formed per pair and
  fought over the peer map — each new one displaced the other and each death
  evicted the survivor, flapping many times per second. Now a deterministic
  tie-break (only the peer whose instance id sorts first dials; the other
  accepts) yields a single stable connection, and a per-connection token keeps a
  dying connection from evicting a newer one.
- **Clipboard payloads now deserialize.** `ClipboardContent` used
  `skip_serializing_if` on its optional `text`/`image_data`, but the MessagePack
  wire format encodes a struct as a positional array — skipping a `None` field
  shortened it to 3 elements, so the receiver always failed with "invalid length
  3, expected 4" and *no* clipboard item ever applied. All four fields are now
  always serialized (`None` → nil); added round-trip regression tests.
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

[0.1.3]: https://github.com/itaskaev-hbs/clipsync/releases/tag/v0.1.3
[0.1.2]: https://github.com/itaskaev-hbs/clipsync/releases/tag/v0.1.2
[0.1.1]: https://github.com/itaskaev-hbs/clipsync/releases/tag/v0.1.1
[0.1.0]: https://github.com/itaskaev-hbs/clipsync/releases/tag/v0.1.0
