//! Custom window frame. The native caption and border are removed (WM_NCCALCSIZE) so egui can
//! draw a title bar and border in the app's own colors, which Windows 10 doesn't allow
//! otherwise. The window stays a normal overlapped window, so it keeps its shadow, snapping,
//! minimize/maximize animations and resize borders: WM_NCHITTEST tells Windows which parts of
//! the egui-drawn title bar drag the window and where the resize edges are.
//!
//! egui reports the title bar layout every frame through [`set_layout`].

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GetWindowRect, IsZoomed, SetWindowLongPtrW, SetWindowPos, GWLP_WNDPROC, HTBOTTOM, HTBOTTOMLEFT,
    HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, NCCALCSIZE_PARAMS,
    SM_CXFRAME, SM_CXPADDEDBORDER, SM_CYFRAME, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, WM_NCCALCSIZE, WM_NCHITTEST, WNDPROC,
};

/// winit's window procedure, which ours forwards everything else to.
static ORIGINAL_PROC: AtomicIsize = AtomicIsize::new(0);
/// Title bar height in physical px (0 = none, e.g. fullscreen).
static CAPTION_HEIGHT: AtomicI32 = AtomicI32::new(0);
/// Width in physical px at the title bar's right end that egui handles itself (its buttons).
static CAPTION_BUTTONS: AtomicI32 = AtomicI32::new(0);
/// Resize edges active (off while maximized or fullscreen).
static RESIZABLE: AtomicBool = AtomicBool::new(true);

