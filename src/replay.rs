//! Rolling in-memory buffer of encoded packets (the "instant replay").
//!
//! Video packets arrive in decode order. The buffer keeps at least `window_us` of video, and
//! always starts on a keyframe so a snapshot can be muxed without re-encoding. Audio is
//! trimmed to start no earlier than the first retained video frame.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::packets::{EncodedPacket, StreamKind};

pub struct ReplayBuffer {
    window_us: i64,
    video: VecDeque<Arc<EncodedPacket>>,
    audio: VecDeque<Arc<EncodedPacket>>,
    bytes: usize,
}

/// A consistent copy of the buffer contents (cheap: packets are reference-counted).
#[derive(Default, Clone)]
pub struct Clip {
    pub video: Vec<Arc<EncodedPacket>>,
    pub audio: Vec<Arc<EncodedPacket>>,
}

impl Clip {
    pub fn duration_us(&self) -> i64 {
        match (self.video.first(), self.video.last()) {
            (Some(a), Some(b)) => b.pts_us.max(b.dts_us) - a.dts_us + b.duration_us,
            _ => 0,
        }
    }
}

impl ReplayBuffer {
    pub fn new(window_us: i64) -> Self {
        Self { window_us, video: VecDeque::new(), audio: VecDeque::new(), bytes: 0 }
    }

    #[cfg(test)]
    fn set_window(&mut self, window_us: i64) {
        self.window_us = window_us;
        self.trim();
    }

    pub fn clear(&mut self) {
        self.video.clear();
        self.audio.clear();
        self.bytes = 0;
    }

    pub fn push(&mut self, pkt: Arc<EncodedPacket>) {
        self.bytes += pkt.data.len();
        match pkt.kind {
            StreamKind::Video => self.video.push_back(pkt),
            StreamKind::Audio => self.audio.push_back(pkt),
        }
        self.trim();
    }

    fn trim(&mut self) {
        if let Some(newest) = self.video.back().map(|p| p.dts_us) {
            let cutoff = newest - self.window_us;
            // Drop everything before the last keyframe at or before the cutoff. This keeps
            // between `window` and `window + GOP` of video.
            let mut drop_to = None;
            for (i, p) in self.video.iter().enumerate().skip(1) {
                if p.dts_us > cutoff {
                    break;
                }
                if p.key {
                    drop_to = Some(i);
                }
            }
            if let Some(n) = drop_to {
                for p in self.video.drain(..n) {
                    self.bytes -= p.data.len();
                }
            }
        }

        // Audio: start at the first retained video frame; without video, keep its own window.
        let audio_start = match self.video.front() {
            Some(v) => v.pts_us.min(v.dts_us),
            None => self.audio.back().map(|a| a.pts_us - self.window_us).unwrap_or(i64::MIN),
        };
        while let Some(a) = self.audio.front() {
            if a.pts_us < audio_start {
                self.bytes -= a.data.len();
                self.audio.pop_front();
            } else {
                break;
            }
        }
    }

    /// Video span currently buffered, in microseconds.
    pub fn duration_us(&self) -> i64 {
        match (self.video.front(), self.video.back()) {
            (Some(a), Some(b)) => b.dts_us - a.dts_us,
            _ => 0,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Copy of the buffer starting at the first video keyframe.
    pub fn snapshot(&self) -> Clip {
        let Some(start) = self.video.iter().position(|p| p.key) else {
            return Clip::default();
        };
        let video: Vec<_> = self.video.iter().skip(start).cloned().collect();
        let first_pts = video[0].pts_us;
        let audio = self.audio.iter().filter(|a| a.pts_us >= first_pts).cloned().collect();
        Clip { video, audio }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_US: i64 = 16_667; // 60 fps
    const GOP: i64 = 60;

    fn video(i: i64) -> Arc<EncodedPacket> {
        Arc::new(EncodedPacket {
            kind: StreamKind::Video,
            data: vec![0; 1000],
            pts_us: i * FRAME_US,
            dts_us: i * FRAME_US,
            duration_us: FRAME_US,
            key: i % GOP == 0,
        })
    }

    fn audio(i: i64) -> Arc<EncodedPacket> {
        // 1024 samples @ 48 kHz
        let pts = i * 1_000_000 * 1024 / 48_000;
        Arc::new(EncodedPacket {
            kind: StreamKind::Audio,
            data: vec![0; 100],
            pts_us: pts,
            dts_us: pts,
            duration_us: 21_333,
            key: true,
        })
    }

    /// Feed `secs` of interleaved A/V in arrival order.
    fn fill(buf: &mut ReplayBuffer, secs: i64) {
        let frames = secs * 60;
        let mut a = 0;
        for i in 0..frames {
            buf.push(video(i));
            while audio(a).pts_us <= i * FRAME_US {
                buf.push(audio(a));
                a += 1;
            }
        }
    }

    #[test]
    fn trims_to_window_and_starts_on_keyframe() {
        let mut buf = ReplayBuffer::new(10_000_000);
        fill(&mut buf, 30);
        let d = buf.duration_us();
        assert!(d >= 10_000_000 - FRAME_US, "too short: {d}");
        assert!(d <= 10_000_000 + GOP * FRAME_US, "too long: {d}");
        assert!(buf.video.front().unwrap().key);
    }

    #[test]
    fn audio_never_precedes_video() {
        let mut buf = ReplayBuffer::new(5_000_000);
        fill(&mut buf, 20);
        let v0 = buf.video.front().unwrap().pts_us;
        assert!(buf.audio.front().unwrap().pts_us >= v0);
        // ...and audio covers the video range up to the newest frame.
        let v_end = buf.video.back().unwrap().pts_us;
        assert!(buf.audio.back().unwrap().pts_us + 21_333 >= v_end);
    }

    #[test]
    fn byte_accounting_is_exact() {
        let mut buf = ReplayBuffer::new(3_000_000);
        fill(&mut buf, 12);
        let expected: usize = buf.video.iter().chain(buf.audio.iter()).map(|p| p.data.len()).sum();
        assert_eq!(buf.bytes(), expected);
    }

    #[test]
    fn snapshot_is_independent_of_later_writes() {
        let mut buf = ReplayBuffer::new(5_000_000);
        fill(&mut buf, 10);
        let clip = buf.snapshot();
        let n = clip.video.len();
        let first = clip.video[0].pts_us;
        // Keep writing, which trims the front of the buffer.
        for i in 600..1200 {
            buf.push(video(i));
        }
        assert_eq!(clip.video.len(), n);
        assert_eq!(clip.video[0].pts_us, first);
        assert!(clip.video[0].key);
        assert!(clip.duration_us() >= 5_000_000);
    }

    #[test]
    fn snapshot_skips_leading_non_keyframes() {
        let mut buf = ReplayBuffer::new(60_000_000);
        for i in 30..200 {
            buf.push(video(i)); // first keyframe at i = 60
        }
        let clip = buf.snapshot();
        assert_eq!(clip.video[0].pts_us, 60 * FRAME_US);
    }

    #[test]
    fn shrinking_window_trims_immediately() {
        let mut buf = ReplayBuffer::new(20_000_000);
        fill(&mut buf, 25);
        buf.set_window(5_000_000);
        assert!(buf.duration_us() <= 5_000_000 + GOP * FRAME_US);
    }
}
