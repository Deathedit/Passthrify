//! Settings window: sidebar sections + cards, styled after the design board.

use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, FontId, Margin, Rect, RichText, Stroke, UiBuilder, Vec2};

use super::app::hotkey_parts;
use super::pickers;
use super::theme::{self, icons, Kind};
use crate::capture::video_mf::VideoMode;
use crate::color::{ColorInfo, RangePref};
use crate::config::{Chroma, Codec, Config, PresentMode};
use crate::updater::{self, UpdateChannel, UpdateState, Updater};

pub enum SettingsOutcome {
    Open,
    Apply(Box<Config>),
    OpenFolder { screenshots: bool },
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Capture,
    Audio,
    Replay,
    Hotkeys,
    Display,
    Updates,
}

impl Section {
    const ALL: [Section; 6] =
        [Section::Capture, Section::Audio, Section::Replay, Section::Hotkeys, Section::Display, Section::Updates];
    fn label(self) -> &'static str {
        match self {
            Section::Capture => "Capture",
            Section::Audio => "Audio",
            Section::Replay => "Replay and recording",
            Section::Hotkeys => "Hotkeys",
            Section::Display => "Display",
            Section::Updates => "Updates",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HotkeyField {
    SaveReplay,
    Record,
    Screenshot,
}

pub struct SettingsUi {
    pub section: Section,
    draft: Config,
    /// Mode preference that is currently running (to tell whether `actual` chroma applies).
    applied_mode: Option<crate::config::ModePref>,
    video_devices: Vec<String>,
    audio_inputs: Vec<String>,
    audio_outputs: Vec<String>,
    capture: Option<HotkeyField>,
    /// Why self-update can't install here (dev build, read-only folder), if so.
    install_blocker: Option<String>,
}

const W: f32 = 860.0;
const H: f32 = 600.0;
const HEADER: f32 = 44.0;
const FOOTER: f32 = 56.0;
const SIDEBAR: f32 = 210.0;

impl SettingsUi {
    pub fn new(cfg: &Config) -> Self {
        let (video_devices, audio_inputs, audio_outputs) = crate::capture::enumerate_all();
        Self {
            section: Section::Capture,
            draft: cfg.clone(),
            applied_mode: cfg.video_mode,
            video_devices,
            audio_inputs,
            audio_outputs,
            capture: None,
            install_blocker: updater::install_blocker(),
        }
    }

    /// Keeps the draft in step with changes made from the toolbar while the window is open.
    pub fn sync_capture(&mut self, cfg: &Config) {
        self.draft.video_mode = cfg.video_mode;
        self.draft.codec = cfg.codec;
        self.draft.chroma = cfg.chroma;
        self.applied_mode = cfg.video_mode;
    }

    pub fn is_capturing_hotkey(&self) -> bool {
        self.capture.is_some()
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        modes: &[VideoMode],
        current: Option<VideoMode>,
        actual_chroma: Option<Chroma>,
        color: Option<ColorInfo>,
        hotkey_errors: &[String],
        updater: &Arc<Updater>,
        recording: bool,
    ) -> SettingsOutcome {
        self.poll_hotkey_capture(ctx);
        let mut outcome = SettingsOutcome::Open;

        egui::Window::new("Settings")
            .id(egui::Id::new("settings_window"))
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([W, H])
            .frame(
                egui::Frame::new()
                    .fill(theme::BASE)
                    .stroke(Stroke::new(1.0_f32, theme::LINE))
                    .corner_radius(14)
                    .shadow(ctx.style().visuals.window_shadow)
                    .inner_margin(0),
            )
            .show(ctx, |ui| {
                ui.set_min_size(Vec2::new(W, H));
                let full = Rect::from_min_size(ui.max_rect().min, Vec2::new(W, H));
                let header = Rect::from_min_size(full.min, Vec2::new(W, HEADER));
                let footer = Rect::from_min_max(egui::pos2(full.left(), full.bottom() - FOOTER), full.max);
                let sidebar = Rect::from_min_max(
                    egui::pos2(full.left(), header.bottom()),
                    egui::pos2(full.left() + SIDEBAR, footer.top()),
                );
                let content = Rect::from_min_max(egui::pos2(sidebar.right(), header.bottom()), egui::pos2(full.right(), footer.top()));

                let p = ui.painter().clone();
                p.line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0_f32, theme::DIVIDER));
                p.rect_filled(sidebar, 0, theme::SURFACE);
                p.line_segment([sidebar.right_top(), sidebar.right_bottom()], Stroke::new(1.0_f32, theme::DIVIDER));
                p.rect_filled(footer, egui::CornerRadius { nw: 0, ne: 0, sw: 14, se: 14 }, theme::SURFACE);
                p.line_segment([footer.left_top(), footer.right_top()], Stroke::new(1.0_f32, theme::DIVIDER));

                // Header.
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(header.shrink2(Vec2::new(14.0, 0.0)))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.add(theme::mark_image(16.0));
                        ui.label(RichText::new("Settings").font(FontId::new(12.0, theme::medium())).color(theme::TEXT2));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::icon_button(ui, icons::CLOSE, "Close without saving", false).clicked() {
                                outcome = SettingsOutcome::Close;
                            }
                        });
                    },
                );

                // Sidebar.
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(sidebar.shrink2(Vec2::new(12.0, 20.0)))
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for s in Section::ALL {
                            if nav_item(ui, s.label(), self.section == s).clicked() {
                                self.section = s;
                                self.capture = None;
                            }
                        }
                    },
                );
                p.text(
                    egui::pos2(sidebar.left() + 24.0, sidebar.bottom() - 18.0),
                    egui::Align2::LEFT_CENTER,
                    format!("Passthrify {}", env!("CARGO_PKG_VERSION")),
                    theme::mono(11.0),
                    theme::TEXT3,
                );

                // Content.
                ui.scope_builder(UiBuilder::new().max_rect(content.shrink(24.0)), |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        match self.section {
                            Section::Capture => self.capture_card(ui, modes, current, actual_chroma, color),
                            Section::Audio => self.audio_card(ui),
                            Section::Replay => {
                                if let Some(screenshots) = self.replay_card(ui, current, actual_chroma) {
                                    outcome = SettingsOutcome::OpenFolder { screenshots };
                                }
                            }
                            Section::Hotkeys => self.hotkeys_card(ui, hotkey_errors),
                            Section::Display => self.display_card(ui),
                            Section::Updates => self.updates_card(ui, updater, recording),
                        }
                    });
                });

                // Footer.
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(footer.shrink2(Vec2::new(24.0, 12.0)))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        if theme::button(ui, Kind::Ghost, None, "Reset to defaults", None).clicked() {
                            let keep_device = (self.draft.video_device.clone(), self.draft.audio_input.clone());
                            self.draft = Config::default();
                            (self.draft.video_device, self.draft.audio_input) = keep_device;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::button(ui, Kind::Primary, None, "Apply", None).clicked() {
                                outcome = SettingsOutcome::Apply(Box::new(self.draft.clone()));
                            }
                            if theme::button(ui, Kind::Ghost, None, "Cancel", None).clicked() {
                                outcome = SettingsOutcome::Close;
                            }
                        });
                    },
                );
            });

        if self.capture.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            outcome = SettingsOutcome::Close;
        }
        outcome
    }

    fn capture_card(
        &mut self,
        ui: &mut egui::Ui,
        modes: &[VideoMode],
        current: Option<VideoMode>,
        actual: Option<Chroma>,
        color: Option<ColorInfo>,
    ) {
        let d = &mut self.draft;
        let applied = self.applied_mode;
        let devices = &self.video_devices;
        theme::card(ui, "Capture", |ui| {
            let w = ui.available_width();
            theme::field_label(ui, "Device");
            optional_combo(ui, "s_video_dev", &mut d.video_device, devices, "Auto (Elgato)", w);
            ui.add_space(6.0);

            theme::field_label(ui, "Chroma");
            let actual = actual.filter(|_| d.video_mode == applied);
            pickers::chroma_picker(ui, "settings", &mut d.chroma, &mut d.video_mode, &mut d.codec, modes, actual, w);
            ui.add_space(6.0);

            ui.columns(2, |cols| {
                theme::field_label(&mut cols[0], "Resolution");
                let cw = cols[0].available_width();
                pickers::resolution_picker(&mut cols[0], "settings", &mut d.video_mode, d.chroma, modes, current, cw);
                theme::field_label(&mut cols[1], "Frame rate");
                let cw = cols[1].available_width();
                pickers::rate_picker(&mut cols[1], "settings", &mut d.video_mode, d.chroma, modes, current, cw);
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new("Resolution and frame-rate lists only show modes that deliver the selected chroma.")
                    .size(12.0)
                    .color(theme::TEXT3),
            );
            ui.add_space(6.0);

            theme::field_label(ui, "Input range");
            egui::ComboBox::from_id_salt("s_range").selected_text(d.color_range.label()).width(w).show_ui(ui, |ui| {
                for r in RangePref::ALL {
                    ui.selectable_value(&mut d.color_range, r, r.label());
                }
            });
            let detected = color.map(|c| c.label()).unwrap_or_else(|| "unknown".into());
            ui.label(
                RichText::new(format!(
                    "Now: {detected}. Override only if blacks look grey (pick Full) or crushed (pick Limited)."
                ))
                .size(12.0)
                .color(theme::TEXT3),
            );
        });
    }

    fn audio_card(&mut self, ui: &mut egui::Ui) {
        let d = &mut self.draft;
        let (inputs, outputs) = (&self.audio_inputs, &self.audio_outputs);
        theme::card(ui, "Audio", |ui| {
            let w = ui.available_width();
            theme::field_label(ui, "Capture device");
            optional_combo(ui, "s_audio_in", &mut d.audio_input, inputs, "Auto (match capture card)", w);
            ui.add_space(8.0);
            theme::toggle_row(ui, "Pass audio through to speakers", &mut d.passthrough_enabled);
            ui.add_space(8.0);
            ui.add_enabled_ui(d.passthrough_enabled, |ui| {
                theme::field_label(ui, "Output device");
                optional_combo(ui, "s_audio_out", &mut d.audio_output, outputs, "Windows default", w);
            });
        });
    }

    /// Returns Some(screenshots?) when a folder's "Open" was clicked.
    fn replay_card(&mut self, ui: &mut egui::Ui, current: Option<VideoMode>, actual: Option<Chroma>) -> Option<bool> {
        let d = &mut self.draft;
        let mut open_folder = None;
        theme::card(ui, "Replay and recording", |ui| {
            theme::toggle_row(ui, "Instant replay (rolling buffer in RAM)", &mut d.replay_enabled);
            ui.add_space(10.0);

            ui.add_enabled_ui(d.replay_enabled, |ui| {
                // Estimate RAM from the mode that will run.
                let (w, h, fps) = match (d.video_mode, current) {
                    (Some(p), _) => (p.width, p.height, p.fps),
                    (None, Some(m)) => (m.width, m.height, m.fps()),
                    _ => (3840, 2160, 60.0),
                };
                let chroma = if d.video_mode == self.applied_mode { actual.unwrap_or(d.chroma) } else { d.chroma };
                let mb = d.bitrate_bps(w, h, fps, chroma) as f64 * d.replay_seconds as f64 / 8.0 / 1_048_576.0;
                ui.horizontal(|ui| {
                    theme::field_label(ui, "Buffer length");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("· about {mb:.0} MB RAM")).font(theme::mono(12.0)).color(theme::TEXT3));
                        ui.label(RichText::new(format!("{} s", d.replay_seconds)).font(theme::mono(12.0)).color(theme::TEXT));
                    });
                });
                ui.scope(|ui| {
                    ui.spacing_mut().slider_width = ui.available_width();
                    ui.spacing_mut().interact_size.y = 18.0; // knob size follows this
                    ui.add(egui::Slider::new(&mut d.replay_seconds, 10..=600).show_value(false).step_by(5.0));
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("10 s").font(theme::mono(11.0)).color(theme::TEXT3));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("600 s").font(theme::mono(11.0)).color(theme::TEXT3));
                    });
                });
            });
            ui.add_space(10.0);

            theme::field_label(ui, "Codec");
            let chroma = d.chroma;
            theme::segmented(
                ui,
                &mut d.codec,
                &[
                    (Codec::Hevc, "HEVC", true),
                    (Codec::H264, "H.264", true),
                    (Codec::Av1, "AV1", Codec::Av1.supports(chroma)),
                ],
            );
            if !Codec::Av1.supports(chroma) {
                ui.label(RichText::new(format!("AV1 is 4:2:0 only on NVENC (chroma is {}).", chroma.label())).size(12.0).color(theme::TEXT3));
            }
            ui.add_space(10.0);

            theme::field_label(ui, "Bitrate");
            ui.horizontal(|ui| {
                let mut manual = d.bitrate_mbps > 0;
                ui.allocate_ui(Vec2::new(ui.available_width() - 120.0, 36.0), |ui| {
                    if theme::segmented(ui, &mut manual, &[(false, "Auto", true), (true, "Manual", true)]) {
                        d.bitrate_mbps = if manual { 50 } else { 0 };
                    }
                });
                if manual {
                    ui.add(
                        egui::DragValue::new(&mut d.bitrate_mbps)
                            .range(5..=150)
                            .speed(0.5)
                            .suffix(" Mbps")
                            .custom_formatter(|v, _| format!("{v:.0}")),
                    );
                } else {
                    let auto = d.bitrate_bps(3840, 2160, 60.0, Chroma::S420) / 1_000_000;
                    ui.label(RichText::new(format!("4K60 ≈ {auto} Mbps")).font(theme::mono(12.0)).color(theme::TEXT3));
                }
            });
            ui.add_space(10.0);

            theme::field_label(ui, "Video folder (replays and recordings)");
            if folder_row(ui, &mut d.output_dir) {
                open_folder = Some(false);
            }
            ui.add_space(6.0);
            theme::field_label(ui, "Screenshot folder");
            if folder_row(ui, &mut d.screenshot_dir) {
                open_folder = Some(true);
            }
        });
        open_folder
    }

    fn hotkeys_card(&mut self, ui: &mut egui::Ui, hotkey_errors: &[String]) {
        let capture = self.capture;
        let mut start: Option<Option<HotkeyField>> = None;
        let d = &mut self.draft;
        theme::card(ui, "Hotkeys", |ui| {
            ui.label(RichText::new("Global: they work while a game or another window has focus.").size(12.0).color(theme::TEXT3));
            ui.add_space(6.0);
            let rows = [
                (HotkeyField::SaveReplay, "Save replay", &mut d.hotkey_save_replay),
                (HotkeyField::Record, "Start / stop recording", &mut d.hotkey_toggle_record),
                (HotkeyField::Screenshot, "Screenshot", &mut d.hotkey_screenshot),
            ];
            for (i, (field, label, value)) in rows.into_iter().enumerate() {
                if i > 0 {
                    let y = ui.cursor().top();
                    let r = ui.max_rect();
                    ui.painter().line_segment([egui::pos2(r.left(), y), egui::pos2(r.right(), y)], Stroke::new(1.0_f32, theme::DIVIDER));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.set_min_height(32.0);
                    ui.label(RichText::new(label).color(theme::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if capture == Some(field) {
                            if theme::button(ui, Kind::Ghost, None, "Cancel", None).clicked() {
                                start = Some(None);
                            }
                            egui::Frame::new()
                                .fill(theme::BASE)
                                .stroke(Stroke::new(1.0_f32, theme::SIGNAL))
                                .corner_radius(4)
                                .inner_margin(Margin::symmetric(8, 3))
                                .show(ui, |ui| {
                                    ui.label(RichText::new("Press keys… (Esc cancels)").font(theme::mono(11.0)).color(theme::SIGNAL))
                                });
                        } else {
                            let change = ui.add(
                                egui::Label::new(RichText::new("Change").font(FontId::new(12.0, theme::medium())).color(theme::SIGNAL))
                                    .sense(egui::Sense::click()),
                            );
                            if change.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                start = Some(Some(field));
                            }
                            let clear = ui.add(
                                egui::Button::image(theme::icon_image(icons::CLOSE, 12.0, theme::TEXT3))
                                    .frame(false)
                                    .min_size(Vec2::splat(20.0)),
                            );
                            if clear.on_hover_text("Clear (no hotkey)").clicked() {
                                value.clear();
                            }
                            let parts = hotkey_parts(value);
                            if parts.is_empty() {
                                ui.label(RichText::new("none").font(theme::mono(11.0)).color(theme::TEXT3));
                            }
                            for part in parts.iter().rev() {
                                theme::key_chip(ui, part);
                            }
                        }
                    });
                });
                ui.add_space(4.0);
            }
            for e in hotkey_errors {
                ui.label(RichText::new(format!("Problem: {e}")).size(12.0).color(theme::REC));
            }
        });
        if let Some(s) = start {
            self.capture = s;
        }
    }

    fn display_card(&mut self, ui: &mut egui::Ui) {
        let d = &mut self.draft;
        theme::card(ui, "Display", |ui| {
            theme::toggle_row(ui, "Windows notification when a clip is saved", &mut d.windows_notifications);
            ui.add_space(10.0);
            theme::field_label(ui, "Presentation");
            let w = ui.available_width();
            egui::ComboBox::from_id_salt("s_present").selected_text(present_label(d.present_mode)).width(w).show_ui(ui, |ui| {
                for p in [PresentMode::Mailbox, PresentMode::Immediate, PresentMode::Fifo] {
                    ui.selectable_value(&mut d.present_mode, p, present_label(p));
                }
            });
            ui.label(RichText::new("Takes effect after restarting Passthrify.").size(12.0).color(theme::TEXT3));
        });
    }

    fn updates_card(&mut self, ui: &mut egui::Ui, updater: &Arc<Updater>, recording: bool) {
        let d = &mut self.draft;
        let blocker = self.install_blocker.clone();
        theme::card(ui, "Updates", |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Installed version").color(theme::TEXT2));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(updater::current_version().to_string()).font(theme::mono(13.0)).color(theme::TEXT));
                });
            });
            ui.add_space(10.0);

            theme::field_label(ui, "Channel");
            let repo = updater::repo(d.update_repo.as_deref());
            if theme::segmented(
                ui,
                &mut d.update_channel,
                &[(UpdateChannel::Stable, "Stable", true), (UpdateChannel::PreRelease, "Pre-release", true)],
            ) {
                // Re-check right away so the card reflects the new channel.
                updater.start_check(repo.clone(), d.update_channel, Duration::ZERO);
            }
            let hint = match d.update_channel {
                UpdateChannel::Stable => "Tested releases only.",
                UpdateChannel::PreRelease => "Betas and release candidates as well: new features sooner, rougher edges.",
            };
            ui.label(RichText::new(hint).size(12.0).color(theme::TEXT3));
            ui.add_space(8.0);
            theme::toggle_row(ui, "Check for updates when Passthrify starts", &mut d.check_updates);
            ui.add_space(12.0);

            // Status + actions.
            if repo.is_none() {
                ui.label(
                    RichText::new("This build has no update source, so it can't check for updates. Official releases include one.")
                        .color(theme::TEXT2),
                );
                ui.label(
                    RichText::new("To point a self-built copy at a repo, set update_repo = \"owner/name\" in config.toml.")
                        .size(11.0)
                        .color(theme::TEXT3),
                );
                return;
            }
            let state = updater.state();
            let check_now = |ui: &mut egui::Ui| {
                if theme::button(ui, Kind::Secondary, None, "Check now", None).clicked() {
                    updater.start_check(repo.clone(), d.update_channel, Duration::ZERO);
                }
            };
            match &state {
                UpdateState::Idle => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Not checked yet.").color(theme::TEXT2));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), check_now);
                    });
                }
                UpdateState::Checking => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Checking GitHub…").color(theme::TEXT2));
                    });
                }
                UpdateState::UpToDate { checked } => {
                    ui.horizontal(|ui| {
                        ui.add(theme::icon_image(icons::CHECK, 16.0, theme::SUCCESS));
                        ui.label(
                            RichText::new(format!("You're up to date ({} channel, checked {checked}).", d.update_channel.label()))
                                .color(theme::TEXT),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), check_now);
                    });
                }
                UpdateState::Error(e) => {
                    ui.horizontal(|ui| {
                        ui.add(theme::icon_image(icons::ALERT, 16.0, theme::REC));
                        ui.add(egui::Label::new(RichText::new(e).color(theme::REC)).wrap());
                    });
                    ui.horizontal(|ui| check_now(ui));
                }
                UpdateState::Available(rel) | UpdateState::Downloading { release: rel, .. } => {
                    let downloading = matches!(state, UpdateState::Downloading { .. });
                    ui.horizontal(|ui| {
                        ui.add(theme::icon_image(icons::DOWNLOAD, 16.0, theme::SIGNAL));
                        ui.label(
                            RichText::new(format!("Passthrify {} is available", rel.tag))
                                .font(FontId::new(14.0, theme::semibold()))
                                .color(theme::TEXT),
                        );
                        if rel.prerelease {
                            egui::Frame::new()
                                .fill(theme::HOVER)
                                .stroke(Stroke::new(1.0_f32, theme::WARNING))
                                .corner_radius(4)
                                .inner_margin(Margin::symmetric(6, 1))
                                .show(ui, |ui| ui.label(RichText::new("PRE-RELEASE").font(theme::mono(10.0)).color(theme::WARNING)));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let link = ui.add(
                                egui::Label::new(RichText::new("Release page").font(FontId::new(12.0, theme::medium())).color(theme::SIGNAL))
                                    .sense(egui::Sense::click()),
                            );
                            if link.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab(&rel.url));
                            }
                        });
                    });
                    ui.add_space(6.0);
                    egui::Frame::new()
                        .fill(theme::BASE)
                        .stroke(Stroke::new(1.0_f32, theme::LINE))
                        .corner_radius(8)
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical().max_height(170.0).auto_shrink([false, true]).show(ui, |ui| {
                                release_notes(ui, &rel.notes);
                            });
                        });
                    ui.add_space(8.0);
                    if let UpdateState::Downloading { progress, .. } = state {
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .text(format!("Downloading {:.0}%", progress * 100.0))
                                .fill(theme::SIGNAL),
                        );
                    } else {
                        let reason = if recording { Some("Stop the recording first.".to_string()) } else { blocker.clone() };
                        ui.horizontal(|ui| {
                            let size_mb = rel.zip_size as f64 / 1_048_576.0;
                            let install = ui.add_enabled_ui(reason.is_none() && !downloading, |ui| {
                                theme::button(ui, Kind::Primary, Some(icons::DOWNLOAD), "Install and restart", Some(&format!("{size_mb:.0} MB")))
                            });
                            if install.inner.clicked() {
                                updater.start_install(rel.clone());
                            }
                            if let Some(r) = &reason {
                                ui.label(RichText::new(r).size(12.0).color(theme::TEXT3));
                            }
                        });
                    }
                }
                UpdateState::Restarting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Installing — Passthrify will restart…").color(theme::TEXT2));
                    });
                }
            }

            ui.add_space(10.0);
            if let Some(r) = &repo {
                ui.label(
                    RichText::new(format!("Source: github.com/{r} · downloads are verified with SHA-256"))
                        .size(11.0)
                        .color(theme::TEXT3),
                );
            }
        });
    }

    /// While a "Change" is active, the next key combination becomes the hotkey.
    fn poll_hotkey_capture(&mut self, ctx: &egui::Context) {
        let Some(field) = self.capture else { return };
        let pressed = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } => Some((*key, *modifiers)),
                _ => None,
            })
        });
        let Some((key, m)) = pressed else { return };
        if key == egui::Key::Escape && !m.any() {
            self.capture = None;
            return;
        }
        let Some(combo) = hotkey_string(key, m) else { return };
        let target = match field {
            HotkeyField::SaveReplay => &mut self.draft.hotkey_save_replay,
            HotkeyField::Record => &mut self.draft.hotkey_toggle_record,
            HotkeyField::Screenshot => &mut self.draft.hotkey_screenshot,
        };
        *target = combo;
        self.capture = None;
    }
}

