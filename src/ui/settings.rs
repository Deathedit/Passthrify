use eframe::egui;

use super::pickers;
use crate::capture::video_mf::VideoMode;
use crate::config::{Config, PresentMode};

pub enum SettingsOutcome {
    Open,
    Apply(Box<Config>),
    Close,
}

pub struct SettingsUi {
    draft: Config,
    video_devices: Vec<String>,
    audio_inputs: Vec<String>,
    audio_outputs: Vec<String>,
}

impl SettingsUi {
    pub fn new(cfg: &Config) -> Self {
        let (video_devices, audio_inputs, audio_outputs) = crate::capture::enumerate_all();
        Self { draft: cfg.clone(), video_devices, audio_inputs, audio_outputs }
    }

    /// Keeps the draft in step with changes made from the toolbar while the window is open.
    pub fn sync_capture(&mut self, cfg: &Config) {
        self.draft.video_mode = cfg.video_mode;
        self.draft.codec = cfg.codec;
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        modes: &[VideoMode],
        current: Option<VideoMode>,
        hotkey_errors: &[String],
    ) -> SettingsOutcome {
        let mut outcome = SettingsOutcome::Open;
        let mut open = true;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let d = &mut self.draft;
                egui::Grid::new("settings").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    section(ui, "Capture");
                    ui.label("Video device");
                    optional_combo(ui, "video_dev", &mut d.video_device, &self.video_devices, "Auto (Elgato)");
                    ui.end_row();

                    ui.label("Resolution");
                    pickers::resolution_picker(ui, "settings", &mut d.video_mode, modes, current, 280.0);
                    ui.end_row();

                    ui.label("Frame rate");
                    pickers::rate_picker(ui, "settings", &mut d.video_mode, modes, current, 280.0);
                    ui.end_row();

                    section(ui, "Encoding");
                    ui.label("Codec");
                    pickers::codec_picker(ui, "settings", &mut d.codec, false, 280.0);
                    ui.end_row();
                    ui.label("Bitrate");
                    ui.horizontal(|ui| {
                        let mut auto = d.bitrate_mbps == 0;
                        if ui.checkbox(&mut auto, "Auto").changed() {
                            d.bitrate_mbps = if auto { 0 } else { 50 };
                        }
                        if !auto {
                            ui.add(egui::Slider::new(&mut d.bitrate_mbps, 5..=150).suffix(" Mbps"));
                        }
                    });
                    ui.end_row();

                    section(ui, "Audio");
                    ui.label("Capture audio");
                    optional_combo(ui, "audio_in", &mut d.audio_input, &self.audio_inputs, "Auto (match card)");
                    ui.end_row();
                    ui.label("Passthrough");
                    ui.checkbox(&mut d.passthrough_enabled, "Play capture audio on");
                    ui.end_row();
                    ui.label("Output device");
                    optional_combo(ui, "audio_out", &mut d.audio_output, &self.audio_outputs, "Windows default");
                    ui.end_row();

                    section(ui, "Instant replay & output");
                    ui.label("Instant replay");
                    ui.checkbox(&mut d.replay_enabled, "Keep a rolling buffer in RAM");
                    ui.end_row();
                    ui.label("Replay length");
                    ui.add(egui::Slider::new(&mut d.replay_seconds, 10..=600).suffix(" s"));
                    ui.end_row();
                    ui.label("Output folder");
                    let mut dir = d.output_dir.to_string_lossy().into_owned();
                    if ui.add(egui::TextEdit::singleline(&mut dir).desired_width(280.0)).changed() {
                        d.output_dir = dir.into();
                    }
                    ui.end_row();

                    section(ui, "Hotkeys (e.g. Alt+F10, Ctrl+Shift+KeyR)");
                    ui.label("Save replay");
                    ui.text_edit_singleline(&mut d.hotkey_save_replay);
                    ui.end_row();
                    ui.label("Start/stop recording");
                    ui.text_edit_singleline(&mut d.hotkey_toggle_record);
                    ui.end_row();
                    ui.label("Screenshot");
                    ui.text_edit_singleline(&mut d.hotkey_screenshot);
                    ui.end_row();

                    section(ui, "Display");
                    ui.label("Notifications");
                    ui.checkbox(&mut d.windows_notifications, "Windows toast when a clip is saved");
                    ui.end_row();
                    ui.label("Presentation");
                    egui::ComboBox::from_id_salt("present")
                        .selected_text(present_label(d.present_mode))
                        .show_ui(ui, |ui| {
                            for p in [PresentMode::Mailbox, PresentMode::Immediate, PresentMode::Fifo] {
                                ui.selectable_value(&mut d.present_mode, p, present_label(p));
                            }
                        });
                    ui.end_row();
                });

                for e in hotkey_errors {
                    ui.colored_label(egui::Color32::from_rgb(240, 120, 90), format!("Hotkey problem: {e}"));
                }
                ui.small("Changing presentation takes effect after restarting Passthrify.");
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        outcome = SettingsOutcome::Apply(Box::new(self.draft.clone()));
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = SettingsOutcome::Close;
                    }
                    if ui.button("Defaults").clicked() {
                        self.draft = Config::default();
                    }
                });
            });
        if !open {
            outcome = SettingsOutcome::Close;
        }
        outcome
    }
}

fn present_label(p: PresentMode) -> &'static str {
    match p {
        PresentMode::Mailbox => "Low latency (mailbox)",
        PresentMode::Immediate => "Lowest latency (may tear)",
        PresentMode::Fifo => "VSync",
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    // No add_space here: advancing the cursor inside a Grid panics in egui.
    ui.add(egui::Label::new(egui::RichText::new(title).strong()).extend());
    ui.end_row();
}

fn optional_combo(ui: &mut egui::Ui, id: &str, value: &mut Option<String>, options: &[String], auto: &str) {
    let text = value.clone().unwrap_or_else(|| auto.to_string());
    egui::ComboBox::from_id_salt(id).selected_text(text).width(280.0).show_ui(ui, |ui| {
        ui.selectable_value(value, None, auto);
        for o in options {
            ui.selectable_value(value, Some(o.clone()), o);
        }
    });
}
