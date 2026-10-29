//! egui front end: vertical-frequency waterfall (time scrolls right-to-left, newest at the right),
//! frequency scale, spot labels aligned to each signal's frequency, and the selected decoder's text.

use crate::engine::{DecoderView, Engine, Settings, wf_db};
use crate::source::{SliceState, SourceConfig};
use crate::source::flex::{self, RadioInfo};
use crossbeam_channel::Receiver;
use eframe::egui::{
    self, Align2, Color32, ColorImage, FontId, Rect, Sense, Stroke, TextureHandle, TextureOptions, pos2, vec2,
};
use std::collections::VecDeque;
use std::time::Duration;

const HISTORY_COLS: usize = 900;
const SCALE_W: f32 = 62.0;
const LINE_H: f32 = 15.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SrcKind {
    Flex,
    Wav,
    Synth,
}

pub struct App {
    started: std::time::Instant,
    screenshot_requested: bool,
    engine: Engine,
    settings: Settings,
    sent_settings: Settings,
    kind: SrcKind,
    flex_addr: String,
    flex_channel: u8,
    flex_rate: u32,
    wav_path: String,
    wav_center_khz: f64,
    synth_rate: u32,
    synth_center_khz: f64,
    synth_stations: usize,
    synth_snr: f32,
    discovery: Option<Receiver<Result<Vec<RadioInfo>, String>>>,
    radios: Vec<RadioInfo>,
    error: Option<String>,
    wf: Waterfall,
    view: (f64, f64),
    view_rate: u32,
    selected: Option<u64>,
    hovered: Option<u64>,
    /// Offset (Hz from center) of a signal the user double-clicked, to tune the radio to.
    tune_request: Option<(u64, f64)>,
    slice_a: Option<SliceState>,
    floor_db: f32,
    range_db: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, settings: Settings, autostart: Option<SourceConfig>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            started: std::time::Instant::now(),
            screenshot_requested: false,
            engine: Engine::new(settings.clone()),
            sent_settings: settings.clone(),
            settings,
            kind: SrcKind::Synth,
            flex_addr: String::new(),
            flex_channel: 1,
            flex_rate: 0,
            wav_path: String::new(),
            wav_center_khz: 7025.0,
            synth_rate: 48_000,
            synth_center_khz: 7025.0,
            synth_stations: 20,
            synth_snr: 6.0,
            discovery: None,
            radios: Vec::new(),
            error: None,
            wf: Waterfall::new(),
            view: (-1.0, 1.0),
            view_rate: 0,
            selected: None,
            hovered: None,
            tune_request: None,
            slice_a: None,
            floor_db: 2.0,
            range_db: 30.0,
        };
        if let Some(cfg) = autostart {
            match &cfg {
                SourceConfig::Flex { addr, daxiq_channel, rate } => {
                    app.kind = SrcKind::Flex;
                    app.flex_addr = addr.clone();
                    app.flex_channel = *daxiq_channel;
                    app.flex_rate = rate.unwrap_or(0);
                }
                SourceConfig::Wav { path, center_hz } => {
                    app.kind = SrcKind::Wav;
                    app.wav_path = path.display().to_string();
                    app.wav_center_khz = center_hz / 1e3;
                }
                SourceConfig::Synth { rate, center_hz, stations, min_snr_db } => {
                    app.kind = SrcKind::Synth;
                    app.synth_rate = *rate;
                    app.synth_center_khz = center_hz / 1e3;
                    app.synth_stations = *stations;
                    app.synth_snr = *min_snr_db;
                }
            }
            app.error = app.engine.start(cfg).err();
        }
        app
    }

    fn source_config(&self) -> Result<SourceConfig, String> {
        Ok(match self.kind {
            SrcKind::Flex => {
                if self.flex_addr.trim().is_empty() {
                    return Err("Enter the radio's IP address or press Discover".into());
                }
                SourceConfig::Flex {
                    addr: self.flex_addr.trim().to_string(),
                    daxiq_channel: self.flex_channel,
                    rate: (self.flex_rate > 0).then_some(self.flex_rate),
                }
            }
            SrcKind::Wav => SourceConfig::Wav { path: self.wav_path.trim().into(), center_hz: self.wav_center_khz * 1e3 },
            SrcKind::Synth => SourceConfig::Synth {
                rate: self.synth_rate,
                center_hz: self.synth_center_khz * 1e3,
                stations: self.synth_stations,
                min_snr_db: self.synth_snr,
            },
        })
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let running = self.engine.is_running();
            ui.add_enabled_ui(!running, |ui| {
                egui::ComboBox::from_id_salt("src")
                    .selected_text(match self.kind {
                        SrcKind::Flex => "FlexRadio DAX IQ",
                        SrcKind::Wav => "I/Q WAV file",
                        SrcKind::Synth => "Synthetic test band",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.kind, SrcKind::Flex, "FlexRadio DAX IQ");
                        ui.selectable_value(&mut self.kind, SrcKind::Wav, "I/Q WAV file");
                        ui.selectable_value(&mut self.kind, SrcKind::Synth, "Synthetic test band");
                    });
                match self.kind {
                    SrcKind::Flex => {
                        ui.label("Radio IP");
                        ui.add(egui::TextEdit::singleline(&mut self.flex_addr).desired_width(110.0));
                        if ui.add_enabled(self.discovery.is_none(), egui::Button::new("Discover")).clicked() {
                            let (tx, rx) = crossbeam_channel::bounded(1);
                            std::thread::spawn(move || {
                                let _ = tx.send(flex::discover(Duration::from_secs(3)));
                            });
                            self.discovery = Some(rx);
                        }
                        if self.discovery.is_some() {
                            ui.spinner();
                        }
                        if !self.radios.is_empty() {
                            egui::ComboBox::from_id_salt("radios").selected_text("Radios…").show_ui(ui, |ui| {
                                for r in &self.radios {
                                    if ui.selectable_label(self.flex_addr == r.ip, r.describe()).clicked() {
                                        self.flex_addr = r.ip.clone();
                                    }
                                }
                            });
                        }
                        ui.label("DAX IQ ch");
                        ui.add(egui::DragValue::new(&mut self.flex_channel).range(1..=4));
                        egui::ComboBox::from_id_salt("flexrate")
                            .selected_text(if self.flex_rate == 0 { "rate: radio".into() } else { format!("{} kHz", self.flex_rate / 1000) })
                            .show_ui(ui, |ui| {
                                for r in [0, 24_000, 48_000, 96_000, 192_000] {
                                    let t = if r == 0 { "radio setting".into() } else { format!("{} kHz", r / 1000) };
                                    ui.selectable_value(&mut self.flex_rate, r, t);
                                }
                            });
                    }
                    SrcKind::Wav => {
                        ui.label("File");
                        ui.add(egui::TextEdit::singleline(&mut self.wav_path).desired_width(240.0).hint_text("drop a stereo I/Q .wav here"));
                        ui.label("Center kHz");
                        ui.add(egui::DragValue::new(&mut self.wav_center_khz).speed(1.0).max_decimals(3));
                    }
                    SrcKind::Synth => {
                        egui::ComboBox::from_id_salt("synthrate").selected_text(format!("{} kHz", self.synth_rate / 1000)).show_ui(ui, |ui| {
                            for r in [24_000, 48_000, 96_000, 192_000] {
                                ui.selectable_value(&mut self.synth_rate, r, format!("{} kHz", r / 1000));
                            }
                        });
                        ui.label("Stations");
                        ui.add(egui::DragValue::new(&mut self.synth_stations).range(1..=300));
                        ui.label("Min SNR");
                        ui.add(egui::DragValue::new(&mut self.synth_snr).range(-10.0..=40.0).suffix(" dB"));
                    }
                }
            });
            let label = if running { "■ Stop" } else { "▶ Start" };
            if ui.button(label).clicked() {
                if running {
                    self.engine.stop();
                } else {
                    self.error = self.source_config().and_then(|cfg| self.engine.start(cfg)).err();
                    self.selected = None;
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Slider::new(&mut self.settings.threshold_db, 4.0..=30.0).text("detect dB"));
            ui.add(egui::Slider::new(&mut self.floor_db, -10.0..=20.0).text("floor"));
            ui.add(egui::Slider::new(&mut self.range_db, 10.0..=50.0).text("range"));
            ui.add(egui::DragValue::new(&mut self.settings.max_decoders).range(1..=2000).prefix("max decoders "));
            ui.checkbox(&mut self.settings.swap_iq, "Swap I/Q");
            if ui.button("Full span").clicked() {
                self.view_rate = 0;
            }
            if let Some(a) = &self.slice_a {
                ui.separator();
                ui.label(egui::RichText::new("A").strong().color(Color32::WHITE).background_color(Color32::from_rgb(255, 70, 70)));
                ui.label(egui::RichText::new(format!("{:.3} kHz  {}", a.hz / 1e3, a.mode)).monospace().size(14.0).strong());
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(Color32::from_rgb(255, 120, 100), e);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(30));
        self.dev_screenshot(ctx);

        if let Some(rx) = &self.discovery {
            if let Ok(res) = rx.try_recv() {
                self.discovery = None;
                match res {
                    Ok(r) if r.is_empty() => self.error = Some("No FlexRadio found on the LAN".into()),
                    Ok(r) => {
                        if self.flex_addr.is_empty() {
                            self.flex_addr = r[0].ip.clone();
                        }
                        self.radios = r;
                        self.error = None;
                    }
                    Err(e) => self.error = Some(format!("Discovery failed: {e}")),
                }
            }
        }
        let dropped: Vec<_> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if let Some(p) = dropped.into_iter().next() {
            self.kind = SrcKind::Wav;
            self.wav_path = p.display().to_string();
        }
        if self.settings != self.sent_settings {
            self.engine.set_settings(self.settings.clone());
            self.sent_settings = self.settings.clone();
        }

        // Snapshot shared state.
        let (rate, center, bins, bin_hz, rows_per_s, decoders, status, load, new_rows, slice_a) = {
            let mut s = self.engine.shared.lock();
            (
                s.rate,
                s.center_hz,
                s.wf_bins,
                s.wf_bin_hz,
                s.wf_rows_per_s,
                s.decoders.clone(),
                s.status.clone(),
                s.load,
                std::mem::take(&mut s.new_rows),
                s.slice_a.clone(),
            )
        };
        if rate != self.view_rate && rate > 0 {
            self.view_rate = rate;
            let half = rate as f64 * 0.48;
            self.view = (-half, half);
        }
        self.wf.push_rows(new_rows, bins);

        if let Some((id, off)) = self.tune_request.take() {
            self.selected = Some(id);
            if !self.engine.tune_cw(center + off) {
                self.error = Some("Tuning needs a FlexRadio source".into());
            }
        }

        self.slice_a = slice_a.clone();
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| self.toolbar(ui));

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let active = decoders.iter().filter(|d| d.idle_s < 30.0 && !d.text.is_empty()).count();
                ui.monospace(format!(
                    "{:.3} kHz | {} kHz IQ | decoders {} ({} decoding) | DSP load {:.0}% | {}",
                    center / 1e3,
                    rate / 1000,
                    decoders.len(),
                    active,
                    load * 100.0,
                    status
                ));
            });
        });

        egui::TopBottomPanel::bottom("decode").min_height(44.0).show(ctx, |ui| {
            let sel = self.selected.and_then(|id| decoders.iter().find(|d| d.id == id));
            match sel {
                Some(d) => {
                    ui.horizontal(|ui| {
                        ui.monospace(format!(
                            "{:.2} kHz  {:.0} WPM  {:.0} dB",
                            (center + d.offset_hz) / 1e3,
                            d.wpm,
                            d.snr_db
                        ));
                    });
                    ui.label(colored_text(tail_chars(&d.text, 160), FontId::monospace(15.0), false));
                }
                None => {
                    ui.weak("Click a signal or spot to follow its decoded text. Double-click a dot or spot to tune slice A in CW. Scroll to zoom, drag to pan, double-click empty waterfall for full span.");
                }
            }
        });

        egui::CentralPanel::default().frame(egui::Frame::none().fill(Color32::from_rgb(8, 20, 34))).show(ctx, |ui| {
            let full = ui.available_rect_before_wrap();
            if rate == 0 || bins == 0 {
                ui.painter().text(full.center(), Align2::CENTER_CENTER, "Choose a source and press Start", FontId::proportional(18.0), Color32::GRAY);
                return;
            }
            // Spot rows get a generous share of the width so they can show running text.
            let label_w = (full.width() * 0.36).clamp(240.0, 720.0);
            let wf_rect = Rect::from_min_max(full.min, pos2(full.max.x - SCALE_W - label_w, full.max.y));
            let scale_rect = Rect::from_min_max(pos2(wf_rect.max.x, full.min.y), pos2(wf_rect.max.x + SCALE_W, full.max.y));
            let label_rect = Rect::from_min_max(pos2(scale_rect.max.x, full.min.y), full.max);

            self.handle_view_input(ui, wf_rect.union(scale_rect), rate, &decoders);

            let (lo, hi) = self.view;
            let f_to_y = |f: f64| -> f32 { wf_rect.top() + ((hi - f) / (hi - lo)) as f32 * wf_rect.height() };

            let tex = self.wf.texture(ctx, wf_rect.height().max(1.0) as usize, self.view, bin_hz, self.floor_db, self.range_db);
            let painter = ui.painter_at(full);
            self.wf.draw(&painter, wf_rect, &tex);

            // Time ruler: one tick per 5 s of history.
            if rows_per_s > 0.0 {
                let px_per_s = wf_rect.width() / HISTORY_COLS as f32 * rows_per_s;
                let mut x = wf_rect.right();
                let mut s = 0;
                while x > wf_rect.left() {
                    painter.line_segment([pos2(x, wf_rect.bottom() - 6.0), pos2(x, wf_rect.bottom())], Stroke::new(1.0_f32, Color32::GRAY));
                    if s > 0 {
                        painter.text(pos2(x, wf_rect.bottom() - 8.0), Align2::CENTER_BOTTOM, format!("-{s}s"), FontId::monospace(10.0), Color32::GRAY);
                    }
                    x -= px_per_s * 5.0;
                    s += 5;
                }
            }

            // Frequency scale.
            painter.rect_filled(scale_rect, 0.0, Color32::from_rgb(20, 24, 30));
            let step = nice_step((hi - lo) / (scale_rect.height() as f64 / 50.0));
            let mut f = ((center + lo) / step).ceil() * step;
            while f <= center + hi {
                let y = f_to_y(f - center);
                painter.line_segment([pos2(scale_rect.left(), y), pos2(scale_rect.left() + 6.0, y)], Stroke::new(1.0_f32, Color32::LIGHT_GRAY));
                let txt = if step >= 1000.0 { format!("{:.0}", f / 1e3) } else if step >= 100.0 { format!("{:.1}", f / 1e3) } else { format!("{:.2}", f / 1e3) };
                painter.text(pos2(scale_rect.left() + 8.0, y), Align2::LEFT_CENTER, txt, FontId::monospace(11.0), Color32::LIGHT_GRAY);
                f += step;
            }

            // Radio slice A marker.
            if let Some(a) = &slice_a {
                let y = f_to_y(a.hz - center);
                if (wf_rect.top()..=wf_rect.bottom()).contains(&y) {
                    let red = Color32::from_rgb(255, 70, 70);
                    painter.line_segment([pos2(wf_rect.left(), y), pos2(scale_rect.left(), y)], Stroke::new(1.0_f32, red.gamma_multiply(0.7)));
                    let tag = Rect::from_min_size(pos2(scale_rect.left() + 1.0, y - 7.0), vec2(14.0, 14.0));
                    painter.rect_filled(tag, 2.0, red);
                    painter.text(tag.center(), Align2::CENTER_CENTER, "A", FontId::monospace(11.0), Color32::WHITE);
                }
            }

            // Highlight selected / hovered signals across the waterfall.
            let band_px = ((60.0 / (hi - lo)) as f32 * wf_rect.height()).max(3.0);
            for (id, alpha) in [(self.hovered, 30u8), (self.selected, 55u8)] {
                if let Some(d) = id.and_then(|id| decoders.iter().find(|d| d.id == id)) {
                    let y = f_to_y(d.offset_hz);
                    if (wf_rect.top()..=wf_rect.bottom()).contains(&y) {
                        let r = Rect::from_center_size(pos2(wf_rect.center().x, y), vec2(wf_rect.width(), band_px * 2.0));
                        painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(80, 255, 120, alpha));
                        painter.rect_stroke(r, 0.0, Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(80, 255, 120, alpha * 3)));
                    }
                }
            }

            // Spot labels.
            painter.rect_filled(label_rect, 0.0, Color32::from_rgb(14, 18, 24));
            let mut items: Vec<(&DecoderView, f32)> = decoders
                .iter()
                .filter_map(|d| {
                    let y = f_to_y(d.offset_hz);
                    if !(wf_rect.top()..=wf_rect.bottom()).contains(&y) {
                        return None;
                    }
                    // Mark every tracked signal at the waterfall's leading edge.
                    let c = if d.key_down { Color32::from_rgb(120, 255, 140) } else { Color32::from_rgb(230, 210, 60) };
                    painter.circle_filled(pos2(scale_rect.right() - 6.0, y), 3.0, c);
                    painter.line_segment([pos2(wf_rect.right() - 8.0, y), pos2(wf_rect.right(), y)], Stroke::new(2.0_f32, c));
                    if d.text.trim().is_empty() || d.idle_s > 120.0 {
                        return None;
                    }
                    Some((d, y))
                })
                .collect();
            let max_n = (label_rect.height() / LINE_H) as usize;
            if items.len() > max_n {
                items.sort_by(|a, b| priority(b.0).total_cmp(&priority(a.0)));
                items.truncate(max_n);
            }
            items.sort_by(|a, b| a.1.total_cmp(&b.1));
            let mut placed: Vec<f32> = items.iter().map(|i| i.1).collect();
            for i in 1..placed.len() {
                placed[i] = placed[i].max(placed[i - 1] + LINE_H);
            }
            let bottom = label_rect.bottom() - LINE_H / 2.0;
            for i in (0..placed.len()).rev() {
                let lim = if i + 1 < placed.len() { placed[i + 1] - LINE_H } else { bottom };
                placed[i] = placed[i].min(lim).max(label_rect.top() + LINE_H / 2.0);
            }

            let label_resp = ui.interact(label_rect, ui.id().with("labels"), Sense::click());
            let pointer = label_resp.hover_pos();
            self.hovered = None;
            let font = FontId::monospace(12.0);
            let char_w = ui.fonts(|f| f.glyph_width(&font, 'M'));
            for ((d, y), py) in items.iter().zip(&placed) {
                let x0 = label_rect.left() + 14.0;
                let row = Rect::from_min_max(pos2(x0 - 2.0, py - LINE_H / 2.0), pos2(label_rect.right() - 2.0, py + LINE_H / 2.0));
                let is_sel = self.selected == Some(d.id);
                let is_hover = pointer.is_some_and(|p| row.contains(p));
                if is_hover {
                    self.hovered = Some(d.id);
                    if label_resp.clicked() {
                        self.selected = Some(d.id);
                    }
                    if label_resp.double_clicked() {
                        self.tune_request = Some((d.id, d.offset_hz));
                    }
                }
                painter.line_segment([pos2(scale_rect.right() - 4.0, *y), pos2(x0 - 4.0, *py)], Stroke::new(1.0_f32, Color32::from_gray(140)));
                if is_sel {
                    painter.rect_filled(row, 2.0, Color32::from_rgb(40, 150, 60));
                } else if is_hover {
                    painter.rect_filled(row, 2.0, Color32::from_gray(50));
                }
                // CQ marker column, then as much of the running text as fits.
                let active = d.idle_s < 30.0;
                if d.cq && active {
                    painter.add(egui::Shape::convex_polygon(
                        vec![pos2(x0, py - 5.0), pos2(x0 + 8.0, *py), pos2(x0, py + 5.0)],
                        Color32::from_rgb(60, 200, 60),
                        Stroke::NONE,
                    ));
                }
                let text_x = x0 + 12.0;
                let max_chars = ((row.right() - text_x - 4.0) / char_w).max(1.0) as usize;
                let text = d.text.trim();
                let shown = if text.chars().count() > max_chars {
                    format!("…{}", tail_chars(text, max_chars - 1))
                } else {
                    text.to_string()
                };
                let galley = painter.layout_job(colored_text(&shown, font.clone(), !active));
                painter.galley(pos2(text_x, py - galley.size().y / 2.0), galley, Color32::WHITE);
            }
        });
    }
}

