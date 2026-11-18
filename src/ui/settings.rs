//! Settings sections, shown in place inside the sidebar: a header with a back arrow (drawn by
//! the sidebar), the section's fields, and a Reset / Cancel / Apply footer. Edits go to a
//! draft that only takes effect on Apply.

use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, FontId, Margin, RichText, Stroke, Vec2};

use super::app::{hotkey_parts, pretty_hotkey};
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
    pub const ALL: [Section; 6] =
        [Section::Capture, Section::Audio, Section::Replay, Section::Hotkeys, Section::Display, Section::Updates];
    pub fn label(self) -> &'static str {
        match self {
            Section::Capture => "Capture",
            Section::Audio => "Audio",
            Section::Replay => "Replay and recording",
            Section::Hotkeys => "Hotkeys",
            Section::Display => "Display",
            Section::Updates => "Updates",
        }
    }
    pub fn icon(self) -> (&'static str, &'static [u8]) {
        match self {
            Section::Capture => icons::CAPTURE,
            Section::Audio => icons::AUDIO,
            Section::Replay => icons::HISTORY,
            Section::Hotkeys => icons::KEYBOARD,
            Section::Display => icons::DISPLAY,
            Section::Updates => icons::DOWNLOAD,
        }
    }
    /// One-line summary of `cfg` for the settings list ("60 s · HEVC").
    pub fn summary(self, cfg: &Config) -> String {
        match self {
            Section::Capture => cfg.video_device.clone().unwrap_or_else(|| "Auto (Elgato)".into()),
            Section::Audio => if cfg.passthrough_enabled { "Passthrough on" } else { "Passthrough off" }.into(),
            Section::Replay if cfg.replay_enabled => format!("{} s · {}", cfg.replay_seconds, cfg.codec.short_label()),
            Section::Replay => "Replay off".into(),
            Section::Hotkeys => pretty_hotkey(&cfg.hotkey_save_replay),
            Section::Display => match cfg.present_mode {
                PresentMode::Mailbox => "Mailbox",
                PresentMode::Immediate => "Immediate",
                PresentMode::Fifo => "VSync",
            }
            .into(),
            Section::Updates => updater::current_version().to_string(),
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
    /// Why self-update cannot install here (dev build, read-only folder), if so.
    install_blocker: Option<String>,
}

/// The section's fields, without a card around them (the sidebar is the frame).
fn section_body<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.spacing_mut().item_spacing.y = 6.0;
    add(ui)
}

impl SettingsUi {
    pub fn new(cfg: &Config, section: Section) -> Self {
        let (video_devices, audio_inputs, audio_outputs) = crate::capture::enumerate_all();
        Self {
            section,
            draft: cfg.clone(),
            applied_mode: cfg.video_mode,
            video_devices,
            audio_inputs,
            audio_outputs,
            capture: None,
            install_blocker: updater::install_blocker(),
        }
    }

    /// Keeps the draft in step with capture changes made from the sidebar's pickers.
    pub fn sync_capture(&mut self, cfg: &Config) {
        self.draft.video_mode = cfg.video_mode;
        self.draft.codec = cfg.codec;
        self.draft.chroma = cfg.chroma;
        self.applied_mode = cfg.video_mode;
    }

    pub fn is_capturing_hotkey(&self) -> bool {
        self.capture.is_some()
    }

    /// Keyboard handling for the open section: hotkey capture, and Esc to cancel.
    pub fn poll(&mut self, ctx: &egui::Context) -> SettingsOutcome {
        if self.capture.is_some() {
            self.poll_hotkey_capture(ctx);
            return SettingsOutcome::Open;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            return SettingsOutcome::Close;
        }
        SettingsOutcome::Open
    }

    /// The open section's fields.
    #[allow(clippy::too_many_arguments)]
    pub fn body(
        &mut self,
        ui: &mut egui::Ui,
        modes: &[VideoMode],
        current: Option<VideoMode>,
        actual_chroma: Option<Chroma>,
        color: Option<ColorInfo>,
        hotkey_errors: &[String],
        updater: &Arc<Updater>,
        recording: bool,
    ) -> SettingsOutcome {
        match self.section {
            Section::Capture => self.capture_card(ui, modes, current, actual_chroma, color, recording),
            Section::Audio => self.audio_card(ui),
            Section::Replay => {
                if let Some(screenshots) = self.replay_card(ui, current, actual_chroma) {
                    return SettingsOutcome::OpenFolder { screenshots };
                }
            }
            Section::Hotkeys => self.hotkeys_card(ui, hotkey_errors),
            Section::Display => self.display_card(ui),
            Section::Updates => self.updates_card(ui, updater, recording),
        }
        SettingsOutcome::Open
    }

