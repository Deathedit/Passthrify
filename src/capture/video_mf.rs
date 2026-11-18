//! Video capture through Media Foundation's `IMFSourceReader` (synchronous mode).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

use super::{looks_like_elgato, FramePool, VideoFrame};
use crate::clock;
use crate::config::ModePref;

const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Keeps Media Foundation started for the lifetime of the guard (COM must already be initialised).
pub struct MfGuard;

impl MfGuard {
    pub fn new() -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
        Ok(MfGuard)
    }
}

impl Drop for MfGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixFmt {
    Nv12,
    Yuy2,
    P010,
    Mjpg,
    Other,
}

impl PixFmt {
    fn from_guid(g: &GUID) -> Self {
        if *g == MFVideoFormat_NV12 {
            PixFmt::Nv12
        } else if *g == MFVideoFormat_YUY2 {
            PixFmt::Yuy2
        } else if *g == MFVideoFormat_P010 {
            PixFmt::P010
        } else if *g == MFVideoFormat_MJPG {
            PixFmt::Mjpg
        } else {
            PixFmt::Other
        }
    }

    pub fn supported(self) -> bool {
        matches!(self, PixFmt::Nv12 | PixFmt::Yuy2)
    }

    /// Lower is better.
    fn rank(self) -> u32 {
        match self {
            PixFmt::Nv12 => 0,
            PixFmt::Yuy2 => 1,
            _ => 9,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoMode {
    /// Index into the reader's native media types.
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub format: PixFmt,
}

impl VideoMode {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den.max(1) as f64
    }

    pub fn matches(&self, p: &ModePref) -> bool {
        self.width == p.width && self.height == p.height && (self.fps() - p.fps).abs() < 0.5
    }

    pub fn same_shape(&self, o: &VideoMode) -> bool {
        self.width == o.width && self.height == o.height && self.fps_num == o.fps_num && self.fps_den == o.fps_den
    }
}

impl fmt::Display for VideoMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{} @ {:.2} {:?}", self.width, self.height, self.fps(), self.format)
    }
}

#[derive(Clone, Debug)]
pub struct VideoDeviceInfo {
    pub name: String,
}

unsafe fn get_string(attrs: &IMFAttributes, key: &GUID) -> Result<String> {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    attrs.GetAllocatedString(key, &mut p, &mut len)?;
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    Ok(s)
}

unsafe fn enum_activates() -> Result<Vec<IMFActivate>> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes returned null"))?;
    attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;

    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFEnumDeviceSources(&attrs, &mut ptr, &mut count)?;
    let mut out = Vec::with_capacity(count as usize);
    if !ptr.is_null() {
        for i in 0..count as usize {
            if let Some(a) = (*ptr.add(i)).take() {
                out.push(a);
            }
        }
        CoTaskMemFree(Some(ptr as *const _));
    }
    Ok(out)
}

