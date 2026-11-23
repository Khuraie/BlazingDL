//! Saville Catalog Pulse shell for BlazingDL:
//! absorbent matte-black workstation with a live multi-line pulsar throughput plot,
//! 5-swatch catalog color coding, strict 0px architectural geometry, and spacious transfer rows.

use std::collections::VecDeque;
use std::time::Instant;

use egui::{Color32, FontId, Pos2, Rect, Stroke, Ui, pos2, vec2};

use crate::chrome::{ToolbarEvent, TreeEvent, TreeRow};
use crate::icons::Icon;
use crate::winui::{self, fill_l as fill, text_font, text_width_font};

/// Height of the top Saville Pulse Header band (below the 20 px menu bar).
pub const HEADER_H: f32 = 86.0;
/// Height of the bottom Saville telemetry footer bar.
pub const FOOTER_H: f32 = 30.0;
/// Height of each row in the Saville Catalog Rail.
const RAIL_ROW_H: f32 = 28.0;
/// Height of the Saville Catalog Rail header band.
const RAIL_HEAD_H: f32 = 30.0;

// Signal palette — strictly matte, flat blocks; never washed across backgrounds.
pub const SIGNAL_EMBER: Color32 = Color32::from_rgb(0xE0, 0x48, 0x26);
pub const SIGNAL_EMBER_BRIGHT: Color32 = Color32::from_rgb(0xF2, 0x74, 0x35);
pub const SIGNAL_COBALT: Color32 = Color32::from_rgb(0x24, 0x60, 0xB8);
pub const SIGNAL_OCHRE: Color32 = Color32::from_rgb(0xDF, 0xB5, 0x16);
pub const SIGNAL_EMERALD: Color32 = Color32::from_rgb(0x28, 0xA7, 0x63);
pub const SIGNAL_STEEL: Color32 = Color32::from_rgb(0x8E, 0x91, 0x96);

/// True when the active theme is BlazingDL's Saville Catalog Pulse shell.
pub fn active() -> bool {
    crate::theme::current().id == "saville"
}

/// Rolling live throughput sample buffer for the Pulse Header waveform.
pub struct PulseHistory {
    samples: VecDeque<f64>,
    last_sample: Instant,
    pub current_rate: f64,
    pub active_count: usize,
    pub active_conns: usize,
    phase: f32,
}

impl Default for PulseHistory {
    fn default() -> Self {
        let mut samples = VecDeque::with_capacity(64);
        samples.resize(64, 0.0);
        Self {
            samples,
            last_sample: Instant::now(),
            current_rate: 0.0,
            active_count: 0,
            active_conns: 0,
            phase: 0.0,
        }
    }
}

impl PulseHistory {
    /// Record live aggregate download speed (bytes/sec), active download count, and active streams.
    pub fn record(&mut self, rate: f64, active_count: usize, active_conns: usize) {
        self.current_rate = rate;
        self.active_count = active_count;
        self.active_conns = active_conns;
        let now = Instant::now();
        if now.duration_since(self.last_sample).as_millis() >= 120 {
            self.last_sample = now;
            self.phase = (self.phase + 0.16) % std::f32::consts::TAU;
            if self.samples.len() >= 64 {
                self.samples.pop_front();
            }
            self.samples.push_back(rate.max(0.0));
        }
    }

    pub fn peak(&self) -> f64 {
        self.samples
            .iter()
            .copied()
            .fold(0.0_f64, f64::max)
            .max(1024.0 * 1024.0)
    }
}

fn bar_bg() -> Color32 {
    crate::theme::colors().header
}

fn rule_color() -> Color32 {
    crate::theme::colors().list_frame
}

fn primary_ink() -> Color32 {
    crate::theme::colors().text
}

fn muted_ink() -> Color32 {
    crate::theme::colors().muted
}

/// Draw the 5-swatch Saville catalog barcode at `(x, y)` in English coordinates.
pub fn draw_barcode(p: &egui::Painter, x: f32, y: f32, sw: f32, sh: f32, gap: f32) {
    let swatches = [
        SIGNAL_EMBER,
        SIGNAL_OCHRE,
        SIGNAL_COBALT,
        SIGNAL_EMERALD,
        primary_ink(),
    ];
    for (i, col) in swatches.into_iter().enumerate() {
        let sx = x + i as f32 * (sw + gap);
        let r = winui::rect(Rect::from_min_size(pos2(sx, y), vec2(sw, sh)));
        p.rect_filled(r, 0.0, col);
    }
}

