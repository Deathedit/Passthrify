use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Color32, FontId, Margin, RichText, Stroke};
use eframe::egui_wgpu;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::pickers;
use super::preview::{letterbox, PreviewCallback, PreviewRenderer};
use super::settings::{Section, SettingsOutcome, SettingsUi};
use super::theme::{self, icons, Kind};
use crate::config::Config;
use crate::hotkeys::Hotkeys;
use crate::pipeline::{Command, Pipeline, Shared};
use crate::tray::{self, Tray};
use crate::updater::{self, UpdateState, Updater};

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
    updater: Arc<Updater>,
    /// Version we already announced, so the toast shows once per release.
    announced_update: Option<semver::Version>,
    toolbar_fit: ToolbarFit,
}

/// Toolbar densities: 0 = full labels and hotkey hints, 1 = no hints and icon-only
/// Screenshot / Open folder, 2 = icon-only actions and a shorter volume slider, 3 = no
/// capture pickers, 4 = no device chip either.
const TOOLBAR_LEVELS: usize = 5;
/// Minimum space between the left (pickers + actions) and right (volume, settings) groups.
const TOOLBAR_GAP: f32 = 12.0;

/// Measured toolbar group widths per density, so we pick the roomiest one that fits.
/// 0.0 means "not measured yet" and is tried optimistically.
#[derive(Default)]
struct ToolbarFit {
    left: [f32; TOOLBAR_LEVELS],
    right: [f32; TOOLBAR_LEVELS],
}

impl ToolbarFit {
    fn pick(&self, width: f32) -> usize {
        (0..TOOLBAR_LEVELS)
            .find(|&l| self.left[l] + self.right[l] + TOOLBAR_GAP <= width)
            .unwrap_or(TOOLBAR_LEVELS - 1)
    }

    /// Records what `level` actually measured. If its contents changed size (a new picker
    /// label, the Update button appearing), the other levels' numbers are stale: forget them.
    fn record(&mut self, level: usize, left: f32, right: f32) {
        let changed = |old: f32, new: f32| old > 0.0 && (old - new).abs() > 0.5;
        if changed(self.left[level], left) || changed(self.right[level], right) {
            *self = Self::default();
        }
        self.left[level] = left;
        self.right[level] = right;
    }
}

/// "Alt+F10" -> "Alt F10", "Ctrl+Shift+KeyR" -> "Ctrl Shift R".
pub fn pretty_hotkey(s: &str) -> String {
    hotkey_parts(s).join(" ")
}

