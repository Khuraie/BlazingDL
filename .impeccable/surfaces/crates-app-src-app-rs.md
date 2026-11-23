---
version: 1
slug: "crates-app-src-app-rs"
primary_target: "crates/app/src/app.rs"
related_targets: ["crates/app/src/chrome.rs","crates/app/src/list.rs","crates/app/src/palette.rs"]
---

# Surface brief — BlazingDL Desktop App (`crates/app/src/app.rs`)

- **Mode:** Operate (native Rust/egui desktop download manager)
- **Audience & job:** Power users and everyday downloaders managing segmented HTTP/FTP transfers, BitTorrent downloads, and yt-dlp videos simultaneously, scanning live speed, completion progress, and status in half a second.
- **Constraints:** Preserve all power-user features (queues, scheduler, torrents, video grabber, right-click menus, keyboard shortcuts, accessibility/AccessKit, RTL mirroring, and selectable built-in themes under View → Theme) while making Saville Catalog Pulse the default visual identity and main-window architecture.

## Direction contract

- **THESIS:** BlazingDL is a matte-black high-throughput transfer workstation anchored by a live white-hairline throughput pulsar waveform and color-block protocol coding, refusing the 2000s IDM gray beveled toolbar and sunken treeview clone.
- **OWN-WORLD:** Deep matte-black shell (`#090A0C` header/footer, `#0E0F11` main canvas, `#111317` navigation rail, `#20232A` crisp 1px rules), stark chalk-white primary ink (`#F2F2F2`), muted steel metadata (`#8A909E`), ember-orange live velocity (`#F27435`), protocol hue blocks (`#F27435` HTTP, `#38BDF8` Torrent, `#EAB308` Video, `#22C55E` Complete), and razor-thin 2px inline progress hairlines directly inside every transfer row.
- **STORY:** Within half a second of opening BlazingDL, the user reads live aggregate bandwidth (`MB/s` + active stream count) beside a living hairline pulsar waveform, drops or adds a URL from the top capture bar, scans every download's protocol block, hairline completion bar, speed, and ETA at a glance, and pauses/resumes/opens files without hunting through nested 2000s chrome.
- **FIRST VIEWPORT:** A three-band matte-black desktop window (1280×800): (1) a 96px top **Pulse Header** split into the BlazingDL brand/version block on the left, a live multi-line white-and-ember throughput pulsar waveform + bold tabular aggregate speed readout (`MB/s` & active streams) in the center, and high-contrast primary actions (`+ Add URL`, `Add Torrent`, `Resume`, `Pause`, `Options`, `Menu`) on the right; (2) a 216px left **Catalog Rail** (`#111317`) listing categories and queues with 8×8 color-block markers and right-aligned counts beside a clean flat **Transfer Table** (`#0E0F11`) featuring double-block protocol tags, crisp filename rows, embedded 2px progress hairlines, live tabular speed, and ETA; (3) a 32px matte-black **Telemetry Footer** (`#090A0C`) displaying active connections, queue state, disk/host status, and speed limiter.
- **FORM:** Saville Catalog Pulse (`brand-identity-canon-saville-catalog-sleeve`, competitive challenger chosen by user; matched HTML sketch `.impeccable/mocks/decision/challenger-saville.html`), `seed:49458d340af1b330`.
- **FINISH:** unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance.
