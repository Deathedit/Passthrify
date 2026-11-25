//! Chroma / resolution / frame-rate / codec dropdowns shared by the toolbar and Settings.
//!
//! Chroma is the primary choice: the resolution and frame-rate lists only offer modes where
//! the card natively delivers the selected chroma (4:2:0 = any mode, 4:2:2 = YUY2 modes,
//! 4:4:4 = RGB24 modes).

use eframe::egui;

use crate::capture::VideoMode;
use crate::config::{Chroma, Codec, ModePref};

fn fps_key(f: f64) -> u32 {
    (f * 100.0).round() as u32
}

pub fn fmt_fps(f: f64) -> String {
    if (f - f.round()).abs() < 0.005 {
        format!("{f:.0} Hz")
    } else {
        format!("{f:.2} Hz")
    }
}

/// Can this mode deliver `chroma` natively?
fn delivers(m: &VideoMode, chroma: Chroma) -> bool {
    m.format.supported() && (chroma == Chroma::S420 || m.format.native_chroma() == chroma)
}

/// Distinct resolutions that deliver `chroma`, highest first.
pub fn resolutions(modes: &[VideoMode], chroma: Chroma) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> =
        modes.iter().filter(|m| delivers(m, chroma)).map(|m| (m.width, m.height)).collect();
    v.sort_by(|a, b| (b.0 * b.1, b.0).cmp(&(a.0 * a.1, a.0)));
    v.dedup();
    v
}

/// Distinct frame rates at `w`x`h` that deliver `chroma`, highest first.
pub fn rates(modes: &[VideoMode], w: u32, h: u32, chroma: Chroma) -> Vec<f64> {
    let mut v: Vec<f64> = modes
        .iter()
        .filter(|m| delivers(m, chroma) && m.width == w && m.height == h)
        .map(|m| m.fps())
        .collect();
    v.sort_by(|a, b| b.total_cmp(a));
    v.dedup_by_key(|f| fps_key(*f));
    v
}

/// Keep `want` if offered, else the highest rate up to 60 Hz, else the highest.
fn pick_fps(available: &[f64], want: f64) -> Option<f64> {
    available
        .iter()
        .copied()
        .find(|f| (f - want).abs() < 0.5)
        .or_else(|| available.iter().copied().find(|f| *f <= 60.5))
        .or(available.first().copied())
}

/// Moves a mode preference to the nearest mode that delivers `chroma`: same resolution and
/// rate if possible, else same resolution at another rate, else the largest resolution that
/// has it. `None` (Auto) stays Auto: capture then prefers modes with that chroma.
pub fn fit_pref_to_chroma(modes: &[VideoMode], pref: Option<ModePref>, chroma: Chroma) -> Option<ModePref> {
    let p = pref?;
    let at_res = rates(modes, p.width, p.height, chroma);
    if let Some(fps) = pick_fps(&at_res, p.fps) {
        return Some(ModePref { fps, ..p });
    }
    for (w, h) in resolutions(modes, chroma) {
        if let Some(fps) = pick_fps(&rates(modes, w, h, chroma), p.fps) {
            return Some(ModePref { width: w, height: h, fps });
        }
    }
    Some(p) // chroma not offered at all: leave it, capture falls back
}

/// Resolution dropdown (filtered by chroma). Picking a resolution keeps the current frame rate
/// when available, otherwise the highest up to 60 Hz. Returns true on change.
#[allow(clippy::too_many_arguments)]
pub fn resolution_picker(
    ui: &mut egui::Ui,
    id: &str,
    pref: &mut Option<ModePref>,
    chroma: Chroma,
    modes: &[VideoMode],
    current: Option<VideoMode>,
    width: f32,
) -> bool {
    let mut changed = false;
    let text = match (pref.as_ref(), current) {
        (Some(p), _) => format!("{}x{}", p.width, p.height),
        (None, Some(m)) => format!("Auto ({}x{})", m.width, m.height),
        (None, None) => "Auto".into(),
    };
    egui::ComboBox::from_id_salt(format!("{id}_res")).selected_text(text).width(width).show_ui(ui, |ui| {
        if ui.selectable_label(pref.is_none(), "Auto (best)").clicked() && pref.is_some() {
            *pref = None;
            changed = true;
        }
        for (w, h) in resolutions(modes, chroma) {
            let selected = pref.is_some_and(|p| p.width == w && p.height == h);
            if ui.selectable_label(selected, format!("{w}x{h}")).clicked() && !selected {
                let want = pref.map(|p| p.fps).or(current.map(|m| m.fps())).unwrap_or(60.0);
                let fps = pick_fps(&rates(modes, w, h, chroma), want).unwrap_or(60.0);
                *pref = Some(ModePref { width: w, height: h, fps });
                changed = true;
            }
        }
    });
    changed
}

