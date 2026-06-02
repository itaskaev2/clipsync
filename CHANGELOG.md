# Changelog

All notable changes to ClipSync.

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