/// Top Saville Pulse Header (below the 20 px menu bar):
/// - Left identity cell (width `rail_w`): icon + BLAZINGDL + version + 5-swatch barcode
/// - Center live pulsar waveform + bold tabular aggregate speed readout
/// - Right action command cluster (0px sharp buttons with signal end-caps)
pub fn header(
    ui: &mut Ui,
    o: Pos2,
    width: f32,
    rail_w: f32,
    pulse: &PulseHistory,
    buttons: &[(Icon, &str, bool)],
    keys: Option<usize>,
) -> Option<ToolbarEvent> {
    let p = ui.painter().clone();
    let top = o.y + crate::chrome::MENU_H;
    let band = Rect::from_min_size(pos2(o.x, top), vec2(width, HEADER_H));
    p.rect_filled(winui::rect(band), 0.0, bar_bg());
    // Crisp 1px bottom rule separating Pulse Header from the workspace panes.
    fill(
        &p,
        o.x,
        band.bottom() - 1.0,
        o.x + width - 1.0,
        band.bottom() - 1.0,
        rule_color(),
    );

    let bar = ui.interact(
        winui::rect(band),
        egui::Id::unique("saville-header"),
        egui::Sense::hover(),
    );
    winui::expose(
        &bar,
        egui::Role::Toolbar,
        crate::i18n::tc("menu", "Toolbar"),
        true,
        None,
    );
    let mut event = None;
    if winui::focus_target(ui.ctx(), bar.id) {
        event = Some(ToolbarEvent::Keys(keys.unwrap_or(0)));
    }
    let bar_ui = ui.new_child(egui::UiBuilder::new().accessibility_parent(bar.id));

    // ---- 1. Left Catalog Identity Cell ----
    let id_w = rail_w.clamp(170.0, (width * 0.28).max(170.0));
    let id_right = o.x + id_w;
    fill(&p, id_right, top, id_right, band.bottom() - 1.0, rule_color());

    let icon_rc = winui::rect(Rect::from_min_size(
        pos2(o.x + 16.0, top + 16.0),
        vec2(28.0, 28.0),
    ));
    egui::Image::new(egui::include_image!("../assets/icons/app/blazingdl-48.png"))
        .paint_at(ui, icon_rc);

    let title_font = FontId::new(16.0, egui::FontFamily::Name("segoe-bold".into()));
    let meta_font = FontId::new(10.0, egui::FontFamily::Name("segoe".into()));
    text_font(
        &p,
        title_font,
        o.x + 54.0,
        top + 30.0,
        "BLAZINGDL",
        primary_ink(),
    );
    let ver = format!("DL-{} · RUST", env!("CARGO_PKG_VERSION"));
    text_font(&p, meta_font.clone(), o.x + 54.0, top + 45.0, &ver, muted_ink());
    draw_barcode(&p, o.x + 16.0, top + 62.0, 14.0, 5.0, 4.0);

    // ---- 3. Right Action Command Cluster (compute width first so center waveform gets the rest) ----
    // Map the existing 11 toolbar buttons into the clean primary command cluster so every
    // shortcut, dropdown, and AccessKit index stays aligned with App::paint's `buttons` slice:
    // 0: AddUrl, 1: AddTorrent, 2: Resume, 3: Stop(Pause), 4: StopAll, 6: DeleteCompleted, 7: Options
    let specs: [(usize, &str, Color32, bool, bool); 7] = [
        (0, "+ ADD URL", SIGNAL_EMBER, true, false),
        (1, "TORRENT", SIGNAL_COBALT, false, true),
        (2, "RESUME", SIGNAL_EMERALD, false, false),
        (3, "PAUSE", SIGNAL_OCHRE, false, false),
        (4, "STOP ALL", SIGNAL_EMBER, false, false),
        (6, "CLEAN", SIGNAL_STEEL, false, false),
        (7, "OPTIONS", SIGNAL_STEEL, false, false),
    ];

    let btn_font = FontId::new(11.0, egui::FontFamily::Name("segoe-bold".into()));
    let btn_h = 30.0;
    let gap = 6.0;
    let pad_r = 16.0;
    // Two-row compact grid on the right: Row 1 = primary capture (+ADD URL, TORRENT, OPTIONS);
    // Row 2 = transfer transport (RESUME, PAUSE, STOP ALL, CLEAN).
    let row1_idx = [0usize, 1, 6]; // indices into `specs`
    let row2_idx = [2usize, 3, 4, 5];

    let measure_btn = |si: usize| -> f32 {
        let (_, lbl, _, _, drop) = specs[si];
        let tw = text_width_font(&p, btn_font.clone(), lbl).ceil();
        (tw + 24.0 + if drop { 14.0 } else { 0.0 }).max(74.0)
    };
    let row1_w: f32 =
        row1_idx.iter().map(|&si| measure_btn(si)).sum::<f32>() + gap * (row1_idx.len() - 1) as f32;
    let row2_w: f32 =
        row2_idx.iter().map(|&si| measure_btn(si)).sum::<f32>() + gap * (row2_idx.len() - 1) as f32;
    let target_row_w = row1_w.max(row2_w);
    let actions_w = target_row_w + pad_r * 2.0;
    let actions_left = (o.x + width - actions_w).max(id_right + 120.0);
    fill(
        &p,
        actions_left,
        top,
        actions_left,
        band.bottom() - 1.0,
        rule_color(),
    );

    let c = crate::theme::colors();
    let mut draw_row = |indices: &[usize], row_natural_w: f32, ry: f32| {
        let extra = (target_row_w - row_natural_w).max(0.0);
        let mut bx = actions_left + pad_r;
        for (pos_in_row, &si) in indices.iter().enumerate() {
            let (orig_i, lbl, cap_color, primary, dropdown) = specs[si];
            let Some((_, full_label, enabled)) = buttons.get(orig_i) else {
                continue;
            };
            let enabled = *enabled;
            let bw = measure_btn(si) + if pos_in_row == 0 { extra } else { 0.0 };
            let rect_en = Rect::from_min_size(pos2(bx, ry), vec2(bw, btn_h));
            let rc = winui::rect(rect_en);
            let resp = bar_ui
                .interact(rc, egui::Id::unique(("saville-tb", orig_i)), winui::pointer_only())
                .on_hover_text(crate::i18n::tc("toolbar", full_label));
            winui::expose(
                &resp,
                egui::Role::Button,
                crate::i18n::tc("toolbar", full_label),
                enabled,
                None,
            );
            if winui::focus_target(ui.ctx(), resp.id) {
                event = Some(ToolbarEvent::Keys(orig_i));
            }
            let hot = enabled && resp.hovered();
            let down = enabled && resp.is_pointer_button_down_on();
            let (bg, border, ink) = if primary {
                if down {
                    (c.button_accent_pressed, c.button_accent_pressed, c.button_accent_text)
                } else if hot {
                    (c.button_accent_hot, c.button_accent_hot, c.button_accent_text)
                } else {
                    (c.button_accent, c.button_accent, c.button_accent_text)
                }
            } else if !enabled {
                (c.button_disabled, c.button_disabled_border, c.text_disabled)
            } else if down {
                (c.button_pressed, c.accent, c.button_text)
            } else if hot {
                (c.button_hot, SIGNAL_STEEL, c.button_text)
            } else {
                (c.button, c.button_border, c.button_text)
            };
            p.rect_filled(rc, 0.0, bg);
            p.rect_stroke(rc, 0.0, Stroke::new(1.0, border), egui::StrokeKind::Inside);
            // Saville signature flat square end-cap along leading edge.
            let cap_w = if primary { 5.0 } else { 3.0 };
            let cap_rc = winui::rect(Rect::from_min_size(pos2(bx, ry), vec2(cap_w, btn_h)));
            p.rect_filled(cap_rc, 0.0, if enabled { cap_color } else { c.button_disabled_border });

            let label_w = text_width_font(&p, btn_font.clone(), lbl);
            let content_w = bw - cap_w - if dropdown { 14.0 } else { 0.0 };
            let lx = bx + cap_w + ((content_w - label_w) * 0.5).max(5.0);
            let ly = ry + 19.0;
            text_font(&p, btn_font.clone(), lx, ly, lbl, ink);

            if dropdown {
                let ax = bx + bw - 12.0;
                let ay = ry + btn_h * 0.5 - 1.5;
                let tri = [
                    pos2(winui::mx(ax), ay),
                    pos2(winui::mx(ax + 6.0), ay),
                    pos2(winui::mx(ax + 3.0), ay + 4.0),
                ];
                p.add(egui::Shape::convex_polygon(
                    tri.to_vec(),
                    ink,
                    Stroke::NONE,
                ));
            }
            if keys == Some(orig_i) {
                p.rect_stroke(
                    rc.shrink(2.0),
                    0.0,
                    Stroke::new(1.0, c.focus_ring),
                    egui::StrokeKind::Inside,
                );
            }
            if enabled && resp.clicked() {
                let on_arrow = dropdown
                    && resp
                        .interact_pointer_pos()
                        .is_some_and(|q| winui::mx(q.x) >= bx + bw - 16.0);
                event = Some(if on_arrow {
                    ToolbarEvent::Drop(orig_i, pos2(winui::mx(bx), ry + btn_h + 2.0))
                } else {
                    ToolbarEvent::Click(orig_i)
                });
            }
            bx += bw + gap;
        }
    };
    draw_row(&row1_idx, row1_w, top + 10.0);
    draw_row(&row2_idx, row2_w, top + 46.0);

    // ---- 2. Center Live Pulsar Waveform + Bold Tabular Throughput Readout ----
    let center_left = id_right + 1.0;
    let center_w = (actions_left - center_left).max(0.0);
    if center_w >= 140.0 {
        let rate_str = if pulse.current_rate > 0.5 {
            crate::model::rate(pulse.current_rate).to_uppercase()
        } else {
            "0 B/S".to_owned()
        };
        let speed_font = FontId::new(22.0, egui::FontFamily::Name("segoe-bold".into()));
        let sub_str = if pulse.active_count > 0 {
            format!(
                "{} ACTIVE · {} STREAMS",
                pulse.active_count,
                pulse.active_conns.max(pulse.active_count)
            )
        } else {
            "STANDBY · READY".to_owned()
        };
        let speed_w = text_width_font(&p, speed_font.clone(), &rate_str);
        let sub_w = text_width_font(&p, meta_font.clone(), &sub_str);
        let readout_w = speed_w.max(sub_w).max(118.0) + 28.0;
        let readout_x = actions_left - 18.0;

        let speed_color = if pulse.active_count > 0 {
            primary_ink()
        } else {
            muted_ink()
        };
        text_font(
            &p,
            speed_font,
            readout_x - speed_w,
            top + 43.0,
            &rate_str,
            speed_color,
        );
        text_font(
            &p,
            meta_font,
            readout_x - sub_w,
            top + 61.0,
            &sub_str,
            if pulse.active_count > 0 {
                SIGNAL_EMBER_BRIGHT
            } else {
                muted_ink()
            },
        );

        // Stacked 5-line Saville pulsar plot occupying the left side of the center cell.
        let wave_left = center_left + 18.0;
        let wave_right = (actions_left - readout_w - 14.0).max(wave_left);
        let wave_w = wave_right - wave_left;
        if wave_w >= 64.0 {
            draw_pulsar_plot(
                &p,
                Rect::from_min_max(pos2(wave_left, top + 14.0), pos2(wave_right, band.bottom() - 14.0)),
                pulse,
            );
        }
    }

    event
}