/// egui key + modifiers -> global-hotkey string ("Alt+F10", "Ctrl+Shift+KeyR").
/// Plain letters/digits need a modifier so typing in a game doesn't trigger them.
fn hotkey_string(key: egui::Key, m: egui::Modifiers) -> Option<String> {
    let name = key.name();
    let code = if name.len() == 1 {
        let c = name.chars().next()?;
        if c.is_ascii_alphabetic() {
            format!("Key{}", c.to_ascii_uppercase())
        } else if c.is_ascii_digit() {
            format!("Digit{c}")
        } else {
            return None;
        }
    } else if name.starts_with('F') && name[1..].parse::<u8>().is_ok() {
        name.to_string()
    } else {
        match name {
            "Up" => "ArrowUp".into(),
            "Down" => "ArrowDown".into(),
            "Left" => "ArrowLeft".into(),
            "Right" => "ArrowRight".into(),
            "Space" | "Home" | "End" | "PageUp" | "PageDown" | "Insert" | "Delete" | "Tab" | "Enter" | "Backspace" => {
                name.into()
            }
            _ => return None,
        }
    };
    let is_fkey = code.starts_with('F');
    if !is_fkey && !(m.ctrl || m.alt || m.shift) {
        return None;
    }
    let mut parts = Vec::new();
    if m.ctrl {
        parts.push("Ctrl".to_string());
    }
    if m.alt {
        parts.push("Alt".to_string());
    }
    if m.shift {
        parts.push("Shift".to_string());
    }
    parts.push(code);
    let s = parts.join("+");
    use std::str::FromStr;
    global_hotkey::hotkey::HotKey::from_str(&s).ok().map(|_| s)
}

