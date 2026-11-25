//! Capture: video and audio from the capture card, one backend per OS behind the same API.
//! - `video`: `list_devices`, `VideoDevice` ([`Device`]), `run_capture`.
//!   Media Foundation on Windows, V4L2 on Linux.
//! - `audio_dev`: `Flow`, `list_endpoints`, `run_capture`, `run_render`.
//!   WASAPI on Windows, PipeWire on Linux.

pub mod modes;

#[cfg(windows)]
pub mod audio_wasapi;
#[cfg(windows)]
pub mod video_mf;
#[cfg(windows)]
pub use {audio_wasapi as audio_dev, video_mf as video};

#[cfg(target_os = "linux")]
pub mod audio_pipewire;
#[cfg(target_os = "linux")]
pub mod video_v4l2;
#[cfg(target_os = "linux")]
pub use {audio_pipewire as audio_dev, video_v4l2 as video};

use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;

pub use crate::config::Chroma;
use crate::color::{ColorInfo, RangePref};
use crate::config::ModePref;
pub use modes::{PixFmt, VideoMode};

/// A frame with tightly packed rows: a Y plane (`width * height` bytes) followed by one
/// interleaved UV plane at chroma resolution (`chroma_size()`, 2 bytes per sample).
/// That is NV12 for 4:2:0, NV16 for 4:2:2 and NV24 for 4:4:4.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub chroma: Chroma,
    /// YCbCr range + matrix of `data`.
    pub color: crate::color::ColorInfo,
    pub data: PooledBuf,
    /// Arrival time on the session clock.
    pub pts_us: i64,
    pub seq: u64,
    /// The same frame in CUDA memory when the GPU frame path is on, shared by NVENC and frame
    /// generation. The preview and screenshots still use `data`. Set by the capture thread
    /// right after the frame went to the preview, before it goes to the encoder.
    pub gpu: std::sync::OnceLock<crate::gpu::GpuFrame>,
}

/// Chroma plane dimensions for a frame size.
pub fn chroma_size(width: u32, height: u32, chroma: Chroma) -> (u32, u32) {
    let (sx, sy) = chroma.subsampling();
    (width / sx, height / sy)
}

/// Total bytes of a packed frame (Y + interleaved UV).
pub fn frame_len(width: u32, height: u32, chroma: Chroma) -> usize {
    let (cw, ch) = chroma_size(width, height, chroma);
    (width * height + cw * ch * 2) as usize
}

impl VideoFrame {
    pub fn chroma_size(&self) -> (u32, u32) {
        chroma_size(self.width, self.height, self.chroma)
    }
    pub fn y_plane(&self) -> &[u8] {
        &self.data[..(self.width * self.height) as usize]
    }
    /// Interleaved UV; each row is `chroma_size().0 * 2` bytes.
    pub fn uv_plane(&self) -> &[u8] {
        &self.data[(self.width * self.height) as usize..]
    }
}

/// Recycles large frame buffers so 4K60 capture doesn't hammer the allocator.
#[derive(Default)]
pub struct FramePool {
    free: Mutex<Vec<Buf>>,
    /// Allocate page-locked (CUDA pinned) memory, so frames upload to the GPU by DMA.
    pinned: bool,
}

const POOL_MAX: usize = 12;

enum Buf {
    Heap(Vec<u8>),
    Pinned(crate::gpu::PinnedBuf),
}

impl Buf {
    fn len(&self) -> usize {
        match self {
            Buf::Heap(v) => v.len(),
            Buf::Pinned(p) => p.len(),
        }
    }
}

impl FramePool {
    /// A pool of pinned buffers (falls back to normal memory without CUDA).
    pub fn pinned() -> Self {
        Self { free: Mutex::default(), pinned: true }
    }

    pub fn get(self: &Arc<Self>, len: usize) -> PooledBuf {
        let buf = match self.free.lock().pop() {
            Some(Buf::Heap(mut v)) if !self.pinned || v.len() == len => {
                v.resize(len, 0);
                Buf::Heap(v)
            }
            Some(b @ Buf::Pinned(_)) if b.len() == len => b,
            _ => match self.pinned.then(|| crate::gpu::PinnedBuf::new(len)).flatten() {
                Some(p) => Buf::Pinned(p),
                None => Buf::Heap(vec![0; len]),
            },
        };
        PooledBuf { buf: Some(buf), pool: self.clone() }
    }
}

pub struct PooledBuf {
    buf: Option<Buf>,
    pool: Arc<FramePool>,
}

impl PooledBuf {
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        match self.buf.as_mut() {
            Some(Buf::Heap(v)) => v,
            Some(Buf::Pinned(p)) => p.as_mut_slice(),
            None => &mut [],
        }
    }
}

impl Deref for PooledBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self.buf.as_ref() {
            Some(Buf::Heap(v)) => v,
            Some(Buf::Pinned(p)) => p.as_slice(),
            None => &[],
        }
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        let Some(buf) = self.buf.take() else { return };
        let mut free = self.pool.free.lock();
        if free.len() < POOL_MAX {
            free.push(buf);
        }
    }
}

/// Interleaved stereo f32 @ 48 kHz from the capture card.
pub struct AudioChunk {
    pub samples: Vec<f32>,
    /// Session-clock time of the first sample.
    pub pts_us: i64,
    pub discontinuity: bool,
}

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: usize = 2;

/// COM (MTA) initialised for the lifetime of the guard.
#[cfg(windows)]
pub struct ComGuard;

#[cfg(windows)]
impl ComGuard {
    pub fn new() -> anyhow::Result<Self> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(ComGuard)
    }
}