/// Draws 5 stacked Joy Division / Saville white-hairline pulsar traces driven by live throughput.
fn draw_pulsar_plot(p: &egui::Painter, rect: Rect, pulse: &PulseHistory) {
    let lines = 5usize;
    let peak = pulse.peak();
    let n = pulse.samples.len().max(2);
    let step_x = rect.width() / (n - 1) as f32;
    let dark = crate::theme::dark_surface();
    let base_ink = if dark {
        Color32::from_rgb(0xEC, 0xEB, 0xE7)
    } else {
        Color32::from_rgb(0x11, 0x13, 0x15)
    };
    let dim_rule = rule_color();

    for row in 0..lines {
        let t_row = row as f32 / (lines - 1) as f32;
        let base_y = (rect.top() + 10.0 + t_row * (rect.height() - 14.0)).round() + 0.5;
        let weight = 1.0 - t_row * 0.55;
        let alpha = ((235.0 - t_row * 135.0) as u8).max(70);
        let stroke_col = Color32::from_rgba_unmultiplied(base_ink.r(), base_ink.g(), base_ink.b(), alpha);
        let mut pts = Vec::with_capacity(n);
        let mut any_signal = false;

        for (i, &sample) in pulse.samples.iter().enumerate() {
            let u = i as f32 / (n - 1) as f32;
            // Taper edges cleanly to zero so outer margins remain flat matte hairlines.
            let env = (std::f32::consts::PI * u).sin().powf(1.6);
            let norm = (sample / peak).clamp(0.0, 1.0) as f32;
            let amp = if norm > 0.002 {
                any_signal = true;
                // Sharp harmonic modulation across the 5 stacked traces for unsmoothed plotter feel.
                let harmonic = 0.65
                    + 0.35 * ((u * 29.0 + row as f32 * 1.7 + pulse.phase).sin().abs());
                norm * env * harmonic * 24.0 * weight
            } else {
                // Quiet architectural standby pulse (tiny crisp central needle ridge, never loud).
                let ridge = (-((u - 0.5) * 7.0).powi(2)).exp();
                let micro = ((u * 38.0 + row as f32 * 1.9).sin().abs()).powf(2.0);
                ridge * micro * 4.5 * weight
            };
            let x = (rect.left() + i as f32 * step_x).round() + 0.5;
            let y = (base_y - amp).max(rect.top() + 1.0);
            pts.push(pos2(winui::mx(x), y));
        }

        // Subtle baseline rule under the bottom trace for plotter grounding.
        if row == lines - 1 {
            p.line_segment(
                [
                    pos2(winui::mx(rect.left()), base_y),
                    pos2(winui::mx(rect.right()), base_y),
                ],
                Stroke::new(1.0, dim_rule),
            );
        }
        let line_color = if row == 0 && any_signal {
            SIGNAL_EMBER_BRIGHT
        } else {
            stroke_col
        };
        let stroke_w = if row == 0 && any_signal { 1.35 } else { 1.0 };
        p.add(egui::Shape::line(pts, Stroke::new(stroke_w, line_color)));
    }
}