pub fn hotkey_parts(s: &str) -> Vec<String> {
    s.split('+')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| {
            let p = p.strip_prefix("Key").filter(|r| r.len() == 1).unwrap_or(p);
            let p = p.strip_prefix("Digit").filter(|r| r.len() == 1).unwrap_or(p);
            match p.to_ascii_lowercase().as_str() {
                "control" | "ctrl" => "Ctrl".into(),
                "alt" | "option" => "Alt".into(),
                "shift" => "Shift".into(),
                "super" | "meta" | "win" | "cmd" => "Win".into(),
                _ => p.to_string(),
            }
        })
        .collect()
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
        theme::install(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        shared.set_repaint(move || ctx.request_repaint());

        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        };
        tray::style_title_bar(hwnd);

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

        let ctx = cc.egui_ctx.clone();
        let updater = Updater::new(move || ctx.request_repaint());
        if let Some((ok, msg)) = updater::take_update_result() {
            shared.notify(msg, !ok);
        }
        if cfg.check_updates {
            updater.start_check(updater::repo(cfg.update_repo.as_deref()), cfg.update_channel, Duration::from_secs(3));
        }

        let mut app = Self {
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
            updater,
            announced_update: None,
            toolbar_fit: ToolbarFit::default(),
        };
        // PASSTHRIFY_SETTINGS=capture|audio|replay|hotkeys|display opens Settings on launch
        // (handy for docs and screenshots).
        if let Ok(s) = std::env::var("PASSTHRIFY_SETTINGS") {
            let section = match s.to_ascii_lowercase().as_str() {
                "audio" => Section::Audio,
                "replay" => Section::Replay,
                "hotkeys" => Section::Hotkeys,
                "display" => Section::Display,
                "updates" => Section::Updates,
                _ => Section::Capture,
            };
            app.open_settings(section);
        }
        app
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        self.fullscreen = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    fn open_settings(&mut self, section: Section) {
        match self.settings.as_mut() {
            Some(s) => s.section = section,
            None => {
                let mut s = SettingsUi::new(&self.cfg);
                s.section = section;
                self.settings = Some(s);
            }
        }
    }

    /// Lays out the toolbar as two groups that never overlap: the right group is drawn first,
    /// the left group gets what is left, and both get more compact until they fit.
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        use egui::{Align, Layout, Rect, UiBuilder};

        ui.spacing_mut().interact_size.y = 32.0;
        ui.spacing_mut().combo_height = 400.0;
        let row = Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), 32.0));
        let level = self.toolbar_fit.pick(row.width());

        let mut right = ui.new_child(UiBuilder::new().max_rect(row).layout(Layout::right_to_left(Align::Center)));
        self.toolbar_right(&mut right, level);
        let right_w = right.min_rect().width();

        let mut left_rect = row;
        left_rect.max.x = (row.max.x - right_w - TOOLBAR_GAP).max(row.min.x);
        let mut left = ui.new_child(UiBuilder::new().max_rect(left_rect).layout(Layout::left_to_right(Align::Center)));
        // Last resort when even the most compact level doesn't fit: cut off rather than overlap.
        let mut clip = ui.clip_rect();
        clip.max.x = clip.max.x.min(left_rect.max.x);
        left.set_clip_rect(clip);
        self.toolbar_left(&mut left, level);
        let left_w = left.min_rect().width();

        self.toolbar_fit.record(level, left_w, right_w);
        if left_w + right_w + TOOLBAR_GAP > row.width() && level + 1 < TOOLBAR_LEVELS {
            // Redo this frame at a denser level instead of showing the overlap.
            ui.ctx().request_discard("toolbar overflow");
        }
        ui.advance_cursor_after_rect(row);
    }

    /// Device chip, capture pickers and the recording actions.
    fn toolbar_left(&mut self, ui: &mut egui::Ui, level: usize) {
        let shared = self.shared.clone();
        let st = &shared.status;
        let device = st.device.lock().clone();
        let mode = *st.mode.lock();
        let modes = st.modes.lock().clone();
        let recording_since = *st.recording_since.lock();
        let recording = recording_since.is_some();
        let capturing = st.capturing.load(Ordering::Relaxed);
        let has_error = st.error.lock().is_some();
        // Device chip: status dot + name; opens Settings > Capture. Dropped last when narrow.
        if level < 4 {
            let status = if capturing {
                theme::SUCCESS
            } else if has_error {
                theme::REC
            } else {
                theme::TEXT3
            };
            let name = if device.is_empty() { "No device".to_string() } else { device };
            let label = RichText::new(name).font(FontId::new(13.0, theme::medium())).color(theme::TEXT);
            let chip = ui.scope(|ui| {
                let w = &mut ui.visuals_mut().widgets;
                w.inactive.weak_bg_fill = theme::BASE;
                w.hovered.weak_bg_fill = theme::RAISED;
                ui.spacing_mut().button_padding.x = 10.0;
                let dot = theme::icon_image(icons::DOT, 12.0, status);
                ui.add(egui::Button::image_and_text(dot, label).corner_radius(6).min_size(egui::vec2(0.0, 32.0)))
            });
            let tip = if capturing { "Capturing — click for capture settings" } else { "Not capturing — click for capture settings" };
            if chip.inner.on_hover_text(tip).clicked() {
                self.open_settings(Section::Capture);
            }
        }

        // Chroma -> resolution -> frame rate -> codec. Applied immediately. When narrow these
        // give way to the actions (they are also in Settings > Capture).
        if level < 3 {
            let mut pref = self.cfg.video_mode;
            let mut codec = self.cfg.codec;
            let mut chroma = self.cfg.chroma;
            let actual_chroma = *st.chroma.lock();
            let changed = ui
                .add_enabled_ui(!recording, |ui| {
                    let mut c = pickers::chroma_picker(
                        ui, "toolbar", &mut chroma, &mut pref, &mut codec, &modes, actual_chroma, 64.0,
                    );
                    c |= pickers::resolution_picker(ui, "toolbar", &mut pref, chroma, &modes, mode, 116.0);
                    c |= pickers::rate_picker(ui, "toolbar", &mut pref, chroma, &modes, mode, 86.0);
                    c |= pickers::codec_picker(ui, "toolbar", &mut codec, chroma, true, 60.0);
                    c
                })
                .inner;
            if recording {
                ui.add(theme::icon_image(icons::LOCK, 14.0, theme::TEXT3))
                    .on_hover_text("Stop recording to change chroma, resolution, frame rate or codec");
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
            ui.add_space(if level >= 2 { 4.0 } else { 8.0 });
        }

        // Actions.
        let save_hint = pretty_hotkey(&self.cfg.hotkey_save_replay);
        let save = theme::button(
            ui,
            Kind::Primary,
            Some(icons::REPLAY),
            if level >= 2 { "" } else { "Save replay" },
            (level == 0 && !save_hint.is_empty()).then_some(save_hint.as_str()),
        );
        let mut save_tip = format!("Save replay: the last {} s", self.cfg.replay_seconds);
        if !save_hint.is_empty() {
            save_tip += &format!(" ({save_hint})");
        }
        if save.on_hover_text(save_tip).clicked() {
            self.pipeline.send(Command::SaveReplay);
        }

        let rec_tip = format!("Start / stop recording ({})", pretty_hotkey(&self.cfg.hotkey_toggle_record));
        let rec = match recording_since {
            Some(since) => {
                let clock = theme::fmt_clock(since.elapsed().as_secs());
                theme::button(ui, Kind::Danger, Some(icons::STOP), if level >= 2 { "" } else { "Stop" }, Some(&clock))
            }
            None => {
                let label = if level >= 2 { "" } else { "Record" };
                theme::button_ex(ui, Kind::Secondary, Some(icons::RECORD), Some(theme::REC), label, None)
            }
        };
        if rec.on_hover_text(rec_tip).clicked() {
            self.pipeline.send(Command::ToggleRecord);
        }

        let shot_tip = format!("Screenshot ({})", pretty_hotkey(&self.cfg.hotkey_screenshot));
        let shot = theme::button(ui, Kind::Secondary, Some(icons::CAMERA), if level >= 1 { "" } else { "Screenshot" }, None);
        if shot.on_hover_text(shot_tip).clicked() {
            self.pipeline.send(Command::Screenshot);
        }
        let folder = theme::button(ui, Kind::Ghost, Some(icons::FOLDER), if level >= 1 { "" } else { "Open folder" }, None);
        if folder.on_hover_text(self.cfg.output_dir.display().to_string()).clicked() {
            self.pipeline.send(Command::OpenOutputFolder);
        }
    }

    /// Right side, laid out right-to-left: settings, update, fullscreen, volume, mute.
    fn toolbar_right(&mut self, ui: &mut egui::Ui, level: usize) {
        ui.spacing_mut().item_spacing.x = 4.0;
        if theme::icon_button(ui, icons::SETTINGS, "Settings", self.settings.is_some()).clicked() {
            if self.settings.is_some() {
                self.settings = None;
            } else {
                self.open_settings(Section::Capture);
            }
        }
        if let UpdateState::Available(rel) = self.updater.state() {
            let label = if level >= 2 { String::new() } else { format!("Update {}", rel.tag) };
            let resp = theme::button_ex(ui, Kind::Secondary, Some(icons::DOWNLOAD), Some(theme::SIGNAL), &label, None);
            let tip = format!("{} is available — open Settings › Updates", rel.tag);
            if resp.on_hover_text(tip).clicked() {
                self.open_settings(Section::Updates);
            }
        }
        if theme::icon_button(ui, icons::FULLSCREEN, "Fullscreen (F11 / double-click)", false).clicked() {
            let on = !self.fullscreen;
            self.set_fullscreen(ui.ctx(), on);
        }
        ui.add_space(4.0);
        // Compact slider: knob size follows interact height, so shrink it here.
        let resp = ui
            .scope(|ui| {
                ui.spacing_mut().slider_width = if level >= 2 { 64.0 } else { 96.0 };
                ui.spacing_mut().interact_size.y = 16.0;
                ui.add(egui::Slider::new(&mut self.cfg.volume, 0.0..=2.0).show_value(false))
            })
            .inner
            .on_hover_text(format!("Passthrough volume {:.0}%", self.cfg.volume * 100.0));
        if resp.changed() {
            self.shared.ring.set_volume(self.cfg.volume);
        }
        let muted = self.cfg.muted || self.cfg.volume == 0.0;
        let icon = if muted { icons::MUTED } else { icons::SPEAKER };
        if theme::icon_button(ui, icon, if self.cfg.muted { "Unmute passthrough" } else { "Mute passthrough" }, false)
            .clicked()
        {
            self.cfg.muted = !self.cfg.muted;
            self.shared.ring.set_muted(self.cfg.muted);
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let st = &self.shared.status;
        let text = |s: String| RichText::new(s).font(theme::mono(12.0)).color(theme::TEXT2);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 20.0;
            if let Some(since) = *st.recording_since.lock() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    theme::dot(ui, theme::REC);
                    ui.label(
                        RichText::new(format!("REC {}", theme::fmt_clock(since.elapsed().as_secs())))
                            .font(FontId::new(12.0, theme::mono_medium()))
                            .color(theme::REC),
                    );
                });
            }
            let signal = st.fps_x100.load(Ordering::Relaxed) as f32 / 100.0;
            let game = st.game_fps_x100.load(Ordering::Relaxed) as f32 / 100.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(format!("{game:.0} fps")).font(theme::mono(12.0)).color(theme::TEXT))
                    .on_hover_text(
                        "Game frame rate: captured frames that differ from the one before. \
                         Reads low on still screens (menus, pause), where the image doesn't change.",
                    );
                ui.label(text(format!("· {signal:.1} Hz")))
                    .on_hover_text("Signal refresh rate: frames per second arriving from the capture card");
            });
            let dropped = st.dropped_frames.load(Ordering::Relaxed);
            if dropped > 0 {
                ui.label(RichText::new(format!("{dropped} dropped")).font(theme::mono(12.0)).color(theme::WARNING));
            }
            if self.cfg.replay_enabled {
                let secs = st.replay_ms.load(Ordering::Relaxed) as f32 / 1000.0;
                let mb = st.replay_bytes.load(Ordering::Relaxed) as f32 / 1_048_576.0;
                let total = self.cfg.replay_seconds.max(1) as f32;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(text("Replay".into()));
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(64.0, 4.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 2, theme::LINE);
                    let mut fill = rect;
                    fill.set_width(rect.width() * (secs / total).clamp(0.0, 1.0));
                    ui.painter().rect_filled(fill, 2, theme::SIGNAL);
                    ui.label(text(format!("{secs:.0}/{} s · {mb:.0} MB", self.cfg.replay_seconds)));
                });
            } else {
                ui.label(RichText::new("Replay off").font(theme::mono(12.0)).color(theme::TEXT3));
            }
            let enc = st.encoder.lock().clone();
            let parts: Vec<&str> = enc.split_whitespace().collect();
            let enc = if parts.len() >= 4 { format!("{} · {} · {} {}", parts[0], parts[1], parts[2], parts[3]) } else { enc };
            ui.label(text(enc));
            if let Some(c) = *st.color.lock() {
                ui.label(text(c.label())).on_hover_text("Input YCbCr matrix and range (Settings > Capture to override)");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(text(format!("Audio: {}", st.audio_in.lock())));
            });
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
                if let Some(since) = *self.shared.status.recording_since.lock() {
                    self.rec_pill(ui, r.min + egui::vec2(16.0, 16.0), since.elapsed().as_secs());
                }
            }
            _ => {
                let (title, detail) = match (&*self.shared.status.error.lock(), self.gpu_ok) {
                    (_, false) => ("GPU renderer unavailable".to_string(), String::new()),
                    (Some(e), _) => ("No signal".to_string(), e.clone()),
                    (None, _) => ("Waiting for video…".to_string(), String::new()),
                };
                let c = rect.center();
                let img = egui::Rect::from_center_size(c - egui::vec2(0.0, 40.0), egui::Vec2::splat(72.0));
                theme::mark_image(72.0).tint(Color32::from_white_alpha(90)).paint_at(ui, img);
                let p = ui.painter();
                p.text(c + egui::vec2(0.0, 12.0), egui::Align2::CENTER_CENTER, title, FontId::new(16.0, theme::semibold()), theme::TEXT2);
                if !detail.is_empty() {
                    let galley = p.layout(detail, FontId::proportional(12.0), theme::TEXT3, (rect.width() - 80.0).clamp(200.0, 640.0));
                    let pos = c + egui::vec2(-galley.size().x / 2.0, 30.0);
                    p.galley(pos, galley, theme::TEXT3);
                }
            }
        }
    }

    /// "● REC 00:01:23" pill over the video.
    fn rec_pill(&self, ui: &egui::Ui, at: egui::Pos2, secs: u64) {
        let p = ui.painter();
        let rec = p.layout_no_wrap("REC".into(), FontId::new(12.0, theme::semibold()), theme::REC);
        let time = p.layout_no_wrap(theme::fmt_clock(secs), FontId::new(12.0, theme::mono_medium()), theme::TEXT);
        let w = 10.0 + 7.0 + 6.0 + rec.size().x + 6.0 + time.size().x + 10.0;
        let rect = egui::Rect::from_min_size(at, egui::vec2(w, 24.0));
        p.rect_filled(rect, 12, Color32::from_rgba_unmultiplied(8, 9, 12, 184));
        let cy = rect.center().y;
        p.circle_filled(egui::pos2(rect.left() + 13.5, cy), 3.5, theme::REC);
        let x = rect.left() + 23.0;
        p.galley(egui::pos2(x, cy - rec.size().y / 2.0), rec.clone(), theme::REC);
        p.galley(egui::pos2(x + rec.size().x + 6.0, cy - time.size().y / 2.0), time, theme::TEXT);
    }

    fn toasts(&self, ctx: &egui::Context) {
        let toasts = self.shared.recent_toasts(Duration::from_secs(5));
        if toasts.is_empty() {
            return;
        }
        let bottom = if self.fullscreen { -20.0 } else { -48.0 };
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-20.0, bottom])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                for (text, error, age) in toasts {
                    let (title, detail) = match text.split_once(": ") {
                        Some((t, d)) => (t.to_string(), d.to_string()),
                        None => (text.clone(), String::new()),
                    };
                    let saved = !error && title.to_lowercase().contains("saved");
                    ui.scope(|ui| {
                        ui.set_opacity((5.0 - age).clamp(0.0, 1.0));
                        egui::Frame::new()
                            .fill(theme::RAISED)
                            .stroke(Stroke::new(1.0_f32, theme::LINE))
                            .corner_radius(10)
                            .shadow(ui.visuals().window_shadow)
                            .inner_margin(Margin::symmetric(14, 12))
                            .show(ui, |ui| {
                                ui.set_width(332.0);
                                ui.horizontal_top(|ui| {
                                    let (icon, color) = if error { (icons::ALERT, theme::REC) } else { (icons::CHECK, theme::SUCCESS) };
                                    ui.add(theme::icon_image(icon, 18.0, color));
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        let w = if saved { 240.0 } else { 290.0 };
                                        ui.set_max_width(w);
                                        ui.label(RichText::new(&title).font(FontId::new(13.0, theme::semibold())).color(theme::TEXT));
                                        if !detail.is_empty() {
                                            ui.add(
                                                egui::Label::new(RichText::new(&detail).size(12.0).color(theme::TEXT2))
                                                    .truncate(),
                                            );
                                        }
                                    });
                                    if saved {
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                                            let show = ui.add(
                                                egui::Label::new(
                                                    RichText::new("Show").font(FontId::new(12.0, theme::semibold())).color(theme::SIGNAL),
                                                )
                                                .sense(egui::Sense::click()),
                                            );
                                            if show.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                                self.pipeline.send(if title.starts_with("Screenshot") {
                                                    Command::OpenScreenshotFolder
                                                } else {
                                                    Command::OpenOutputFolder
                                                });
                                            }
                                        });
                                    }
                                });
                            });
                    });
                    ui.add_space(8.0);
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
        let capturing_hotkey = self.settings.as_ref().is_some_and(|s| s.is_capturing_hotkey());
        let (f11, esc) = ctx.input(|i| (i.key_pressed(egui::Key::F11), i.key_pressed(egui::Key::Escape)));
        if !capturing_hotkey {
            if f11 {
                let on = !self.fullscreen;
                self.set_fullscreen(ctx, on);
            } else if esc && self.fullscreen {
                self.set_fullscreen(ctx, false);
            }
        }

        if !self.fullscreen {
            egui::TopBottomPanel::top("toolbar")
                .frame(
                    egui::Frame::new()
                        .fill(theme::SURFACE)
                        .inner_margin(Margin::symmetric(12, 8))
                        .stroke(Stroke::new(1.0_f32, theme::DIVIDER)),
                )
                .show(ctx, |ui| self.toolbar(ui));
            egui::TopBottomPanel::bottom("status")
                .frame(
                    egui::Frame::new()
                        .fill(theme::SURFACE)
                        .inner_margin(Margin::symmetric(12, 5))
                        .stroke(Stroke::new(1.0_f32, theme::DIVIDER)),
                )
                .show(ctx, |ui| self.status_bar(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::VOID))
            .show(ctx, |ui| self.preview(ui));

        // Updates: announce a new release once; close when the installer has taken over.
        match self.updater.state() {
            UpdateState::Available(rel) if self.announced_update.as_ref() != Some(&rel.version) => {
                self.announced_update = Some(rel.version.clone());
                let kind = if rel.prerelease { "Pre-release" } else { "New version" };
                self.shared.notify(
                    format!("Passthrify {} is available: {kind} — open Settings › Updates to install", rel.tag),
                    false,
                );
            }
            UpdateState::Restarting => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }

        self.toasts(ctx);

        if let Some(s) = self.settings.as_mut() {
            let modes = self.shared.status.modes.lock().clone();
            let current = *self.shared.status.mode.lock();
            let actual_chroma = *self.shared.status.chroma.lock();
            let color = *self.shared.status.color.lock();
            let recording = self.shared.status.recording_since.lock().is_some();
            match s.show(ctx, &modes, current, actual_chroma, color, &self.hotkey_errors, &self.updater, recording) {
                SettingsOutcome::Open => {}
                SettingsOutcome::Close => self.settings = None,
                SettingsOutcome::OpenFolder { screenshots: false } => self.pipeline.send(Command::OpenOutputFolder),
                SettingsOutcome::OpenFolder { screenshots: true } => self.pipeline.send(Command::OpenScreenshotFolder),
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

        // Keep the REC timer and status text fresh even when no frames arrive.
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn on_exit(&mut self) {
        self.cfg.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys_read_naturally() {
        assert_eq!(pretty_hotkey("Alt+F10"), "Alt F10");
        assert_eq!(pretty_hotkey("Ctrl+Shift+KeyR"), "Ctrl Shift R");
        assert_eq!(pretty_hotkey("Control+Digit1"), "Ctrl 1");
        assert_eq!(pretty_hotkey(""), "");
    }
}
