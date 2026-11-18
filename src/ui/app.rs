use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use eframe::egui_wgpu;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::pickers;
use super::preview::{letterbox, PreviewCallback, PreviewRenderer};
use super::settings::{SettingsOutcome, SettingsUi};
use crate::config::Config;
use crate::hotkeys::Hotkeys;
use crate::pipeline::{Command, Pipeline, Shared};
use crate::tray::{self, Tray};

const REC_RED: Color32 = Color32::from_rgb(230, 40, 50);

pub struct App {
    cfg: Config,
    shared: Arc<Shared>,
    pipeline: Pipeline,
    hotkeys: Option<Hotkeys>,
    hotkey_errors: Vec<String>,
    _tray: Option<Tray>,
    settings: Option<SettingsUi>,
    fullscreen: bool,
    gpu_ok: bool,
    cycle_modes: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, shared: Arc<Shared>, pipeline: Pipeline) -> Self {
        let gpu_ok = match cc.wgpu_render_state.as_ref() {
            Some(rs) => {
                PreviewRenderer::install(rs);
                true
            }
            None => false,
        };
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let ctx = cc.egui_ctx.clone();
        shared.set_repaint(move || ctx.request_repaint());

        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        };

        let mut hotkey_errors = Vec::new();
        let hotkeys = match Hotkeys::new(pipeline.sender()) {
            Ok(mut h) => {
                hotkey_errors = h.apply(&cfg);
                Some(h)
            }
            Err(e) => {
                hotkey_errors.push(format!("global hotkeys unavailable: {e}"));
                None
            }
        };
        for e in &hotkey_errors {
            shared.notify(format!("Hotkey problem: {e}"), true);
        }
        let tray = match tray::create(hwnd, pipeline.sender()) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("tray icon unavailable: {e:#}");
                None
            }
        };

        Self {
            cfg,
            shared,
            pipeline,
            hotkeys,
            hotkey_errors,
            _tray: tray,
            settings: None,
            fullscreen: false,
            gpu_ok,
            cycle_modes: std::env::var_os("PASSTHRIFY_CYCLE_MODES").is_some(),
        }
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        self.fullscreen = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let shared = self.shared.clone();
        let st = &shared.status;
        ui.horizontal(|ui| {
            let device = st.device.lock().clone();
            let mode = *st.mode.lock();
            let modes = st.modes.lock().clone();
            let recording = st.recording_since.lock().is_some();
            ui.label(RichText::new(if device.is_empty() { "No device".into() } else { device }).strong());

            // Quick capture/encode switches; applied immediately (restarts the pipeline).
            let mut pref = self.cfg.video_mode;
            let mut codec = self.cfg.codec;
            let mut chroma = self.cfg.chroma;
            let actual_chroma = *st.chroma.lock();
            let changed = ui
                .add_enabled_ui(!recording, |ui| {
                    // Chroma first: it decides which resolutions / frame rates are offered.
                    let mut c = pickers::chroma_picker(
                        ui, "toolbar", &mut chroma, &mut pref, &mut codec, &modes, actual_chroma, 64.0,
                    );
                    c |= pickers::resolution_picker(ui, "toolbar", &mut pref, chroma, &modes, mode, 120.0);
                    c |= pickers::rate_picker(ui, "toolbar", &mut pref, chroma, &modes, mode, 92.0);
                    c |= pickers::codec_picker(ui, "toolbar", &mut codec, chroma, true, 62.0);
                    c
                })
                .inner;
            if recording {
                ui.label(RichText::new("🔒").weak())
                    .on_hover_text("Stop recording to change resolution, frame rate, chroma or codec");
            }
            if changed {
                let mut c = self.cfg.clone();
                c.video_mode = pref;
                c.codec = codec;
                c.chroma = chroma;
                self.apply_settings(c);
                if let Some(s) = self.settings.as_mut() {
                    s.sync_capture(&self.cfg);
                }
            }
            ui.separator();

            if ui
                .button("⏪ Save replay")
                .on_hover_text(format!("Save the last {} s ({})", self.cfg.replay_seconds, self.cfg.hotkey_save_replay))
                .clicked()
            {
                self.pipeline.send(Command::SaveReplay);
            }
            let rec_text = if recording {
                RichText::new("⏹ Stop").color(REC_RED)
            } else {
                RichText::new("⏺ Record")
            };
            if ui.button(rec_text).on_hover_text(self.cfg.hotkey_toggle_record.clone()).clicked() {
                self.pipeline.send(Command::ToggleRecord);
            }
            if ui.button("📷 Screenshot").on_hover_text(self.cfg.hotkey_screenshot.clone()).clicked() {
                self.pipeline.send(Command::Screenshot);
            }
            if ui.button("Open folder").on_hover_text(self.cfg.output_dir.display().to_string()).clicked() {
                self.pipeline.send(Command::OpenOutputFolder);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⚙ Settings").clicked() && self.settings.is_none() {
                    self.settings = Some(SettingsUi::new(&self.cfg));
                }
                if ui.button("⛶").on_hover_text("Fullscreen (F11 / double-click)").clicked() {
                    let on = !self.fullscreen;
                    self.set_fullscreen(ui.ctx(), on);
                }
                ui.separator();
                let resp = ui.add(egui::Slider::new(&mut self.cfg.volume, 0.0..=2.0).show_value(false));
                if resp.changed() {
                    self.shared.ring.set_volume(self.cfg.volume);
                }
                let icon = if self.cfg.muted || self.cfg.volume == 0.0 { "🔇" } else { "🔊" };
                if ui.selectable_label(self.cfg.muted, icon).on_hover_text("Mute passthrough").clicked() {
                    self.cfg.muted = !self.cfg.muted;
                    self.shared.ring.set_muted(self.cfg.muted);
                }
            });
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let st = &self.shared.status;
        ui.horizontal(|ui| {
            if let Some(since) = *st.recording_since.lock() {
                let s = since.elapsed().as_secs();
                ui.label(
                    RichText::new(format!("● REC {:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60))
                        .color(REC_RED)
                        .strong(),
                );
                ui.separator();
            }
            let fps = st.fps_x100.load(Ordering::Relaxed) as f32 / 100.0;
            ui.label(format!("{fps:.1} fps"));
            let dropped = st.dropped_frames.load(Ordering::Relaxed);
            if dropped > 0 {
                ui.label(RichText::new(format!("{dropped} dropped")).color(Color32::YELLOW));
            }
            ui.separator();
            if self.cfg.replay_enabled {
                let secs = st.replay_ms.load(Ordering::Relaxed) as f32 / 1000.0;
                let mb = st.replay_bytes.load(Ordering::Relaxed) as f32 / 1_048_576.0;
                ui.label(format!("Replay {secs:.0}/{} s · {mb:.0} MB", self.cfg.replay_seconds));
            } else {
                ui.label(RichText::new("Replay off").weak());
            }
            ui.separator();
            ui.label(RichText::new(st.encoder.lock().clone()).weak());
            ui.separator();
            ui.label(RichText::new(format!("Audio: {}", st.audio_in.lock())).weak());
        });
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let resp = ui.allocate_rect(rect, egui::Sense::click());
        if resp.double_clicked() {
            let on = !self.fullscreen;
            self.set_fullscreen(ui.ctx(), on);
        }
        match self.shared.latest_frame.load_full() {
            Some(frame) if self.gpu_ok => {
                let r = letterbox(rect, frame.width, frame.height);
                ui.painter().add(egui_wgpu::Callback::new_paint_callback(r, PreviewCallback { frame }));
            }
            _ => {
                let msg = match (&*self.shared.status.error.lock(), self.gpu_ok) {
                    (_, false) => "GPU renderer unavailable".to_string(),
                    (Some(e), _) => format!("No signal\n\n{e}"),
                    (None, _) => "Waiting for video…".to_string(),
                };
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    msg,
                    egui::FontId::proportional(18.0),
                    Color32::from_gray(150),
                );
            }
        }
    }

    fn toasts(&self, ctx: &egui::Context) {
        let toasts = self.shared.recent_toasts(Duration::from_secs(5));
        if toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -40.0])
            .interactable(false)
            .show(ctx, |ui| {
                for (text, error, age) in toasts {
                    let alpha = ((5.0 - age).clamp(0.0, 1.0) * 230.0) as u8;
                    let bg = if error {
                        Color32::from_rgba_unmultiplied(120, 30, 30, alpha)
                    } else {
                        Color32::from_rgba_unmultiplied(30, 30, 36, alpha)
                    };
                    egui::Frame::NONE.fill(bg).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
                        ui.label(RichText::new(text).color(Color32::from_white_alpha(alpha)));
                    });
                    ui.add_space(6.0);
                }
            });
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn apply_settings(&mut self, new_cfg: Config) {
        let restart_needed = {
            let mut a = self.cfg.clone();
            let mut b = new_cfg.clone();
            // These don't need a pipeline restart.
            for c in [&mut a, &mut b] {
                c.volume = 0.0;
                c.muted = false;
                c.hotkey_save_replay.clear();
                c.hotkey_toggle_record.clear();
                c.hotkey_screenshot.clear();
                c.present_mode = crate::config::PresentMode::Mailbox;
                c.windows_notifications = false;
            }
            a != b
        };
        let mut cfg = new_cfg;
        cfg.volume = self.cfg.volume;
        cfg.muted = self.cfg.muted;
        self.cfg = cfg;
        self.cfg.save();
        self.shared.windows_notifications.store(self.cfg.windows_notifications, Ordering::Relaxed);
        if let Some(h) = self.hotkeys.as_mut() {
            self.hotkey_errors = h.apply(&self.cfg);
        }
        if restart_needed {
            self.pipeline.send(Command::Apply(Box::new(self.cfg.clone())));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (f11, esc) = ctx.input(|i| (i.key_pressed(egui::Key::F11), i.key_pressed(egui::Key::Escape)));
        if f11 {
            let on = !self.fullscreen;
            self.set_fullscreen(ctx, on);
        } else if esc && self.fullscreen {
            self.set_fullscreen(ctx, false);
        }

        if !self.fullscreen {
            egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
                ui.add_space(2.0);
                self.toolbar(ui);
                ui.add_space(2.0);
            });
            egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ctx, |ui| self.preview(ui));

        self.toasts(ctx);

        if let Some(s) = self.settings.as_mut() {
            let modes = self.shared.status.modes.lock().clone();
            let current = *self.shared.status.mode.lock();
            let actual_chroma = *self.shared.status.chroma.lock();
            match s.show(ctx, &modes, current, actual_chroma, &self.hotkey_errors) {
                SettingsOutcome::Open => {}
                SettingsOutcome::Close => self.settings = None,
                SettingsOutcome::Apply(cfg) => {
                    self.settings = None;
                    self.apply_settings(*cfg);
                }
            }
        }

        // Stress test: PASSTHRIFY_CYCLE_MODES=1 applies a new capture mode every 5 s through the
        // same path as the Settings window.
        if self.cycle_modes {
            use std::sync::atomic::AtomicU64;
            static LAST: AtomicU64 = AtomicU64::new(0);
            let t = ctx.input(|i| i.time) as u64;
            if t >= 5 && t % 5 == 0 && LAST.swap(t, Ordering::Relaxed) != t {
                const MODES: [(u32, u32, f64); 4] =
                    [(1920, 1080, 60.0), (3840, 2160, 60.0), (2560, 1440, 120.0), (1280, 720, 60.0)];
                let (w, h, fps) = MODES[(t / 5) as usize % MODES.len()];
                let mut c = self.cfg.clone();
                c.video_mode = Some(crate::config::ModePref { width: w, height: h, fps });
                tracing::info!("cycle-modes: applying {w}x{h} @ {fps}");
                self.apply_settings(c);
            }
        }

        // Keep status text fresh even when no frames arrive.
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn on_exit(&mut self) {
        self.cfg.save();
    }
}