/// Flat Saville color-block hue for each catalog row (category/queue/state).
fn rail_block_color(label: &str, level: usize) -> Color32 {
    if level == 0 {
        match label {
            "All Downloads" => SIGNAL_EMBER,
            "Unfinished" => primary_ink(),
            "Finished" => SIGNAL_EMERALD,
            "Queues" => SIGNAL_COBALT,
            _ => SIGNAL_STEEL,
        }
    } else {
        match label {
            "Video" => SIGNAL_OCHRE,
            "Torrents" => SIGNAL_COBALT,
            "Compressed" => SIGNAL_EMBER_BRIGHT,
            "Programs" => SIGNAL_EMERALD,
            _ => SIGNAL_STEEL,
        }
    }
}

/// Left Saville Catalog Index Rail (replaces the 2000s boxed Categories treeview).
pub fn catalog_rail(
    ui: &mut Ui,
    o: Pos2,
    top: f32,
    bottom: f32,
    right: f32,
    rows: &[TreeRow<'_>],
    keys: bool,
) -> Option<TreeEvent> {
    let p = ui.painter().clone();
    let c = crate::theme::colors();
    let panel = Rect::from_min_max(pos2(o.x, top), pos2(right, bottom));
    p.rect_filled(winui::rect(panel), 0.0, c.tree);
    // Single crisp 1px vertical right rule dividing rail from the transfer list.
    fill(&p, right - 1.0, top, right - 1.0, bottom - 1.0, rule_color());

    // Rail header strip: wide-tracked uppercase "CATALOG" + quiet close button.
    let head_font = FontId::new(10.0, egui::FontFamily::Name("segoe-bold".into()));
    text_font(&p, head_font, o.x + 16.0, top + 19.0, "CATALOG", muted_ink());
    fill(
        &p,
        o.x,
        top + RAIL_HEAD_H - 1.0,
        right - 1.0,
        top + RAIL_HEAD_H - 1.0,
        rule_color(),
    );

    let mut event = None;
    let close_rc = winui::rect(Rect::from_min_size(
        pos2(right - 26.0, top + 5.0),
        vec2(20.0, 20.0),
    ));
    let cr = ui
        .interact(close_rc, egui::Id::unique("saville-rail-close"), winui::pointer_only())
        .on_hover_text(crate::i18n::t("Close"));
    if cr.hovered() {
        p.rect_filled(close_rc, 0.0, c.subtle_hot);
    }
    text_font(
        &p,
        FontId::new(11.0, egui::FontFamily::Name("segoe".into())),
        right - 19.0,
        top + 19.0,
        "×",
        muted_ink(),
    );
    if cr.clicked() {
        event = Some(TreeEvent::Close);
    }

    let area = Rect::from_min_max(pos2(o.x, top + RAIL_HEAD_H), pos2(right - 1.0, bottom));
    let content_h = rows.len() as f32 * RAIL_ROW_H;
    let scroll_id = egui::Id::unique("saville-rail-scroll");
    let mut scroll = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(scroll_id))
        .unwrap_or(0.0);
    let max_scroll = (content_h - area.height()).max(0.0);
    if ui.rect_contains_pointer(winui::rect(area)) {
        let delta = ui.input(|i| i.smooth_scroll_delta.y);
        scroll = (scroll - delta).clamp(0.0, max_scroll);
    }
    ui.ctx().data_mut(|d| d.insert_temp(scroll_id, scroll));

    let tp = p.with_clip_rect(winui::rect(area));
    let blank = ui.interact(
        winui::rect(area),
        egui::Id::unique("saville-rail-blank"),
        winui::pointer_only(),
    );
    if blank.secondary_clicked()
        && let Some(k) = rows.iter().position(|r| r.selected)
        && let Some(at) = blank.interact_pointer_pos()
    {
        event = Some(TreeEvent::Context(k, at));
    }

    let row_font = FontId::new(12.0, egui::FontFamily::Name("segoe".into()));
    let row_font_bold = FontId::new(12.0, egui::FontFamily::Name("segoe-bold".into()));
    let cnt_font = FontId::new(11.0, egui::FontFamily::Name("segoe".into()));

    for (k, row) in rows.iter().enumerate() {
        let ry = area.top() + k as f32 * RAIL_ROW_H - scroll;
        if ry + RAIL_ROW_H < area.top() || ry > area.bottom() {
            continue;
        }
        let row_rect_en = Rect::from_min_size(pos2(area.left(), ry), vec2(area.width(), RAIL_ROW_H));
        let row_rc = winui::rect(row_rect_en);
        let resp = ui.interact(
            row_rc.intersect(winui::rect(area)),
            egui::Id::unique(("saville-rail-row", k)),
            winui::pointer_only(),
        );
        let label_txt = crate::i18n::t(&row.label);
        winui::expose(&resp, egui::Role::TreeItem, label_txt, true, None);
        if winui::focus_target(ui.ctx(), resp.id) {
            event = Some(TreeEvent::Keys(Some(k)));
        }
        if row.selected {
            tp.rect_filled(row_rc, 0.0, c.selection);
            // 3px solid Saville signal edge bar on the active category.
            let bar_rc = winui::rect(Rect::from_min_size(pos2(area.left(), ry), vec2(3.0, RAIL_ROW_H)));
            tp.rect_filled(bar_rc, 0.0, SIGNAL_EMBER);
        } else if resp.hovered() {
            tp.rect_filled(row_rc, 0.0, c.menu_hot);
        }

        let indent = if row.level == 0 { 14.0 } else { 30.0 };
        let mut x = area.left() + indent;
        // Optional expand/collapse caret for branches.
        if let Some(open) = row.expander {
            let glyph = if open { "▾" } else { "▸" };
            let exp_rc = winui::rect(Rect::from_min_size(pos2(x - 10.0, ry), vec2(14.0, RAIL_ROW_H)));
            text_font(&tp, cnt_font.clone(), x - 8.0, ry + 18.0, glyph, muted_ink());
            if resp.clicked()
                && resp
                    .interact_pointer_pos()
                    .is_some_and(|pt| exp_rc.contains(pt))
            {
                event = Some(TreeEvent::Toggle(k));
            }
        }
        x += 4.0;
        // 8x8 flat Saville catalog color block.
        let blk = winui::rect(Rect::from_min_size(pos2(x, ry + 10.0), vec2(8.0, 8.0)));
        tp.rect_filled(blk, 0.0, rail_block_color(&row.label, row.level));
        x += 16.0;

        // Right-aligned tabular item count.
        let count_str = row.count.map(|n| format!("{n:02}"));
        let count_w = count_str
            .as_ref()
            .map_or(0.0, |s| text_width_font(&tp, cnt_font.clone(), s) + 16.0);
        let max_lbl_w = (area.right() - x - count_w - 6.0).max(20.0);
        let font = if row.selected {
            row_font_bold.clone()
        } else {
            row_font.clone()
        };
        let shown = winui::ellipsize(&tp, &font, label_txt, max_lbl_w);
        let ink = if row.selected {
            primary_ink()
        } else if row.level == 0 {
            c.text
        } else {
            muted_ink()
        };
        text_font(&tp, font, x, ry + 18.5, &shown, ink);
        if let Some(cs) = count_str {
            let cw = text_width_font(&tp, cnt_font.clone(), &cs);
            text_font(
                &tp,
                cnt_font.clone(),
                area.right() - 12.0 - cw,
                ry + 18.5,
                &cs,
                if row.selected { primary_ink() } else { muted_ink() },
            );
        }
        if keys && row.selected {
            tp.rect_stroke(
                row_rc.shrink(1.0),
                0.0,
                Stroke::new(1.0, c.focus_ring),
                egui::StrokeKind::Inside,
            );
        }
        if event.is_none() && resp.clicked() {
            event = Some(TreeEvent::Select(k));
        }
        if resp.double_clicked() && row.expander.is_some() {
            event = Some(TreeEvent::Toggle(k));
        }
        if resp.secondary_clicked()
            && let Some(at) = resp.interact_pointer_pos()
        {
            event = Some(TreeEvent::Context(k, at));
        }
    }

    event
}

