<p align="center">
  <img src="art/BlazingDL.png" width="112" height="112" alt="BlazingDL icon" />
</p>

<h1 align="center">BlazingDL</h1>

<p align="center">
  <strong>High-throughput native Rust download manager redesigned with the Saville Catalog Pulse interface.</strong><br />
  Segmented HTTP/HTTPS/FTP · BitTorrent &amp; Magnets · Video &amp; Gallery Capture · Browser Extension · CLI · Remote Web UI · MCP Server
</p>

<p align="center">
  <a href="https://github.com/Khuraie/BlazingDL/releases"><img src="https://img.shields.io/github/v/release/Khuraie/BlazingDL?style=flat-square&amp;color=e04826&amp;label=release" alt="Release" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-ecebe7?style=flat-square" alt="GPL-3.0-or-later" /></a>
  <img src="https://img.shields.io/badge/rust-1.98%2B-090a0c?style=flat-square" alt="Rust 1.98+" />
</p>

---

## Highlights

- **Redesigned Desktop Shell (Saville Catalog Pulse)** — Absorbent matte-black workstation (`#0E0F11`) with bone-white typography (`#ECEBE7`), strict `0px` architectural geometry, a **live 5-line pulsar throughput waveform** in the top header, flat color-block protocol tags, razor-thin `2px` inline progress hairlines, and a spacious Catalog Index Rail. Classic, Metro, Adwaita, Breeze, Luna, Aero, Aqua, Emerald, High Contrast, and Native themes remain selectable under **View → Theme**.
- **Multi-Connection Segmented Engine (`blazingdl-engine`)** — Concurrent byte-range transfers, crash-safe resume states, custom DNS (DoH/DoT), SOCKS/HTTP proxy & PAC support, network adapter binding, checksum verification (MD5/SHA-1/SHA-256/CRC32), and damaged ZIP range repair.
- **Built-in BitTorrent (`blazingdl-torrent`)** — Magnet links, `.torrent` files, DHT, uTP, selective file downloads, IP blocklists, and per-torrent rate limits powered by `librqbit`.
- **Video & Media Capture (`blazingdl-tools`)** — Managed, signature-verified `yt-dlp`, `gallery-dl`, `ffmpeg`, and Node.js helpers with quality selection directly from the browser extension.
- **Browser Integration (`extension/` + `blazingdl-nmhost`)** — Native messaging extension for Chrome, Edge, Brave, Opera, Vivaldi, Firefox, and Safari with download interception, floating video download button, and live popup controls.
- **Automation & Remote Control** — Headless CLI (`blazingdl-cli`), local HTTPS phone/remote web UI (`blazingdl-remote`), Click'n'Load (`blazingdl-cnl`), Debrid support (`blazingdl-debrid`), and a Model Context Protocol server (`blazingdl-mcp`) for AI assistants.

## Workspace Architecture

| Crate | Binary / Role |
| :--- | :--- |
| `blazingdl-app` | `blazingdl` — Native desktop GUI (egui, Windows / Linux / macOS) |
| `blazingdl-cli` | `blazingdl-cli` — Headless command-line launcher |
| `blazingdl-nmhost` | `blazingdl-nmhost` — Browser native messaging host (`com.blazingdl.host`) |
| `blazingdl-mcp` | `blazingdl-mcp` — Model Context Protocol (MCP) stdio server |
| `blazingdl-engine` | Segmented HTTP/HTTPS/FTP download engine & resume/repair core |
| `blazingdl-torrent` | BitTorrent & magnet session engine |
| `blazingdl-ipc` | Local single-instance IPC channel & automation API schema |
| `blazingdl-tools` | Verified helper tool installer & self-update verification |
| `blazingdl-remote` | Local HTTPS remote control server & web UI |
| `blazingdl-extract` | Built-in ZIP/tar/xz extraction & archive integrity checker |
| `blazingdl-crash` | Crash/freeze monitor & minidump symbolicator (`blazingdl-symbolicate`) |
| `blazingdl-debrid`, `blazingdl-cnl`, `blazingdl-geo`, `blazingdl-labserver` | Debrid hosts, Click'n'Load, offline IP-to-country, fault-injection test server |

## Building from Source

Requires **Rust 1.98+** (Edition 2024).

```powershell
# Desktop app + CLI + Native Host + MCP server (pure Rust TLS; no BoringSSL native prereqs)
cargo build -p blazingdl-app --no-default-features -p blazingdl-cli -p blazingdl-nmhost -p blazingdl-mcp

# Full build including `browser-connection` (BoringSSL TLS impersonation; requires CMake, libclang, NASM on Windows)
cargo build --workspace

# Run core tests
cargo test -p blazingdl-ipc -p blazingdl-mcp -p blazingdl-nmhost
cargo test -p blazingdl-tools --no-default-features --lib
node tools/extension/test.mjs
```

## Packaging & Assets

- **Browser extension** (Chromium, Firefox, Safari): `tools/extension/pack.ps1` → `target/extension/`
- **Windows installer & portable zips** (Inno Setup 6): `tools/installer/build.ps1` → `target/dist/`
- **Regenerate app & extension icons** from `art/BlazingDL.png`: `python tools/icon/build_icon.py`

## License & Attribution

Licensed under the **GNU General Public License v3.0 or later** ([LICENSE](LICENSE)).

BlazingDL is a fork of [Mister Download Manager](https://github.com/anefzaoui/misterdownloadmanager) by Ahmed Nefzaoui. See [ATTRIBUTION.md](ATTRIBUTION.md) for full upstream and third-party asset credits.
