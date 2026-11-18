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

pub fn icon_rgba(size: u32) -> Vec<u8> {
    // Red record dot with a soft white ring.
    let mut px = vec![0u8; (size * size * 4) as usize];
    let c = (size as f32 - 1.0) / 2.0;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() / c;
            let i = ((y * size + x) * 4) as usize;
            let (rgb, a) = if d <= 0.62 {
                ([230, 40, 50], 255)
            } else if d <= 0.8 {
                ([20, 20, 24], 255)
            } else if d <= 1.0 {
                ([235, 235, 240], 255)
            } else {
                ([0, 0, 0], 0)
            };
            px[i..i + 3].copy_from_slice(&rgb);
            px[i + 3] = a;
        }
    }
    px
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

    let icon = tray_icon::Icon::from_rgba(icon_rgba(32), 32, 32)?;
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
