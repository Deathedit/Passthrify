//! System tray icon + menu. Window show/hide uses Win32 directly so it works even while eframe
//! isn't repainting a hidden window.

use anyhow::Result;
use crossbeam_channel::{select, Sender};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindowVisible, PostMessageW, SetForegroundWindow, ShowWindow, SW_HIDE, SW_RESTORE, WM_CLOSE,
};

use crate::pipeline::Command;

pub struct Tray {
    _icon: TrayIcon,
}

/// "Lens P" on its dark tile (window / taskbar / exe icon).
const MARK_SVG: &[u8] = include_bytes!("../assets/logo/passthrify-mark.svg");
/// Pixel-snapped 16 px glyph for the notification area.
const TRAY_SVG: &[u8] = include_bytes!("../assets/logo/passthrify-tray-16.svg");

/// App icon as straight RGBA, `size`x`size`.
pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    crate::svg_raster::rasterize(MARK_SVG, size, size).unwrap_or_else(|e| {
        tracing::warn!("app icon: {e}");
        vec![0; (size * size * 4) as usize]
    })
}

/// Tray glyph rendered at 2x (32 px) so it stays crisp at 125-200% scaling.
fn tray_icon_rgba() -> (Vec<u8>, u32) {
    let size = 32;
    let rgba = crate::svg_raster::rasterize(TRAY_SVG, size, size).unwrap_or_else(|_| app_icon_rgba(size));
    (rgba, size)
}

/// Dark title bar; on Windows 11 also paints the caption in the app's base color.
pub fn style_title_bar(hwnd: isize) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
    };
    if hwnd == 0 {
        return;
    }
    let h = HWND(hwnd as *mut _);
    // COLORREF is 0x00BBGGRR.
    let colorref = |c: eframe::egui::Color32| u32::from(c.r()) | (u32::from(c.g()) << 8) | (u32::from(c.b()) << 16);
    unsafe {
        let on: i32 = 1;
        let _ = DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as *const _, 4);
        for (attr, color) in [
            (DWMWA_CAPTION_COLOR, crate::ui::theme::BASE),
            (DWMWA_TEXT_COLOR, crate::ui::theme::TEXT2),
            (DWMWA_BORDER_COLOR, crate::ui::theme::LINE),
        ] {
            let c = colorref(color);
            let _ = DwmSetWindowAttribute(h, attr, &c as *const _ as *const _, 4);
        }
    }
}

pub fn toggle_window(hwnd: isize) {
    unsafe {
        let h = HWND(hwnd as *mut _);
        if IsWindowVisible(h).as_bool() {
            let _ = ShowWindow(h, SW_HIDE);
        } else {
            show_window(hwnd);
        }
    }
}

pub fn show_window(hwnd: isize) {
    unsafe {
        let h = HWND(hwnd as *mut _);
        let _ = ShowWindow(h, SW_RESTORE);
        let _ = SetForegroundWindow(h);
    }
}

pub fn create(hwnd: isize, cmd: Sender<Command>) -> Result<Tray> {
    let menu = Menu::new();
    let show = MenuItem::new("Show / hide window", true, None);
    let save = MenuItem::new("Save instant replay", true, None);
    let rec = MenuItem::new("Start / stop recording", true, None);
    let shot = MenuItem::new("Screenshot", true, None);
    let open = MenuItem::new("Open output folder", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append_items(&[
        &show,
        &PredefinedMenuItem::separator(),
        &save,
        &rec,
        &shot,
        &open,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;

    let (rgba, size) = tray_icon_rgba();
    let icon = tray_icon::Icon::from_rgba(rgba, size, size)?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Passthrify")
        .with_icon(icon)
        .build()?;

    let ids = (
        show.id().clone(),
        save.id().clone(),
        rec.id().clone(),
        shot.id().clone(),
        open.id().clone(),
        quit.id().clone(),
    );
    std::thread::Builder::new().name("tray".into()).spawn(move || {
        let (show, save, rec, shot, open, quit) = ids;
        let menu_rx = MenuEvent::receiver();
        let tray_rx = TrayIconEvent::receiver();
        loop {
            select! {
                recv(menu_rx) -> ev => {
                    let Ok(ev) = ev else { break };
                    let c = if ev.id == show {
                        toggle_window(hwnd);
                        None
                    } else if ev.id == save {
                        Some(Command::SaveReplay)
                    } else if ev.id == rec {
                        Some(Command::ToggleRecord)
                    } else if ev.id == shot {
                        Some(Command::Screenshot)
                    } else if ev.id == open {
                        Some(Command::OpenOutputFolder)
                    } else if ev.id == quit {
                        show_window(hwnd);
                        unsafe {
                            let _ = PostMessageW(HWND(hwnd as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                        None
                    } else {
                        None
                    };
                    if let Some(c) = c {
                        if cmd.send(c).is_err() {
                            break;
                        }
                    }
                },
                recv(tray_rx) -> ev => {
                    let Ok(ev) = ev else { break };
                    if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = ev {
                        toggle_window(hwnd);
                    }
                },
            }
        }
    })?;
    Ok(Tray { _icon: tray })
}
