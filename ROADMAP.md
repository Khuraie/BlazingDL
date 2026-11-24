# BlazingDL Roadmap

Current engineering status for **BlazingDL** (`v0.5.3`+). Released changes are tracked in [CHANGELOG.md](CHANGELOG.md). Bug reports and feature requests belong in [Issues](https://github.com/Khuraie/BlazingDL/issues).

## Shipped (v0.5.3)

- **300+ GB 1× Disk Stream Unpack (`.zip`/ZIP64, `.tar.*`, `.acpkg` v1):** Pure-Rust HTTP Range streaming extraction (`<512 KiB`/worker RAM), parallel multi-connection ZIP `STORE` slicing, O(1) instant Pause → Resume, `crc32fast` verification, engine HTTP transport (Cookies/Auth/Proxy/PAC/DNS/Multi-NIC), and transparent fallback on non-Range or encrypted archives.
- **Saville Catalog Pulse UI:** Matte-obsidian workstation shell (86px Pulse Header, 36px inline Command & Capture Strip, Catalog Index Rail, Telemetry Footer), redesigned Saville Download Progress Window (live throughput sparkline, 2×4 telemetry matrix, server Geo/RTT, per-worker speeds, `Log` tab), and matching Saville browser extension (floating video overlay + toolbar popup).
- **High-Throughput Transfer Engine:** Multi-NIC bandwidth aggregation (`Route::Aggregate`), adaptive stream concurrency (`AdaptiveScaler`), speed-limit-aware hedged tail racing (`Planner::preempt_slow_tail`), RFC 5854 Metalink multi-mirror racing, and native async NNTP/NNTPS + streaming yEnc Usenet (`.nzb`) downloads.
- **Automation & Control:** Smart clipboard/sibling checksum auto-matching (off-UI-thread), post-download regex/category automation rules (move, shell command, Discord/ntfy/JSON webhooks), fullscreen Game/Focus auto-throttle with 24-hour bandwidth curve, CLI (`blazingdl-cli`), Remote Web UI (`blazingdl-remote`), and MCP stdio server (`blazingdl-mcp`).
- **Multi-Platform Packaging & CI:** Automated GitHub Actions CI and Release workflows covering Windows (`x64`, `ARM64`, `x86`), Linux (`x86_64`, `ARM64`: AppImage, `.deb`, `.rpm`, `.tar.gz`), macOS (`arm64`, `x86_64` `.dmg` + Safari native extension relay), and Docker.

## In Progress

- **Browser Extension Store Listings:** Complete Mozilla Add-ons (AMO) review/signing for `blazingdl-firefox.xpi` and publish the Chrome Web Store listing so one-click install & auto-updates work across Firefox and Chromium browsers (and re-enable Chromium store links in the extension dialog).
- **BlazingDL Release Signing Key (Auto-Updater):** Rotate `crates/tools/keys/` from the inherited upstream OpenPGP key to BlazingDL's own Ed25519 release signing key and configure `BLAZINGDL_RELEASE_KEY` in GitHub Actions so in-app `Help → Check for updates` verifies and installs BlazingDL releases directly.
- **OS Code Signing:** Obtain Windows Authenticode and macOS Developer ID signing/notarization certificates to remove first-run SmartScreen and Gatekeeper warnings.

## Planned

- **Real-World CDN Hardening:** Expand automated fault tests and real hoster compatibility for signed expiring CDN links, tricky `Content-Range` edge cases, and flaky Wi-Fi/cellular Multi-NIC handoffs.
- **Linux & macOS Live Desktop Verification:** Validate Wayland/X11 packaging, tray panels, and macOS `.dmg`/Safari extension flows on live desktop installs to remove the `-experimental` macOS suffix.
- **Extended Streaming Archive Support:** Investigate bounded-memory streaming improvements for additional multi-file archive containers alongside existing 7-Zip / HatchZip `.rar` and `.7z` extraction.

---

*Upstream history: BlazingDL forked from [Mister Download Manager](https://github.com/anefzaoui/misterdownloadmanager) at v0.5.2. See [ATTRIBUTION.md](ATTRIBUTION.md) for upstream credits and the `0.5.2` and earlier entries in [CHANGELOG.md](CHANGELOG.md).*
