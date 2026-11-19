//! Passthrify design system: palette, fonts, egui visuals, line icons and styled widgets.
//! Values come from the "Lens P" design board (dark, cyan signal accent, rec red).

use std::sync::Arc;

use eframe::egui::{
    self, text::LayoutJob, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Response,
    RichText, Sense, Shadow, Stroke, TextFormat, Ui, Vec2,
};

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

// --- Palette -------------------------------------------------------------------------------
pub const VOID: Color32 = hex(0x08090C); // video letterbox
pub const BASE: Color32 = hex(0x0D0F13); // window ground
pub const SURFACE: Color32 = hex(0x13161B); // toolbar, status bar, cards
pub const RAISED: Color32 = hex(0x1A1E25); // buttons, inputs, toasts
pub const HOVER: Color32 = hex(0x232831);
pub const PRESSED: Color32 = hex(0x2B313B);
pub const DIVIDER: Color32 = hex(0x1F242C);
pub const LINE: Color32 = hex(0x2A303A);
pub const LINE_STRONG: Color32 = hex(0x3A4250);
pub const TEXT: Color32 = hex(0xECEEF2);
pub const TEXT2: Color32 = hex(0xA3ACBA);
pub const TEXT3: Color32 = hex(0x808A9B);
pub const SIGNAL: Color32 = hex(0x5BD3FF);
pub const SIGNAL_HOVER: Color32 = hex(0x7FDDFF);
pub const SIGNAL_DEEP: Color32 = hex(0x2FB5E8);
pub const ON_SIGNAL: Color32 = hex(0x031219);
pub const REC: Color32 = hex(0xFF4D5E);
pub const REC_FILL: Color32 = hex(0xD92B3A);
pub const REC_FILL_HOVER: Color32 = hex(0xE63A49);
pub const SUCCESS: Color32 = hex(0x3DD68C);
pub const WARNING: Color32 = hex(0xF5B841);