/// Frame-rate dropdown: only rates where the selected resolution delivers `chroma`.
#[allow(clippy::too_many_arguments)]
pub fn rate_picker(
    ui: &mut egui::Ui,
    id: &str,
    pref: &mut Option<ModePref>,
    chroma: Chroma,
    modes: &[VideoMode],
    current: Option<VideoMode>,
    width: f32,
) -> bool {
    let mut changed = false;
    let (w, h) = match (pref.as_ref(), current) {
        (Some(p), _) => (p.width, p.height),
        (None, Some(m)) => (m.width, m.height),
        (None, None) => (0, 0),
    };
    let available = rates(modes, w, h, chroma);
    let text = match (pref.as_ref(), current) {
        (Some(p), _) => fmt_fps(p.fps),
        (None, Some(m)) => format!("Auto ({})", fmt_fps(m.fps())),
        (None, None) => "Auto".into(),
    };
    ui.add_enabled_ui(!available.is_empty(), |ui| {
        egui::ComboBox::from_id_salt(format!("{id}_fps")).selected_text(text).width(width).show_ui(ui, |ui| {
            for f in available {
                let selected = pref.is_some_and(|p| fps_key(p.fps) == fps_key(f));
                if ui.selectable_label(selected, fmt_fps(f)).clicked() && !selected {
                    *pref = Some(ModePref { width: w, height: h, fps: f });
                    changed = true;
                }
            }
        });
    });
    changed
}

pub fn codec_picker(ui: &mut egui::Ui, id: &str, codec: &mut Codec, chroma: Chroma, short: bool, width: f32) -> bool {
    let mut changed = false;
    let label = |c: Codec| if short { c.short_label() } else { c.label() };
    egui::ComboBox::from_id_salt(format!("{id}_codec")).selected_text(label(*codec)).width(width).show_ui(ui, |ui| {
        for c in Codec::ALL {
            let ok = c.supports(chroma);
            let resp = ui
                .add_enabled(ok, egui::SelectableLabel::new(*codec == c, c.label()))
                .on_disabled_hover_text(format!("NVENC can't encode {} at {}", c.short_label(), chroma.label()));
            if resp.clicked() && *codec != c {
                *codec = c;
                changed = true;
            }
        }
    });
    changed
}

