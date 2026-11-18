use std::path::Path;

use anyhow::{Context, Result};

use crate::capture::VideoFrame;

/// NV12 (BT.709, limited range) -> packed RGB8.
pub fn nv12_to_rgb(f: &VideoFrame) -> Vec<u8> {
    let (w, h) = (f.width as usize, f.height as usize);
    let (yp, uvp) = (f.y_plane(), f.uv_plane());
    let mut out = vec![0u8; w * h * 3];
    for (row, out_row) in out.chunks_exact_mut(w * 3).enumerate() {
        let y_row = &yp[row * w..(row + 1) * w];
        let uv_row = &uvp[(row / 2) * w..(row / 2 + 1) * w];
        for (x, px) in out_row.chunks_exact_mut(3).enumerate() {
            let y = 1.164 * (y_row[x] as f32 - 16.0);
            let u = uv_row[x & !1] as f32 - 128.0;
            let v = uv_row[(x & !1) + 1] as f32 - 128.0;
            px[0] = (y + 1.793 * v).clamp(0.0, 255.0) as u8;
            px[1] = (y - 0.213 * u - 0.533 * v).clamp(0.0, 255.0) as u8;
            px[2] = (y + 2.112 * u).clamp(0.0, 255.0) as u8;
        }
    }
    out
}

pub fn save_png(f: &VideoFrame, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let rgb = nv12_to_rgb(f);
    image::save_buffer(path, &rgb, f.width, f.height, image::ExtendedColorType::Rgb8)
        .with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::FramePool;
    use std::sync::Arc;

    #[test]
    fn converts_reference_colors() {
        // 2x2 frame: white luma with neutral chroma -> white; black -> black.
        let pool = Arc::new(FramePool::default());
        let mut buf = pool.get(6);
        buf.as_mut_slice().copy_from_slice(&[235, 235, 16, 16, 128, 128]);
        let f = VideoFrame { width: 2, height: 2, data: buf, pts_us: 0, seq: 0 };
        let rgb = nv12_to_rgb(&f);
        assert!(rgb[..6].iter().all(|&c| c >= 254), "{rgb:?}");
        assert!(rgb[6..].iter().all(|&c| c <= 1), "{rgb:?}");
    }
}