/// Lists video capture devices. Requires COM + MF on the calling thread.
pub fn list_devices() -> Result<Vec<VideoDeviceInfo>> {
    unsafe {
        let mut out = Vec::new();
        for a in enum_activates()? {
            out.push(VideoDeviceInfo {
                name: get_string(&a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_default(),
            });
        }
        Ok(out)
    }
}

// Note: never call IMFMediaSource::Shutdown from another thread while a synchronous ReadSample
// is pending — it deadlocks the reader instead of cancelling the read. Stopping is done by flag.

pub struct VideoDevice {
    pub name: String,
    pub modes: Vec<VideoMode>,
    source: IMFMediaSource,
    reader: IMFSourceReader,
    current: Option<VideoMode>,
}

impl VideoDevice {
    /// Opens the device whose friendly name contains `name_pref` (case-insensitive), or the
    /// first Elgato-looking device, or the first device.
    pub fn open(name_pref: Option<&str>) -> Result<Self> {
        unsafe {
            let acts = enum_activates()?;
            if acts.is_empty() {
                bail!("no video capture devices found");
            }
            let names: Vec<String> = acts
                .iter()
                .map(|a| get_string(a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_default())
                .collect();
            let idx = match name_pref {
                Some(p) => {
                    let p = p.to_lowercase();
                    names
                        .iter()
                        .position(|n| n.to_lowercase().contains(&p))
                        .ok_or_else(|| anyhow!("capture device '{p}' not found (have: {names:?})"))?
                }
                None => names.iter().position(|n| looks_like_elgato(n)).unwrap_or(0),
            };
            let name = names[idx].clone();
            let source: IMFMediaSource =
                acts[idx].ActivateObject().with_context(|| format!("activating '{name}'"))?;

            let mut ra: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut ra, 2)?;
            let ra = ra.ok_or_else(|| anyhow!("MFCreateAttributes returned null"))?;
            ra.SetUINT32(&MF_LOW_LATENCY, 1)?;
            ra.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, &ra)
                .with_context(|| format!("creating source reader for '{name}' (is another app using it?)"))?;

            let modes = enumerate_modes(&reader)?;
            Ok(Self { name, modes, source, reader, current: None })
        }
    }

    /// Best mode for the preference: exact match if possible (NV12 over YUY2), otherwise the
    /// largest mode up to 3840x2160, then highest fps.
    pub fn choose_mode(&self, pref: Option<&ModePref>) -> Option<VideoMode> {
        choose_mode(&self.modes, pref)
    }

    pub fn set_mode(&mut self, mode: &VideoMode) -> Result<()> {
        unsafe {
            let mt = self.reader.GetNativeMediaType(VIDEO_STREAM, mode.index)?;
            self.reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            self.reader.SetStreamSelection(VIDEO_STREAM, true)?;
            self.reader
                .SetCurrentMediaType(VIDEO_STREAM, None, &mt)
                .with_context(|| format!("setting mode {mode}"))?;
        }
        self.current = Some(*mode);
        Ok(())
    }

    /// Blocks until the next frame and converts it to packed NV12. Returns `Ok(None)` for
    /// stream ticks / gaps.
    pub fn read_frame(&mut self, pool: &Arc<FramePool>, seq: u64) -> Result<Option<VideoFrame>> {
        let mode = self.current.ok_or_else(|| anyhow!("mode not set"))?;
        unsafe {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            self.reader.ReadSample(VIDEO_STREAM, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample))?;
            let pts_us = clock::now_us();
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                bail!("end of stream");
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                bail!("source reader error");
            }
            let Some(sample) = sample else { return Ok(None) };
            let buffer = sample.ConvertToContiguousBuffer()?;

            let (w, h) = (mode.width as usize, mode.height as usize);
            let mut out = pool.get(w * h * 3 / 2);

            // Prefer the 2D interface: it gives us the real pitch.
            if let Ok(buf2d) = windows::core::Interface::cast::<IMF2DBuffer>(&buffer) {
                let mut scan0: *mut u8 = std::ptr::null_mut();
                let mut pitch = 0i32;
                buf2d.Lock2D(&mut scan0, &mut pitch)?;
                let res = if pitch <= 0 {
                    Err(anyhow!("bottom-up frames are not supported"))
                } else {
                    // NV12: Y plane + half-height UV plane. YUY2: one packed plane (pitch = 2*w).
                    let len = match mode.format {
                        PixFmt::Nv12 => pitch as usize * h * 3 / 2,
                        _ => pitch as usize * h,
                    };
                    let src = std::slice::from_raw_parts(scan0, len);
                    convert(mode.format, src, pitch as usize, w, h, out.as_mut_slice())
                };
                let _ = buf2d.Unlock2D();
                res?;
            } else {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut cur = 0u32;
                buffer.Lock(&mut ptr, None, Some(&mut cur))?;
                let src = std::slice::from_raw_parts(ptr, cur as usize);
                let pitch = match mode.format {
                    PixFmt::Yuy2 => w * 2,
                    _ => w,
                };
                let res = convert(mode.format, src, pitch, w, h, out.as_mut_slice());
                let _ = buffer.Unlock();
                res?;
            }

            Ok(Some(VideoFrame { width: mode.width, height: mode.height, data: out, pts_us, seq }))
        }
    }
}

impl Drop for VideoDevice {
    fn drop(&mut self) {
        unsafe {
            let _ = self.source.Shutdown();
        }
    }
}

