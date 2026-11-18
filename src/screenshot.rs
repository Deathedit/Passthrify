use std::path::Path;

use anyhow::{Context, Result};

use crate::capture::VideoFrame;

/// Y + interleaved UV (NV12/NV16/NV24) -> packed RGB8, honoring the frame's range and matrix.
pub fn nv12_to_rgb(f: &VideoFrame) -> Vec<u8> {
    let (w, h) = (f.width as usize, f.height as usize);
    let (sx, sy) = f.chroma.subsampling();
    let (sx, sy) = (sx as usize, sy as usize);
    let uv_stride = f.chroma_size().0 as usize * 2;
    let (yp, uvp) = (f.y_plane(), f.uv_plane());
    let mut out = vec![0u8; w * h * 3];
    for (row, out_row) in out.chunks_exact_mut(w * 3).enumerate() {
        let y_row = &yp[row * w..(row + 1) * w];
        let uv_row = &uvp[(row / sy) * uv_stride..(row / sy + 1) * uv_stride];
        for (x, px) in out_row.chunks_exact_mut(3).enumerate() {
            let c = (x / sx) * 2;
            px.copy_from_slice(&f.color.to_rgb(y_row[x], uv_row[c], uv_row[c + 1]));
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
        let f = VideoFrame {
            width: 2,
            height: 2,
            chroma: crate::capture::Chroma::S420,
            color: crate::color::ColorInfo::default(),
            data: buf,
            pts_us: 0,
            seq: 0,
        };
        let rgb = nv12_to_rgb(&f);
        assert!(rgb[..6].iter().all(|&c| c >= 254), "{rgb:?}");
        assert!(rgb[6..].iter().all(|&c| c <= 1), "{rgb:?}");
    }

    #[test]
    fn full_range_frames_keep_shadow_detail() {
        // Y=16 is pure black in limited range but dark grey in full range.
        let pool = Arc::new(FramePool::default());
        let mut buf = pool.get(6);
        buf.as_mut_slice().copy_from_slice(&[16, 16, 16, 16, 128, 128]);
        let full = crate::color::ColorInfo { full_range: true, matrix: crate::color::Matrix::Bt601 };
        let f = VideoFrame {
            width: 2,
            height: 2,
            chroma: crate::capture::Chroma::S420,
            color: full,
            data: buf,
            pts_us: 0,
            seq: 0,
        };
        assert_eq!(&nv12_to_rgb(&f)[..3], &[16, 16, 16]);
    }
}