impl App {
    /// Dev aid: with IQSKIMMER_SCREENSHOT=<file.bmp>, save a screenshot after 15 s and exit.
    fn dev_screenshot(&mut self, ctx: &egui::Context) {
        let Ok(path) = std::env::var("IQSKIMMER_SCREENSHOT") else { return };
        if !self.screenshot_requested && self.started.elapsed() > Duration::from_secs(15) {
            self.screenshot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let _ = write_bmp(&path, &img);
            std::process::exit(0);
        }
    }

    fn handle_view_input(&mut self, ui: &mut egui::Ui, rect: Rect, rate: u32, decoders: &[DecoderView]) {
        let resp = ui.interact(rect, ui.id().with("wf"), Sense::click_and_drag());
        let (mut lo, mut hi) = self.view;
        let limit = rate as f64 / 2.0;
        let y_to_f = |y: f32, lo: f64, hi: f64| hi - ((y - rect.top()) / rect.height()) as f64 * (hi - lo);
        if resp.double_clicked() {
            // On a signal's dot: tune the radio there. Anywhere else: back to full span.
            let p = resp.interact_pointer_pos();
            let dot_x = rect.right() - 6.0;
            let hit = p.filter(|p| (p.x - dot_x).abs() < 10.0).and_then(|p| {
                decoders
                    .iter()
                    .map(|d| (d, (rect.top() + ((hi - d.offset_hz) / (hi - lo)) as f32 * rect.height() - p.y).abs()))
                    .filter(|(_, dy)| *dy < 8.0)
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(d, _)| d)
            });
            match hit {
                Some(d) => self.tune_request = Some((d.id, d.offset_hz)),
                None => self.view_rate = 0,
            }
            return;
        }
        if let Some(p) = resp.hover_pos() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let f = y_to_f(p.y, lo, hi);
                let k = (-scroll as f64 * 0.003).exp();
                let span = ((hi - lo) * k).clamp(300.0, 2.0 * limit);
                let frac = (hi - f) / (hi - lo);
                hi = f + frac * span;
                lo = hi - span;
            }
        }
        if resp.dragged() {
            let df = resp.drag_delta().y as f64 / rect.height() as f64 * (hi - lo);
            lo += df;
            hi += df;
        }
        let span = hi - lo;
        if lo < -limit {
            lo = -limit;
            hi = lo + span;
        }
        if hi > limit {
            hi = limit;
            lo = (hi - span).max(-limit);
        }
        self.view = (lo, hi);
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let f = y_to_f(p.y, lo, hi);
                let tol = (hi - lo) / rect.height() as f64 * 10.0;
                self.selected = decoders
                    .iter()
                    .filter(|d| (d.offset_hz - f).abs() < tol.max(60.0))
                    .min_by(|a, b| (a.offset_hz - f).abs().total_cmp(&(b.offset_hz - f).abs()))
                    .map(|d| d.id);
            }
        }
    }
}