unsafe fn enumerate_modes(reader: &IMFSourceReader) -> Result<Vec<VideoMode>> {
    let mut modes = Vec::new();
    let mut i = 0u32;
    loop {
        let mt = match reader.GetNativeMediaType(VIDEO_STREAM, i) {
            Ok(mt) => mt,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(e.into()),
        };
        let subtype = mt.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
        let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let rate = mt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        modes.push(VideoMode {
            index: i,
            width: (size >> 32) as u32,
            height: size as u32,
            fps_num: (rate >> 32) as u32,
            fps_den: (rate as u32).max(1),
            format: PixFmt::from_guid(&subtype),
        });
        i += 1;
    }
    Ok(modes)
}

pub fn choose_mode(modes: &[VideoMode], pref: Option<&ModePref>) -> Option<VideoMode> {
    let usable = || modes.iter().filter(|m| m.format.supported() && m.width % 2 == 0 && m.height % 2 == 0);
    if let Some(p) = pref {
        if let Some(m) = usable().filter(|m| m.matches(p)).min_by_key(|m| m.format.rank()) {
            return Some(*m);
        }
    }
    // Default: up to 4K, at most ~60 fps (high-refresh modes are opt-in via Settings).
    let default_ok = |m: &&VideoMode| m.width <= 3840 && m.height <= 2160 && m.fps() <= 60.5;
    let pool: Vec<&VideoMode> =
        if usable().any(|m| default_ok(&m)) { usable().filter(default_ok).collect() } else { usable().collect() };
    pool.into_iter()
        .max_by(|a, b| {
            let ka = (a.width * a.height, (a.fps() * 100.0) as u64, std::cmp::Reverse(a.format.rank()));
            let kb = (b.width * b.height, (b.fps() * 100.0) as u64, std::cmp::Reverse(b.format.rank()));
            ka.cmp(&kb)
        })
        .copied()
}

/// Converts a locked capture buffer into packed NV12.
fn convert(fmt: PixFmt, src: &[u8], pitch: usize, w: usize, h: usize, out: &mut [u8]) -> Result<()> {
    match fmt {
        PixFmt::Nv12 => {
            if src.len() < pitch * h * 3 / 2 {
                bail!("short NV12 buffer ({} < {})", src.len(), pitch * h * 3 / 2);
            }
            let (y_out, uv_out) = out.split_at_mut(w * h);
            for row in 0..h {
                y_out[row * w..(row + 1) * w].copy_from_slice(&src[row * pitch..row * pitch + w]);
            }
            let uv = &src[pitch * h..];
            for row in 0..h / 2 {
                uv_out[row * w..(row + 1) * w].copy_from_slice(&uv[row * pitch..row * pitch + w]);
            }
            Ok(())
        }
        PixFmt::Yuy2 => {
            if src.len() < pitch * h {
                bail!("short YUY2 buffer");
            }
            yuy2_to_nv12(src, pitch, w, h, out);
            Ok(())
        }
        other => bail!("unsupported pixel format {other:?}"),
    }
}

/// YUY2 (Y0 U Y1 V) -> NV12. Chroma is taken from the even row of each pair.
pub fn yuy2_to_nv12(src: &[u8], pitch: usize, w: usize, h: usize, out: &mut [u8]) {
    let (y_out, uv_out) = out.split_at_mut(w * h);
    for row in 0..h {
        let s = &src[row * pitch..row * pitch + w * 2];
        let y = &mut y_out[row * w..(row + 1) * w];
        for (dst, px) in y.chunks_exact_mut(2).zip(s.chunks_exact(4)) {
            dst[0] = px[0];
            dst[1] = px[2];
        }
        if row % 2 == 0 {
            let uv = &mut uv_out[(row / 2) * w..(row / 2 + 1) * w];
            for (dst, px) in uv.chunks_exact_mut(2).zip(s.chunks_exact(4)) {
                dst[0] = px[1];
                dst[1] = px[3];
            }
        }
    }
}

