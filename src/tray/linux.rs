//! Linux tray: not wired up yet (Phase 2: StatusNotifierItem over D-Bus).

use anyhow::{bail, Result};
use crossbeam_channel::Sender;

use crate::pipeline::Command;

pub struct Tray;

/// Native title bar styling: nothing to do (egui draws the title bar).
pub fn style_title_bar(_hwnd: isize) {}

pub fn create(_hwnd: isize, _cmd: Sender<Command>) -> Result<Tray> {
    bail!("the tray icon isn't available on Linux yet")
}
