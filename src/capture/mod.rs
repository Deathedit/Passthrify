pub mod audio_wasapi;
pub mod video_mf;

use std::ops::Deref;
use std::sync::Arc;

use parking_lot::Mutex;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

pub use crate::config::Chroma;

/// A frame with tightly packed rows: a Y plane (`width * height` bytes) followed by one
/// interleaved UV plane at chroma resolution (`chroma_size()`, 2 bytes per sample).
/// That is NV12 for 4:2:0, NV16 for 4:2:2 and NV24 for 4:4:4.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub chroma: Chroma,
    pub data: PooledBuf,
    /// Arrival time on the session clock.
    pub pts_us: i64,
    pub seq: u64,
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
    free: Mutex<Vec<Vec<u8>>>,
}

const POOL_MAX: usize = 12;

impl FramePool {
    pub fn get(self: &Arc<Self>, len: usize) -> PooledBuf {
        let mut buf = self.free.lock().pop().unwrap_or_default();
        if buf.len() != len {
            buf.resize(len, 0);
        }
        PooledBuf { buf, pool: self.clone() }
    }
}

pub struct PooledBuf {
    buf: Vec<u8>,
    pool: Arc<FramePool>,
}

impl PooledBuf {
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buf
    }
}

impl Deref for PooledBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.buf
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        let buf = std::mem::take(&mut self.buf);
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
pub struct ComGuard;

impl ComGuard {
    pub fn new() -> anyhow::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// (video devices, audio inputs, audio outputs), enumerated on a fresh MTA thread so it's safe
/// to call from the UI thread.
pub fn enumerate_all() -> (Vec<String>, Vec<String>, Vec<String>) {
    std::thread::spawn(|| {
        let Ok(_com) = ComGuard::new() else { return Default::default() };
        let video = video_mf::MfGuard::new()
            .and_then(|_mf| video_mf::list_devices())
            .map(|v| v.into_iter().map(|d| d.name).collect())
            .unwrap_or_default();
        let ain = audio_wasapi::list_endpoints(audio_wasapi::Flow::Capture).unwrap_or_default();
        let aout = audio_wasapi::list_endpoints(audio_wasapi::Flow::Render).unwrap_or_default();
        (video, ain, aout)
    })
    .join()
    .unwrap_or_default()
}

/// Names that identify Elgato hardware, used for auto-detection.
pub fn looks_like_elgato(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["elgato", "game capture", "4k x", "4k60", "cam link", "hd60"].iter().any(|k| n.contains(k))
}
