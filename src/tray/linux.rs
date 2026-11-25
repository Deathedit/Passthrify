//! Linux tray: a StatusNotifierItem over D-Bus (ksni), shown by bars like Waybar, KDE's panel
//! or GNOME with the AppIndicator extension. Same menu as on Windows.
//!
//! Wayland apps can't hide or raise their own window the way Win32 does, so "Show window" asks
//! egui to un-minimize and focus it (the compositor decides), and Quit closes it through egui
//! so recordings are finished as on a normal close.

use anyhow::{Context as _, Result};
use crossbeam_channel::Sender;
use eframe::egui;
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};

use crate::pipeline::Command;

pub struct Tray {
    _handle: ksni::blocking::Handle<SniTray>,
}

/// Native title bar styling: nothing to do on Linux.
pub fn style_title_bar(_hwnd: isize) {}

pub fn create(ctx: &egui::Context, cmd: Sender<Command>) -> Result<Tray> {
    let tray = SniTray { ctx: ctx.clone(), cmd, icons: icons() };
    let handle = tray.spawn().context("registering the tray icon on D-Bus")?;
    Ok(Tray { _handle: handle })
}

pub struct SniTray {
    ctx: egui::Context,
    cmd: Sender<Command>,
    icons: Vec<ksni::Icon>,
}

impl SniTray {
    fn show_window(&self) {
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.ctx.request_repaint();
    }

    fn send(&self, c: Command) {
        let _ = self.cmd.send(c);
    }
}

impl ksni::Tray for SniTray {
    fn id(&self) -> String {
        "passthrify".into()
    }

    fn title(&self) -> String {
        "Passthrify".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "Passthrify".into(), ..Default::default() }
    }

    /// Left click.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.show_window();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        fn item(label: &str, f: impl Fn(&mut SniTray) + Send + 'static) -> MenuItem<SniTray> {
            StandardItem { label: label.into(), activate: Box::new(f), ..Default::default() }.into()
        }
        vec![
            item("Show window", |t| t.show_window()),
            MenuItem::Separator,
            item("Save instant replay", |t| t.send(Command::SaveReplay)),
            item("Start / stop recording", |t| t.send(Command::ToggleRecord)),
            item("Start / stop streaming", |t| t.send(Command::ToggleStream)),
            item("Screenshot", |t| t.send(Command::Screenshot)),
            item("Open output folder", |t| t.send(Command::OpenOutputFolder)),
            MenuItem::Separator,
            item("Quit", |t| {
                // A window the compositor isn't showing gets no frames, so it wouldn't act on
                // Close until shown; bring it up first (Windows does the same).
                t.show_window();
                t.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }),
        ]
    }
}

/// The tray glyph at 1x and 2x, as ARGB32 in network byte order.
fn icons() -> Vec<ksni::Icon> {
    [16, 32]
        .into_iter()
        .filter_map(|size| {
            let rgba = crate::svg_raster::rasterize(super::TRAY_SVG, size, size).ok()?;
            let data = rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
            Some(ksni::Icon { width: size as i32, height: size as i32, data })
        })
        .collect()
}
