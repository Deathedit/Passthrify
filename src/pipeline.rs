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
use crate::capture::{audio_dev, video, VideoMode};
use crate::capture::{AudioChunk, VideoFrame};
use crate::color::ColorInfo;
use crate::config::{Chroma, Codec, Config};
use crate::encode;
use crate::gpu::CudaFrames;
use crate::packets::{PacketBus, StreamParams};
use crate::recorder::Recorder;
use crate::{mux, notify, screenshot};

pub enum Command {
    SaveReplay,
    ToggleRecord,
    Screenshot,
    OpenOutputFolder,
    OpenScreenshotFolder,
    Apply(Box<Config>),
    /// Settings that don't need a restart (stream settings, hotkeys): just remembered.
    SetConfig(Box<Config>),
    ToggleStream,
    Shutdown,
}

/// Commands that can be bound to hotkeys / tray items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SaveReplay,
    ToggleRecord,
    Screenshot,
    ToggleStream,
}

impl From<Action> for Command {
    fn from(a: Action) -> Self {
        match a {
            Action::SaveReplay => Command::SaveReplay,
            Action::ToggleRecord => Command::ToggleRecord,
            Action::Screenshot => Command::Screenshot,
            Action::ToggleStream => Command::ToggleStream,
        }
    }
}

#[derive(Default)]
pub struct Status {
    pub device: Mutex<String>,
    pub mode: Mutex<Option<VideoMode>>,
    /// Chroma actually delivered by the current mode (may be lower than requested).
    pub chroma: Mutex<Option<Chroma>>,
    /// YCbCr range/matrix of the current mode.
    pub color: Mutex<Option<ColorInfo>>,
    pub modes: Mutex<Vec<VideoMode>>,
    pub capturing: AtomicBool,
    /// Frames per second arriving from the card (the signal refresh).
    pub fps_x100: AtomicU32,
    /// Frames per second that differ from the previous one: what the game renders at.
    pub game_fps_x100: AtomicU32,
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
    /// Preview frame generation (off unless turned on in Settings › Display).
    pub frame_gen: crate::vfg::FrameGen,
    /// The preview's GPU frame buffer shared with CUDA (set by the UI once it has one for
    /// the current shape); the capture thread copies each CUDA frame into it.
    pub preview_target: Arc<ArcSwapOption<crate::gpu::PreviewTarget>>,
    /// Live stream state for the UI.
    pub stream: Arc<crate::stream::StreamStatus>,
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
            frame_gen: crate::vfg::FrameGen::new(cfg),
            preview_target: Arc::default(),
            stream: Arc::default(),
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

    /// Passes a captured frame on to preview frame generation, if it's on.
    fn feed_frame_gen(self: &Arc<Self>, frame: &Arc<VideoFrame>) {
        self.frame_gen.feed(frame, || {
            let shared = self.clone();
            spawn("frame-gen", move || crate::vfg::run_worker(&shared.frame_gen, || shared.request_repaint()))
        });
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
    // The stream lives outside the session, so applying settings doesn't end it.
    let mut live = Live { streamer: None, frames: Arc::default() };
    let mut session = Session::start(cfg.clone(), shared.clone(), live.frames.clone());
    loop {
        select! {
            recv(rx) -> cmd => match cmd {
                Err(_) | Ok(Command::Shutdown) => break,
                Ok(Command::Apply(new_cfg)) => {
                    session.stop();
                    cfg = *new_cfg;
                    shared.windows_notifications.store(cfg.windows_notifications, Ordering::Relaxed);
                    session = Session::start(cfg.clone(), shared.clone(), live.frames.clone());
                    live.attach_audio(&session.bus);
                }
                Ok(Command::SetConfig(new_cfg)) => cfg = *new_cfg,
                Ok(Command::ToggleStream) => live.toggle(&cfg, &shared, &session.bus),
                Ok(cmd) => session.handle(cmd),
            },
            recv(session.mode_rx) -> m => match m {
                Ok((mode, chroma, color)) => session.on_mode(mode, chroma, color),
                Err(_) => session.mode_rx = never(),
            },
            default(Duration::from_millis(250)) => {
                session.tick();
                live.reap(&session.bus);
            }
        }
    }
    live.stop(&session.bus);
    session.stop();
}

/// The live stream, if one is running, and where the capture thread sends it frames.
struct Live {
    streamer: Option<crate::stream::Streamer>,
    frames: Arc<Mutex<Option<Sender<Arc<VideoFrame>>>>>,
}

impl Live {
    fn toggle(&mut self, cfg: &Config, shared: &Arc<Shared>, bus: &PacketBus) {
        if self.streamer.is_some() {
            return self.stop(bus);
        }
        let key = crate::secrets::stream_key(cfg.stream_service).unwrap_or_default();
        let service = cfg.stream_service;
        if service != crate::config::StreamService::Custom && key.is_empty() {
            return shared.notify(format!("Add your {} stream key in Settings › Streaming", service.label()), true);
        }
        let url = service.url(&cfg.stream_server, &key);
        if !url.starts_with("rtmp://") && !url.starts_with("rtmps://") {
            return shared.notify("Set an rtmp:// or rtmps:// server in Settings › Streaming", true);
        }
        let target = crate::stream::StreamTarget {
            url,
            height: cfg.stream_height,
            max_fps: cfg.stream_fps.clamp(1, 120),
            bitrate_kbps: cfg.stream_bitrate_kbps.clamp(500, 50_000),
        };
        tracing::info!("going live on {} ({}p{}, {} kbps)", service.label(), target.height, target.max_fps, target.bitrate_kbps);
        shared.stream.dropped.store(0, Ordering::Relaxed);
        let notify_to = shared.clone();
        let s = crate::stream::Streamer::start(target, shared.stream.clone(), move |text, error| {
            notify_to.notify(text, error)
        });
        *self.frames.lock() = Some(s.frame_sender());
        self.streamer = Some(s);
        self.attach_audio(bus);
        shared.notify(format!("Connecting to {}…", service.label()), false);
    }

