//! Custom window frame on Linux. The window is created without decorations (main.rs) and egui
//! draws the title bar; moving and resizing go through the compositor's interactive move /
//! resize (winit's drag_window / drag_resize_window), so they behave like native ones, snapping
//! and tiling included. Tiling compositors that size the window themselves simply ignore them.

use eframe::egui::{self, CursorIcon, ResizeDirection, ViewportCommand};

/// Same signature as the Windows frame; nothing to hook on Linux.
pub fn install(_hwnd: isize) {}

/// Same signature as the Windows frame; the layout is handled by [`title_bar`] and
/// [`resize_edges`] instead of a window procedure.
pub fn set_layout(_caption_height: i32, _caption_buttons: i32, _resizable: bool) {}

/// Makes the title bar's empty area move the window (drag) and maximize it (double-click).
pub fn title_bar(resp: &egui::Response, maximized: bool) {
    if resp.double_clicked() {
        resp.ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    } else if resp.drag_started_by(egui::PointerButton::Primary) {
        resp.ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
}

/// Width of the grab area along each window edge, in points.
const EDGE: f32 = 5.0;

/// Resize from the window edges and corners (call while the window is neither maximized nor
/// fullscreen). Shows the matching cursor and hands the press to the compositor.
pub fn resize_edges(ctx: &egui::Context) {
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else { return };
    let Some(dir) = edge_at(ctx.screen_rect(), pos) else { return };
    ctx.set_cursor_icon(match dir {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
    });
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
    }
}

fn edge_at(r: egui::Rect, p: egui::Pos2) -> Option<ResizeDirection> {
    let (left, right) = (p.x - r.left() < EDGE, r.right() - p.x < EDGE);
    let (top, bottom) = (p.y - r.top() < EDGE, r.bottom() - p.y < EDGE);
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => ResizeDirection::NorthWest,
        (_, true, true, _) => ResizeDirection::NorthEast,
        (true, _, _, true) => ResizeDirection::SouthWest,
        (_, true, _, true) => ResizeDirection::SouthEast,
        (true, ..) => ResizeDirection::West,
        (_, true, ..) => ResizeDirection::East,
        (_, _, true, _) => ResizeDirection::North,
        (.., true) => ResizeDirection::South,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_and_corners() {
        let r = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        assert!(matches!(edge_at(r, egui::pos2(1.0, 1.0)), Some(ResizeDirection::NorthWest)));
        assert!(matches!(edge_at(r, egui::pos2(799.0, 599.0)), Some(ResizeDirection::SouthEast)));
        assert!(matches!(edge_at(r, egui::pos2(400.0, 2.0)), Some(ResizeDirection::North)));
        assert!(matches!(edge_at(r, egui::pos2(2.0, 300.0)), Some(ResizeDirection::West)));
        assert!(edge_at(r, egui::pos2(400.0, 300.0)).is_none());
    }
}