// --- Fonts ---------------------------------------------------------------------------------
pub fn medium() -> FontFamily {
    FontFamily::Name("inter-medium".into())
}
pub fn semibold() -> FontFamily {
    FontFamily::Name("inter-semibold".into())
}
pub fn mono_medium() -> FontFamily {
    FontFamily::Name("mono-medium".into())
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    };
    add(&mut fonts, "inter", include_bytes!("../../assets/fonts/Inter-Regular.ttf"));
    add(&mut fonts, "inter-medium", include_bytes!("../../assets/fonts/Inter-Medium.ttf"));
    add(&mut fonts, "inter-semibold", include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"));
    add(&mut fonts, "jbmono", include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"));
    add(&mut fonts, "jbmono-medium", include_bytes!("../../assets/fonts/JetBrainsMono-Medium.ttf"));

    // egui's bundled fonts stay as fallbacks (symbols, arrows, emoji).
    // JetBrains Mono goes last: it has the arrows (→) that Inter and egui's fonts lack here.
    let mut prop_fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    prop_fallback.push("jbmono".into());
    let mono_fallback = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with = |first: &str, rest: &[String]| {
        let mut v = vec![first.to_string()];
        v.extend(rest.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, with("inter", &prop_fallback));
    fonts.families.insert(FontFamily::Monospace, with("jbmono", &mono_fallback));
    fonts.families.insert(medium(), with("inter-medium", &prop_fallback));
    fonts.families.insert(semibold(), with("inter-semibold", &prop_fallback));
    fonts.families.insert(mono_medium(), with("jbmono-medium", &mono_fallback));
    ctx.set_fonts(fonts);
}

/// Installs fonts, image loaders (for SVG icons) and the dark visuals.
pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    egui_extras::install_image_loaders(ctx);
    ctx.set_visuals(egui::Visuals::dark());
    ctx.style_mut(|style| {
        use egui::TextStyle;
        style.text_styles = [
            (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.0, medium())),
            (TextStyle::Heading, FontId::new(15.0, semibold())),
            (TextStyle::Monospace, mono(12.0)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 6.0);
        s.button_padding = Vec2::new(12.0, 6.0);
        s.interact_size = Vec2::new(40.0, 28.0);
        s.window_margin = Margin::same(16);
        s.menu_margin = Margin::same(6);
        s.combo_width = 120.0;
        s.slider_width = 120.0;

        let v = &mut style.visuals;
        v.panel_fill = SURFACE;
        v.window_fill = BASE;
        v.extreme_bg_color = RAISED; // text edits
        v.faint_bg_color = RAISED;
        v.code_bg_color = RAISED;
        v.window_stroke = Stroke::new(1.0_f32, LINE);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.window_shadow = Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(128) };
        v.popup_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(110) };
        v.selection.bg_fill = Color32::from_rgba_unmultiplied(91, 211, 255, 72);
        v.selection.stroke = Stroke::new(1.0_f32, SIGNAL);
        v.hyperlink_color = SIGNAL;
        v.error_fg_color = REC;
        v.warn_fg_color = WARNING;
        v.slider_trailing_fill = true;
        v.indent_has_left_vline = false;

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = SURFACE;
        w.noninteractive.weak_bg_fill = SURFACE;
        w.noninteractive.bg_stroke = Stroke::new(1.0_f32, DIVIDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT2);
        w.inactive.bg_fill = LINE; // slider rail, checkbox box
        w.inactive.weak_bg_fill = RAISED;
        w.inactive.bg_stroke = Stroke::new(1.0_f32, LINE);
        w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
        w.hovered.bg_fill = LINE_STRONG;
        w.hovered.weak_bg_fill = HOVER;
        w.hovered.bg_stroke = Stroke::new(1.0_f32, LINE_STRONG);
        w.hovered.fg_stroke = Stroke::new(1.5_f32, TEXT);
        w.active.bg_fill = SIGNAL;
        w.active.weak_bg_fill = PRESSED;
        w.active.bg_stroke = Stroke::new(1.0_f32, SIGNAL);
        w.active.fg_stroke = Stroke::new(1.5_f32, TEXT);
        w.open.bg_fill = HOVER;
        w.open.weak_bg_fill = HOVER;
        w.open.bg_stroke = Stroke::new(1.0_f32, LINE_STRONG);
        w.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
        for st in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            st.corner_radius = CornerRadius::same(6);
            st.expansion = 0.0;
        }
    });
}

// --- Icons ---------------------------------------------------------------------------------
// 24-unit line icons, 1.75 stroke, drawn white and tinted at use.

macro_rules! icon {
    ($name:ident, $body:literal) => {
        pub const $name: (&str, &[u8]) = (
            concat!("bytes://icon-", stringify!($name), ".svg"),
            concat!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">"#,
                $body,
                "</svg>"
            )
            .as_bytes(),
        );
    };
}