    fn attach_audio(&self, bus: &PacketBus) {
        bus.set_streamer(self.streamer.as_ref().map(|s| s.audio_sender()));
    }

    fn stop(&mut self, bus: &PacketBus) {
        *self.frames.lock() = None;
        bus.set_streamer(None);
        if let Some(s) = self.streamer.take() {
            s.stop();
        }
    }

    /// Cleans up after a stream that ended by itself (a fatal error).
    fn reap(&mut self, bus: &PacketBus) {
        if self.streamer.as_ref().is_some_and(|s| s.is_finished()) {
            self.stop(bus);
        }
    }
}

struct Encoders {
    mode: VideoMode,
    chroma: Chroma,
    color: ColorInfo,
    force_idr: Arc<AtomicBool>,
    joins: Vec<JoinHandle<()>>,
}

struct Session {
    cfg: Config,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    mode_rx: Receiver<(VideoMode, Chroma, ColorInfo)>,
    frame_tx: Arc<Mutex<Option<Sender<Arc<VideoFrame>>>>>,
    audio_tx: Arc<Mutex<Option<Sender<AudioChunk>>>>,
    /// CUDA frames for the current mode; the capture thread uploads each frame into one.
    gpu_frames: Arc<Mutex<Option<Arc<CudaFrames>>>>,
    encoders: Option<Encoders>,
    bus: Arc<PacketBus>,
    params: Arc<StreamParams>,
    recorder: Option<Recorder>,
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new().name(name.into()).spawn(f).expect("spawn thread")
}

impl Session {
    fn start(cfg: Config, shared: Arc<Shared>, stream_frames: Arc<Mutex<Option<Sender<Arc<VideoFrame>>>>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let frame_tx: Arc<Mutex<Option<Sender<Arc<VideoFrame>>>>> = Arc::default();
        let audio_tx: Arc<Mutex<Option<Sender<AudioChunk>>>> = Arc::default();
        let gpu_frames: Arc<Mutex<Option<Arc<CudaFrames>>>> = Arc::default();
        let (mode_tx, mode_rx) = unbounded();
        let mut threads = Vec::new();

        // Video capture.
        {
            let (sh_open, sh_frame) = (shared.clone(), shared.clone());
            let (stop, frame_tx, gpu_frames) = (stop.clone(), frame_tx.clone(), gpu_frames.clone());
            let (device, pref, chroma, range) = (cfg.video_device.clone(), cfg.video_mode, cfg.chroma, cfg.color_range);
            threads.push(spawn("video-capture", move || {
                let mut rates = crate::game_fps::FrameRates::default();
                let mut scratch = Vec::new();
                let mut upload_failed = false;
                let stream_frames = stream_frames.clone();
                video::run_capture(
                    device,
                    pref,
                    chroma,
                    range,
                    stop,
                    move |res| match res {
                        Ok((name, modes, mode, out_chroma, color)) => {
                            let st = &sh_open.status;
                            *st.device.lock() = name;
                            *st.modes.lock() = modes;
                            *st.mode.lock() = Some(mode);
                            *st.chroma.lock() = Some(out_chroma);
                            *st.color.lock() = Some(color);
                            st.capturing.store(true, Ordering::Relaxed);
                            sh_open.set_error(None);
                            let _ = mode_tx.send((mode, out_chroma, color));
                        }
                        Err(e) => {
                            sh_open.status.capturing.store(false, Ordering::Relaxed);
                            sh_open.status.fps_x100.store(0, Ordering::Relaxed);
                            sh_open.status.game_fps_x100.store(0, Ordering::Relaxed);
                            sh_open.latest_frame.store(None);
                            sh_open.set_error(Some(format!("{e:#}")));
                        }
                    },
                    move |frame| {
                        let frame = Arc::new(frame);
                        if let Some(r) = rates.push(&frame) {
                            let st = &sh_frame.status;
                            st.fps_x100.store((r.signal * 100.0) as u32, Ordering::Relaxed);
                            st.game_fps_x100.store((r.game * 100.0) as u32, Ordering::Relaxed);
                        }
                        let cuda = gpu_frames.lock().clone().filter(|c| c.matches(&frame));
                        // With a preview buffer on the GPU, the preview takes the frame from
                        // there, so it waits for the upload. Otherwise it goes first and
                        // uploads it itself, without waiting for ours.
                        let target = sh_frame.preview_target.load_full().filter(|t| t.matches(&frame));
                        let preview_first = cuda.is_none() || target.is_none();
                        if preview_first {
                            sh_frame.latest_frame.store(Some(frame.clone()));
                            sh_frame.request_repaint();
                        }
                        // One upload to CUDA for the encoder, frame generation and the preview.
                        if let Some(cuda) = cuda {
                            match crate::perf::GPU_UPLOAD.time(|| cuda.upload(&frame, &mut scratch)) {
                                Ok(g) => {
                                    if let Some(t) = &target {
                                        crate::perf::PREVIEW_COPY.time(|| t.write(&g, frame.seq));
                                    }
                                    let _ = frame.gpu.set(g);
                                    upload_failed = false;
                                }
                                Err(e) if !upload_failed => {
                                    upload_failed = true;
                                    tracing::warn!("CUDA upload failed: {e:#}");
                                }
                                Err(_) => {}
                            }
                        }
                        if !preview_first {
                            sh_frame.latest_frame.store(Some(frame.clone()));
                            sh_frame.request_repaint();
                        }
                        sh_frame.feed_frame_gen(&frame);
                        if let Some(tx) = stream_frames.lock().as_ref() {
                            if let Err(TrySendError::Full(_)) = tx.try_send(frame.clone()) {
                                sh_frame.stream.dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
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
                audio_dev::run_capture(
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
            threads.push(spawn("audio-render", move || audio_dev::run_render(out, stop, ring)));
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
            gpu_frames,
            encoders: None,
            bus,
            params: Arc::default(),
            recorder: None,
        }
    }

    fn on_mode(&mut self, mode: VideoMode, chroma: Chroma, color: ColorInfo) {
        if self.encoders.as_ref().is_some_and(|e| e.mode.same_shape(&mode) && e.chroma == chroma && e.color == color) {
            return;
        }
        self.stop_encoders();
        self.start_encoders(mode, chroma, color);
    }

    fn start_encoders(&mut self, mode: VideoMode, chroma: Chroma, color: ColorInfo) {
        self.bus.clear();
        self.params.clear();
        let (ftx, frx) = bounded::<Arc<VideoFrame>>(4);
        let (atx, arx) = bounded::<AudioChunk>(256);
        let force_idr = Arc::new(AtomicBool::new(false));
        let mut codec = self.cfg.codec;
        if !codec.supports(chroma) {
            codec = Codec::Hevc;
            self.shared.notify(
                format!("{} can't encode {}; using HEVC instead", self.cfg.codec.short_label(), chroma.label()),
                true,
            );
        }
        let bitrate = self.cfg.bitrate_bps(mode.width, mode.height, mode.fps(), chroma);
        let gpu = if self.cfg.gpu_frames {
            match CudaFrames::new(mode.width, mode.height, chroma) {
                Ok(g) => Some(g),
                Err(e) => {
                    tracing::warn!("GPU frame path unavailable, encoding from system memory: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        let vcfg = encode::video::VideoEncoderConfig { codec, mode, chroma, color, bitrate, gpu: gpu.clone() };
        *self.shared.status.encoder.lock() =
            format!("{} {} {:.0} Mbps", codec.nvenc_name(), chroma.label(), bitrate as f64 / 1e6);

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
        *self.gpu_frames.lock() = gpu;
        *self.frame_tx.lock() = Some(ftx);
        *self.audio_tx.lock() = Some(atx);
        self.encoders = Some(Encoders { mode, chroma, color, force_idr, joins });
    }

    fn stop_encoders(&mut self) {
        self.stop_recording();
        *self.gpu_frames.lock() = None;
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
        crate::perf::report_if_due();
    }

    fn output_path(&self, kind: &str, ext: &str) -> PathBuf {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
        let dir = if ext == "png" { &self.cfg.screenshot_dir } else { &self.cfg.output_dir };
        dir.join(format!("Passthrify {kind} {stamp}.{ext}"))
    }

    fn open_folder(dir: &std::path::Path) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
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
            Command::OpenOutputFolder => Self::open_folder(&self.cfg.output_dir),
            Command::OpenScreenshotFolder => Self::open_folder(&self.cfg.screenshot_dir),
            Command::Apply(_) | Command::SetConfig(_) | Command::ToggleStream | Command::Shutdown => {
                unreachable!("handled by control loop")
            }
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