/// Capture loop with automatic reconnect. Runs until `stop` is set. `on_open` is called each
/// time the device is (re)opened with the chosen mode.
pub fn run_capture(
    device: Option<String>,
    pref: Option<ModePref>,
    stop: Arc<AtomicBool>,
    mut on_open: impl FnMut(Result<(String, Vec<VideoMode>, VideoMode)>),
    mut on_frame: impl FnMut(Arc<VideoFrame>),
) {
    let _com = match super::ComGuard::new() {
        Ok(g) => g,
        Err(e) => return on_open(Err(e)),
    };
    let _mf = match MfGuard::new() {
        Ok(g) => g,
        Err(e) => return on_open(Err(e)),
    };
    let pool = Arc::new(FramePool::default());
    let mut seq = 0u64;

    while !stop.load(Ordering::Relaxed) {
        let opened = (|| -> Result<VideoDevice> {
            let mut dev = VideoDevice::open(device.as_deref())?;
            let mode = dev
                .choose_mode(pref.as_ref())
                .ok_or_else(|| anyhow!("'{}' offers no NV12/YUY2 modes: {:?}", dev.name, dev.modes))?;
            dev.set_mode(&mode)?;
            Ok(dev)
        })();
        let mut dev = match opened {
            Ok(d) => d,
            Err(e) => {
                on_open(Err(e));
                sleep_unless_stopped(&stop, 2000);
                continue;
            }
        };
        let mode = dev.current.unwrap();
        tracing::info!("capturing '{}' at {mode}", dev.name);
        on_open(Ok((dev.name.clone(), dev.modes.clone(), mode)));

        while !stop.load(Ordering::Relaxed) {
            match dev.read_frame(&pool, seq) {
                Ok(Some(frame)) => {
                    seq += 1;
                    on_frame(Arc::new(frame));
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

fn sleep_unless_stopped(stop: &AtomicBool, ms: u64) {
    let mut left = ms;
    while left > 0 && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(left.min(100)));
        left = left.saturating_sub(100);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(index: u32, w: u32, h: u32, fps: u32, format: PixFmt) -> VideoMode {
        VideoMode { index, width: w, height: h, fps_num: fps, fps_den: 1, format }
    }

    #[test]
    fn picks_best_mode() {
        let modes = [
            m(0, 1920, 1080, 60, PixFmt::Yuy2),
            m(1, 3840, 2160, 30, PixFmt::Nv12),
            m(2, 3840, 2160, 60, PixFmt::Yuy2),
            m(3, 3840, 2160, 60, PixFmt::Nv12),
            m(4, 3840, 2160, 60, PixFmt::P010),
            m(5, 7680, 4320, 30, PixFmt::Nv12),
            m(6, 3840, 2160, 144, PixFmt::Nv12),
        ];
        assert_eq!(choose_mode(&modes, None).unwrap().index, 3);
        let pref = ModePref { width: 1920, height: 1080, fps: 60.0 };
        assert_eq!(choose_mode(&modes, Some(&pref)).unwrap().index, 0);
        let missing = ModePref { width: 1280, height: 720, fps: 60.0 };
        assert_eq!(choose_mode(&modes, Some(&missing)).unwrap().index, 3);
        // High refresh is only used when asked for explicitly.
        let hfr = ModePref { width: 3840, height: 2160, fps: 144.0 };
        assert_eq!(choose_mode(&modes, Some(&hfr)).unwrap().index, 6);
    }

    #[test]
    fn yuy2_conversion() {
        // 4x2 image: row0 = Y 10,20,30,40 U 100 V 200 / U 101 V 201; row1 = Y 50..80
        let src = [
            10, 100, 20, 200, 30, 101, 40, 201, //
            50, 1, 60, 2, 70, 3, 80, 4,
        ];
        let mut out = vec![0u8; 4 * 2 * 3 / 2];
        yuy2_to_nv12(&src, 8, 4, 2, &mut out);
        assert_eq!(&out[..8], &[10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(&out[8..], &[100, 200, 101, 201]);
    }

    #[test]
    fn nv12_compaction_drops_padding() {
        // 2x2 image with pitch 4.
        let src = [1, 2, 0, 0, 3, 4, 0, 0, 9, 8, 0, 0];
        let mut out = vec![0u8; 6];
        convert(PixFmt::Nv12, &src, 4, 2, 2, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 9, 8]);
    }
}
