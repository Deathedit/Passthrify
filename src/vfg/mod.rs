//! Frame generation for the preview (NVIDIA Video Frame Generation, "VFG").
//!
//! ```text
//! capture ─► FrameGen::feed ─► worker: drop repeats ─► VFG (GPU) ─► Presenter ─► preview
//! ```
//!
//! Only the preview changes: recordings, replays and screenshots keep the real frames.
//! Generating the frame between A and B needs B, so B is shown a little later than without
//! frame generation (half a game frame at 2x, plus the GPU time).
//!
//! Repeated captured frames (a 30 fps game in a 60 Hz signal) are dropped before the effect, so
//! it interpolates between the game's own frames, and the output is paced from their timing.

pub mod runtime;
pub mod sdk;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;

use crate::capture::VideoFrame;
use crate::config::Config;
pub use sdk::Quality;

/// Gaps longer than this (a paused game, a still menu, a signal hiccup) start over: the next
/// new frame is shown as is instead of being blended with a stale one.
const MAX_GAP_US: i64 = 250_000;
/// Shortest/longest game frame interval the pacer believes (240 fps .. 20 fps).
const MIN_INTERVAL_US: f64 = 1e6 / 240.0;
const MAX_INTERVAL_US: f64 = 1e6 / 20.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub quality: Quality,
    /// Output frames per game frame (2 = one generated frame between each pair).
    pub multiplier: u32,
    /// SDK folder override; None = search the usual places.
    pub dir: Option<PathBuf>,
}

impl Settings {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.frame_gen,
            quality: cfg.frame_gen_quality,
            multiplier: cfg.frame_gen_multiplier.clamp(2, 4),
            dir: cfg.frame_gen_dir.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Off,
    /// Loading the SDK / model (takes a few seconds the first time).
    Starting,
    Running,
    Failed(String),
}

/// Shared between the capture thread (feed), the worker and the UI (presenter, status).
pub struct FrameGen {
    settings: Mutex<Settings>,
    /// Bumped by every settings change; a failed start isn't retried until it moves.
    revision: Arc<AtomicU64>,
    tx: Sender<Arc<VideoFrame>>,
    rx: Receiver<Arc<VideoFrame>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    state: Mutex<State>,
    presenter: Mutex<Presenter>,
    /// GPU time per game frame (upload + generation + download), in 0.1 ms.
    pub gen_ms_x10: AtomicU32,
    /// Optional runtime download (Settings).
    download: Arc<Mutex<runtime::DownloadState>>,
}

impl FrameGen {
    pub fn new(cfg: &Config) -> Self {
        let (tx, rx) = bounded(3);
        Self {
            settings: Mutex::new(Settings::from_config(cfg)),
            revision: Arc::new(AtomicU64::new(0)),
            tx,
            rx,
            worker: Mutex::new(None),
            state: Mutex::new(State::Off),
            presenter: Mutex::new(Presenter::default()),
            gen_ms_x10: AtomicU32::new(0),
            download: Arc::new(Mutex::new(runtime::DownloadState::Idle)),
        }
    }