/// Replaces the native frame on `hwnd` (winit's main window). Safe to call once.
pub fn install(hwnd: isize) {
    if hwnd == 0 || ORIGINAL_PROC.load(Ordering::Relaxed) != 0 {
        return;
    }
    let h = HWND(hwnd as *mut _);
    unsafe {
        let original = SetWindowLongPtrW(h, GWLP_WNDPROC, wndproc as *const () as isize);
        if original == 0 {
            tracing::warn!("custom frame: could not hook the window procedure");
            return;
        }
        ORIGINAL_PROC.store(original, Ordering::Relaxed);
        // A 1 px DWM frame keeps the native drop shadow on the borderless window.
        let margins = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 1, cyBottomHeight: 0 };
        let _ = DwmExtendFrameIntoClientArea(h, &margins);
        // Make Windows recompute the (now empty) non-client area.
        let _ = SetWindowPos(
            h,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Called by egui every frame: title bar height and the width of its buttons (physical px),
/// and whether the window edges resize.
pub fn set_layout(caption_height: i32, caption_buttons: i32, resizable: bool) {
    CAPTION_HEIGHT.store(caption_height, Ordering::Relaxed);
    CAPTION_BUTTONS.store(caption_buttons, Ordering::Relaxed);
    RESIZABLE.store(resizable, Ordering::Relaxed);
}

/// Frame thickness Windows uses for this window's DPI (resize grip, maximize overhang).
unsafe fn frame_thickness(h: HWND) -> (i32, i32) {
    let dpi = GetDpiForWindow(h);
    let pad = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
    (GetSystemMetricsForDpi(SM_CXFRAME, dpi) + pad, GetSystemMetricsForDpi(SM_CYFRAME, dpi) + pad)
}

/// Client rect for a maximized-flagged window: one that overhangs its monitor (a real
/// maximize) is clipped to the work area; one inside the monitor (fullscreen) is left alone.
fn maximized_client(r: RECT, monitor: RECT, work: RECT) -> RECT {
    let overhangs =
        r.left < monitor.left || r.top < monitor.top || r.right > monitor.right || r.bottom > monitor.bottom;
    if !overhangs {
        return r;
    }
    RECT {
        left: r.left.max(work.left),
        top: r.top.max(work.top),
        right: r.right.min(work.right),
        bottom: r.bottom.min(work.bottom),
    }
}

/// Which part of the window a screen point is over, for WM_NCHITTEST.
fn hit_test(x: i32, y: i32, w: i32, h: i32, grip: (i32, i32), top_grip: i32) -> u32 {
    if RESIZABLE.load(Ordering::Relaxed) {
        let (left, right) = (x < grip.0, x >= w - grip.0);
        let (top, bottom) = (y < top_grip, y >= h - grip.1);
        match (top, bottom, left, right) {
            (true, _, true, _) => return HTTOPLEFT,
            (true, _, _, true) => return HTTOPRIGHT,
            (_, true, true, _) => return HTBOTTOMLEFT,
            (_, true, _, true) => return HTBOTTOMRIGHT,
            (true, ..) => return HTTOP,
            (_, true, ..) => return HTBOTTOM,
            (_, _, true, _) => return HTLEFT,
            (_, _, _, true) => return HTRIGHT,
            _ => {}
        }
    }
    if y < CAPTION_HEIGHT.load(Ordering::Relaxed) && x < w - CAPTION_BUTTONS.load(Ordering::Relaxed) {
        return HTCAPTION;
    }
    HTCLIENT
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // Client area = the whole window. Maximized windows overhang the monitor by the frame
        // thickness, so pull them back to the work area. A window can also be flagged maximized
        // while fullscreen (winit keeps the flag), but then it covers the monitor exactly and
        // must keep every pixel.
        WM_NCCALCSIZE if wp.0 != 0 => {
            if IsZoomed(h).as_bool() {
                let params = &mut *(lp.0 as *mut NCCALCSIZE_PARAMS);
                let r = &mut params.rgrc[0];
                let mon = MonitorFromRect(r, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                if GetMonitorInfoW(mon, &mut info).as_bool() {
                    *r = maximized_client(*r, info.rcMonitor, info.rcWork);
                }
            }
            return LRESULT(0);
        }
        WM_NCHITTEST => {
            let (sx, sy) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
            let mut r = RECT::default();
            if GetWindowRect(h, &mut r).is_ok() {
                let grip = frame_thickness(h);
                let dpi = GetDpiForWindow(h) as i32;
                let top_grip = (4 * dpi / 96).max(2); // thin, so it doesn't eat the title bar
                let hit = hit_test(sx - r.left, sy - r.top, r.right - r.left, r.bottom - r.top, grip, top_grip);
                return LRESULT(hit as isize);
            }
        }
        _ => {}
    }
    let original: WNDPROC = std::mem::transmute::<isize, WNDPROC>(ORIGINAL_PROC.load(Ordering::Relaxed));
    CallWindowProcW(original, h, msg, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test_regions() {
        // 1000x600 window, 8 px side grips, 4 px top grip, 32 px title bar, 138 px of buttons.
        set_layout(32, 138, true);
        let ht = |x, y| hit_test(x, y, 1000, 600, (8, 8), 4);
        assert_eq!(ht(2, 2), HTTOPLEFT);
        assert_eq!(ht(500, 1), HTTOP);
        assert_eq!(ht(999, 300), HTRIGHT);
        assert_eq!(ht(500, 599), HTBOTTOM);
        assert_eq!(ht(400, 16), HTCAPTION, "title bar drags the window");
        assert_eq!(ht(950, 16), HTCLIENT, "the buttons are egui's");
        assert_eq!(ht(400, 300), HTCLIENT);

        set_layout(32, 138, false); // maximized: no resize edges, the title bar still drags
        let ht = |x, y| hit_test(x, y, 1000, 600, (8, 8), 4);
        assert_eq!(ht(500, 1), HTCAPTION);
        assert_eq!(ht(2, 300), HTCLIENT);

        set_layout(0, 0, false); // fullscreen: all client
        assert_eq!(hit_test(500, 1, 1000, 600, (8, 8), 4), HTCLIENT);
    }

    #[test]
    fn maximized_vs_fullscreen_client() {
        let rect = |left, top, right, bottom| RECT { left, top, right, bottom };
        let monitor = rect(0, 0, 3440, 1440);
        let work = rect(0, 0, 3440, 1400); // taskbar at the bottom
        // Maximized: overhangs by the frame -> clipped to the work area (taskbar stays visible).
        assert_eq!(maximized_client(rect(-8, -8, 3448, 1408), monitor, work), work);
        // Fullscreen while flagged maximized: exactly the monitor -> untouched.
        assert_eq!(maximized_client(monitor, monitor, work), monitor);
    }
}
