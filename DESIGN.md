---
name: BlazingDL Desktop App — Saville Catalog Pulse
world: brand-identity-canon-saville-catalog-sleeve
description: Absorbent matte-black high-throughput transfer workstation with live white-hairline pulsar telemetry, flat signal color blocks, and strict 0px architectural geometry.
colors:
  canvas: "#0e0f11"
  header: "#090a0c"
  rail: "#111317"
  rule: "#23262e"
  ink: "#ecebe7"
  muted: "#9498a0"
  disabled: "#555860"
  signal-ember: "#e04826"
  signal-ember-bright: "#f27435"
  signal-cobalt: "#2460b8"
  signal-ochre: "#dfb516"
  signal-emerald: "#28a763"
  selection: "#1b1f27"
  canvas-light: "#f3f2ee"
  header-light: "#eae8e2"
  ink-light: "#111315"
  accent-light: "#c43c12"
---

# BlazingDL Design System — Saville Catalog Pulse

## North Star
**Absorbent matte-black transfer catalog.** Ninety percent of the viewport is matte charcoal-black (`#0E0F11`), deep header obsidian (`#090A0C`), and bone-white ink (`#ECEBE7`). Color never washes across surfaces — it speaks strictly through small flat signal blocks, button end-caps, 2px progress hairlines, and live pulsar peak highlights.

## Core Tokens (`crates/app/assets/themes/saville.toml`)
- **Geometry**: Strict `0px` corner radius across every frame, button, input, menu, progress track, and scrollbar.
- **Rules**: `1px` solid architectural dividers (`#23262E` dark, `#CFCDC6` light) partitioning the Pulse Header, Catalog Rail, Transfer Table, and Telemetry Footer.
- **Row Rhythm**: `32px` transfer rows, `28px` uppercase column headers, `28px` Catalog Rail entries, `86px` Pulse Header, `30px` Telemetry Footer.
- **Signal Color Coding**:
  - `#E04826` / `#F27435` — Primary capture action end-cap, HTTP/Web protocol block, active selection bar, active download velocity & pulsar crest.
  - `#2460B8` — BitTorrent protocol block & Torrent button end-cap.
  - `#DFB516` — Video category block, Pause/Stopped hairline, Speed Limiter warning.
  - `#28A763` — Completed/Verified state & Resume end-cap.
  - `#ECEBE7` — Primary bone-white ink, primary `+ ADD URL` button fill, pulsar baseline traces.

## Signature Component Architecture (`crates/app/src/saville.rs`, `list.rs`)
1. **Top Pulse Header**: Three-column horizontal bar divided by `1px` vertical rules — left `BLAZINGDL` catalog block with 5-swatch barcode, center live 5-line bone-white pulsar waveform + right-aligned bold tabular live speed readout, right two-row `0px` command cluster with signal end-caps.
2. **Catalog Index Rail**: Full-height matte `#111317` sidebar with `8×8` flat color-block category markers, right-aligned two-digit counts, and a `3px` signal-ember active edge bar.
3. **Transfer Table**: Flat `#0E0F11` rows separated by `1px` horizontal hairlines (zero vertical column grid clutter), double `5×14px` vertical protocol color blocks in the Source column, and razor-thin `2px` embedded progress hairlines directly inside the Status cell.
4. **Telemetry Footer**: `30px` `#090A0C` ledger bar with status indicator block, uppercase transfer counts, limiter state, and live aggregate rate.