/// Draw Saville double color-block protocol tag inside the Source column cell.
pub fn protocol_blocks(
    p: &egui::Painter,
    font: &FontId,
    (x0, x1): (f32, f32),
    top: f32,
    row_h: f32,
    label: &str,
) -> bool {
    if label.is_empty() {
        return false;
    }
    let primary = if label == "Torrent" {
        SIGNAL_COBALT
    } else {
        SIGNAL_EMBER
    };
    let secondary = primary_ink();
    let bh = 14.0_f32.min((row_h - 6.0).max(8.0));
    let by = top + ((row_h - bh) * 0.5).round();
    let b1 = winui::rect(Rect::from_min_size(pos2(x0 + 8.0, by), vec2(5.0, bh)));
    let b2 = winui::rect(Rect::from_min_size(pos2(x0 + 15.0, by), vec2(5.0, bh)));
    p.rect_filled(b1, 0.0, primary);
    p.rect_filled(b2, 0.0, secondary);

    let full = crate::i18n::t(label).to_uppercase();
    let tx = x0 + 27.0;
    let avail = (x1 - tx - 6.0).max(0.0);
    let shown = winui::ellipsize(p, font, &full, avail);
    let baseline = top + (row_h * 0.5).round() + 4.0;
    text_font(p, font.clone(), tx, baseline, &shown, muted_ink());
    shown != full
}

