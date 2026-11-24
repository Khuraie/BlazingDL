# Product

## Purpose

**BlazingDL** is a free, open-source (GPL-3.0-or-later) native Rust download manager for Windows, Linux, and macOS. It pairs a high-throughput segmented transfer engine with the **Saville Catalog Pulse** desktop workstation UI (plus 10 classic and native themes).

## Core Differentiators

- **300+ GB 1× Disk Stream Unpack (`.zip`/ZIP64, `.tar.*`, `.acpkg`):** Streams remote archives directly into extracted folders in `<512 KiB`/worker RAM without saving the archive to disk, with parallel multi-connection ZIP `STORE` slicing, O(1) Pause → Resume, and transparent fallback on non-Range or encrypted archives.
- **High-Throughput Engine:** Multi-NIC bandwidth aggregation (`Route::Aggregate`), adaptive stream concurrency, speed-limit-aware hedged tail racing, RFC 5854 Metalink multi-mirror racing, BitTorrent (`librqbit`), and native async NNTP/yEnc Usenet (`.nzb`).
- **Automation & Control:** Browser extension capture (Firefox, Chromium, Safari), smart checksum matching, post-download automation rules & webhooks (Discord/ntfy/JSON), fullscreen Game/Focus throttle + 24h curve, CLI (`blazingdl-cli`), Remote Web UI (`blazingdl-remote`), and MCP server (`blazingdl-mcp`).

## Principles

1. **Prove, don't claim:** Performance and memory claims link to reproducible tests and live benchmarks (e.g. `crates/extract/examples/live_unpack.rs`).
2. **Honest about limits:** Beta caveats (unsigned binaries, pending store approvals, format fallbacks) are documented openly in [README.md](README.md#current-beta-limitations).
3. **Clean room & GPL-3.0 compliance:** No proprietary code, binaries, or artwork from closed-source download managers. Upstream credits are preserved in [ATTRIBUTION.md](ATTRIBUTION.md).
4. **Zero bloat or telemetry:** Free, offline-first, no ads, no trials, no accounts.