pub mod icons {
    icon!(REPLAY, r#"<path d="M4 12a8 8 0 1 0 2.34-5.66M4 4v4h4"/><circle cx="12" cy="12" r="2" fill="white" stroke="none"/>"#);
    icon!(STOP, r#"<rect x="7" y="7" width="10" height="10" rx="2" fill="white" stroke="none"/>"#);
    icon!(RECORD, r#"<circle cx="12" cy="12" r="5" fill="white" stroke="none"/>"#);
    icon!(CAMERA, r#"<path d="M4 8.5A1.5 1.5 0 0 1 5.5 7H8l1.5-2h5L16 7h2.5A1.5 1.5 0 0 1 20 8.5v9a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 4 17.5z"/><circle cx="12" cy="13" r="3.5"/>"#);
    icon!(FOLDER, r#"<path d="M3.5 7.5A1.5 1.5 0 0 1 5 6h4l2 2h8a1.5 1.5 0 0 1 1.5 1.5v8A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5z"/>"#);
    icon!(SPEAKER, r#"<path d="M4 9.5h3l4.5-4v13L7 14.5H4zM15 9a4 4 0 0 1 0 6M17.5 6.5a7.5 7.5 0 0 1 0 11"/>"#);
    icon!(MUTED, r#"<path d="M4 9.5h3l4.5-4v13L7 14.5H4zM16 9.5l5 5M21 9.5l-5 5"/>"#);
    icon!(FULLSCREEN, r#"<path d="M4 9V5h4M16 5h4v4M20 15v4h-4M8 19H4v-4"/>"#);
    icon!(EXIT_FULLSCREEN, r#"<path d="M8 4v4H4M16 4v4h4M20 16h-4v4M4 16h4v4"/>"#);
    icon!(CHECK, r#"<path d="M5 12.5l4.5 4.5L19 7.5"/>"#);
    icon!(ALERT, r#"<circle cx="12" cy="12" r="9"/><path d="M12 7.5v5.5M12 16.5v.01"/>"#);
    icon!(LOCK, r#"<rect x="5.5" y="10.5" width="13" height="9" rx="2"/><path d="M8.5 10.5V8a3.5 3.5 0 0 1 7 0v2.5"/>"#);
    icon!(CLOSE, r#"<path d="M7 7l10 10M17 7L7 17"/>"#);
    icon!(DOWNLOAD, r#"<path d="M12 4v11M7.5 10.5L12 15l4.5-4.5M5 19h14"/>"#);
    // Sidebar: hide (arrow into the panel edge) / show (arrow out of it).
    icon!(SIDEBAR_HIDE, r#"<path d="M4 6.5A1.5 1.5 0 0 1 5.5 5h13A1.5 1.5 0 0 1 20 6.5v11a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 4 17.5zM15 5v14M9 10l2 2-2 2"/>"#);
    icon!(SIDEBAR_SHOW, r#"<path d="M4 6.5A1.5 1.5 0 0 1 5.5 5h13A1.5 1.5 0 0 1 20 6.5v11a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 4 17.5zM15 5v14M11 10l-2 2 2 2"/>"#);
    icon!(BACK, r#"<path d="M14.5 6l-6 6 6 6"/>"#);
    icon!(CHEVRON, r#"<path d="M9.5 6l6 6-6 6"/>"#);
    // Settings sections.
    icon!(CAPTURE, r#"<path d="M4 7.5A1.5 1.5 0 0 1 5.5 6h9A1.5 1.5 0 0 1 16 7.5v9a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 16.5zM16 10.5l4-2.5v8l-4-2.5"/>"#);
    icon!(AUDIO, r#"<path d="M4 15v-3a8 8 0 0 1 16 0v3M4 15a2 2 0 0 1 2-2h1v6H6a2 2 0 0 1-2-2zM20 15a2 2 0 0 0-2-2h-1v6h1a2 2 0 0 0 2-2z"/>"#);
    icon!(HISTORY, r#"<path d="M4 12a8 8 0 1 0 2.34-5.66M4 4v4h4M12 9v3l2 1.5"/>"#);
    icon!(KEYBOARD, r#"<path d="M3.5 7.5A1.5 1.5 0 0 1 5 6h14a1.5 1.5 0 0 1 1.5 1.5v9A1.5 1.5 0 0 1 19 18H5a1.5 1.5 0 0 1-1.5-1.5zM7 10h.01M10 10h.01M13 10h.01M17 10h.01M8 14h8"/>"#);
    icon!(DISPLAY, r#"<path d="M3 6.5A1.5 1.5 0 0 1 4.5 5h15A1.5 1.5 0 0 1 21 6.5v9a1.5 1.5 0 0 1-1.5 1.5h-15A1.5 1.5 0 0 1 3 15.5zM8 20h8M12 17v3"/>"#);
}

/// The Lens P glyph (keeps its own white + cyan colors).
pub const MARK_GLYPH: (&str, &[u8]) =
    ("bytes://passthrify-mark-glyph.svg", include_bytes!("../../assets/logo/passthrify-mark-glyph.svg"));

/// egui 0.31 caches textures by URI alone, so an SVG first drawn at 16 px would be reused,
/// blurry, at 72 px. Giving each display size its own URI makes each one rasterize crisply.
fn sized_uri(uri: &str, size: f32) -> String {
    format!("{}@{}.svg", uri.trim_end_matches(".svg"), size.round() as u32)
}

pub fn icon_image(icon: (&'static str, &'static [u8]), size: f32, tint: Color32) -> egui::Image<'static> {
    egui::Image::from_bytes(sized_uri(icon.0, size), icon.1).fit_to_exact_size(Vec2::splat(size)).tint(tint)
}

pub fn mark_image(size: f32) -> egui::Image<'static> {
    egui::Image::from_bytes(sized_uri(MARK_GLYPH.0, size), MARK_GLYPH.1).fit_to_exact_size(Vec2::splat(size))
}

// --- Widgets -------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Cyan, the one main action (Save replay).
    Primary,
    /// Red fill (Stop recording).
    Danger,
    /// Raised + outline.
    Secondary,
    /// Text only.
    Ghost,
}

impl Kind {
    /// (fill, hover fill, pressed fill, stroke, text)
    fn colors(self) -> (Color32, Color32, Color32, Stroke, Color32) {
        match self {
            Kind::Primary => (SIGNAL, SIGNAL_HOVER, SIGNAL_DEEP, Stroke::NONE, ON_SIGNAL),
            Kind::Danger => (REC_FILL, REC_FILL_HOVER, hex(0xB82230), Stroke::NONE, Color32::WHITE),
            Kind::Secondary => (RAISED, HOVER, PRESSED, Stroke::new(1.0_f32, LINE), TEXT),
            Kind::Ghost => (Color32::TRANSPARENT, HOVER, PRESSED, Stroke::NONE, TEXT2),
        }
    }
}

/// A 32 px design-system button: optional icon, label, optional mono hint (e.g. "Alt F10").
pub fn button(ui: &mut Ui, kind: Kind, icon: Option<(&'static str, &'static [u8])>, label: &str, hint: Option<&str>) -> Response {
    button_ex(ui, kind, icon, None, label, hint)
}

/// [`button`] with an explicit icon color (e.g. the red dot on "Record").
pub fn button_ex(
    ui: &mut Ui,
    kind: Kind,
    icon: Option<(&'static str, &'static [u8])>,
    icon_tint: Option<Color32>,
    label: &str,
    hint: Option<&str>,
) -> Response {
    let (fill, hover, pressed, stroke, fg) = kind.colors();
    let tint = icon_tint.unwrap_or(fg);
    let family = if matches!(kind, Kind::Primary | Kind::Danger) { semibold() } else { medium() };
    let mut job = LayoutJob::default();
    if !label.is_empty() {
        job.append(label, 0.0, TextFormat { font_id: FontId::new(13.0, family), color: fg, ..Default::default() });
    }
    if let Some(h) = hint {
        let spacing = if label.is_empty() { 0.0 } else { 6.0 };
        job.append(h, spacing, TextFormat { font_id: FontId::new(11.5, mono_medium()), color: fg.gamma_multiply(0.75), ..Default::default() });
    }
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        w.inactive.weak_bg_fill = fill;
        w.hovered.weak_bg_fill = hover;
        w.active.weak_bg_fill = pressed;
        w.inactive.bg_stroke = stroke;
        w.hovered.bg_stroke = if stroke == Stroke::NONE { Stroke::NONE } else { Stroke::new(1.0_f32, LINE_STRONG) };
        w.active.bg_stroke = w.hovered.bg_stroke;
        let pad_x = if label.is_empty() && hint.is_none() { 8.0 } else { 12.0 };
        ui.spacing_mut().button_padding = Vec2::new(pad_x, 6.0);
        let btn = match icon {
            Some(i) if job.is_empty() => egui::Button::image(icon_image(i, 16.0, tint)),
            Some(i) => egui::Button::image_and_text(icon_image(i, 16.0, tint), job),
            None => egui::Button::new(job),
        };
        ui.add(btn.corner_radius(6).min_size(Vec2::new(32.0, 32.0)))
    })
    .inner
}

/// A full-width button of `height`: icon and label on the left, optional mono hint on the
/// right (the sidebar's Save replay / Record rows).
pub fn wide_button(
    ui: &mut Ui,
    kind: Kind,
    icon: (&'static str, &'static [u8]),
    icon_tint: Option<Color32>,
    label: &str,
    hint: Option<&str>,
    size: Vec2,
) -> Response {
    let (fill, hover, pressed, stroke, fg) = kind.colors();
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let enabled = ui.is_enabled();
    let bg = if !enabled {
        fill
    } else if resp.is_pointer_button_down_on() {
        pressed
    } else if resp.hovered() {
        hover
    } else {
        fill
    };
    let p = ui.painter();
    let fade = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };
    p.rect_filled(rect, 6, fade(bg));
    let stroke = if resp.hovered() && stroke != Stroke::NONE { Stroke::new(1.0_f32, LINE_STRONG) } else { stroke };
    if stroke != Stroke::NONE {
        p.rect_stroke(rect, 6, stroke, egui::StrokeKind::Inside);
    }
    let cy = rect.center().y;
    let icon_rect = egui::Rect::from_center_size(egui::pos2(rect.left() + 20.0, cy), Vec2::splat(16.0));
    icon_image(icon, 16.0, fade(icon_tint.unwrap_or(fg))).paint_at(ui, icon_rect);
    let family = if matches!(kind, Kind::Primary | Kind::Danger) { semibold() } else { medium() };
    p.text(egui::pos2(rect.left() + 36.0, cy), egui::Align2::LEFT_CENTER, label, FontId::new(13.0, family), fade(fg));
    if let Some(h) = hint {
        let c = fade(fg.gamma_multiply(0.75));
        p.text(egui::pos2(rect.right() - 12.0, cy), egui::Align2::RIGHT_CENTER, h, FontId::new(11.5, mono_medium()), c);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Square icon-only button of `size` px in the given style.
pub fn square_button(ui: &mut Ui, kind: Kind, icon: (&'static str, &'static [u8]), size: f32) -> Response {
    let (fill, hover, pressed, stroke, fg) = kind.colors();
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        w.inactive.weak_bg_fill = fill;
        w.hovered.weak_bg_fill = hover;
        w.active.weak_bg_fill = pressed;
        w.inactive.bg_stroke = stroke;
        w.hovered.bg_stroke = if stroke == Stroke::NONE { Stroke::NONE } else { Stroke::new(1.0_f32, LINE_STRONG) };
        w.active.bg_stroke = w.hovered.bg_stroke;
        ui.spacing_mut().button_padding = Vec2::splat((size - 16.0) / 2.0);
        ui.add(egui::Button::image(icon_image(icon, 16.0, fg)).corner_radius(6).min_size(Vec2::splat(size)))
    })
    .inner
}

/// Square ghost icon button with a tooltip.
pub fn icon_button(ui: &mut Ui, icon: (&'static str, &'static [u8]), tooltip: &str, active: bool) -> Response {
    let resp = button(ui, Kind::Ghost, Some(icon), "", None);
    if active {
        ui.painter().rect_stroke(resp.rect, 6, Stroke::new(1.0_f32, SIGNAL.gamma_multiply(0.6)), egui::StrokeKind::Inside);
    }
    resp.on_hover_text(tooltip)
}

/// iOS-style switch, 36x20.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let size = Vec2::new(36.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let track = if *on { SIGNAL } else if resp.hovered() { LINE_STRONG } else { LINE };
    let painter = ui.painter();
    painter.rect_filled(rect, 10, track);
    let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
    let knob = if *on { ON_SIGNAL } else { TEXT };
    painter.circle_filled(egui::pos2(x, rect.center().y), 7.0, knob);
    resp
}

/// Label + switch on one row, label left (wrapping, never under the switch), switch right.
pub fn toggle_row(ui: &mut Ui, label: &str, on: &mut bool) -> Response {
    ui.horizontal(|ui| {
        let gap = 12.0;
        let label_w = (ui.available_width() - 36.0 - gap).max(60.0);
        ui.allocate_ui_with_layout(Vec2::new(label_w, 20.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(label_w);
            ui.add(egui::Label::new(RichText::new(label).color(TEXT)).wrap());
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| toggle(ui, on)).inner
    })
    .inner
}

/// Pill segmented control. Returns true when the selection changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str, bool)]) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(BASE)
        .stroke(Stroke::new(1.0_f32, LINE))
        .corner_radius(8)
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let n = options.len().max(1) as f32;
            let w = ((ui.available_width() - 3.0 * (n - 1.0)) / n).max(56.0);
            ui.horizontal(|ui| {
                for (v, label, enabled) in options {
                    let selected = *value == *v;
                    let text = RichText::new(*label)
                        .family(if selected { semibold() } else { medium() })
                        .color(if selected { TEXT } else if *enabled { TEXT2 } else { TEXT3.gamma_multiply(0.6) });
                    let fill = if selected { HOVER } else { Color32::TRANSPARENT };
                    let resp = ui.scope(|ui| {
                        let wv = &mut ui.visuals_mut().widgets;
                        wv.inactive.weak_bg_fill = fill;
                        wv.inactive.bg_stroke = Stroke::NONE;
                        wv.hovered.weak_bg_fill = if selected { HOVER } else { RAISED };
                        wv.hovered.bg_stroke = Stroke::NONE;
                        ui.add_enabled(*enabled, egui::Button::new(text).corner_radius(6).min_size(Vec2::new(w, 28.0)))
                    });
                    if resp.inner.clicked() && !selected {
                        *value = *v;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// Small uppercase field label ("BUFFER LENGTH").
pub fn field_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).font(FontId::new(11.0, semibold())).color(TEXT2));
}

/// Keyboard key chip ("Alt", "F10").
pub fn key_chip(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(HOVER)
        .stroke(Stroke::new(1.0_f32, LINE))
        .corner_radius(4)
        .inner_margin(Margin::symmetric(7, 3))
        .show(ui, |ui| ui.label(RichText::new(text).font(mono(11.0)).color(TEXT)));
}

/// Small status dot.
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

pub fn fmt_clock(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_and_logo_rasterize() {
        for (name, svg) in [
            icons::REPLAY,
            icons::STOP,
            icons::CAMERA,
            icons::FOLDER,
            icons::SPEAKER,
            icons::MUTED,
            icons::FULLSCREEN,
            icons::EXIT_FULLSCREEN,
            icons::CHECK,
            icons::ALERT,
            icons::LOCK,
            icons::CLOSE,
            icons::SIDEBAR_HIDE,
            icons::SIDEBAR_SHOW,
            icons::BACK,
            icons::CHEVRON,
            icons::CAPTURE,
            icons::AUDIO,
            icons::HISTORY,
            icons::KEYBOARD,
            icons::DISPLAY,
            MARK_GLYPH,
        ] {
            let px = crate::svg_raster::rasterize(svg, 32, 32).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(px.chunks(4).any(|p| p[3] > 0), "{name} rendered empty");
        }
    }

    #[test]
    fn palette_matches_design() {
        assert_eq!(SIGNAL, Color32::from_rgb(0x5B, 0xD3, 0xFF));
        assert_eq!(REC, Color32::from_rgb(0xFF, 0x4D, 0x5E));
        assert_eq!(fmt_clock(83), "00:01:23");
    }
}