/// Minimal Markdown for release notes: headings, bullets, `code`, **bold** markers stripped.
fn release_notes(ui: &mut egui::Ui, md: &str) {
    if md.trim().is_empty() {
        ui.label(RichText::new("No release notes.").color(theme::TEXT3));
        return;
    }
    ui.spacing_mut().item_spacing.y = 3.0;
    for line in md.lines() {
        let t = line.trim_end();
        let clean = |s: &str| s.replace("**", "").replace('`', "");
        if t.trim().is_empty() {
            ui.add_space(4.0);
        } else if let Some(h) = t.trim_start().strip_prefix('#') {
            let h = h.trim_start_matches('#').trim();
            ui.label(RichText::new(clean(h)).font(FontId::new(13.0, theme::semibold())).color(theme::TEXT));
        } else if let Some(b) = t.trim_start().strip_prefix("- ").or_else(|| t.trim_start().strip_prefix("* ")) {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new("•").color(theme::SIGNAL));
                ui.add(egui::Label::new(RichText::new(clean(b)).size(12.5).color(theme::TEXT2)).wrap());
            });
        } else {
            ui.add(egui::Label::new(RichText::new(clean(t)).size(12.5).color(theme::TEXT2)).wrap());
        }
    }
}

/// Path text field + "Open" button. Returns true when Open was clicked.
fn folder_row(ui: &mut egui::Ui, path: &mut std::path::PathBuf) -> bool {
    ui.horizontal(|ui| {
        let mut dir = path.to_string_lossy().into_owned();
        let edit = egui::TextEdit::singleline(&mut dir)
            .font(theme::mono(13.0))
            .desired_width(ui.available_width() - 80.0)
            .margin(Margin::symmetric(10, 7));
        if ui.add(edit).changed() {
            *path = dir.into();
        }
        theme::button(ui, Kind::Secondary, Some(icons::FOLDER), "Open", None).clicked()
    })
    .inner
}

