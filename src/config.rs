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

/// Where the live stream goes.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamService {
    Twitch,
    YouTube,
    /// Any RTMP(S) server (`stream_server`).
    Custom,
}

impl StreamService {
    pub fn label(self) -> &'static str {
        match self {
            StreamService::Twitch => "Twitch",
            StreamService::YouTube => "YouTube",
            StreamService::Custom => "Custom RTMP",
        }
    }

    /// Ingest URL without the key (Custom: the configured server).
    pub fn server(self, custom: &str) -> String {
        match self {
            // Twitch picks the nearest ingest itself for this address.
            StreamService::Twitch => "rtmp://live.twitch.tv/app".into(),
            StreamService::YouTube => "rtmps://a.rtmps.youtube.com:443/live2".into(),
            StreamService::Custom => custom.trim().trim_end_matches('/').into(),
        }
    }

    /// Full publish URL: server + "/" + key.
    pub fn url(self, custom: &str, key: &str) -> String {
        let server = self.server(custom);
        let key = key.trim();
        if key.is_empty() {
            server
        } else {
            format!("{server}/{key}")
        }
    }

    /// Where the user finds their stream key.
    pub fn key_page(self) -> Option<&'static str> {
        match self {
            StreamService::Twitch => Some("https://dashboard.twitch.tv/settings/stream"),
            StreamService::YouTube => Some("https://studio.youtube.com/"),
            StreamService::Custom => None,
        }
    }
}

/// Chroma subsampling kept through capture, preview and encoding.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Chroma {
    /// 4:2:0: most compatible, every mode. (NV12)
    #[default]
    #[serde(rename = "4:2:0")]
    S420,
    /// 4:2:2: full vertical chroma. (YUY2 capture, NV16 encode)
    #[serde(rename = "4:2:2")]
    S422,
    /// 4:4:4: full-resolution color, sharpest text/UI. (RGB24 capture, YUV444 encode)
    #[serde(rename = "4:4:4")]
    S444,
}

impl Chroma {
    pub const ALL: [Chroma; 3] = [Chroma::S420, Chroma::S422, Chroma::S444];

    pub fn label(self) -> &'static str {
        match self {
            Chroma::S420 => "4:2:0",
            Chroma::S422 => "4:2:2",
            Chroma::S444 => "4:4:4",
        }
    }

    /// Horizontal / vertical chroma subsampling divisors.
    pub fn subsampling(self) -> (u32, u32) {
        match self {
            Chroma::S420 => (2, 2),
            Chroma::S422 => (2, 1),
            Chroma::S444 => (1, 1),
        }
    }

    /// Relative bitrate needed for similar quality.
    fn bitrate_factor(self) -> f64 {
        match self {
            Chroma::S420 => 1.0,
            Chroma::S422 => 1.3,
            Chroma::S444 => 1.6,
        }
    }
}

