//! Owns all capture/encode threads and executes user commands.
//!
//! ```text
//! video capture ─┬─► latest_frame (preview)
//!                └─► NVENC ─┐
//! audio capture ─┬─► PassRing ─► render
//!                └─► AAC ───┴─► PacketBus ─► ReplayBuffer / Recorder
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use crossbeam_channel::{bounded, never, select, unbounded, Receiver, Sender, TrySendError};
use parking_lot::Mutex;

use crate::audio::PassRing;
use crate::capture::video_mf::{self, VideoMode};
use crate::capture::{audio_wasapi, AudioChunk, VideoFrame};
use crate::config::Config;
use crate::encode;
use crate::packets::{PacketBus, StreamParams};
use crate::recorder::Recorder;
use crate::{mux, notify, screenshot};

pub enum Command {
    SaveReplay,
    ToggleRecord,
    Screenshot,
    OpenOutputFolder,
    Apply(Box<Config>),
    Shutdown,
}

/// Commands that can be bound to hotkeys / tray items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SaveReplay,
    ToggleRecord,
    Screenshot,
}

impl From<Action> for Command {
    fn from(a: Action) -> Self {
        match a {
            Action::SaveReplay => Command::SaveReplay,
            Action::ToggleRecord => Command::ToggleRecord,
            Action::Screenshot => Command::Screenshot,
        }
    }
}

#[derive(Default)]
pub struct Status {
    pub device: Mutex<String>,
    pub mode: Mutex<Option<VideoMode>>,
    pub modes: Mutex<Vec<VideoMode>>,
    pub capturing: AtomicBool,
    pub fps_x100: AtomicU32,
    pub dropped_frames: AtomicU64,
    pub encoder: Mutex<String>,
    pub audio_in: Mutex<String>,
    pub replay_ms: AtomicU64,
    pub replay_bytes: AtomicU64,
    pub recording_since: Mutex<Option<Instant>>,
    pub error: Mutex<Option<String>>,
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

/// State shared between the pipeline threads and the UI.
pub struct Shared {
    pub latest_frame: ArcSwapOption<VideoFrame>,
    pub status: Status,
    pub ring: Arc<PassRing>,
    pub windows_notifications: AtomicBool,
    toasts: Mutex<Vec<Toast>>,
    repaint: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Shared {
    pub fn new(cfg: &Config) -> Self {
        Self {
            latest_frame: ArcSwapOption::empty(),
            status: Status::default(),
            ring: Arc::new(PassRing::new(cfg.volume, cfg.muted)),
            windows_notifications: AtomicBool::new(cfg.windows_notifications),
            toasts: Mutex::new(Vec::new()),
            repaint: OnceLock::new(),
        }
    }

    pub fn set_repaint(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.repaint.set(Box::new(f));
    }

    pub fn request_repaint(&self) {
        if let Some(f) = self.repaint.get() {
            f();
        }
    }

    pub fn notify(&self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            tracing::error!("{text}");
        } else {
            tracing::info!("{text}");
        }
        if self.windows_notifications.load(Ordering::Relaxed) {
            let t = text.clone();
            std::thread::spawn(move || notify::toast(&t));
        }
        self.toasts.lock().push(Toast { text, error, at: Instant::now() });
        self.request_repaint();
    }

    /// Toasts younger than `max_age` (older ones are discarded).
    pub fn recent_toasts(&self, max_age: Duration) -> Vec<(String, bool, f32)> {
        let mut t = self.toasts.lock();
        t.retain(|x| x.at.elapsed() < max_age);
        t.iter().map(|x| (x.text.clone(), x.error, x.at.elapsed().as_secs_f32())).collect()
    }

    fn set_error(&self, e: Option<String>) {
        *self.status.error.lock() = e;
        self.request_repaint();
    }
}

pub struct Pipeline {
    tx: Sender<Command>,
    join: Option<JoinHandle<()>>,
}

impl Pipeline {
    pub fn start(cfg: Config, shared: Arc<Shared>) -> Self {
        let (tx, rx) = unbounded();
        let join = std::thread::Builder::new()
            .name("pipeline".into())
            .spawn(move || control_loop(cfg, shared, rx))
            .expect("spawn pipeline thread");
        Self { tx, join: Some(join) }
    }