/// Chroma dropdown. Changing it moves `pref` to the nearest mode that delivers the new chroma
/// and switches AV1 to HEVC for 4:2:2/4:4:4. `actual` = what the running capture delivers.
#[allow(clippy::too_many_arguments)]
pub fn chroma_picker(
    ui: &mut egui::Ui,
    id: &str,
    chroma: &mut Chroma,
    pref: &mut Option<ModePref>,
    codec: &mut Codec,
    modes: &[VideoMode],
    actual: Option<Chroma>,
    width: f32,
) -> bool {
    let mut changed = false;
    let text = match actual {
        Some(a) if a != *chroma => format!("{} → {}", chroma.label(), a.label()),
        _ => chroma.label().to_string(),
    };
    let resp = egui::ComboBox::from_id_salt(format!("{id}_chroma")).selected_text(text).width(width).show_ui(ui, |ui| {
        for c in Chroma::ALL {
            let top = resolutions(modes, c).first().copied();
            let offered = modes.is_empty() || top.is_some();
            let label = match top {
                Some((w, h)) if c != Chroma::S420 => format!("{}   up to {w}x{h}", c.label()),
                _ => c.label().to_string(),
            };
            let hint = match c {
                Chroma::S420 => "Smallest files, plays everywhere. All resolutions and frame rates.",
                Chroma::S422 => "Full vertical color. Captured as YUY2. Needs an RTX 50-series GPU to encode.",
                Chroma::S444 => "Full color resolution: crisp text, HUDs and red/blue edges. Captured as RGB.",
            };
            let resp = ui
                .add_enabled(offered, egui::SelectableLabel::new(*chroma == c, label))
                .on_hover_text(hint)
                .on_disabled_hover_text("This capture device doesn't offer it");
            if resp.clicked() && *chroma != c {
                *chroma = c;
                *pref = fit_pref_to_chroma(modes, *pref, c);
                if !codec.supports(c) {
                    *codec = Codec::Hevc;
                }
                changed = true;
            }
        }
    });
    resp.response.on_hover_text(if cfg!(windows) {
        "Chroma subsampling. The resolution and frame-rate lists show only modes that deliver it.\n\
         4:2:2 / 4:4:4 files need VLC, mpv, MPC-HC or a pro editor; Windows' built-in player only decodes 4:2:0."
    } else {
        "Chroma subsampling. The resolution and frame-rate lists show only modes that deliver it.\n\
         4:2:2 / 4:4:4 files need mpv, VLC or a pro editor; many players and browsers only decode 4:2:0."
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::PixFmt;

    fn m(w: u32, h: u32, num: u32, den: u32, format: PixFmt) -> VideoMode {
        VideoMode { index: 0, width: w, height: h, fps_num: num, fps_den: den, format }
    }

    fn sample() -> Vec<VideoMode> {
        vec![
            m(3840, 2160, 144, 1, PixFmt::Nv12),
            m(3840, 2160, 60, 1, PixFmt::Nv12),
            m(3840, 2160, 60000, 1001, PixFmt::Nv12),
            m(3840, 2160, 30, 1, PixFmt::Yuy2),
            m(2560, 1440, 144, 1, PixFmt::Nv12),
            m(2560, 1440, 60, 1, PixFmt::Nv12),
            m(2560, 1440, 60, 1, PixFmt::Yuy2),
            m(2560, 1440, 60, 1, PixFmt::Rgb24),
            m(2560, 1440, 30, 1, PixFmt::Rgb24),
            m(1920, 1080, 240, 1, PixFmt::Nv12),
            m(1920, 1080, 120, 1, PixFmt::Rgb24),
            m(1920, 1080, 60, 1, PixFmt::Yuy2),
            m(1280, 720, 60, 1, PixFmt::P010), // unsupported format: hidden
        ]
    }

    #[test]
    fn resolutions_follow_chroma() {
        let modes = sample();
        assert_eq!(resolutions(&modes, Chroma::S420), vec![(3840, 2160), (2560, 1440), (1920, 1080)]);
        assert_eq!(resolutions(&modes, Chroma::S422), vec![(3840, 2160), (2560, 1440), (1920, 1080)]);
        assert_eq!(resolutions(&modes, Chroma::S444), vec![(2560, 1440), (1920, 1080)]);
    }

    #[test]
    fn rates_follow_chroma() {
        let modes = sample();
        let hz = |w, h, c| rates(&modes, w, h, c).into_iter().map(fmt_fps).collect::<Vec<_>>();
        assert_eq!(hz(3840, 2160, Chroma::S420), ["144 Hz", "60 Hz", "59.94 Hz", "30 Hz"]);
        assert_eq!(hz(3840, 2160, Chroma::S422), ["30 Hz"]);
        assert!(hz(3840, 2160, Chroma::S444).is_empty());
        assert_eq!(hz(2560, 1440, Chroma::S444), ["60 Hz", "30 Hz"]);
        assert_eq!(hz(1920, 1080, Chroma::S444), ["120 Hz"]);
        assert_eq!(hz(2560, 1440, Chroma::S420), ["144 Hz", "60 Hz", "30 Hz"]);
    }

    #[test]
    fn switching_chroma_moves_to_a_valid_mode() {
        let modes = sample();
        let p = |w, h, fps| Some(ModePref { width: w, height: h, fps });
        // 4K60 has no 4:4:4: jump to the largest 4:4:4 resolution, keeping 60 Hz.
        assert_eq!(fit_pref_to_chroma(&modes, p(3840, 2160, 60.0), Chroma::S444), p(2560, 1440, 60.0));
        // 1440p144 has no 4:4:4 at 144: same resolution, best rate up to 60.
        assert_eq!(fit_pref_to_chroma(&modes, p(2560, 1440, 144.0), Chroma::S444), p(2560, 1440, 60.0));
        // 4K60 -> 4:2:2: 4K only does 4:2:2 at 30.
        assert_eq!(fit_pref_to_chroma(&modes, p(3840, 2160, 60.0), Chroma::S422), p(3840, 2160, 30.0));
        // Already valid: unchanged. Auto stays Auto.
        assert_eq!(fit_pref_to_chroma(&modes, p(1920, 1080, 120.0), Chroma::S444), p(1920, 1080, 120.0));
        assert_eq!(fit_pref_to_chroma(&modes, None, Chroma::S444), None);
    }
}
