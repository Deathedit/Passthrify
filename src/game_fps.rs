//! Game frame rate from the capture signal.
//!
//! A game rendering slower than the HDMI refresh makes the console resend the previous
//! image, so counting only frames whose picture changed gives the rate the game actually
//! renders at.
//!
//! Repeats aren't bit-identical in practice: consoles/capture cards add temporal dithering
//! (a static PS5 screen through an Elgato 4K X changes ~25% of pixels by 1-3 levels every
//! frame, on a 16-frame cycle). That noise is near zero-mean, so frames are compared by
//! small block averages, which cancel it, rather than by single pixels.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::capture::VideoFrame;

/// Blocks are `BLOCK_W` pixels wide and 2 rows tall (rows y, y+1), taken every `ROW_STEP`
/// rows: a quarter of the luma plane, across the full width.
const BLOCK_W: usize = 16;
const ROW_STEP: usize = 8;
/// A block changed when its mean luma moved by more than this many levels. Dither noise
/// measured up to ~1.9; real motion moves blocks by tens of levels.
const BLOCK_MEAN_DELTA: i32 = 3;
/// Changed blocks needed before a frame counts as new.
const MIN_BLOCKS: usize = 3;

fn sum(px: &[u8]) -> i32 {
    px.iter().map(|&v| v as i32).sum()
}

/// Whether two packed 8-bit luma planes of `width` x `height` show different pictures.
pub fn luma_differs(a: &[u8], b: &[u8], width: usize, height: usize) -> bool {
    let threshold = BLOCK_MEAN_DELTA * (BLOCK_W as i32) * 2;
    fn row(p: &[u8], width: usize, y: usize) -> &[u8] {
        &p[y * width..(y + 1) * width]
    }
    let row = |p, y| row(p, width, y);
    let mut changed = 0;
    for y in (0..height.saturating_sub(1)).step_by(ROW_STEP) {
        let (a0, a1, b0, b1) = (row(a, y), row(a, y + 1), row(b, y), row(b, y + 1));
        if a0 == b0 && a1 == b1 {
            continue; // bit-identical repeat (clean sources): a straight memcmp
        }
        let blocks = a0
            .chunks_exact(BLOCK_W)
            .zip(a1.chunks_exact(BLOCK_W))
            .zip(b0.chunks_exact(BLOCK_W).zip(b1.chunks_exact(BLOCK_W)));
        for ((p0, p1), (q0, q1)) in blocks {
            let delta = sum(q0) + sum(q1) - sum(p0) - sum(p1);
            if delta.abs() > threshold {
                changed += 1;
                if changed >= MIN_BLOCKS {
                    return true;
                }
            }
        }
    }
    false
}

/// Counts signal frames and new (changed) frames over a rolling one-second window.
pub struct FrameRates {
    prev: Option<Arc<VideoFrame>>,
    since: Instant,
    frames: u32,
    unique: u32,
}

/// Rates over the last window, in frames per second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    pub signal: f64,
    pub game: f64,
}

impl Default for FrameRates {
    fn default() -> Self {
        Self { prev: None, since: Instant::now(), frames: 0, unique: 0 }
    }
}

impl FrameRates {
    /// Feeds one captured frame; returns fresh rates once a second.
    pub fn push(&mut self, frame: &Arc<VideoFrame>) -> Option<Rates> {
        let new = match &self.prev {
            Some(p) if p.width == frame.width && p.height == frame.height => {
                luma_differs(p.y_plane(), frame.y_plane(), frame.width as usize, frame.height as usize)
            }
            _ => true,
        };
        self.prev = Some(frame.clone());
        self.frames += 1;
        self.unique += new as u32;

        let el = self.since.elapsed();
        if el < Duration::from_secs(1) {
            return None;
        }
        let secs = el.as_secs_f64();
        let rates = Rates { signal: self.frames as f64 / secs, game: self.unique as f64 / secs };
        self.frames = 0;
        self.unique = 0;
        self.since = Instant::now();
        Some(rates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 128;
    const H: usize = 72;

    fn plane(v: u8) -> Vec<u8> {
        vec![v; W * H]
    }

    /// Paints `blocks` adjacent sampled blocks (rows 0-1) brighter.
    fn with_object(base: &[u8], blocks: usize) -> Vec<u8> {
        let mut p = base.to_vec();
        for y in 0..2 {
            p[y * W..y * W + blocks * BLOCK_W].iter_mut().for_each(|v| *v += 100);
        }
        p
    }

    #[test]
    fn identical_frames_are_repeats() {
        assert!(!luma_differs(&plane(80), &plane(80), W, H));
    }

    #[test]
    fn dither_noise_is_ignored() {
        let a = plane(80);
        // Zero-mean +-3 checkerboard on every pixel.
        let dither: Vec<u8> = (0..W * H).map(|i| if (i + i / W) % 2 == 0 { 83 } else { 77 }).collect();
        assert!(!luma_differs(&a, &dither, W, H));
        // A small uniform bias, below the block threshold.
        assert!(!luma_differs(&a, &plane(82), W, H));
    }

    #[test]
    fn real_changes_count() {
        let a = plane(80);
        assert!(luma_differs(&a, &with_object(&a, MIN_BLOCKS), W, H));
        assert!(!luma_differs(&a, &with_object(&a, MIN_BLOCKS - 1), W, H));
        assert!(luma_differs(&a, &plane(120), W, H)); // scene cut / big fade step
    }
}