#[cfg(windows)]
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}

/// Runs `f` on a thread set up for the capture APIs: a fresh MTA thread with Media Foundation
/// started on Windows (safe to call from the UI thread), the calling thread on Linux.
fn with_capture_apis<T: Default + Send>(f: impl FnOnce() -> T + Send) -> T {
    #[cfg(windows)]
    {
        std::thread::scope(|s| {
            s.spawn(|| {
                let Ok(_com) = ComGuard::new() else { return T::default() };
                let Ok(_mf) = video_mf::MfGuard::new() else { return T::default() };
                f()
            })
            .join()
            .unwrap_or_default()
        })
    }
    #[cfg(not(windows))]
    f()
}

/// (video devices, audio inputs, audio outputs).
pub fn enumerate_all() -> (Vec<String>, Vec<String>, Vec<String>) {
    with_capture_apis(|| {
        let video = video::list_devices().map(|v| v.into_iter().map(|d| d.name).collect()).unwrap_or_default();
        let ain = audio_dev::list_endpoints(audio_dev::Flow::Capture).unwrap_or_default();
        let aout = audio_dev::list_endpoints(audio_dev::Flow::Render).unwrap_or_default();
        (video, ain, aout)
    })
}

/// Each video device with its modes (or why it couldn't be opened), for `--list-devices`.
pub fn describe_devices() -> Result<Vec<(String, Result<Vec<VideoMode>>)>> {
    with_capture_apis(|| -> Option<Result<_>> {
        Some(video::list_devices().map(|devs| {
            devs.into_iter()
                .map(|d| {
                    let modes = <video::VideoDevice as Device>::open(Some(&d.name)).map(|dev| dev.modes().to_vec());
                    (d.name, modes)
                })
                .collect()
        }))
    })
    .unwrap_or_else(|| Err(anyhow!("capture APIs unavailable")))
}

/// A video capture device as the capture loop drives it.
pub trait Device: Sized {
    /// Opens the device whose name contains `name_pref` (case-insensitive), or the first
    /// Elgato-looking device, or the first device.
    fn open(name_pref: Option<&str>) -> Result<Self>;
    fn name(&self) -> &str;
    fn modes(&self) -> &[VideoMode];
    fn set_mode(&mut self, mode: &VideoMode, chroma: Chroma, range: RangePref) -> Result<()>;
    /// The mode set last and the chroma it yields.
    fn current(&self) -> Option<(VideoMode, Chroma)>;
    /// Range + matrix of the frames handed out.
    fn color(&self) -> ColorInfo;
    /// Waits for the next frame and converts it to the packed Y + interleaved-UV layout.
    /// `Ok(None)` for stream ticks, gaps and timeouts (so the caller can check its stop flag).
    fn read_frame(&mut self, pool: &Arc<FramePool>, seq: u64) -> Result<Option<VideoFrame>>;
}

/// Capture loop with automatic reconnect. Runs until `stop` is set. `on_open` is called each
/// time the device is (re)opened with the chosen mode and the chroma it delivers.
pub fn run_capture_loop<D: Device>(
    device: Option<String>,
    pref: Option<ModePref>,
    chroma: Chroma,
    range: RangePref,
    stop: Arc<AtomicBool>,
    mut on_open: impl FnMut(Result<(String, Vec<VideoMode>, VideoMode, Chroma, ColorInfo)>),
    mut on_frame: impl FnMut(VideoFrame),
) {
    let pool = Arc::new(FramePool::pinned());
    let mut seq = 0u64;

    while !stop.load(Ordering::Relaxed) {
        let opened = (|| -> Result<D> {
            let mut dev = D::open(device.as_deref())?;
            let (mode, out_chroma) = modes::choose_mode(dev.modes(), pref.as_ref(), chroma)
                .ok_or_else(|| anyhow!("'{}' offers no NV12/YUY2/RGB24 modes: {:?}", dev.name(), dev.modes()))?;
            dev.set_mode(&mode, out_chroma, range)?;
            Ok(dev)
        })();
        let mut dev = match opened {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("video capture: {e:#}");
                on_open(Err(e));
                sleep_unless_stopped(&stop, 2000);
                continue;
            }
        };
        let (mode, out_chroma) = dev.current().expect("mode set");
        tracing::info!("capturing '{}' at {mode} -> {} {}", dev.name(), out_chroma.label(), dev.color().label());
        on_open(Ok((dev.name().to_string(), dev.modes().to_vec(), mode, out_chroma, dev.color())));

        while !stop.load(Ordering::Relaxed) {
            match dev.read_frame(&pool, seq) {
                Ok(Some(frame)) => {
                    seq += 1;
                    on_frame(frame);
                }
                Ok(None) => {}
                Err(e) => {
                    if !stop.load(Ordering::Relaxed) {
                        tracing::warn!("capture error: {e:#}; reconnecting");
                        on_open(Err(e.context("capture stopped")));
                    }
                    break;
                }
            }
        }
        tracing::debug!("video capture: releasing device");
        drop(dev);
        tracing::debug!("video capture: device released");
        sleep_unless_stopped(&stop, 500);
    }
    tracing::debug!("video capture: exiting");
}

pub fn sleep_unless_stopped(stop: &AtomicBool, ms: u64) {
    let mut left = ms;
    while left > 0 && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(left.min(100)));
        left = left.saturating_sub(100);
    }
}

/// Names that identify Elgato hardware, used for auto-detection.
pub fn looks_like_elgato(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["elgato", "game capture", "4k x", "4k60", "cam link", "hd60"].iter().any(|k| n.contains(k))
}
