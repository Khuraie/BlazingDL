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

- **Redesigned Desktop Shell (Saville Catalog Pulse)** — Absorbent matte-black workstation (`#0E0F11`) with bone-white typography (`#ECEBE7`), strict `0px` architectural geometry, a **live 5-line pulsar throughput waveform** in the top header, an **inline Command & Capture Strip** (`Ctrl+L` Quick-Add for URLs, `magnet:`, `.meta4`/`.metalink`, `.nzb`, and inline checksums; `Ctrl+F` instant transfer filter; one-click **MULTI-NIC** and **FOCUS** controls), flat color-block protocol tags, razor-thin `2px` inline progress hairlines, and a spacious Catalog Index Rail. Classic, Metro, Adwaita, Breeze, Luna, Aero, Aqua, Emerald, High Contrast, and Native themes remain selectable under **View → Theme**.
- **Plexo-Style Multi-NIC Bandwidth Aggregation (`Route::Aggregate`)** — Bond multiple active network interfaces (e.g. Ethernet + Wi-Fi or 5G tethering) simultaneously across parallel segmented workers with per-NIC byte/throughput reporting in the Pulse Header.
- **Auto-Adaptive Concurrency & Hedged Tail Racing** — Dynamically scales worker concurrency (`AdaptiveScaler`) against real throughput gain and disk pressure, and preempts sluggish straggler tails (`Planner::preempt_slow_tail`) with anti-livelock cooldowns so slow TCP paths never stall the final 5%.
- **Multi-Mirror Racing & Metalink (`.meta4` / `.metalink`) Support** — Parses RFC 5854 Metalink v4 and v3 manifests, pulls byte ranges across primary and mirror URLs in parallel, cools down failing mirrors automatically for 60s, and verifies the strongest declared checksum.
- **Native Usenet (`.nzb`) Engine (`blazingdl-engine::nzb`)** — Pure-Rust NZB XML parser, streaming yEnc decoder with per-segment CRC32 verification, and multi-connection async NNTP/NNTPS (`rustls`) downloader.
- **Smart Checksum Auto-Match & Post-Download Automation Rules / Webhooks** — Auto-detects SHA-512/SHA-256/SHA-1/MD5 hashes from pasted text or sibling `SHA256SUMS`/`.sha256` manifests; triggers regex/category rules on completion for auto-move, shell command templates (`{path}`, `{file}`, `{dir}`, `{url}`, `{size}`), and async Discord / ntfy / JSON webhooks.
- **Live 24h Bandwidth Schedule Curve & Game/Focus Auto-Throttle** — Caps aggregate download bandwidth automatically during fullscreen Direct3D games or presentations (`SHQueryUserNotificationState` on Windows) and/or per-hour 24h schedules, always respecting the tightest active limit without overwriting manual Speed Limiter settings.
- **Built-in BitTorrent, Video Capture & Ecosystem** — `librqbit` torrents/magnets, managed `yt-dlp`/`gallery-dl`/`ffmpeg` helpers, browser extension (`extension/` + `blazingdl-nmhost`), headless CLI (`blazingdl-cli`), local HTTPS remote web UI (`blazingdl-remote`), Click'n'Load (`blazingdl-cnl`), Debrid (`blazingdl-debrid`), and MCP server (`blazingdl-mcp`).

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
cargo build -p blazingdl-app -p blazingdl-cli -p blazingdl-nmhost -p blazingdl-mcp --no-default-features

# Full build including `browser-connection` (BoringSSL TLS impersonation; requires CMake, libclang, NASM on Windows)
cargo build --workspace

# Run core & feature tests
cargo test -p blazingdl-engine --no-default-features --lib
cargo test -p blazingdl-engine --no-default-features --test blazing_features
cargo test -p blazingdl-app --no-default-features --bin blazingdl "palette::tests|list::tests|automation::tests|focusmode::tests|default_apps_tests"
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