impl Codec {
    /// NVENC can't encode AV1 at 4:2:2 / 4:4:4.
    pub fn supports(self, chroma: Chroma) -> bool {
        chroma == Chroma::S420 || self != Codec::Av1
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
    /// Desired chroma subsampling; falls back to what the chosen mode can deliver.
    pub chroma: Chroma,
    /// Input YCbCr range; Auto trusts the capture device's media type.
    pub color_range: crate::color::RangePref,
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
    /// Replays and recordings.
    pub output_dir: PathBuf,
    /// Screenshots (PNG).
    pub screenshot_dir: PathBuf,

    pub hotkey_save_replay: String,
    pub hotkey_toggle_record: String,
    pub hotkey_screenshot: String,
    pub hotkey_toggle_stream: String,

    pub windows_notifications: bool,
    pub present_mode: PresentMode,

    /// Which releases to offer: stable only, or pre-releases too.
    pub update_channel: crate::updater::UpdateChannel,
    /// Check GitHub for a newer version at startup.
    pub check_updates: bool,
    /// `owner/name` override for the update source (official builds have one built in).
    pub update_repo: Option<String>,

    /// Show the sidebar (controls + settings). Narrow windows start with it hidden anyway.
    pub sidebar_open: bool,

    /// Frame generation for the preview (NVIDIA Video Frame Generation, needs the VFX SDK).
    pub frame_gen: bool,
    pub frame_gen_quality: crate::vfg::Quality,
    /// Output frames per game frame: 2, 3 or 4.
    pub frame_gen_multiplier: u32,
    /// Folder with NVVideoEffects.dll etc.; None = search the usual install locations.
    pub frame_gen_dir: Option<PathBuf>,

    /// Upload each captured frame to CUDA once and encode from there (NVENC and frame
    /// generation share it). Off = the encoder takes system-memory frames as before.
    pub gpu_frames: bool,

    /// Live streaming: service, Custom server URL, output height (0 = source), max frame rate
    /// and video bitrate. The stream key lives in Windows Credential Manager, not here.
    pub stream_service: StreamService,
    pub stream_server: String,
    pub stream_height: u32,
    pub stream_fps: u32,
    pub stream_bitrate_kbps: u32,
}

impl Default for Config {
    fn default() -> Self {
        let user = directories::UserDirs::new();
        let output_dir = user
            .as_ref()
            .and_then(|d| d.video_dir().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Passthrify");
        let screenshot_dir = user
            .as_ref()
            .and_then(|d| d.picture_dir().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| output_dir.clone())
            .join("Passthrify");
        Self {
            video_device: None,
            video_mode: None,
            chroma: Chroma::S420,
            color_range: crate::color::RangePref::Auto,
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
            screenshot_dir,
            hotkey_save_replay: "Alt+F10".into(),
            hotkey_toggle_record: "Alt+F9".into(),
            hotkey_screenshot: "Alt+F1".into(),
            hotkey_toggle_stream: "Alt+F8".into(),
            windows_notifications: true,
            present_mode: PresentMode::Mailbox,
            update_channel: crate::updater::UpdateChannel::Stable,
            check_updates: true,
            update_repo: None,
            sidebar_open: true,
            frame_gen: false,
            frame_gen_quality: crate::vfg::Quality::Medium,
            frame_gen_multiplier: 2,
            frame_gen_dir: None,
            gpu_frames: true,
            stream_service: StreamService::Twitch,
            stream_server: String::new(),
            stream_height: 1080,
            stream_fps: 60,
            stream_bitrate_kbps: 6000,
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

    /// Bitrate in bits/s for the given capture mode and the chroma actually captured.
    pub fn bitrate_bps(&self, width: u32, height: u32, fps: f64, chroma: Chroma) -> u64 {
        if self.bitrate_mbps > 0 {
            return self.bitrate_mbps as u64 * 1_000_000;
        }
        // ~0.1 bits/pixel for HEVC/AV1 (4K60 4:2:0 ≈ 50 Mbps), 1.5x for H.264, more for 4:2:2/4:4:4.
        let bpp = match self.codec {
            Codec::H264 => 0.15,
            _ => 0.10,
        } * chroma.bitrate_factor();
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
    fn screenshots_have_their_own_folder() {
        // Older configs only had output_dir: screenshots still get the Pictures default.
        let cfg: Config = toml::from_str(r#"output_dir = 'D:\Clips'"#).unwrap();
        assert_eq!(cfg.output_dir, PathBuf::from(r"D:\Clips"));
        assert_ne!(cfg.screenshot_dir, cfg.output_dir);
        assert!(cfg.screenshot_dir.ends_with("Passthrify"));
    }

    #[test]
    fn auto_bitrate() {
        let cfg = Config::default();
        let b = cfg.bitrate_bps(3840, 2160, 60.0, Chroma::S420);
        assert!((45_000_000..=55_000_000).contains(&b), "{b}");
        assert_eq!(cfg.bitrate_bps(1280, 720, 30.0, Chroma::S420), 12_000_000);
        assert!(cfg.bitrate_bps(2560, 1440, 60.0, Chroma::S444) > cfg.bitrate_bps(2560, 1440, 60.0, Chroma::S420));
    }

    #[test]
    fn chroma_serializes_readably() {
        let mut cfg = Config::default();
        cfg.chroma = Chroma::S444;
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("chroma = \"4:4:4\""), "{text}");
        assert!(!Codec::Av1.supports(Chroma::S444));
        assert!(Codec::Hevc.supports(Chroma::S422));
    }
}