    /// Reset (this section) on the left, Cancel / Apply on the right.
    pub fn footer(&mut self, ui: &mut egui::Ui) -> SettingsOutcome {
        let mut outcome = SettingsOutcome::Open;
        ui.horizontal_centered(|ui| {
            if theme::button(ui, Kind::Ghost, None, "Reset", None).on_hover_text("Reset this section to its defaults").clicked() {
                self.reset_section();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::button(ui, Kind::Primary, None, "Apply", None).clicked() {
                    outcome = SettingsOutcome::Apply(Box::new(self.draft.clone()));
                }
                if theme::button(ui, Kind::Ghost, None, "Cancel", None).clicked() {
                    outcome = SettingsOutcome::Close;
                }
            });
        });
        outcome
    }

    /// Puts this section's fields back to their defaults (devices stay as picked).
    fn reset_section(&mut self) {
        let def = Config::default();
        let d = &mut self.draft;
        match self.section {
            Section::Capture => {
                d.chroma = def.chroma;
                d.video_mode = def.video_mode;
                d.color_range = def.color_range;
            }
            Section::Audio => {
                d.passthrough_enabled = def.passthrough_enabled;
                d.audio_output = def.audio_output;
            }
            Section::Replay => {
                d.replay_enabled = def.replay_enabled;
                d.replay_seconds = def.replay_seconds;
                d.codec = def.codec;
                d.bitrate_mbps = def.bitrate_mbps;
                d.output_dir = def.output_dir;
                d.screenshot_dir = def.screenshot_dir;
            }
            Section::Hotkeys => {
                d.hotkey_save_replay = def.hotkey_save_replay;
                d.hotkey_toggle_record = def.hotkey_toggle_record;
                d.hotkey_screenshot = def.hotkey_screenshot;
                self.capture = None;
            }
            Section::Display => {
                d.windows_notifications = def.windows_notifications;
                d.present_mode = def.present_mode;
            }
            Section::Updates => {
                d.update_channel = def.update_channel;
                d.check_updates = def.check_updates;
            }
        }
    }

    fn capture_card(
        &mut self,
        ui: &mut egui::Ui,
        modes: &[VideoMode],
        current: Option<VideoMode>,
        actual: Option<Chroma>,
        color: Option<ColorInfo>,
        recording: bool,
    ) {
        let d = &mut self.draft;
        let applied = self.applied_mode;
        let devices = &self.video_devices;
        section_body(ui, |ui| {
            let w = ui.available_width();
            theme::field_label(ui, "Device");
            optional_combo(ui, "s_video_dev", &mut d.video_device, devices, "Auto (Elgato)", w);
            ui.add_space(6.0);

            ui.add_enabled_ui(!recording, |ui| {
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
            });
            ui.add_space(4.0);
            let note = if recording {
                "Stop recording to change chroma, resolution or frame rate."
            } else {
                "Resolution and frame-rate lists only show modes that deliver the selected chroma."
            };
            ui.label(RichText::new(note).size(12.0).color(theme::TEXT3));
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
        section_body(ui, |ui| {
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
        section_body(ui, |ui| {
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
        section_body(ui, |ui| {
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
                ui.add_space(6.0);
                // Two lines so long combos fit the sidebar: label + action, then the keys.
                ui.horizontal(|ui| {
                    ui.label(RichText::new(label).color(theme::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (text, next) = if capture == Some(field) { ("Cancel", None) } else { ("Change", Some(field)) };
                        let link = ui.add(
                            egui::Label::new(RichText::new(text).font(FontId::new(12.0, theme::medium())).color(theme::SIGNAL))
                                .sense(egui::Sense::click()),
                        );
                        if link.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            start = Some(next);
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.set_min_height(24.0);
                    if capture == Some(field) {
                        egui::Frame::new()
                            .fill(theme::BASE)
                            .stroke(Stroke::new(1.0_f32, theme::SIGNAL))
                            .corner_radius(4)
                            .inner_margin(Margin::symmetric(8, 3))
                            .show(ui, |ui| {
                                ui.label(RichText::new("Press keys… (Esc cancels)").font(theme::mono(11.0)).color(theme::SIGNAL))
                            });
                    } else {
                        let parts = hotkey_parts(value);
                        if parts.is_empty() {
                            ui.label(RichText::new("none").font(theme::mono(11.0)).color(theme::TEXT3));
                        }
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for part in &parts {
                            theme::key_chip(ui, part);
                        }
                        if !parts.is_empty() {
                            let clear = ui.add(
                                egui::Button::image(theme::icon_image(icons::CLOSE, 12.0, theme::TEXT3))
                                    .frame(false)
                                    .min_size(Vec2::splat(20.0)),
                            );
                            if clear.on_hover_text("Clear (no hotkey)").clicked() {
                                value.clear();
                            }
                        }
                    }
                });
                ui.add_space(6.0);
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
        section_body(ui, |ui| {
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
        section_body(ui, |ui| {
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
                    ui.horizontal_top(|ui| {
                        ui.add(theme::icon_image(icons::CHECK, 16.0, theme::SUCCESS));
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "You're up to date ({} channel, checked {checked}).",
                                    d.update_channel.label()
                                ))
                                .color(theme::TEXT),
                            )
                            .wrap(),
                        );
                    });
                    ui.horizontal(|ui| check_now(ui));
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
                    ui.horizontal_top(|ui| {
                        ui.add(theme::icon_image(icons::DOWNLOAD, 16.0, theme::SIGNAL));
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("Passthrify {} is available", rel.tag))
                                    .font(FontId::new(14.0, theme::semibold()))
                                    .color(theme::TEXT),
                            )
                            .wrap(),
                        );
                    });
                    ui.horizontal(|ui| {
                        if rel.prerelease {
                            egui::Frame::new()
                                .fill(theme::HOVER)
                                .stroke(Stroke::new(1.0_f32, theme::WARNING))
                                .corner_radius(4)
                                .inner_margin(Margin::symmetric(6, 1))
                                .show(ui, |ui| ui.label(RichText::new("PRE-RELEASE").font(theme::mono(10.0)).color(theme::WARNING)));
                        }
                        let link = ui.add(
                            egui::Label::new(RichText::new("Release page").font(FontId::new(12.0, theme::medium())).color(theme::SIGNAL))
                                .sense(egui::Sense::click()),
                        );
                        if link.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(&rel.url));
                        }
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
                        // Text above a thin bar: text on the cyan fill itself would be unreadable.
                        let total_mb = rel.zip_size as f64 / 1_048_576.0;
                        let done_mb = total_mb * progress as f64;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Downloading…").size(12.0).color(theme::TEXT2));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("{done_mb:.0} / {total_mb:.0} MB · {:.0}%", progress * 100.0))
                                        .font(theme::mono(11.0))
                                        .color(theme::TEXT),
                                );
                            });
                        });
                        let (bar, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), egui::Sense::hover());
                        ui.painter().rect_filled(bar, 3, theme::LINE);
                        let mut fill = bar;
                        fill.set_width(bar.width() * progress.clamp(0.0, 1.0));
                        ui.painter().rect_filled(fill, 3, theme::SIGNAL);
                    } else {
                        let reason = if recording { Some("Stop the recording first.".to_string()) } else { blocker.clone() };
                        let size_mb = rel.zip_size as f64 / 1_048_576.0;
                        let install = ui.add_enabled_ui(reason.is_none() && !downloading, |ui| {
                            theme::button(ui, Kind::Primary, Some(icons::DOWNLOAD), "Install and restart", Some(&format!("{size_mb:.0} MB")))
                        });
                        if install.inner.clicked() {
                            updater.start_install(rel.clone());
                        }
                        if let Some(r) = &reason {
                            ui.add(egui::Label::new(RichText::new(r).size(12.0).color(theme::TEXT3)).wrap());
                        }
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
            .desired_width(ui.available_width() - 32.0 - ui.spacing().item_spacing.x - 20.0)
            .margin(Margin::symmetric(10, 7));
        if ui.add(edit).changed() {
            *path = dir.into();
        }
        theme::button(ui, Kind::Secondary, Some(icons::FOLDER), "", None).on_hover_text("Open folder").clicked()
    })
    .inner
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
