use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// Sidebar shown. Starts from `cfg.sidebar_open`, but narrow windows start without it and
    /// fullscreen puts it away.
    sidebar_open: bool,
    /// Sidebar state to restore when leaving fullscreen.
    sidebar_before_fullscreen: bool,
    /// The window's fullscreen state as last reported, to notice the window manager changing it.
    seen_fullscreen: Option<bool>,
    /// The first frame has not run yet (the window size is unknown until then).
    first_frame: bool,
    /// The window manager decorates the window (Linux; follows the Title bar setting live).
    #[cfg(target_os = "linux")]
    native_decorations: bool,
    /// egui time until which the show-sidebar button stays visible.
    show_button_until: f64,
    /// egui time of the last mouse movement or click (hides the cursor over the video when idle).
    cursor_moved_at: f64,
}

/// Sidebar width, and its gap to the video area's edges.
const SIDEBAR_W: f32 = 304.0;
const SIDEBAR_INSET: f32 = 8.0;
/// Windows narrower than this start with the sidebar hidden.
const NARROW_WINDOW: f32 = 1180.0;
/// Seconds without mouse movement before the show-sidebar button fades out.
const SHOW_BUTTON_IDLE: f64 = 3.0;
/// Seconds without mouse movement before the cursor over the video hides.
const CURSOR_IDLE: f64 = 3.0;
/// Our own title bar (the native one is removed, see `frame.rs`) and its caption buttons.
const TITLE_BAR_H: f32 = 32.0;
const CAPTION_BUTTON_W: f32 = 46.0;

#[derive(Clone, Copy, PartialEq)]
enum CaptionButton {
    Minimize,
    Maximize,
    Close,
}

/// One title bar button, drawn like Windows' own: thin glyph, grey hover, red hover on Close.
fn caption_button(ui: &mut egui::Ui, rect: egui::Rect, kind: CaptionButton, maximized: bool, focused: bool) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with(("caption", kind as u8)), egui::Sense::click());
    let p = ui.painter();
    let close = kind == CaptionButton::Close;
    let (bg, fg) = match (close, resp.is_pointer_button_down_on(), resp.hovered()) {
        (true, true, _) => (Color32::from_rgb(0x94, 0x1F, 0x15), Color32::WHITE),
        (true, false, true) => (Color32::from_rgb(0xC4, 0x2B, 0x1C), Color32::WHITE),
        (false, true, _) => (theme::PRESSED, theme::TEXT),
        (false, false, true) => (theme::HOVER, theme::TEXT),
        _ => (Color32::TRANSPARENT, if focused { theme::TEXT2 } else { theme::TEXT3 }),
    };
    p.rect_filled(rect, 0, bg);
    let c = rect.center();
    let s = Stroke::new(1.0_f32, fg);
    let sq = |half: f32, at: egui::Pos2| egui::Rect::from_center_size(at, egui::Vec2::splat(half * 2.0));
    match kind {
        CaptionButton::Minimize => {
            p.hline((c.x - 5.0)..=(c.x + 5.0), c.y, s);
        }
        CaptionButton::Maximize if maximized => {
            // Restore: a square with another peeking out behind it.
            let front = sq(4.0, c + egui::vec2(-1.0, 1.0));
            p.rect_stroke(front, 0, s, egui::StrokeKind::Inside);
            p.hline((front.left() + 2.0)..=(front.right() + 2.0), front.top() - 2.0, s);
            p.vline(front.right() + 2.0, (front.top() - 2.0)..=(front.bottom() - 2.0), s);
        }
        CaptionButton::Maximize => {
            p.rect_stroke(sq(5.0, c), 0, s, egui::StrokeKind::Inside);
        }
        CaptionButton::Close => {
            p.line_segment([c + egui::vec2(-5.0, -5.0), c + egui::vec2(5.0, 5.0)], s);
            p.line_segment([c + egui::vec2(5.0, -5.0), c + egui::vec2(-5.0, 5.0)], s);
        }
    }
    resp
}