    pub fn configure(&self, cfg: &Config) {
        let s = Settings::from_config(cfg);
        let mut cur = self.settings.lock();
        if *cur != s {
            *cur = s;
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn enabled(&self) -> bool {
        self.settings.lock().enabled
    }

    pub fn state(&self) -> State {
        self.state.lock().clone()
    }

    fn set_state(&self, s: State) {
        *self.state.lock() = s;
    }

    /// Hands a captured frame to the worker (started on first use). Never blocks capture.
    pub fn feed(&self, frame: &Arc<VideoFrame>, spawn_worker: impl FnOnce() -> JoinHandle<()>) {
        if !self.enabled() {
            return;
        }
        {
            let mut w = self.worker.lock();
            if w.is_none() {
                *w = Some(spawn_worker());
            }
        }
        let _ = self.tx.try_send(frame.clone());
    }

    /// The frame the preview should show now, and when the next one is due. None while frame
    /// generation isn't producing frames: the preview then shows the live frame.
    pub fn present(&self, now: Instant) -> Option<(Arc<VideoFrame>, Option<Instant>)> {
        self.presenter.lock().take(now)
    }

    pub fn download_state(&self) -> runtime::DownloadState {
        self.download.lock().clone()
    }

    /// Downloads and installs the runtime from `repo` in the background.
    pub fn start_download(&self, repo: String, repaint: impl Fn() + Send + 'static) {
        if matches!(*self.download.lock(), runtime::DownloadState::Downloading(_)) {
            return;
        }
        *self.download.lock() = runtime::DownloadState::Downloading(0.0);
        let state = self.download.clone();
        let revision = self.revision.clone();
        let res = std::thread::Builder::new().name("vfg-download".into()).spawn(move || {
            let result = runtime::download(&repo, |p| {
                *state.lock() = runtime::DownloadState::Downloading(p);
                repaint();
            });
            *state.lock() = match result {
                Ok(dir) => {
                    // Let a start that failed for lack of the runtime try again.
                    revision.fetch_add(1, Ordering::Relaxed);
                    runtime::DownloadState::Done(dir)
                }
                Err(e) => {
                    tracing::error!("frame generation runtime download failed: {e:#}");
                    runtime::DownloadState::Error(format!("{e:#}"))
                }
            };
            repaint();
        });
        if let Err(e) = res {
            *self.download.lock() = runtime::DownloadState::Error(e.to_string());
        }
    }

    /// Frames per second the preview is getting from frame generation.
    pub fn output_fps(&self) -> f32 {
        self.presenter.lock().fps
    }
}

/// Worker loop: owns the SDK effect. `repaint` wakes the UI when frames are queued.
pub fn run_worker(fg: &FrameGen, repaint: impl Fn()) {
    let mut interp: Option<sdk::Interpolator> = None;
    let mut failed_revision: Option<u64> = None;
    let mut last_unique: Option<Arc<VideoFrame>> = None;
    let mut pacer = Pacer::default();
    let mut seq = 1u64 << 62;
    loop {
        let mut frame = match fg.rx.recv_timeout(Duration::from_millis(500)) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => {
                // Nothing arriving (capture stopped or frame generation off): let go of the GPU.
                if !fg.enabled() && interp.take().is_some() {
                    tracing::info!("frame generation off; model unloaded");
                }
                if !fg.enabled() {
                    fg.set_state(State::Off);
                }
                fg.presenter.lock().stop();
                last_unique = None;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Behind? Skip to the newest frame; the pacer sees the longer interval.
        while let Ok(f) = fg.rx.try_recv() {
            frame = f;
        }
        let settings = fg.settings.lock().clone();
        let revision = fg.revision.load(Ordering::Relaxed);
        if !settings.enabled {
            interp = None;
            fg.set_state(State::Off);
            fg.presenter.lock().stop();
            continue;
        }

        let repeat = last_unique.as_ref().is_some_and(|p| {
            p.width == frame.width
                && p.height == frame.height
                && !crate::game_fps::luma_differs(p.y_plane(), frame.y_plane(), frame.width as usize, frame.height as usize)
        });
        if repeat {
            continue;
        }

        let shape = sdk::Shape::of(&frame, settings.quality, settings.multiplier);
        if interp.as_ref().is_none_or(|i| i.shape != shape) {
            if failed_revision == Some(revision) {
                continue; // stays failed until the settings change
            }
            interp = None;
            fg.presenter.lock().stop();
            fg.set_state(State::Starting);
            repaint();
            match sdk::load(settings.dir.as_deref()).and_then(|s| sdk::Interpolator::new(s, shape)) {
                Ok(i) => {
                    interp = Some(i);
                    failed_revision = None;
                    fg.set_state(State::Running);
                    pacer = Pacer::default();
                }
                Err(e) => {
                    tracing::error!("frame generation unavailable: {e:#}");
                    fg.set_state(State::Failed(format!("{e:#}")));
                    failed_revision = Some(revision);
                    repaint();
                    continue;
                }
            }
        }
        let Some(gen) = interp.as_mut() else { continue };

        let interval = pacer.interval(frame.pts_us);
        if interval.is_none() {
            gen.reset();
        }
        let t = Instant::now();
        let generated = match gen.push(&frame) {
            Ok(g) => g,
            Err(e) => {
                tracing::error!("frame generation failed: {e:#}");
                fg.set_state(State::Failed(format!("{e:#}")));
                failed_revision = Some(revision);
                interp = None;
                fg.presenter.lock().stop();
                repaint();
                continue;
            }
        };
        let done = Instant::now();
        fg.gen_ms_x10.store(((done - t).as_secs_f64() * 1e4) as u32, Ordering::Relaxed);
        last_unique = Some(frame.clone());

        let generated = match interval {
            Some(_) => generated,
            None => Vec::new(), // first frame after a gap: nothing to blend
        };
        let mut frames: Vec<Arc<VideoFrame>> = generated
            .into_iter()
            .map(|mut f| {
                seq += 1;
                f.seq = seq;
                Arc::new(f)
            })
            .collect();
        frames.push(frame);
        let (slots, flush) = pacer.slots(done, interval, frames.len());
        fg.presenter.lock().push(slots.into_iter().zip(frames), flush);
        repaint();
    }
}

/// `--frame-gen-test`: loads the SDK and interpolates a moving square at 1080p and 4K, printing
/// timings and writing before / generated / after PNGs to `out`. Needs no capture card.
pub fn self_test(dir: Option<&std::path::Path>, out: &std::path::Path) -> anyhow::Result<()> {
    use crate::capture::{Chroma, FramePool};
    let found = sdk::find_runtime(dir);
    println!("VFX SDK folder: {}", found.as_ref().map(|d| d.display().to_string()).unwrap_or("not found".into()));
    let sdk = sdk::load(dir)?;
    std::fs::create_dir_all(out)?;
    let pool = Arc::new(FramePool::default());
    // Gray NV12 frame with a bright 1/8-width square at horizontal position `x` (0..1).
    let make = |w: u32, h: u32, x: f32, seq: u64| {
        let mut data = pool.get(crate::capture::frame_len(w, h, Chroma::S420));
        let buf = data.as_mut_slice();
        let (luma, chroma) = buf.split_at_mut((w * h) as usize);
        luma.fill(60);
        chroma.fill(128);
        let side = w / 8;
        let (x0, y0) = ((x * (w - side) as f32) as u32, (h - side) / 2);
        for y in y0..y0 + side {
            luma[(y * w + x0) as usize..(y * w + x0 + side) as usize].fill(220);
            let cy = y / 2;
            for cx in x0 / 2..(x0 + side) / 2 {
                let i = (cy * w + cx * 2) as usize;
                chroma[i] = 90; // a blue-ish square, so color makes it through too
                chroma[i + 1] = 110;
            }
        }
        VideoFrame { width: w, height: h, chroma: Chroma::S420, color: Default::default(), data, pts_us: 0, seq }
    };
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        let shape = sdk::Shape {
            width: w,
            height: h,
            chroma: Chroma::S420,
            color: Default::default(),
            quality: Quality::Medium,
            multiplier: 2,
        };
        let t = Instant::now();
        let mut interp = sdk::Interpolator::new(sdk.clone(), shape)?;
        println!("{w}x{h}: model loaded in {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
        let (a, b) = (make(w, h, 0.40, 1), make(w, h, 0.44, 2));
        interp.push(&a)?;
        let generated = interp.push(&b)?;
        anyhow::ensure!(generated.len() == 1, "expected 1 generated frame, got {}", generated.len());
        let tag = format!("{w}x{h}");
        crate::screenshot::save_png(&a, &out.join(format!("{tag}-1-before.png")))?;
        crate::screenshot::save_png(&generated[0], &out.join(format!("{tag}-2-generated.png")))?;
        crate::screenshot::save_png(&b, &out.join(format!("{tag}-3-after.png")))?;
        // Timing: alternate the two frames, as a 2x stream would.
        let runs = 30;
        let t = Instant::now();
        for k in 0..runs {
            interp.push(if k % 2 == 0 { &a } else { &b })?;
        }
        let per = t.elapsed().as_secs_f64() * 1e3 / runs as f64;
        let [up, run, down] = interp.timings;
        println!("{w}x{h}: {per:.2} ms per game frame (last: upload {up:.2}, generate {run:.2}, download {down:.2})");
    }
    // Every model setting must load (4K, 2x), and 4x must work too.
    let (a, b) = (make(3840, 2160, 0.40, 1), make(3840, 2160, 0.44, 2));
    for (quality, multiplier) in [(Quality::Low, 2), (Quality::High, 2), (Quality::Medium, 4)] {
        let shape = sdk::Shape {
            width: 3840,
            height: 2160,
            chroma: Chroma::S420,
            color: Default::default(),
            quality,
            multiplier,
        };
        let mut interp = sdk::Interpolator::new(sdk.clone(), shape)?;
        interp.push(&a)?;
        let t = Instant::now();
        let n = interp.push(&b)?.len();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        println!("4K {} {multiplier}x: {n} generated in {ms:.2} ms", quality.label());
    }
    println!("PNGs in {}", out.display());
    Ok(())
}

/// Turns game-frame timing into display times for the generated + real frames.
#[derive(Default)]
struct Pacer {
    last_pts: Option<i64>,
    /// Smoothed game frame interval (µs).
    ema_us: f64,
    /// When the last scheduled frame is due.
    last_due: Option<Instant>,
}

impl Pacer {
    /// Interval to the previous new frame, or None after a gap (start over).
    fn interval(&mut self, pts_us: i64) -> Option<Duration> {
        let prev = self.last_pts.replace(pts_us);
        let gap = pts_us - prev?;
        if gap <= 0 || gap > MAX_GAP_US {
            self.ema_us = 0.0;
            return None;
        }
        let gap = (gap as f64).clamp(MIN_INTERVAL_US, MAX_INTERVAL_US);
        self.ema_us = if self.ema_us == 0.0 { gap } else { self.ema_us * 0.8 + gap * 0.2 };
        Some(Duration::from_secs_f64(self.ema_us / 1e6))
    }

    /// Due times for `count` frames (generated ones, then the real one), evenly spread over one
    /// game frame interval starting now. The bool says to drop whatever is still queued (we
    /// fell more than a frame behind, so catch up instead of adding latency).
    fn slots(&mut self, now: Instant, interval: Option<Duration>, count: usize) -> (Vec<Instant>, bool) {
        let Some(interval) = interval else {
            self.last_due = Some(now);
            return (vec![now; count], true);
        };
        let step = interval / count as u32;
        let mut start = now;
        let mut flush = false;
        if let Some(last) = self.last_due {
            // Keep the spacing even if this frame came in a bit early.
            start = start.max(last + step / 2);
            if start > now + interval {
                start = now;
                flush = true;
            }
        }
        let slots: Vec<Instant> = (0..count).map(|k| start + step * k as u32).collect();
        self.last_due = slots.last().copied();
        (slots, flush)
    }
}

/// Frames waiting for their display time.
#[derive(Default)]
struct Presenter {
    queue: VecDeque<(Instant, Arc<VideoFrame>)>,
    current: Option<Arc<VideoFrame>>,
    /// Shown-frame rate over the last second.
    fps: f32,
    window_start: Option<Instant>,
    window_count: u32,
}

impl Presenter {
    fn push(&mut self, items: impl IntoIterator<Item = (Instant, Arc<VideoFrame>)>, flush: bool) {
        if flush {
            self.queue.clear();
        }
        self.queue.extend(items);
        // Never let latency pile up.
        while self.queue.len() > 16 {
            self.queue.pop_front();
        }
    }

    fn stop(&mut self) {
        self.queue.clear();
        self.current = None;
        self.fps = 0.0;
        self.window_start = None;
        self.window_count = 0;
    }

    fn take(&mut self, now: Instant) -> Option<(Arc<VideoFrame>, Option<Instant>)> {
        while self.queue.front().is_some_and(|(due, _)| *due <= now) {
            let (_, f) = self.queue.pop_front().unwrap();
            self.current = Some(f);
            self.window_count += 1;
        }
        let start = *self.window_start.get_or_insert(now);
        let elapsed = now.duration_since(start);
        if elapsed >= Duration::from_secs(1) {
            self.fps = self.window_count as f32 / elapsed.as_secs_f32();
            self.window_start = Some(now);
            self.window_count = 0;
        }
        let next = self.queue.front().map(|(due, _)| *due);
        self.current.clone().map(|f| (f, next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_spreads_frames_over_the_game_interval() {
        let mut p = Pacer::default();
        assert_eq!(p.interval(0), None, "first frame has nothing before it");
        let t0 = Instant::now();
        let (s, flush) = p.slots(t0, None, 1);
        assert_eq!((s, flush), (vec![t0], true));

        let iv = p.interval(33_333).unwrap();
        assert!((iv.as_secs_f64() - 0.033333).abs() < 1e-4);
        let t1 = t0 + Duration::from_millis(35);
        let (s, flush) = p.slots(t1, Some(iv), 2);
        assert!(!flush);
        assert_eq!(s[0], t1, "generated frame right away");
        let half = s[1] - s[0];
        assert!((half.as_secs_f64() - 0.016666).abs() < 1e-4, "real frame half a game frame later: {half:?}");
    }

    #[test]
    fn pacer_starts_over_after_a_gap() {
        let mut p = Pacer::default();
        p.interval(0);
        assert!(p.interval(16_667).is_some());
        assert_eq!(p.interval(16_667 + 500_000), None, "half a second without a new frame");
        assert!(p.interval(16_667 + 500_000 + 16_667).is_some());
    }

    #[test]
    fn pacer_catches_up_instead_of_adding_latency() {
        let mut p = Pacer::default();
        let iv = Duration::from_millis(16);
        let t0 = Instant::now();
        p.last_due = Some(t0 + Duration::from_millis(100)); // queue is far ahead of now
        let (s, flush) = p.slots(t0, Some(iv), 2);
        assert!(flush);
        assert_eq!(s[0], t0);
    }

    fn frame(seq: u64) -> Arc<VideoFrame> {
        let pool = Arc::new(crate::capture::FramePool::default());
        Arc::new(VideoFrame {
            width: 2,
            height: 2,
            chroma: crate::config::Chroma::S420,
            color: Default::default(),
            data: pool.get(6),
            pts_us: 0,
            seq,
        })
    }

    #[test]
    fn presenter_shows_frames_when_due() {
        let mut pr = Presenter::default();
        let t0 = Instant::now();
        assert!(pr.take(t0).is_none(), "nothing until the first frame is due");
        pr.push([(t0, frame(1)), (t0 + Duration::from_millis(8), frame(2))], false);
        let (f, next) = pr.take(t0).unwrap();
        assert_eq!(f.seq, 1);
        assert_eq!(next, Some(t0 + Duration::from_millis(8)));
        let (f, next) = pr.take(t0 + Duration::from_millis(9)).unwrap();
        assert_eq!((f.seq, next), (2, None));
        pr.stop();
        assert!(pr.take(t0).is_none());
    }
}