fn priority(d: &DecoderView) -> f32 {
    let mut p = d.snr_db - d.idle_s;
    if d.cq {
        p += 30.0;
    }
    if !d.label.is_empty() {
        p += 20.0;
    }
    p
}

fn nice_step(raw: f64) -> f64 {
    for s in [10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1e3, 2e3, 5e3, 10e3, 20e3, 50e3] {
        if s >= raw {
            return s;
        }
    }
    100e3
}

fn tail_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    match s.char_indices().nth(count.saturating_sub(n)) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

const CALL_COLOR: Color32 = Color32::from_rgb(255, 95, 85);
const KEYWORD_COLOR: Color32 = Color32::from_rgb(110, 150, 255);
const TEXT_COLOR: Color32 = Color32::from_rgb(215, 220, 225);

/// Decoded text with callsigns in red and common CW words in blue, like CW Skimmer.
/// Words are runs of letters, digits and '/', so calls glued to punctuation or prosigns
/// (`VE2GDI+`, `<BK>N3MK`) still highlight.
fn colored_text(text: &str, font: FontId, dim: bool) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let fmt = |c: Color32| egui::TextFormat::simple(font.clone(), if dim { c.gamma_multiply(0.5) } else { c });
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '/';
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let w = is_word(c);
        let end = rest.find(|ch: char| is_word(ch) != w).unwrap_or(rest.len());
        let (tok, tail) = rest.split_at(end);
        let color = if !w {
            TEXT_COLOR
        } else if crate::dsp::callsign::is_callsign(tok) {
            CALL_COLOR
        } else if matches!(tok, "CQ" | "TEST" | "TU" | "DE" | "QRZ" | "QRL" | "K" | "5NN" | "73") {
            KEYWORD_COLOR
        } else {
            TEXT_COLOR
        };
        job.append(tok, 0.0, fmt(color));
        rest = tail;
    }
    job
}

