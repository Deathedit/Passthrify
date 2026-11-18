//! Low-latency passthrough buffer between the capture and render threads.
//!
//! The two endpoints run on different clocks, so the fill level slowly drifts. We keep it near
//! a target by dropping a frame now and then when it runs high, and re-priming (brief silence)
//! after an underrun.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use parking_lot::Mutex;

use crate::capture::{AUDIO_CHANNELS, AUDIO_RATE};

pub struct PassRing {
    q: Mutex<VecDeque<f32>>,
    primed: AtomicBool,
    /// f32 bits
    volume: AtomicU32,
    muted: AtomicBool,
    target: usize,
}

const TARGET_MS: usize = 30;

impl PassRing {
    pub fn new(volume: f32, muted: bool) -> Self {
        let target = AUDIO_RATE as usize * TARGET_MS / 1000 * AUDIO_CHANNELS;
        Self {
            q: Mutex::new(VecDeque::with_capacity(target * 8)),
            primed: AtomicBool::new(false),
            volume: AtomicU32::new(volume.to_bits()),
            muted: AtomicBool::new(muted),
            target,
        }
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        self.q.lock().clear();
        self.primed.store(false, Ordering::Relaxed);
    }

    pub fn push(&self, samples: &[f32]) {
        let mut q = self.q.lock();
        q.extend(samples.iter().copied());
        // Way over target (render stalled or device switched): jump back to target.
        if q.len() > self.target * 4 {
            let excess = q.len() - self.target;
            q.drain(..excess - excess % AUDIO_CHANNELS);
        }
    }

    #[cfg(test)]
    fn fill_frames(&self) -> usize {
        self.q.lock().len() / AUDIO_CHANNELS
    }

    /// Fills `out` (interleaved) entirely, padding with silence when starved.
    pub fn pop_into(&self, out: &mut [f32]) {
        let mut q = self.q.lock();
        if !self.primed.load(Ordering::Relaxed) {
            if q.len() >= self.target {
                self.primed.store(true, Ordering::Relaxed);
            } else {
                out.fill(0.0);
                return;
            }
        }
        // Gentle drift correction: drop one frame per period while running high.
        if q.len() > self.target * 3 / 2 + out.len() {
            for _ in 0..AUDIO_CHANNELS {
                q.pop_front();
            }
        }
        let gain = if self.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        };
        let n = out.len().min(q.len());
        for (o, s) in out[..n].iter_mut().zip(q.drain(..n)) {
            *o = s * gain;
        }
        if n < out.len() {
            out[n..].fill(0.0);
            self.primed.store(false, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primes_before_playing() {
        let r = PassRing::new(1.0, false);
        r.push(&[0.5; 100]);
        let mut out = [1.0f32; 64];
        r.pop_into(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        r.push(&vec![0.5; r.target]);
        r.pop_into(&mut out);
        assert!(out.iter().all(|&s| s == 0.5));
    }

    #[test]
    fn bounded_when_render_stalls() {
        let r = PassRing::new(1.0, false);
        for _ in 0..1000 {
            r.push(&[0.1; 960]);
        }
        assert!(r.fill_frames() * AUDIO_CHANNELS <= r.target * 4);
    }

    #[test]
    fn volume_and_mute() {
        let r = PassRing::new(0.5, false);
        r.push(&vec![1.0; r.target * 2]);
        let mut out = [0.0f32; 8];
        r.pop_into(&mut out);
        assert_eq!(out[0], 0.5);
        r.set_muted(true);
        r.pop_into(&mut out);
        assert_eq!(out[0], 0.0);
    }
}
