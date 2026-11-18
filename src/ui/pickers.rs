//! Resolution / frame-rate / codec dropdowns shared by the toolbar and the Settings window.

use eframe::egui;

use crate::capture::video_mf::VideoMode;
use crate::config::{Codec, ModePref};

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

/// Distinct capturable resolutions, highest first.
pub fn resolutions(modes: &[VideoMode]) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> =
        modes.iter().filter(|m| m.format.supported()).map(|m| (m.width, m.height)).collect();
    v.sort_by(|a, b| (b.0 * b.1, b.0).cmp(&(a.0 * a.1, a.0)));
    v.dedup();
    v
}

/// Distinct frame rates available at `w`x`h`, highest first.
pub fn rates(modes: &[VideoMode], w: u32, h: u32) -> Vec<f64> {
    let mut v: Vec<f64> = modes
        .iter()
        .filter(|m| m.format.supported() && m.width == w && m.height == h)
        .map(|m| m.fps())
        .collect();
    v.sort_by(|a, b| b.total_cmp(a));
    v.dedup_by_key(|f| fps_key(*f));
    v
}

/// Resolution dropdown. Picking a resolution keeps the current frame rate when that resolution
/// offers it, otherwise falls back to the highest rate up to 60 Hz. Returns true on change.
pub fn resolution_picker(
    ui: &mut egui::Ui,
    id: &str,
    pref: &mut Option<ModePref>,
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
        for (w, h) in resolutions(modes) {
            let selected = pref.is_some_and(|p| p.width == w && p.height == h);
            if ui.selectable_label(selected, format!("{w}x{h}")).clicked() && !selected {
                let available = rates(modes, w, h);
                let want = pref.map(|p| p.fps).or(current.map(|m| m.fps())).unwrap_or(60.0);
                let fps = available
                    .iter()
                    .copied()
                    .find(|f| (f - want).abs() < 0.5)
                    .or_else(|| available.iter().copied().find(|f| *f <= 60.5))
                    .or(available.first().copied())
                    .unwrap_or(60.0);
                *pref = Some(ModePref { width: w, height: h, fps });
                changed = true;
            }
        }
    });
    changed
}

/// Frame-rate dropdown for the selected (or currently captured) resolution.
pub fn rate_picker(
    ui: &mut egui::Ui,
    id: &str,
    pref: &mut Option<ModePref>,
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
    let available = rates(modes, w, h);
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

pub fn codec_picker(ui: &mut egui::Ui, id: &str, codec: &mut Codec, short: bool, width: f32) -> bool {
    let mut changed = false;
    let label = |c: Codec| if short { c.short_label() } else { c.label() };
    egui::ComboBox::from_id_salt(format!("{id}_codec")).selected_text(label(*codec)).width(width).show_ui(ui, |ui| {
        for c in Codec::ALL {
            if ui.selectable_label(*codec == c, c.label()).clicked() && *codec != c {
                *codec = c;
                changed = true;
            }
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::video_mf::PixFmt;

    fn m(w: u32, h: u32, num: u32, den: u32, format: PixFmt) -> VideoMode {
        VideoMode { index: 0, width: w, height: h, fps_num: num, fps_den: den, format }
    }

    #[test]
    fn lists_are_sorted_and_deduped() {
        let modes = [
            m(1920, 1080, 60, 1, PixFmt::Nv12),
            m(1920, 1080, 60, 1, PixFmt::Yuy2),
            m(3840, 2160, 30, 1, PixFmt::Nv12),
            m(3840, 2160, 60000, 1001, PixFmt::Nv12),
            m(3840, 2160, 60, 1, PixFmt::Nv12),
            m(3440, 1440, 60, 1, PixFmt::Nv12),
            m(2560, 1440, 144, 1, PixFmt::Nv12),
            m(1280, 720, 60, 1, PixFmt::P010), // unsupported format: hidden
        ];
        assert_eq!(resolutions(&modes), vec![(3840, 2160), (3440, 1440), (2560, 1440), (1920, 1080)]);
        let r = rates(&modes, 3840, 2160);
        assert_eq!(r.len(), 3);
        assert_eq!(fmt_fps(r[0]), "60 Hz");
        assert_eq!(fmt_fps(r[1]), "59.94 Hz");
        assert_eq!(fmt_fps(r[2]), "30 Hz");
        assert_eq!(rates(&modes, 1920, 1080).len(), 1);
    }
}
