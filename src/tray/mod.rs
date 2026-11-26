//! System tray icon + menu (Save replay, Record, Stream, Screenshot, Open folder, Quit).
//! Windows: tray-icon with Win32 window show/hide. Linux: see `linux.rs`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(windows)]
pub use win::*;

/// "Lens P" on its dark tile (window / taskbar / exe icon).
const MARK_SVG: &[u8] = include_bytes!("../../assets/logo/passthrify-mark.svg");
/// Pixel-snapped 16 px glyph for the notification area.
const TRAY_SVG: &[u8] = include_bytes!("../../assets/logo/passthrify-tray-16.svg");

/// App icon as straight RGBA, `size`x`size`.
pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    crate::svg_raster::rasterize(MARK_SVG, size, size).unwrap_or_else(|e| {
        tracing::warn!("app icon: {e}");
        vec![0; (size * size * 4) as usize]
    })
}

/// Tray glyph rendered at 2x (32 px) so it stays crisp at 125-200% scaling.
#[allow(dead_code)]
pub(crate) fn tray_icon_rgba() -> (Vec<u8>, u32) {
    let size = 32;
    let rgba = crate::svg_raster::rasterize(TRAY_SVG, size, size).unwrap_or_else(|_| app_icon_rgba(size));
    (rgba, size)
}