/// Waterfall history and its ring-buffer texture (one texture column per row of spectrum).
struct Waterfall {
    rows: VecDeque<Vec<u8>>,
    total: u64,
    bins: usize,
    tex: Option<TextureHandle>,
    key: Option<(usize, (f64, f64), f64, f32, f32)>,
    pix_bins: Vec<(usize, usize)>,
    lut: [Color32; 256],
    unrendered: usize,
}

impl Waterfall {
    fn new() -> Self {
        Self {
            rows: VecDeque::new(),
            total: 0,
            bins: 0,
            tex: None,
            key: None,
            pix_bins: Vec::new(),
            lut: [Color32::BLACK; 256],
            unrendered: 0,
        }
    }

    fn push_rows(&mut self, rows: Vec<Vec<u8>>, bins: usize) {
        if bins != self.bins {
            self.rows.clear();
            self.bins = bins;
            self.key = None;
        }
        for r in rows {
            if r.len() != bins {
                continue;
            }
            if self.rows.len() == HISTORY_COLS {
                self.rows.pop_front();
            }
            self.rows.push_back(r);
            self.total += 1;
            self.unrendered += 1;
        }
    }

    fn render_column(&self, row: &[u8], out: &mut [Color32]) {
        for (px, &(a, b)) in out.iter_mut().zip(&self.pix_bins) {
            let v = row[a..=b].iter().copied().max().unwrap_or(0);
            *px = self.lut[v as usize];
        }
    }