/// Draw razor-thin 2px embedded Saville progress hairline along the bottom of the Status cell.
pub fn inline_progress(
    p: &egui::Painter,
    cell: Rect,
    frac: f64,
    status_group: &str,
) {
    let frac = frac.clamp(0.0, 1.0) as f32;
    let track_l = cell.left() + 6.0;
    let track_r = cell.right() - 8.0;
    if track_r <= track_l + 8.0 {
        return;
    }
    let bar_y = (cell.bottom() - 6.0).round();
    let track = winui::rect(Rect::from_min_max(pos2(track_l, bar_y), pos2(track_r, bar_y + 2.0)));
    p.rect_filled(track, 0.0, crate::theme::colors().progress_track);
    let fill_w = ((track_r - track_l) * frac).round();
    if fill_w > 0.0 {
        let col = match status_group {
            "Complete" => SIGNAL_EMERALD,
            "Error" => crate::theme::colors().error,
            "Stopped" | "Waiting" => SIGNAL_OCHRE,
            _ => SIGNAL_EMBER_BRIGHT,
        };
        let done = winui::rect(Rect::from_min_size(pos2(track_l, bar_y), vec2(fill_w, 2.0)));
        p.rect_filled(done, 0.0, col);
    }
}

/// Clean matte-black Saville empty state for the download list.
pub fn empty_state(
    ui: &Ui,
    body: Rect,
    title: &str,
    why: &str,
    action: &str,
) -> bool {
    let p = ui.painter();
    let c = crate::theme::colors();
    let title_font = FontId::new(18.0, egui::FontFamily::Name("segoe-bold".into()));
    let body_font = FontId::new(12.0, egui::FontFamily::Name("segoe".into()));
    let btn_font = FontId::new(11.0, egui::FontFamily::Name("segoe-bold".into()));

    let title_up = title.to_uppercase();
    let tw = text_width_font(p, title_font.clone(), &title_up);
    let ww = text_width_font(p, body_font.clone(), why);
    let action_up = format!("+ {}", action.to_uppercase());
    let aw = text_width_font(p, btn_font.clone(), &action_up);

    let block_h = 138.0;
    let top = (body.center().y - block_h * 0.5).max(body.top() + 28.0);
    let cx = body.center().x;

    // Centered 5-swatch Saville catalog barcode above headline.
    let bar_w = 5.0 * 18.0 + 4.0 * 5.0;
    draw_barcode(p, cx - bar_w * 0.5, top, 18.0, 5.0, 5.0);

    let y = top + 36.0;
    text_font(p, title_font, cx - tw * 0.5, y, &title_up, primary_ink());
    let y = y + 24.0;
    text_font(p, body_font, cx - ww * 0.5, y, why, muted_ink());

    // Sharp 0px bone-white primary button with 5px ember end-cap.
    let bw = (aw + 36.0).max(168.0);
    let bh = 34.0;
    let bx = cx - bw * 0.5;
    let by = y + 22.0;
    let rc = winui::rect(Rect::from_min_size(pos2(bx, by), vec2(bw, bh)));
    let resp = ui.interact(rc, egui::Id::unique("saville-empty-action"), winui::pointer_only());
    winui::expose(&resp, egui::Role::Button, action, true, None);
    let fill_col = if resp.is_pointer_button_down_on() {
        c.button_accent_pressed
    } else if resp.hovered() {
        c.button_accent_hot
    } else {
        c.button_accent
    };
    p.rect_filled(rc, 0.0, fill_col);
    p.rect_filled(
        winui::rect(Rect::from_min_size(pos2(bx, by), vec2(5.0, bh))),
        0.0,
        SIGNAL_EMBER,
    );
    text_font(
        p,
        btn_font,
        bx + 5.0 + (bw - 5.0 - aw) * 0.5,
        by + 21.0,
        &action_up,
        c.button_accent_text,
    );
    resp.clicked()
}