/// A row of the sidebar's settings list: icon, label, summary (or an update badge), chevron.
fn settings_row(ui: &mut egui::Ui, section: Section, summary: &str, badge: Option<&str>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 6, theme::RAISED);
    }
    let cy = rect.center().y;
    let icon = egui::Rect::from_center_size(egui::pos2(rect.left() + 18.0, cy), egui::Vec2::splat(16.0));
    theme::icon_image(section.icon(), 16.0, theme::TEXT2).paint_at(ui, icon);
    p.text(egui::pos2(rect.left() + 36.0, cy), egui::Align2::LEFT_CENTER, section.label(), FontId::new(13.0, theme::medium()), theme::TEXT);
    let chevron = egui::Rect::from_center_size(egui::pos2(rect.right() - 15.0, cy), egui::Vec2::splat(14.0));
    theme::icon_image(icons::CHEVRON, 14.0, theme::TEXT3).paint_at(ui, chevron);
    let right = rect.right() - 28.0;
    if let Some(tag) = badge {
        let galley = p.layout_no_wrap(tag.to_string(), theme::mono(11.0), theme::SIGNAL);
        let r = egui::Rect::from_min_size(
            egui::pos2(right - galley.size().x - 12.0, cy - galley.size().y / 2.0 - 2.0),
            galley.size() + egui::vec2(12.0, 4.0),
        );
        p.rect_stroke(r, 4, Stroke::new(1.0_f32, theme::SIGNAL.gamma_multiply(0.5)), egui::StrokeKind::Inside);
        p.galley(r.min + egui::vec2(6.0, 2.0), galley, theme::SIGNAL);
    } else if !summary.is_empty() {
        // Keep long summaries (device names) clear of the label.
        let label_end = rect.left() + 36.0 + 8.0 + p.layout_no_wrap(section.label().into(), FontId::new(13.0, theme::medium()), theme::TEXT).size().x;
        let clip = egui::Rect::from_min_max(egui::pos2(label_end, rect.top()), egui::pos2(right, rect.bottom()));
        p.with_clip_rect(clip).text(egui::pos2(right, cy), egui::Align2::RIGHT_CENTER, summary, FontId::proportional(12.0), theme::TEXT3);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
                "super" | "meta" | "win" | "cmd" => if cfg!(windows) { "Win" } else { "Super" }.into(),
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
        super::frame::install(hwnd);

        // `passthrify --action …` and second launches (control.rs).
        #[cfg(unix)]
        {
            let ctx = cc.egui_ctx.clone();
            crate::control::serve(pipeline.sender(), move || {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();
            });
        }

        #[cfg(target_os = "linux")]
        let native_decorations = !cfg.title_bar.app_drawn();
        let mut hotkey_errors = Vec::new();
        // Wayland: the compositor binds `passthrify --action …` instead (Settings › Hotkeys).
        let manager = if crate::hotkeys::wayland_session() { Ok(None) } else { Hotkeys::new(pipeline.sender()).map(Some) };
        let hotkeys = match manager {
            Ok(None) => None,
            Ok(Some(mut h)) => {
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
        #[cfg(windows)]
        let tray = tray::create(hwnd, pipeline.sender());
        #[cfg(target_os = "linux")]
        let tray = tray::create(&cc.egui_ctx, pipeline.sender());
        let tray = match tray {
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
            sidebar_open: true,
            sidebar_before_fullscreen: true,
            seen_fullscreen: None,
            first_frame: true,
            #[cfg(target_os = "linux")]
            native_decorations,
            show_button_until: 0.0,
            cursor_moved_at: 0.0,
        };
        // PASSTHRIFY_SETTINGS=capture|audio|replay|streaming|hotkeys|display opens Settings on
        // launch (handy for docs and screenshots).
        if let Ok(s) = std::env::var("PASSTHRIFY_SETTINGS") {
            let section = match s.to_ascii_lowercase().as_str() {
                "audio" => Section::Audio,
                "replay" => Section::Replay,
                "streaming" => Section::Streaming,
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
        if self.follow_fullscreen(on) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
        }
    }

    /// Switches the layout in or out of fullscreen; false if it already was.
    fn follow_fullscreen(&mut self, on: bool) -> bool {
        if on == self.fullscreen {
            return false;
        }
        // The sidebar steps aside in fullscreen and comes back afterwards.
        if on {
            self.sidebar_before_fullscreen = self.sidebar_open;
            self.sidebar_open = false;
        } else {
            self.sidebar_open = self.sidebar_before_fullscreen;
        }
        self.fullscreen = on;
        true
    }

    /// Shows or hides the sidebar at the user's request (remembered in the config).
    fn set_sidebar(&mut self, ctx: &egui::Context, open: bool) {
        self.sidebar_open = open;
        self.cfg.sidebar_open = open;
        if !open {
            // Keep the show button up for a moment so it can be found again.
            self.show_button_until = ctx.input(|i| i.time) + SHOW_BUTTON_IDLE;
        }
    }

    /// Opens a settings section in the sidebar (showing the sidebar if needed).
    fn open_settings(&mut self, section: Section) {
        self.sidebar_open = true;
        match self.settings.as_mut() {
            Some(s) if s.section == section => {}
            _ => self.settings = Some(SettingsUi::new(&self.cfg, section)),
        }
    }

    fn handle_settings(&mut self, outcome: SettingsOutcome) {
        match outcome {
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

    /// The sidebar: a floating panel over the video's right edge. From the top: device chip,
    /// pinned actions, then (scrolling) the capture pickers and the settings list, or the open
    /// settings section; at the bottom the update notice, volume and fullscreen.
    fn sidebar(&mut self, ctx: &egui::Context, video: egui::Rect) {
        let rect = egui::Rect::from_min_max(
            egui::pos2(video.right() - SIDEBAR_INSET - SIDEBAR_W, video.top() + SIDEBAR_INSET),
            egui::pos2(video.right() - SIDEBAR_INSET, video.bottom() - SIDEBAR_INSET),
        );
        if rect.height() < 200.0 || rect.left() < video.left() {
            return; // window too small to hold it
        }
        egui::Area::new(egui::Id::new("sidebar"))
            .fixed_pos(rect.min)
            .constrain(false)
            .order(egui::Order::Middle)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .stroke(Stroke::new(1.0_f32, theme::LINE))
                    .corner_radius(10)
                    .shadow(ctx.style().visuals.window_shadow)
                    .show(ui, |ui| {
                        ui.set_min_size(rect.size());
                        ui.set_max_size(rect.size());
                        let full = egui::Rect::from_min_size(ui.min_rect().min, rect.size());
                        self.sidebar_contents(ui, full);
                    });
            });
    }

    fn sidebar_contents(&mut self, ui: &mut egui::Ui, full: egui::Rect) {
        use egui::{pos2, Layout, Rect, UiBuilder};

        ui.spacing_mut().interact_size.y = 32.0;
        ui.spacing_mut().combo_height = 400.0;
        let update = match self.updater.state() {
            UpdateState::Available(rel) => Some(rel),
            _ => None,
        };
        let section_open = self.settings.as_ref().map(|s| s.section);
        let show_update_card = update.is_some() && section_open != Some(Section::Updates);

        // Fixed bands, top-down and bottom-up; the middle scrolls.
        let header = Rect::from_min_size(full.min, egui::vec2(full.width(), 56.0));
        // Save replay (40) + Record row (36) + Go live row (36), with 8 px gaps and 12 px margins.
        let actions = Rect::from_min_max(pos2(full.left(), header.bottom()), pos2(full.right(), header.bottom() + 152.0));
        let volume = Rect::from_min_max(pos2(full.left(), full.bottom() - 48.0), full.max);
        let mut bottom = volume.top();
        let update_card = show_update_card.then(|| {
            let r = Rect::from_min_max(pos2(full.left(), bottom - 62.0), pos2(full.right(), bottom));
            bottom = r.top();
            r
        });
        let section_footer = section_open.map(|_| {
            let r = Rect::from_min_max(pos2(full.left(), bottom - 52.0), pos2(full.right(), bottom));
            bottom = r.top();
            r
        });
        let middle = Rect::from_min_max(pos2(full.left(), actions.bottom()), pos2(full.right(), bottom));

        let p = ui.painter().clone();
        let divider = |y: f32| p.hline(full.x_range(), y, Stroke::new(1.0_f32, theme::DIVIDER));
        divider(header.bottom());
        divider(actions.bottom());
        divider(volume.top());
        if let Some(r) = section_footer {
            divider(r.top());
        }

        let in_rect = |ui: &mut egui::Ui, r: Rect, layout: Layout, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.scope_builder(UiBuilder::new().max_rect(r).layout(layout), |ui| add(ui));
        };

        in_rect(ui, header.shrink2(egui::vec2(8.0, 8.0)), Layout::left_to_right(egui::Align::Center), &mut |ui| {
            self.sidebar_header(ui)
        });
        in_rect(ui, actions.shrink(12.0), Layout::top_down(egui::Align::Min), &mut |ui| self.sidebar_actions(ui));

        ui.scope_builder(UiBuilder::new().max_rect(middle), |ui| {
            egui::ScrollArea::vertical().id_salt("sidebar_scroll").auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::new().inner_margin(Margin { left: 12, right: 12, top: 12, bottom: 14 }).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if self.settings.is_some() {
                        self.sidebar_section(ui);
                    } else {
                        self.sidebar_home(ui, update.as_ref().map(|r| r.tag.as_str()));
                    }
                });
            });
        });

        if let Some(r) = section_footer {
            let mut outcome = SettingsOutcome::Open;
            in_rect(ui, r.shrink2(egui::vec2(12.0, 10.0)), Layout::left_to_right(egui::Align::Center), &mut |ui| {
                if let Some(s) = self.settings.as_mut() {
                    outcome = s.footer(ui);
                }
            });
            self.handle_settings(outcome);
        }
        if let (Some(r), Some(rel)) = (update_card, &update) {
            let r = Rect::from_min_max(pos2(r.left() + 12.0, r.top()), pos2(r.right() - 12.0, r.bottom() - 10.0));
            in_rect(ui, r, Layout::top_down(egui::Align::Min), &mut |ui| self.update_card(ui, rel));
        }
        in_rect(ui, volume.shrink2(egui::vec2(8.0, 0.0)), Layout::left_to_right(egui::Align::Center), &mut |ui| {
            self.sidebar_volume(ui)
        });
    }

    /// Device chip (status, name, mode; opens Capture settings) and the hide button.
    fn sidebar_header(&mut self, ui: &mut egui::Ui) {
        let st = &self.shared.status;
        let capturing = st.capturing.load(Ordering::Relaxed);
        let has_error = st.error.lock().is_some();
        let device = st.device.lock().clone();
        let mode = *st.mode.lock();
        let (dot, detail) = if capturing {
            let m = mode.map(|m| format!(" · {}x{} · {:.0} Hz", m.width, m.height, m.fps())).unwrap_or_default();
            (theme::SUCCESS, format!("Capturing{m}"))
        } else if has_error {
            (theme::REC, "No signal".to_string())
        } else {
            (theme::TEXT3, "Not capturing".to_string())
        };
        let name = if device.is_empty() { "No device".to_string() } else { device };

        let chip_w = ui.available_width() - 32.0 - ui.spacing().item_spacing.x;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(chip_w, 40.0), egui::Sense::click());
        let p = ui.painter().with_clip_rect(rect);
        if resp.hovered() {
            p.rect_filled(rect, 6, theme::RAISED);
        }
        p.circle_filled(egui::pos2(rect.left() + 14.0, rect.center().y), 4.0, dot);
        let x = rect.left() + 28.0;
        p.text(egui::pos2(x, rect.center().y - 8.0), egui::Align2::LEFT_CENTER, name, FontId::new(13.0, theme::medium()), theme::TEXT);
        p.text(egui::pos2(x, rect.center().y + 9.0), egui::Align2::LEFT_CENTER, detail, theme::mono(11.0), theme::TEXT3);
        let tip = if capturing { "Capturing — click for capture settings" } else { "Not capturing — click for capture settings" };
        if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.open_settings(Section::Capture);
        }
        if theme::icon_button(ui, icons::SIDEBAR_HIDE, "Hide sidebar (Ctrl+B)", false).clicked() {
            self.set_sidebar(ui.ctx(), false);
        }
    }

    /// Save replay, Record / Stop, Screenshot, Open folder: pinned, never scroll away.
    fn sidebar_actions(&mut self, ui: &mut egui::Ui) {
        let recording_since = *self.shared.status.recording_since.lock();
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        let w = ui.available_width();

        let save_hint = pretty_hotkey(&self.cfg.hotkey_save_replay);
        let save = theme::wide_button(
            ui,
            Kind::Primary,
            icons::REPLAY,
            None,
            "Save replay",
            (!save_hint.is_empty()).then_some(save_hint.as_str()),
            egui::vec2(w, 40.0),
        );
        let mut save_tip = format!("Save replay: the last {} s", self.cfg.replay_seconds);
        if !save_hint.is_empty() {
            save_tip += &format!(" ({save_hint})");
        }
        if save.on_hover_text(save_tip).clicked() {
            self.pipeline.send(Command::SaveReplay);
        }

        self.live_button(ui, w);

        ui.horizontal(|ui| {
            let rec_hint = pretty_hotkey(&self.cfg.hotkey_toggle_record);
            let rec_w = w - 2.0 * (36.0 + 8.0);
            let size = egui::vec2(rec_w, 36.0);
            let rec = match recording_since {
                Some(since) => {
                    let clock = theme::fmt_clock(since.elapsed().as_secs());
                    theme::wide_button(ui, Kind::Danger, icons::STOP, None, "Stop", Some(&clock), size)
                }
                None => {
                    let hint = (!rec_hint.is_empty()).then_some(rec_hint.as_str());
                    theme::wide_button(ui, Kind::Secondary, icons::RECORD, Some(theme::REC), "Record", hint, size)
                }
            };
            if rec.on_hover_text(format!("Start / stop recording ({rec_hint})")).clicked() {
                self.pipeline.send(Command::ToggleRecord);
            }
            let shot_tip = format!("Screenshot ({})", pretty_hotkey(&self.cfg.hotkey_screenshot));
            if theme::square_button(ui, Kind::Secondary, icons::CAMERA, 36.0).on_hover_text(shot_tip).clicked() {
                self.pipeline.send(Command::Screenshot);
            }
            let folder_tip = format!("Open video folder ({})", self.cfg.output_dir.display());
            if theme::square_button(ui, Kind::Ghost, icons::FOLDER, 36.0).on_hover_text(folder_tip).clicked() {
                self.pipeline.send(Command::OpenOutputFolder);
            }
        });
    }

    /// Go live / End stream (under Save replay).
    fn live_button(&mut self, ui: &mut egui::Ui, w: f32) {
        let live_hint = pretty_hotkey(&self.cfg.hotkey_toggle_stream);
        let state = self.shared.stream.state.lock().clone();
        let size = egui::vec2(w, 36.0);
        let live = match &state {
            crate::stream::StreamState::Off => {
                let hint = (!live_hint.is_empty()).then_some(live_hint.as_str());
                let label = format!("Go live on {}", self.cfg.stream_service.label());
                theme::wide_button(ui, Kind::Secondary, icons::LIVE, Some(theme::REC), &label, hint, size)
            }
            crate::stream::StreamState::Live { since } => {
                let clock = theme::fmt_clock(since.elapsed().as_secs());
                theme::wide_button(ui, Kind::Danger, icons::STOP, None, "End stream", Some(&clock), size)
            }
            crate::stream::StreamState::Connecting => {
                theme::wide_button(ui, Kind::Danger, icons::STOP, None, "Connecting… (cancel)", None, size)
            }
            crate::stream::StreamState::Reconnecting { .. } => {
                theme::wide_button(ui, Kind::Danger, icons::STOP, None, "Reconnecting… (end)", None, size)
            }
        };
        if live.on_hover_text(format!("Start / stop streaming ({live_hint}); set it up in Settings › Streaming")).clicked() {
            self.pipeline.send(Command::ToggleStream);
        }
    }

    /// Capture pickers (2x2, applied immediately) and the settings list.
    fn sidebar_home(&mut self, ui: &mut egui::Ui, update_tag: Option<&str>) {
        let shared = self.shared.clone();
        let st = &shared.status;
        let mode = *st.mode.lock();
        let modes = st.modes.lock().clone();
        let recording = st.recording_since.lock().is_some();
        let actual_chroma = *st.chroma.lock();

        ui.horizontal(|ui| {
            theme::field_label(ui, "Capture");
            if recording {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Locked while recording").size(11.0).color(theme::TEXT3));
                    ui.add(theme::icon_image(icons::LOCK, 13.0, theme::TEXT3));
                });
            }
        });
        ui.add_space(2.0);

        let mut pref = self.cfg.video_mode;
        let mut codec = self.cfg.codec;
        let mut chroma = self.cfg.chroma;
        let small = |ui: &mut egui::Ui, text: &str| {
            ui.label(RichText::new(text.to_uppercase()).font(FontId::new(10.5, theme::semibold())).color(theme::TEXT3));
        };
        let changed = ui
            .add_enabled_ui(!recording, |ui| {
                let mut c = false;
                ui.columns(2, |cols| {
                    small(&mut cols[0], "Chroma");
                    let w = cols[0].available_width();
                    c |= pickers::chroma_picker(&mut cols[0], "sidebar", &mut chroma, &mut pref, &mut codec, &modes, actual_chroma, w);
                    small(&mut cols[1], "Resolution");
                    let w = cols[1].available_width();
                    c |= pickers::resolution_picker(&mut cols[1], "sidebar", &mut pref, chroma, &modes, mode, w);
                });
                ui.add_space(4.0);
                ui.columns(2, |cols| {
                    small(&mut cols[0], "Frame rate");
                    let w = cols[0].available_width();
                    c |= pickers::rate_picker(&mut cols[0], "sidebar", &mut pref, chroma, &modes, mode, w);
                    small(&mut cols[1], "Codec");
                    let w = cols[1].available_width();
                    c |= pickers::codec_picker(&mut cols[1], "sidebar", &mut codec, chroma, true, w);
                });
                c
            })
            .inner;
        if changed {
            let mut c = self.cfg.clone();
            c.video_mode = pref;
            c.codec = codec;
            c.chroma = chroma;
            self.apply_settings(c);
        }

        ui.add_space(10.0);
        let y = ui.cursor().top();
        ui.painter().hline(ui.max_rect().x_range(), y, Stroke::new(1.0_f32, theme::DIVIDER));
        ui.add_space(12.0);

        theme::field_label(ui, "Settings");
        ui.spacing_mut().item_spacing.y = 0.0;
        for s in Section::ALL {
            let summary = if s == Section::Updates && update_tag.is_some() { String::new() } else { s.summary(&self.cfg) };
            let badge = if s == Section::Updates { update_tag } else { None };
            if settings_row(ui, s, &summary, badge).clicked() {
                self.open_settings(s);
            }
        }
    }

    /// An open settings section: back arrow + title, then its fields.
    fn sidebar_section(&mut self, ui: &mut egui::Ui) {
        let Some(mut s) = self.settings.take() else { return };
        let mut back = false;
        ui.horizontal(|ui| {
            back = theme::icon_button(ui, icons::BACK, "Back to settings (Esc)", false).clicked();
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(RichText::new("Settings").size(11.0).color(theme::TEXT3));
                ui.label(RichText::new(s.section.label()).font(FontId::new(15.0, theme::semibold())).color(theme::TEXT));
            });
        });
        ui.add_space(10.0);

        let st = &self.shared.status;
        let modes = st.modes.lock().clone();
        let current = *st.mode.lock();
        let actual_chroma = *st.chroma.lock();
        let color = *st.color.lock();
        let recording = st.recording_since.lock().is_some();
        let outcome = s.body(
            ui,
            &modes,
            current,
            actual_chroma,
            color,
            &self.hotkey_errors,
            &self.updater,
            recording,
            &self.shared.frame_gen,
        );
        self.settings = Some(s);
        self.handle_settings(if back { SettingsOutcome::Close } else { outcome });
    }

    /// "Update vX · channel · View" card above the volume row.
    fn update_card(&mut self, ui: &mut egui::Ui, rel: &updater::Release) {
        let mut view = false;
        egui::Frame::new()
            .fill(theme::BASE)
            .stroke(Stroke::new(1.0_f32, theme::SIGNAL.gamma_multiply(0.35)))
            .corner_radius(8)
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add(theme::icon_image(icons::DOWNLOAD, 16.0, theme::SIGNAL));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(RichText::new(format!("Update {}", rel.tag)).font(FontId::new(12.0, theme::semibold())).color(theme::TEXT));
                        let kind = if rel.prerelease { "Pre-release" } else { "Stable" };
                        ui.label(RichText::new(kind).font(theme::mono(11.0)).color(theme::TEXT3));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let link = ui.add(
                            egui::Label::new(RichText::new("View").font(FontId::new(12.0, theme::semibold())).color(theme::SIGNAL))
                                .sense(egui::Sense::click()),
                        );
                        view = link.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
                    });
                });
            });
        if view {
            self.open_settings(Section::Updates);
        }
    }

    /// Mute, volume slider + percentage, fullscreen.
    fn sidebar_volume(&mut self, ui: &mut egui::Ui) {
        let muted = self.cfg.muted || self.cfg.volume == 0.0;
        let icon = if muted { icons::MUTED } else { icons::SPEAKER };
        let tip = if self.cfg.muted { "Unmute passthrough" } else { "Mute passthrough" };
        if theme::icon_button(ui, icon, tip, false).clicked() {
            self.cfg.muted = !self.cfg.muted;
            self.shared.ring.set_muted(self.cfg.muted);
        }
        let slider_w = ui.available_width() - 38.0 - 32.0 - 2.0 * ui.spacing().item_spacing.x;
        let resp = ui
            .scope(|ui| {
                // Compact slider: knob size follows interact height, so shrink it here.
                ui.spacing_mut().slider_width = slider_w.max(40.0);
                ui.spacing_mut().interact_size.y = 16.0;
                ui.add(egui::Slider::new(&mut self.cfg.volume, 0.0..=2.0).show_value(false))
            })
            .inner
            .on_hover_text("Passthrough volume");
        if resp.changed() {
            self.shared.ring.set_volume(self.cfg.volume);
        }
        ui.allocate_ui_with_layout(
            egui::vec2(38.0, 32.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| ui.label(RichText::new(format!("{:.0}%", self.cfg.volume * 100.0)).font(theme::mono(11.0)).color(theme::TEXT3)),
        );
        let (icon, tip) = if self.fullscreen {
            (icons::EXIT_FULLSCREEN, "Exit fullscreen (F11 / Esc / double-click)")
        } else {
            (icons::FULLSCREEN, "Fullscreen (F11 / double-click)")
        };
        if theme::icon_button(ui, icon, tip, false).clicked() {
            let on = !self.fullscreen;
            self.set_fullscreen(ui.ctx(), on);
        }
    }

    /// With the sidebar hidden: a small button over the video's top-right corner brings it
    /// back. It fades out after a few seconds without mouse movement over the video.
    fn show_sidebar_button(&mut self, ctx: &egui::Context, video: egui::Rect) {
        let now = ctx.input(|i| i.time);
        let rect = egui::Rect::from_min_size(egui::pos2(video.right() - 12.0 - 36.0, video.top() + 12.0), egui::Vec2::splat(36.0));
        let (moved, hovered) = ctx.input(|i| {
            let pos = i.pointer.hover_pos();
            (i.pointer.delta() != egui::Vec2::ZERO && pos.is_some_and(|p| video.contains(p)), pos.is_some_and(|p| rect.contains(p)))
        });
        if moved {
            self.show_button_until = now + SHOW_BUTTON_IDLE;
        }
        let visible = hovered || now < self.show_button_until;
        let t = ctx.animate_bool_with_time(egui::Id::new("show_sidebar_fade"), visible, 0.4);
        if t <= 0.0 {
            return;
        }
        let has_update = matches!(self.updater.state(), UpdateState::Available(_));
        let mut clicked = false;
        egui::Area::new(egui::Id::new("show_sidebar"))
            .fixed_pos(rect.min)
            .constrain(false)
            .order(egui::Order::Middle)
            .show(ctx, |ui| {
                ui.set_opacity(t);
                let (r, resp) = ui.allocate_exact_size(rect.size(), egui::Sense::click());
                let p = ui.painter();
                let (bg, fg) = if resp.hovered() {
                    (theme::HOVER, theme::TEXT)
                } else {
                    (Color32::from_rgba_unmultiplied(19, 22, 27, 199), theme::TEXT2)
                };
                p.rect_filled(r, 8, bg);
                p.rect_stroke(r, 8, Stroke::new(1.0_f32, theme::LINE_STRONG), egui::StrokeKind::Inside);
                let icon = egui::Rect::from_center_size(r.center(), egui::Vec2::splat(16.0));
                theme::icon_image(icons::SIDEBAR_SHOW, 16.0, fg).paint_at(ui, icon);
                if has_update {
                    let c = egui::pos2(r.right() - 8.5, r.top() + 8.5);
                    p.circle_filled(c, 5.5, theme::SURFACE);
                    p.circle_filled(c, 3.5, theme::SIGNAL);
                }
                let tip = if has_update { "Show sidebar (Ctrl+B) · an update is available" } else { "Show sidebar (Ctrl+B)" };
                clicked = resp.on_hover_text(tip).clicked();
            });
        if clicked {
            self.set_sidebar(ctx, true);
        }
        if visible {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    /// Logo + title on the left (the rest of the bar drags the window, see `frame.rs`), and
    /// minimize / maximize / close on the right.
    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let ctx = ui.ctx().clone();
        let (focused, maximized) =
            ctx.input(|i| (i.viewport().focused.unwrap_or(true), i.viewport().maximized.unwrap_or(false)));
        // Windows: the window procedure makes this area drag the window. Linux: egui does.
        #[cfg(windows)]
        ui.allocate_rect(rect, egui::Sense::hover());
        #[cfg(target_os = "linux")]
        super::frame::title_bar(&ui.allocate_rect(rect, egui::Sense::click_and_drag()), maximized);
        let p = ui.painter();
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0_f32, theme::DIVIDER));
        let cy = rect.center().y;
        let logo = egui::Rect::from_center_size(egui::pos2(rect.left() + 20.0, cy), theme::mark_size(16.0));
        let tint = if focused { Color32::WHITE } else { Color32::from_white_alpha(140) };
        theme::mark_image(16.0).tint(tint).paint_at(ui, logo);
        let title_color = if focused { theme::TEXT2 } else { theme::TEXT3 };
        p.text(egui::pos2(rect.left() + 36.0, cy), egui::Align2::LEFT_CENTER, "Passthrify", FontId::new(12.0, theme::medium()), title_color);

        let buttons = [CaptionButton::Close, CaptionButton::Maximize, CaptionButton::Minimize];
        for (i, kind) in buttons.into_iter().enumerate() {
            let right = rect.right() - CAPTION_BUTTON_W * i as f32;
            let r = egui::Rect::from_min_max(egui::pos2(right - CAPTION_BUTTON_W, rect.top()), egui::pos2(right, rect.bottom() - 1.0));
            let tip = match kind {
                CaptionButton::Minimize => "Minimize",
                CaptionButton::Maximize if maximized => "Restore down",
                CaptionButton::Maximize => "Maximize",
                CaptionButton::Close => "Close",
            };
            if caption_button(ui, r, kind, maximized, focused).on_hover_text(tip).clicked() {
                ctx.send_viewport_cmd(match kind {
                    CaptionButton::Minimize => egui::ViewportCommand::Minimized(true),
                    CaptionButton::Maximize => egui::ViewportCommand::Maximized(!maximized),
                    CaptionButton::Close => egui::ViewportCommand::Close,
                });
            }
        }
    }

    /// "LIVE 00:12:03 · 6.1 Mbps" (or connecting / reconnecting) in the status bar.
    fn stream_status(&self, ui: &mut egui::Ui) {
        use crate::stream::StreamState;
        let s = &self.shared.stream;
        let state = s.state.lock().clone();
        let (text, color, tip) = match &state {
            StreamState::Off => return,
            StreamState::Connecting => ("LIVE connecting…".to_string(), theme::TEXT2, "Connecting to the stream server".to_string()),
            StreamState::Reconnecting { error } => {
                ("LIVE reconnecting…".to_string(), theme::REC, format!("Connection lost: {error}. Retrying."))
            }
            StreamState::Live { since } => {
                let mbps = s.kbps.load(Ordering::Relaxed) as f64 / 1000.0;
                let dropped = s.dropped.load(Ordering::Relaxed);
                (
                    format!("LIVE {} · {mbps:.1} Mbps", theme::fmt_clock(since.elapsed().as_secs())),
                    theme::REC,
                    format!("Streaming to {}. Frames the stream couldn't keep up with: {dropped}", self.cfg.stream_service.label()),
                )
            }
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            theme::dot(ui, color);
            ui.label(RichText::new(text).font(FontId::new(12.0, theme::mono_medium())).color(color)).on_hover_text(tip);
        });
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
            self.stream_status(ui);
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
                let fg = &self.shared.frame_gen;
                match fg.state() {
                    crate::vfg::State::Running if fg.output_fps() > 0.0 => {
                        let ms = fg.gen_ms_x10.load(Ordering::Relaxed) as f32 / 10.0;
                        ui.label(
                            RichText::new(format!("→ {:.0} fps", fg.output_fps()))
                                .font(theme::mono(12.0))
                                .color(theme::SIGNAL),
                        )
                        .on_hover_text(format!(
                            "Frame generation: frames per second shown in the preview, generated + real. \
                             {ms:.1} ms of GPU work per game frame."
                        ));
                    }
                    crate::vfg::State::Starting => {
                        ui.label(text("· frame gen starting…".into()));
                    }
                    crate::vfg::State::Failed(_) if fg.enabled() => {
                        ui.label(RichText::new("· frame gen off").font(theme::mono(12.0)).color(theme::WARNING))
                            .on_hover_text("Frame generation couldn't start. Details in Settings › Display.");
                    }
                    _ => {}
                }
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
                // Shortens itself (…) rather than overlapping the items on the left.
                ui.add(egui::Label::new(text(format!("Audio: {}", st.audio_in.lock()))).truncate());
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
        // Hide the cursor over the video after a few seconds without movement. Only while the
        // video itself is hovered: over the sidebar, toasts or other UI it stays visible.
        let now = ui.input(|i| i.time);
        if ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO || i.pointer.any_down()) {
            self.cursor_moved_at = now;
        }
        if resp.hovered() {
            let idle = now - self.cursor_moved_at;
            if idle >= CURSOR_IDLE {
                ui.ctx().set_cursor_icon(egui::CursorIcon::None);
            } else {
                ui.ctx().request_repaint_after(Duration::from_secs_f64(CURSOR_IDLE - idle));
            }
        }
        // With frame generation running, its paced stream replaces the live frame.
        let generated = self.shared.frame_gen.present(Instant::now()).map(|(frame, next)| {
            if let Some(due) = next {
                ui.ctx().request_repaint_after(due.saturating_duration_since(Instant::now()));
            }
            frame
        });
        match generated.or_else(|| self.shared.latest_frame.load_full()) {
            Some(frame) if self.gpu_ok => {
                let r = letterbox(rect, frame.width, frame.height);
                ui.painter().add(egui_wgpu::Callback::new_paint_callback(r, PreviewCallback { frame, target: self.shared.preview_target.clone() }));
                // REC and LIVE pills in the video's top-left corner, side by side.
                let mut at = r.min + egui::vec2(16.0, 16.0);
                if let Some(since) = *self.shared.status.recording_since.lock() {
                    at.x += self.pill(ui, at, "REC", &theme::fmt_clock(since.elapsed().as_secs())) + 8.0;
                }
                let live = match &*self.shared.stream.state.lock() {
                    crate::stream::StreamState::Off => None,
                    crate::stream::StreamState::Live { since } => Some(theme::fmt_clock(since.elapsed().as_secs())),
                    crate::stream::StreamState::Connecting => Some("connecting…".to_string()),
                    crate::stream::StreamState::Reconnecting { .. } => Some("reconnecting…".to_string()),
                };
                if let Some(text) = live {
                    self.pill(ui, at, "LIVE", &text);
                }
            }
            _ => {
                let (title, detail) = match (&*self.shared.status.error.lock(), self.gpu_ok) {
                    (_, false) => ("GPU renderer unavailable".to_string(), String::new()),
                    (Some(e), _) => ("No signal".to_string(), e.clone()),
                    (None, _) => ("Waiting for video…".to_string(), String::new()),
                };
                let c = rect.center();
                let img = egui::Rect::from_center_size(c - egui::vec2(0.0, 40.0), theme::mark_size(72.0));
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

    /// "● REC 00:01:23" / "● LIVE 00:12:03" pill over the video; returns its width.
    fn pill(&self, ui: &egui::Ui, at: egui::Pos2, label: &str, value: &str) -> f32 {
        let p = ui.painter();
        let rec = p.layout_no_wrap(label.into(), FontId::new(12.0, theme::semibold()), theme::REC);
        let time = p.layout_no_wrap(value.into(), FontId::new(12.0, theme::mono_medium()), theme::TEXT);
        let w = 10.0 + 7.0 + 6.0 + rec.size().x + 6.0 + time.size().x + 10.0;
        let rect = egui::Rect::from_min_size(at, egui::vec2(w, 24.0));
        p.rect_filled(rect, 12, Color32::from_rgba_unmultiplied(8, 9, 12, 184));
        let cy = rect.center().y;
        p.circle_filled(egui::pos2(rect.left() + 13.5, cy), 3.5, theme::REC);
        let x = rect.left() + 23.0;
        p.galley(egui::pos2(x, cy - rec.size().y / 2.0), rec.clone(), theme::REC);
        p.galley(egui::pos2(x + rec.size().x + 6.0, cy - time.size().y / 2.0), time, theme::TEXT);
        w
    }

    fn toasts(&self, ctx: &egui::Context) {
        let toasts = self.shared.recent_toasts(Duration::from_secs(5));
        if toasts.is_empty() {
            return;
        }
        let bottom = if self.fullscreen { -20.0 } else { -48.0 };
        // Stay clear of the sidebar while it is open.
        let right = if self.sidebar_open { -(SIDEBAR_W + SIDEBAR_INSET + 16.0) } else { -20.0 };
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [right, bottom])
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
                c.hotkey_toggle_stream.clear();
                // Read when going live.
                c.stream_service = crate::config::StreamService::Twitch;
                c.stream_server.clear();
                c.stream_height = 0;
                c.stream_fps = 0;
                c.stream_bitrate_kbps = 0;
                c.present_mode = crate::config::PresentMode::Mailbox;
                c.title_bar = crate::config::TitleBar::Auto;
                c.windows_notifications = false;
                c.sidebar_open = false;
                // Frame generation applies live (FrameGen::configure).
                c.frame_gen = false;
                c.frame_gen_quality = Default::default();
                c.frame_gen_multiplier = 2;
                c.frame_gen_dir = None;
            }
            a != b
        };
        let mut cfg = new_cfg;
        // Live UI state, not part of a settings draft.
        cfg.volume = self.cfg.volume;
        cfg.muted = self.cfg.muted;
        cfg.sidebar_open = self.cfg.sidebar_open;
        if let Some(s) = self.settings.as_mut() {
            s.sync_capture(&cfg);
        }
        self.cfg = cfg;
        self.cfg.save();
        self.shared.windows_notifications.store(self.cfg.windows_notifications, Ordering::Relaxed);
        self.shared.frame_gen.configure(&self.cfg);
        if let Some(h) = self.hotkeys.as_mut() {
            self.hotkey_errors = h.apply(&self.cfg);
        }
        if restart_needed {
            self.pipeline.send(Command::Apply(Box::new(self.cfg.clone())));
        } else {
            self.pipeline.send(Command::SetConfig(Box::new(self.cfg.clone())));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.first_frame {
            self.first_frame = false;
            // Narrow windows start with the sidebar hidden (unless settings were asked for).
            let wide = ctx.screen_rect().width() >= NARROW_WINDOW;
            if self.settings.is_none() {
                self.sidebar_open = self.cfg.sidebar_open && wide;
            }
        }

        let capturing_hotkey = self.settings.as_ref().is_some_and(|s| s.is_capturing_hotkey());
        let (f11, esc) = ctx.input(|i| (i.key_pressed(egui::Key::F11), i.key_pressed(egui::Key::Escape)));
        if !capturing_hotkey {
            if f11 {
                let on = !self.fullscreen;
                self.set_fullscreen(ctx, on);
            } else if esc && self.fullscreen {
                self.set_fullscreen(ctx, false);
            }
            // Ctrl+B shows / hides the sidebar (app-only, not a global hotkey).
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::B)) {
                let open = !self.sidebar_open;
                if self.fullscreen {
                    self.sidebar_open = open; // just for now; fullscreen exit restores the saved state
                } else {
                    self.set_sidebar(ctx, open);
                }
            }
        }
        // Esc closes an open settings section (after fullscreen had its turn above).
        if self.sidebar_open && !(esc && self.fullscreen) {
            if let Some(s) = self.settings.as_mut() {
                let outcome = s.poll(ctx);
                self.handle_settings(outcome);
            }
        }

        // The window manager can enter or leave fullscreen on its own (niri's fullscreen key,
        // moving the window): follow it, or the title bar stays hidden on a normal window.
        let reported = ctx.input(|i| i.viewport().fullscreen);
        if reported.is_some() && reported != self.seen_fullscreen {
            self.seen_fullscreen = reported;
            self.follow_fullscreen(reported == Some(true));
        }

        let app_title_bar = self.cfg.title_bar.app_drawn();
        #[cfg(target_os = "linux")]
        if self.native_decorations == app_title_bar {
            self.native_decorations = !app_title_bar;
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(!app_title_bar));
        }

        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        if self.fullscreen || !app_title_bar {
            super::frame::set_layout(0, 0, false);
        } else {
            egui::TopBottomPanel::top("title_bar")
                .exact_height(TITLE_BAR_H)
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(theme::BASE))
                .show(ctx, |ui| self.title_bar(ui));
            // Tell the window procedure where the drag area and the buttons are.
            let ppp = ctx.pixels_per_point();
            super::frame::set_layout(
                (TITLE_BAR_H * ppp).round() as i32,
                (3.0 * CAPTION_BUTTON_W * ppp).round() as i32,
                !maximized,
            );
        }
        if !self.fullscreen {
            egui::TopBottomPanel::bottom("status")
                .frame(
                    egui::Frame::new()
                        .fill(theme::SURFACE)
                        .inner_margin(Margin::symmetric(12, 5))
                        .stroke(Stroke::new(1.0_f32, theme::DIVIDER)),
                )
                .show(ctx, |ui| self.status_bar(ui));
        }
        let video = egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::VOID))
            .show(ctx, |ui| self.preview(ui))
            .response
            .rect;

        // The sidebar floats over the video, so showing or hiding it never moves the picture.
        if self.sidebar_open {
            self.sidebar(ctx, video);
        } else {
            self.show_sidebar_button(ctx, video);
        }

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

        // Our 1 px window border (the native one is gone); none when maximized or fullscreen.
        if !self.fullscreen && !maximized && app_title_bar {
            #[cfg(target_os = "linux")]
            super::frame::resize_edges(ctx);
            ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("window_border"))).rect_stroke(
                ctx.screen_rect(),
                0,
                Stroke::new(1.0_f32, theme::LINE_STRONG),
                egui::StrokeKind::Inside,
            );
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
