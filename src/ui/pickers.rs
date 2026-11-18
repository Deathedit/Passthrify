//! Resolution / frame-rate / codec dropdowns shared by the toolbar and the Settings window.

use eframe::egui;

use crate::capture::video_mf::VideoMode;
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

/// Chroma formats the card can natively deliver at `w`x`h` (optionally at `fps`), ascending.
/// 4:2:0 is always possible when any mode exists.
pub fn available_chromas(modes: &[VideoMode], w: u32, h: u32, fps: Option<f64>) -> Vec<Chroma> {
    let mut v: Vec<Chroma> = modes
        .iter()
        .filter(|m| m.format.supported() && m.width == w && m.height == h)
        .filter(|m| fps.is_none_or(|f| (m.fps() - f).abs() < 0.5))
        .map(|m| m.format.native_chroma())
        .collect();
    if !v.is_empty() {
        v.push(Chroma::S420);
    }
    v.sort();
    v.dedup();
    v
}

/// Chromas available for the mode selected in `pref`, or the running mode when on Auto.
pub fn available_for(modes: &[VideoMode], pref: Option<&ModePref>, current: Option<VideoMode>) -> Vec<Chroma> {
    match (pref, current) {
        (Some(p), _) => available_chromas(modes, p.width, p.height, Some(p.fps)),
        (None, Some(m)) => available_chromas(modes, m.width, m.height, Some(m.fps())),
        _ => vec![Chroma::S420],
    }
}

/// " · 4:4:4" style tag naming the best chroma above 4:2:0, if any.
fn chroma_tag(av: &[Chroma]) -> String {
    match av.last() {
        Some(c) if *c > Chroma::S420 => format!("   {}", c.label()),
        _ => String::new(),
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
            let tag = chroma_tag(&available_chromas(modes, w, h, None));
            if ui.selectable_label(selected, format!("{w}x{h}{tag}")).clicked() && !selected {
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
                let tag = chroma_tag(&available_chromas(modes, w, h, Some(f)));
                if ui.selectable_label(selected, format!("{}{tag}", fmt_fps(f))).clicked() && !selected {
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

/// Chroma subsampling dropdown. `available` = what the selected mode can deliver; `actual` =
/// what the running capture delivers. Picking 4:2:2/4:4:4 with AV1 switches the codec to HEVC.
#[allow(clippy::too_many_arguments)]
pub fn chroma_picker(
    ui: &mut egui::Ui,
    id: &str,
    chroma: &mut Chroma,
    codec: &mut Codec,
    available: &[Chroma],
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
            let here = available.contains(&c);
            let label = if here { c.label().to_string() } else { format!("{}  (not at this mode)", c.label()) };
            let hint = match c {
                Chroma::S420 => "Smallest files, plays everywhere.",
                Chroma::S422 => "Full vertical color. Captured as YUY2. Needs an RTX 50-series GPU to encode.",
                Chroma::S444 => {
                    "Full color resolution: crisp text, HUDs and red/blue edges. Captured as RGB \
                     (on the 4K X up to 1440p60 / 1080p120)."
                }
            };
            if ui.selectable_label(*chroma == c, label).on_hover_text(hint).clicked() && *chroma != c {
                *chroma = c;
                if !codec.supports(c) {
                    *codec = Codec::Hevc;
                }
                changed = true;
            }
        }
    });
    resp.response.on_hover_text(
        "Chroma subsampling. 4:2:2 / 4:4:4 files need VLC, mpv, MPC-HC or a pro editor; \
         Windows' built-in player only decodes 4:2:0.",
    );
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

    #[test]
    fn chroma_availability() {
        let modes = [
            m(2560, 1440, 60, 1, PixFmt::Nv12),
            m(2560, 1440, 60, 1, PixFmt::Rgb24),
            m(2560, 1440, 30, 1, PixFmt::Yuy2),
            m(3840, 2160, 60, 1, PixFmt::Nv12),
        ];
        assert_eq!(available_chromas(&modes, 2560, 1440, Some(60.0)), vec![Chroma::S420, Chroma::S444]);
        assert_eq!(available_chromas(&modes, 2560, 1440, Some(30.0)), vec![Chroma::S420, Chroma::S422]);
        assert_eq!(available_chromas(&modes, 2560, 1440, None), vec![Chroma::S420, Chroma::S422, Chroma::S444]);
        assert_eq!(available_chromas(&modes, 3840, 2160, None), vec![Chroma::S420]);
        assert!(available_chromas(&modes, 1280, 720, None).is_empty());
    }
}