    fn texture(&mut self, ctx: &egui::Context, h: usize, view: (f64, f64), bin_hz: f64, floor: f32, range: f32) -> TextureHandle {
        let key = (h, view, bin_hz, floor, range);
        if self.key != Some(key) || self.tex.is_none() {
            self.key = Some(key);
            let n = self.bins;
            let (lo, hi) = view;
            let to_bin = |f: f64| ((f / bin_hz + n as f64 / 2.0).floor().max(0.0) as usize).min(n.saturating_sub(1));
            self.pix_bins = (0..h)
                .map(|y| {
                    let f_top = hi - y as f64 / h as f64 * (hi - lo);
                    let f_bot = hi - (y + 1) as f64 / h as f64 * (hi - lo);
                    let (a, b) = (to_bin(f_bot), to_bin(f_top));
                    (a.min(b), b.max(a))
                })
                .collect();
            for (q, c) in self.lut.iter_mut().enumerate() {
                *c = palette(((wf_db(q as u8) - floor) / range).clamp(0.0, 1.0));
            }
            let mut img = ColorImage::new([HISTORY_COLS, h], Color32::from_rgb(0, 12, 30));
            let mut col = vec![Color32::BLACK; h];
            let first = self.total - self.rows.len() as u64;
            for (i, r) in self.rows.iter().enumerate() {
                let x = ((first + i as u64) % HISTORY_COLS as u64) as usize;
                self.render_column(r, &mut col);
                for (y, c) in col.iter().enumerate() {
                    img.pixels[y * HISTORY_COLS + x] = *c;
                }
            }
            match &mut self.tex {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => self.tex = Some(ctx.load_texture("waterfall", img, TextureOptions::LINEAR)),
            }
            self.unrendered = 0;
        } else if self.unrendered > 0 {
            let n_new = self.unrendered.min(self.rows.len());
            let mut col = vec![Color32::BLACK; h];
            for i in self.rows.len() - n_new..self.rows.len() {
                let seq = self.total - (self.rows.len() - i) as u64;
                self.render_column(&self.rows[i], &mut col);
                let img = ColorImage { size: [1, h], pixels: col.clone() };
                self.tex.as_mut().unwrap().set_partial([(seq % HISTORY_COLS as u64) as usize, 0], img, TextureOptions::LINEAR);
            }
            self.unrendered = 0;
        }
        self.tex.clone().unwrap()
    }

