//! Per-frame timing stats for the frame path, logged every few seconds:
//! where the copies and uploads of each captured frame cost time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub struct Stat {
    pub name: &'static str,
    sum_us: AtomicU64,
    count: AtomicU64,
    max_us: AtomicU64,
}

impl Stat {
    const fn new(name: &'static str) -> Self {
        Self { name, sum_us: AtomicU64::new(0), count: AtomicU64::new(0), max_us: AtomicU64::new(0) }
    }

    pub fn add(&self, d: Duration) {
        let us = d.as_micros() as u64;
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Times `f` and records it.
    pub fn time<T>(&self, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let out = f();
        self.add(t.elapsed());
        out
    }

    /// (average ms, max ms, count) since the last take, and resets.
    pub fn take(&self) -> (f64, f64, u64) {
        let sum = self.sum_us.swap(0, Ordering::Relaxed);
        let n = self.count.swap(0, Ordering::Relaxed);
        let max = self.max_us.swap(0, Ordering::Relaxed);
        if n == 0 {
            return (0.0, 0.0, 0);
        }
        (sum as f64 / n as f64 / 1e3, max as f64 / 1e3, n)
    }
}

/// Media Foundation buffer -> our frame buffer (plus YUY2/RGB24 conversion).
pub static CAPTURE_COPY: Stat = Stat::new("capture copy");
/// Our frame buffer -> CUDA frame (one upload shared by the encoder and frame generation).
pub static GPU_UPLOAD: Stat = Stat::new("gpu upload");
/// Getting a frame ready for NVENC: CPU copy into FFmpeg's frame, or the CUDA frame reference.
pub static ENCODE_PREP: Stat = Stat::new("encode prep");
/// avcodec_send_frame + receiving packets (includes NVENC's own upload of system memory frames).
pub static ENCODE_SEND: Stat = Stat::new("encode send");
/// Preview texture upload on the UI thread.
pub static PREVIEW_UPLOAD: Stat = Stat::new("preview upload");

pub static ALL: [&Stat; 5] = [&CAPTURE_COPY, &GPU_UPLOAD, &ENCODE_PREP, &ENCODE_SEND, &PREVIEW_UPLOAD];

const REPORT_EVERY: Duration = Duration::from_secs(10);

/// "capture copy 1.21/3.40 ms ×600 · …" for the stats with samples, and resets them all.
pub fn summary() -> Option<String> {
    let parts: Vec<String> = ALL
        .iter()
        .filter_map(|s| {
            let (avg, max, n) = s.take();
            (n > 0).then(|| format!("{} {avg:.2}/{max:.2} ms ×{n}", s.name))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Logs the summary every `REPORT_EVERY` (call it often; it rate-limits itself).
pub fn report_if_due() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock();
    let now = Instant::now();
    match *last {
        None => *last = Some(now),
        Some(t) if now - t >= REPORT_EVERY => {
            *last = Some(now);
            if let Some(s) = summary() {
                tracing::info!("frame timings avg/max: {s}");
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_and_resets() {
        let s = Stat::new("t");
        s.add(Duration::from_micros(1000));
        s.add(Duration::from_micros(3000));
        assert_eq!(s.take(), (2.0, 3.0, 2));
        assert_eq!(s.take(), (0.0, 0.0, 0));
    }
}
