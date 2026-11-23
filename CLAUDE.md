# BlazingDL — developer & agent guide

BlazingDL is a cross-platform download manager written in Rust (egui + craft-libs), forked from Mister Download Manager under GPL-3.0-or-later.

## Key Rules

1. **Clean room & GPL-3.0 compliance.** Preserve `LICENSE` and `ATTRIBUTION.md`. Never incorporate proprietary code or artwork from closed-source download managers.
2. Every user-visible string goes through `crate::i18n`; run `python tools/i18n/extract.py` after adding strings.
3. Windows-only FFI stays in `crates/app/src/win32.rs`, Unix counterparts in `crates/app/src/unix.rs`; the rest of the workspace enforces `unsafe_code = "deny"`.
4. All internal crates use the `blazingdl-*` prefix; binaries are `blazingdl`, `blazingdl-cli`, `blazingdl-nmhost`, and `blazingdl-mcp`.

## Common Commands

- `cargo build -p blazingdl-app --no-default-features` — build desktop app without BoringSSL (`browser-connection`) native prerequisites.
- `cargo build` / `cargo test` / `cargo clippy --all-targets` / `cargo fmt --all` — workspace (requires CMake, libclang, NASM on Windows for default `browser-connection` feature).
- `cargo run -p blazingdl-labserver -- --faults "rate=2M"` — local fault-injecting test server.
- `cargo run -p blazingdl-app --no-default-features -- --cli get <url> -o <dir>` — headless CLI download.
- `tools/extension/pack.ps1` — build Chromium/Firefox/Safari browser extensions into `target/extension/`.
- `tools/installer/build.ps1` — build Windows installer and portable zips into `target/dist/`.
- `python -I tools/icon/build_icon.py` — regenerate all icon sizes from `art/BlazingDL.png`.