    fn draw(&self, painter: &egui::Painter, rect: Rect, tex: &TextureHandle) {
        // Newest column at the right edge.
        let head = (self.total % HISTORY_COLS as u64) as f32 / HISTORY_COLS as f32;
        let split = rect.left() + (1.0 - head) * rect.width();
        let uv = |a: f32, b: f32| Rect::from_min_max(pos2(a, 0.0), pos2(b, 1.0));
        painter.image(tex.id(), Rect::from_min_max(rect.min, pos2(split, rect.bottom())), uv(head, 1.0), Color32::WHITE);
        painter.image(tex.id(), Rect::from_min_max(pos2(split, rect.top()), rect.max), uv(0.0, head), Color32::WHITE);
    }
}

fn palette(t: f32) -> Color32 {
    const STOPS: [(f32, [f32; 3]); 5] = [
        (0.0, [0.0, 14.0, 34.0]),
        (0.3, [0.0, 60.0, 100.0]),
        (0.6, [30.0, 180.0, 210.0]),
        (0.85, [170.0, 240.0, 255.0]),
        (1.0, [255.0, 255.0, 255.0]),
    ];
    for w in STOPS.windows(2) {
        let ((t0, c0), (t1, c1)) = (w[0], w[1]);
        if t <= t1 {
            let f = ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
            let c = |i: usize| (c0[i] + (c1[i] - c0[i]) * f) as u8;
            return Color32::from_rgb(c(0), c(1), c(2));
        }
    }
    Color32::WHITE
}


fn write_bmp(path: &str, img: &ColorImage) -> std::io::Result<()> {
    let [w, h] = img.size;
    let size = 54 + 4 * w * h;
    let mut b = Vec::with_capacity(size);
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&(size as u32).to_le_bytes());
    b.extend_from_slice(&[0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0]);
    b.extend_from_slice(&(w as i32).to_le_bytes());
    b.extend_from_slice(&(-(h as i32)).to_le_bytes());
    b.extend_from_slice(&[1, 0, 32, 0]);
    b.extend_from_slice(&[0u8; 24]);
    for p in &img.pixels {
        b.extend_from_slice(&[p.b(), p.g(), p.r(), 255]);
    }
    std::fs::write(path, b)
}
