//! SVG -> straight (non-premultiplied) RGBA. Shared by the app (window/tray icons) and build.rs
//! (exe .ico) via `#[path]`, so it must only depend on `resvg`.

use resvg::usvg::{self, TreeParsing};

/// Renders `svg` scaled to exactly `w`x`h` pixels.
pub fn rasterize(svg: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let rtree = resvg::Tree::from_usvg(&tree);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).ok_or("invalid size")?;
    let sx = w as f32 / tree.size.width();
    let sy = h as f32 / tree.size.height();
    rtree.render(resvg::tiny_skia::Transform::from_scale(sx, sy), &mut pixmap.as_mut());
    Ok(pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect())
}