fn nav_item(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), egui::Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 6, theme::HOVER);
        let bar = Rect::from_center_size(egui::pos2(rect.left() + 13.5, rect.center().y), Vec2::new(3.0, 14.0));
        p.rect_filled(bar, 2, theme::SIGNAL);
    } else if resp.hovered() {
        p.rect_filled(rect, 6, theme::RAISED);
    }
    let (font, color) = if selected {
        (FontId::new(13.0, theme::semibold()), theme::TEXT)
    } else {
        (FontId::new(13.0, theme::medium()), theme::TEXT2)
    };
    let x = rect.left() + if selected { 23.0 } else { 12.0 };
    p.text(egui::pos2(x, rect.center().y), egui::Align2::LEFT_CENTER, label, font, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn present_label(p: PresentMode) -> &'static str {
    match p {
        PresentMode::Mailbox => "Low latency (mailbox)",
        PresentMode::Immediate => "Lowest latency (may tear)",
        PresentMode::Fifo => "VSync",
    }
}

fn optional_combo(ui: &mut egui::Ui, id: &str, value: &mut Option<String>, options: &[String], auto: &str, width: f32) {
    let text = value.clone().unwrap_or_else(|| auto.to_string());
    egui::ComboBox::from_id_salt(id).selected_text(text).width(width).show_ui(ui, |ui| {
        ui.selectable_value(value, None, auto);
        for o in options {
            ui.selectable_value(value, Some(o.clone()), o);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Key, Modifiers};

    #[test]
    fn records_hotkeys() {
        assert_eq!(hotkey_string(Key::F10, Modifiers::ALT).as_deref(), Some("Alt+F10"));
        assert_eq!(hotkey_string(Key::F9, Modifiers::NONE).as_deref(), Some("F9"));
        assert_eq!(hotkey_string(Key::R, Modifiers::CTRL | Modifiers::SHIFT).as_deref(), Some("Ctrl+Shift+KeyR"));
        assert_eq!(hotkey_string(Key::Num1, Modifiers::ALT).as_deref(), Some("Alt+Digit1"));
        assert_eq!(hotkey_string(Key::R, Modifiers::NONE), None, "bare letters would fire while typing");
    }
}
