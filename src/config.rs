use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
    Av1,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::Hevc, Codec::H264, Codec::Av1];

    pub fn nvenc_name(self) -> &'static str {
        match self {
            Codec::Hevc => "hevc_nvenc",
            Codec::H264 => "h264_nvenc",
            Codec::Av1 => "av1_nvenc",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Codec::Hevc => "HEVC",
            Codec::H264 => "H.264",
            Codec::Av1 => "AV1",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Codec::Hevc => "HEVC (H.265)",
            Codec::H264 => "H.264 (most compatible)",
            Codec::Av1 => "AV1 (RTX 40+)",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentMode {
    /// Low latency, no tearing (default).
    Mailbox,
    /// Lowest latency, may tear.
    Immediate,
    /// Classic vsync.
    Fifo,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ModePref {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Friendly-name substring of the capture device; None = auto-detect Elgato.
    pub video_device: Option<String>,
    /// Preferred capture mode; None = best available (up to 4K, highest fps, NV12 preferred).
    pub video_mode: Option<ModePref>,
    /// Audio capture endpoint name substring; None = auto-match the capture card.
    pub audio_input: Option<String>,
    /// Audio render endpoint for passthrough; None = Windows default output.
    pub audio_output: Option<String>,
    pub passthrough_enabled: bool,
    pub volume: f32,
    pub muted: bool,

    pub codec: Codec,
    /// 0 = automatic based on resolution/fps.
    pub bitrate_mbps: u32,
    pub replay_enabled: bool,
    pub replay_seconds: u32,
    pub output_dir: PathBuf,

    pub hotkey_save_replay: String,
    pub hotkey_toggle_record: String,
    pub hotkey_screenshot: String,

    pub windows_notifications: bool,
    pub present_mode: PresentMode,
}

impl Default for Config {
    fn default() -> Self {
        let output_dir = directories::UserDirs::new()
            .and_then(|d| d.video_dir().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Passthrify");
        Self {
            video_device: None,
            video_mode: None,
            audio_input: None,
            audio_output: None,
            passthrough_enabled: true,
            volume: 1.0,
            muted: false,
            codec: Codec::Hevc,
            bitrate_mbps: 0,
            replay_enabled: true,
            replay_seconds: 60,
            output_dir,
            hotkey_save_replay: "Alt+F10".into(),
            hotkey_toggle_record: "Alt+F9".into(),
            hotkey_screenshot: "Alt+F1".into(),
            windows_notifications: true,
            present_mode: PresentMode::Mailbox,
        }
    }
}

impl Config {
    pub fn data_dir() -> PathBuf {
        directories::ProjectDirs::from("", "", "Passthrify")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn path() -> PathBuf {
        Self::data_dir().join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!("config {} is invalid ({e}); using defaults", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    tracing::error!("saving config to {}: {e}", path.display());
                }
            }
            Err(e) => tracing::error!("serializing config: {e}"),
        }
    }

    /// Bitrate in bits/s for the given capture mode.
    pub fn bitrate_bps(&self, width: u32, height: u32, fps: f64) -> u64 {
        if self.bitrate_mbps > 0 {
            return self.bitrate_mbps as u64 * 1_000_000;
        }
        // ~0.1 bits/pixel for HEVC/AV1 (4K60 ≈ 50 Mbps), 1.5x for H.264.
        let bpp = match self.codec {
            Codec::H264 => 0.15,
            _ => 0.10,
        };
        let bps = width as f64 * height as f64 * fps.max(1.0) * bpp;
        (bps as u64).clamp(12_000_000, 150_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let cfg: Config = toml::from_str("replay_seconds = 30").unwrap();
        assert_eq!(cfg.replay_seconds, 30);
        assert_eq!(cfg.codec, Codec::Hevc);
    }

    #[test]
    fn auto_bitrate() {
        let cfg = Config::default();
        let b = cfg.bitrate_bps(3840, 2160, 60.0);
        assert!((45_000_000..=55_000_000).contains(&b), "{b}");
        assert_eq!(cfg.bitrate_bps(1280, 720, 30.0), 12_000_000);
    }
}