/// Bottom 30px matte-black Saville telemetry footer bar.
pub fn footer_bar(
    ui: &Ui,
    o: Pos2,
    w: f32,
    h: f32,
    left: &str,
    right: Option<&str>,
    limiter_on: bool,
) {
    let p = ui.painter();
    let top = o.y + h - FOOTER_H;
    let rc = Rect::from_min_max(pos2(o.x, top), pos2(o.x + w, o.y + h));
    p.rect_filled(winui::rect(rc), 0.0, bar_bg());
    fill(&p, o.x, top, o.x + w - 1.0, top, rule_color());

    let strip = ui.interact(
        winui::rect(rc),
        egui::Id::unique("status-strip"),
        egui::Sense::hover(),
    );
    let said = match right {
        Some(r) => format!("{left}, {r}"),
        None => left.to_owned(),
    };
    winui::expose(
        &strip,
        egui::Role::Status,
        &said.replace("   ", ", "),
        true,
        None,
    );

    let font = FontId::new(11.0, egui::FontFamily::Name("segoe".into()));
    let baseline = top + 19.0;
    // Small status indicator square on the left.
    let dot_col = if limiter_on { SIGNAL_OCHRE } else { SIGNAL_EMERALD };
    p.rect_filled(
        winui::rect(Rect::from_min_size(pos2(o.x + 14.0, top + 12.0), vec2(6.0, 6.0))),
        0.0,
        dot_col,
    );
    let left_up = left.to_uppercase();
    text_font(p, font.clone(), o.x + 28.0, baseline, &left_up, muted_ink());

    if let Some(r) = right {
        let r_up = r.to_uppercase();
        let rw = text_width_font(p, font.clone(), &r_up);
        text_font(
            p,
            font,
            o.x + w - 16.0 - rw,
            baseline,
            &r_up,
            if limiter_on { SIGNAL_OCHRE } else { primary_ink() },
        );
    }
}