    pub fn sender(&self) -> Sender<Command> {
        self.tx.clone()
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn control_loop(mut cfg: Config, shared: Arc<Shared>, rx: Receiver<Command>) {
    let mut session = Session::start(cfg.clone(), shared.clone());
    loop {
        select! {
            recv(rx) -> cmd => match cmd {
                Err(_) | Ok(Command::Shutdown) => break,
                Ok(Command::Apply(new_cfg)) => {
                    session.stop();
                    cfg = *new_cfg;
                    shared.windows_notifications.store(cfg.windows_notifications, Ordering::Relaxed);
                    session = Session::start(cfg.clone(), shared.clone());
                }
                Ok(cmd) => session.handle(cmd),
            },
            recv(session.mode_rx) -> m => match m {
                Ok(mode) => session.on_mode(mode),
                Err(_) => session.mode_rx = never(),
            },
            default(Duration::from_millis(250)) => session.tick(),
        }
    }
    session.stop();
}

struct Encoders {
    mode: VideoMode,
    force_idr: Arc<AtomicBool>,
    joins: Vec<JoinHandle<()>>,
}

struct Session {
    cfg: Config,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    mode_rx: Receiver<VideoMode>,
    frame_tx: Arc<Mutex<Option<Sender<Arc<VideoFrame>>>>>,
    audio_tx: Arc<Mutex<Option<Sender<AudioChunk>>>>,
    encoders: Option<Encoders>,
    bus: Arc<PacketBus>,
    params: Arc<StreamParams>,
    recorder: Option<Recorder>,
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new().name(name.into()).spawn(f).expect("spawn thread")
}

impl Session {
    fn start(cfg: Config, shared: Arc<Shared>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let frame_tx: Arc<Mutex<Option<Sender<Arc<VideoFrame>>>>> = Arc::default();
        let audio_tx: Arc<Mutex<Option<Sender<AudioChunk>>>> = Arc::default();
        let (mode_tx, mode_rx) = unbounded();
        let mut threads = Vec::new();

        // Video capture.
        {
            let (sh_open, sh_frame) = (shared.clone(), shared.clone());
            let (stop, frame_tx) = (stop.clone(), frame_tx.clone());
            let (device, pref) = (cfg.video_device.clone(), cfg.video_mode);
            threads.push(spawn("video-capture", move || {
                let mut fps_frames = 0u32;
                let mut fps_since = Instant::now();
                video_mf::run_capture(
                    device,
                    pref,
                    stop,
                    move |res| match res {
                        Ok((name, modes, mode)) => {
                            let st = &sh_open.status;
                            *st.device.lock() = name;
                            *st.modes.lock() = modes;
                            *st.mode.lock() = Some(mode);
                            st.capturing.store(true, Ordering::Relaxed);
                            sh_open.set_error(None);
                            let _ = mode_tx.send(mode);
                        }
                        Err(e) => {
                            sh_open.status.capturing.store(false, Ordering::Relaxed);
                            sh_open.status.fps_x100.store(0, Ordering::Relaxed);
                            sh_open.latest_frame.store(None);
                            sh_open.set_error(Some(format!("{e:#}")));
                        }
                    },
                    move |frame| {
                        fps_frames += 1;
                        let el = fps_since.elapsed();
                        if el >= Duration::from_secs(1) {
                            let fps = fps_frames as f64 / el.as_secs_f64();
                            sh_frame.status.fps_x100.store((fps * 100.0) as u32, Ordering::Relaxed);
                            fps_frames = 0;
                            fps_since = Instant::now();
                        }
                        sh_frame.latest_frame.store(Some(frame.clone()));
                        sh_frame.request_repaint();
                        if let Some(tx) = frame_tx.lock().as_ref() {
                            if let Err(TrySendError::Full(_)) = tx.try_send(frame) {
                                sh_frame.status.dropped_frames.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    },
                );
            }));
        }

        // Audio capture (+ feed to passthrough and AAC).
        {
            let (sh_open, ring) = (shared.clone(), shared.clone().ring.clone());
            let (stop, audio_tx) = (stop.clone(), audio_tx.clone());
            let (pref, video_device, passthrough) =
                (cfg.audio_input.clone(), cfg.video_device.clone(), cfg.passthrough_enabled);
            threads.push(spawn("audio-capture", move || {
                audio_wasapi::run_capture(
                    pref,
                    video_device,
                    stop,
                    move |res| {
                        *sh_open.status.audio_in.lock() = match res {
                            Ok(name) => name,
                            Err(e) => format!("no audio: {e:#}"),
                        };
                        sh_open.request_repaint();
                    },
                    move |samples, pts_us, discontinuity| {
                        if passthrough {
                            ring.push(samples);
                        }
                        if let Some(tx) = audio_tx.lock().as_ref() {
                            let _ = tx.try_send(AudioChunk { samples: samples.to_vec(), pts_us, discontinuity });
                        }
                    },
                );
            }));
        }

        // Audio passthrough render.
        if cfg.passthrough_enabled {
            let (stop, ring, out) = (stop.clone(), shared.ring.clone(), cfg.audio_output.clone());
            threads.push(spawn("audio-render", move || audio_wasapi::run_render(out, stop, ring)));
        }

        let bus = Arc::new(PacketBus::new(cfg.replay_seconds, cfg.replay_enabled));
        Self {
            cfg,
            shared,
            stop,
            threads,
            mode_rx,
            frame_tx,
            audio_tx,
            encoders: None,
            bus,
            params: Arc::default(),
            recorder: None,
        }
    }

    fn on_mode(&mut self, mode: VideoMode) {
        if self.encoders.as_ref().is_some_and(|e| e.mode.same_shape(&mode)) {
            return;
        }
        self.stop_encoders();
        self.start_encoders(mode);
    }

    fn start_encoders(&mut self, mode: VideoMode) {
        self.bus.clear();
        self.params.clear();
        let (ftx, frx) = bounded::<Arc<VideoFrame>>(4);
        let (atx, arx) = bounded::<AudioChunk>(256);
        let force_idr = Arc::new(AtomicBool::new(false));
        let bitrate = self.cfg.bitrate_bps(mode.width, mode.height, mode.fps());
        let vcfg = encode::video::VideoEncoderConfig { codec: self.cfg.codec, mode, bitrate };
        *self.shared.status.encoder.lock() =
            format!("{} {:.0} Mbps", self.cfg.codec.nvenc_name(), bitrate as f64 / 1e6);

        let mut joins = Vec::new();
        {
            let (bus, params, shared, idr) = (self.bus.clone(), self.params.clone(), self.shared.clone(), force_idr.clone());
            joins.push(spawn("video-encoder", move || {
                if let Err(e) = encode::video::run(vcfg, frx, idr, params, |p| bus.publish(p)) {
                    *shared.status.encoder.lock() = "video encoder failed".into();
                    shared.notify(format!("Video encoder error: {e:#}"), true);
                }
            }));
        }
        {
            let (bus, params, shared) = (self.bus.clone(), self.params.clone(), self.shared.clone());
            joins.push(spawn("audio-encoder", move || {
                if let Err(e) = encode::audio::run(arx, params, |p| bus.publish(p)) {
                    shared.notify(format!("Audio encoder error: {e:#}"), true);
                }
            }));
        }
        *self.frame_tx.lock() = Some(ftx);
        *self.audio_tx.lock() = Some(atx);
        self.encoders = Some(Encoders { mode, force_idr, joins });
    }

    fn stop_encoders(&mut self) {
        self.stop_recording();
        *self.frame_tx.lock() = None;
        *self.audio_tx.lock() = None;
        if let Some(enc) = self.encoders.take() {
            for j in enc.joins {
                let name = j.thread().name().unwrap_or("?").to_string();
                let _ = j.join();
                tracing::debug!("joined {name}");
            }
        }
    }

    fn stop(&mut self) {
        tracing::debug!("session stopping");
        self.stop.store(true, Ordering::Relaxed);
        self.stop_encoders();
        for t in self.threads.drain(..) {
            let name = t.thread().name().unwrap_or("?").to_string();
            // Capture threads notice `stop` within one frame/period. A device that stopped
            // delivering frames can leave a synchronous ReadSample blocked; don't hang the app on
            // it — detach the thread instead (it exits when the read finally returns).
            let deadline = Instant::now() + Duration::from_secs(3);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
                tracing::debug!("joined {name}");
            } else {
                tracing::warn!("{name} did not stop in time; detaching");
            }
        }
        self.shared.latest_frame.store(None);
        self.shared.status.capturing.store(false, Ordering::Relaxed);
    }

    fn tick(&mut self) {
        let (ms, bytes) = {
            let r = self.bus.replay.lock();
            (r.duration_us() / 1000, r.bytes())
        };
        self.shared.status.replay_ms.store(ms.max(0) as u64, Ordering::Relaxed);
        self.shared.status.replay_bytes.store(bytes as u64, Ordering::Relaxed);
    }

    fn output_path(&self, kind: &str, ext: &str) -> PathBuf {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
        self.cfg.output_dir.join(format!("Passthrify {kind} {stamp}.{ext}"))
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::SaveReplay => self.save_replay(),
            Command::ToggleRecord => {
                if self.recorder.is_some() {
                    self.stop_recording();
                } else {
                    self.start_recording();
                }
            }
            Command::Screenshot => self.screenshot(),
            Command::OpenOutputFolder => {
                let _ = std::fs::create_dir_all(&self.cfg.output_dir);
                let _ = std::process::Command::new("explorer").arg(&self.cfg.output_dir).spawn();
            }
            Command::Apply(_) | Command::Shutdown => unreachable!("handled by control loop"),
        }
    }

    fn save_replay(&mut self) {
        if !self.cfg.replay_enabled {
            return self.shared.notify("Instant replay is turned off (Settings)", true);
        }
        let clip = self.bus.replay.lock().snapshot();
        let (video, audio) = self.params.snapshot();
        let Some(video) = video else {
            return self.shared.notify("Can't save replay: encoder is not running", true);
        };
        if clip.video.is_empty() {
            return self.shared.notify("Replay buffer is still empty", true);
        }
        let path = self.output_path("Replay", "mp4");
        let shared = self.shared.clone();
        spawn("save-replay", move || {
            let res = path
                .parent()
                .map(std::fs::create_dir_all)
                .transpose()
                .map_err(anyhow::Error::from)
                .and_then(|_| mux::write_clip(&path, &clip, &video, audio.as_ref()));
            match res {
                Ok(()) => shared.notify(
                    format!("Saved {:.0}s replay: {}", clip.duration_us() as f64 / 1e6, file_name(&path)),
                    false,
                ),
                Err(e) => shared.notify(format!("Saving replay failed: {e:#}"), true),
            }
        });
    }

    fn start_recording(&mut self) {
        let (video, audio) = self.params.snapshot();
        let (Some(video), Some(enc)) = (video, self.encoders.as_ref()) else {
            return self.shared.notify("Can't record: encoder is not running", true);
        };
        let path = self.output_path("Recording", "mp4");
        match Recorder::start(path, video, audio) {
            Ok(rec) => {
                self.bus.set_recorder(Some(rec.sender()));
                enc.force_idr.store(true, Ordering::Release);
                *self.shared.status.recording_since.lock() = Some(Instant::now());
                self.shared.notify(format!("Recording started: {}", file_name(&rec.path)), false);
                self.recorder = Some(rec);
            }
            Err(e) => self.shared.notify(format!("Can't start recording: {e:#}"), true),
        }
    }

    fn stop_recording(&mut self) {
        let Some(rec) = self.recorder.take() else { return };
        self.bus.set_recorder(None);
        *self.shared.status.recording_since.lock() = None;
        let shared = self.shared.clone();
        spawn("finish-recording", move || match rec.finish() {
            Ok(path) => shared.notify(format!("Recording saved: {}", file_name(&path)), false),
            Err(e) => shared.notify(format!("Recording failed: {e:#}"), true),
        });
    }

    fn screenshot(&mut self) {
        let Some(frame) = self.shared.latest_frame.load_full() else {
            return self.shared.notify("No video to screenshot", true);
        };
        let path = self.output_path("Screenshot", "png");
        let shared = self.shared.clone();
        spawn("screenshot", move || match screenshot::save_png(&frame, &path) {
            Ok(()) => shared.notify(format!("Screenshot saved: {}", file_name(&path)), false),
            Err(e) => shared.notify(format!("Screenshot failed: {e:#}"), true),
        });
    }
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}
