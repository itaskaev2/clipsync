# ClipSync

LAN clipboard synchronization — copy on one machine, paste on another.

[![CI](https://github.com/itaskaev-hbs/clipsync/actions/workflows/ci.yml/badge.svg)](https://github.com/itaskaev-hbs/clipsync/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/itaskaev-hbs/clipsync?label=latest)](https://github.com/itaskaev-hbs/clipsync/releases)

## Quick Start

1. Download the latest `clipsync.exe` from [Releases](https://github.com/itaskaev-hbs/clipsync/releases)
2. Run it — it lives in your system tray
3. Copy something on one machine, paste on another (same LAN)

## Development

```bash
npm ci
npm run build:frontend
cd src-tauri && cargo build --release
```
