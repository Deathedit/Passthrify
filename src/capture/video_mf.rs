//! Video capture through Media Foundation's `IMFSourceReader` (synchronous mode).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

use super::modes::{choose_mode, convert_raw, PixFmt, VideoDeviceInfo, VideoMode};
use super::{looks_like_elgato, Chroma, FramePool, VideoFrame};
use crate::clock;
use crate::color::{ColorInfo, RangePref};
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

/// Media Foundation subtype GUIDs.
trait FromGuid {
    fn from_guid(g: &GUID) -> Self;
}

impl FromGuid for PixFmt {
    fn from_guid(g: &GUID) -> Self {
        if *g == MFVideoFormat_NV12 {
            PixFmt::Nv12
        } else if *g == MFVideoFormat_YUY2 {
            PixFmt::Yuy2
        } else if *g == MFVideoFormat_RGB24 {
            PixFmt::Rgb24
        } else if *g == MFVideoFormat_P010 {
            PixFmt::P010
        } else if *g == MFVideoFormat_MJPG {
            PixFmt::Mjpg
        } else {
            PixFmt::Other(g.data1)
        }
    }
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
    current: Option<(VideoMode, Chroma)>,
    /// MF_MT_DEFAULT_STRIDE of the current type (negative = bottom-up).
    default_stride: i32,
    /// Range + matrix of the frames we hand out.
    color: ColorInfo,
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
            Ok(Self { name, modes, source, reader, current: None, default_stride: 0, color: ColorInfo::default() })
        }
    }

    /// Best mode (and the chroma it will yield) for the preferences; see [`choose_mode`].
    pub fn choose_mode(&self, pref: Option<&ModePref>, chroma: Chroma) -> Option<(VideoMode, Chroma)> {
        choose_mode(&self.modes, pref, chroma)
    }

    pub fn set_mode(&mut self, mode: &VideoMode, chroma: Chroma, range: RangePref) -> Result<()> {
        unsafe {
            let mt = self.reader.GetNativeMediaType(VIDEO_STREAM, mode.index)?;
            self.reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            self.reader.SetStreamSelection(VIDEO_STREAM, true)?;
            self.reader
                .SetCurrentMediaType(VIDEO_STREAM, None, &mt)
                .with_context(|| format!("setting mode {mode}"))?;
            let bpp = match mode.format {
                PixFmt::Rgb24 => 3,
                PixFmt::Yuy2 => 2,
                _ => 1,
            };
            // The 4K X reports NV12 as full-range BT.601 and YUY2 as limited BT.709 (verified
            // against each other on real captures), so honor what the media type says.
            let nominal = mt.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE).ok();
            let matrix = mt.GetUINT32(&MF_MT_YUV_MATRIX).ok();
            self.color = match mode.format {
                // We convert full-range RGB to limited BT.709 ourselves.
                PixFmt::Rgb24 => ColorInfo::default(),
                _ => ColorInfo::from_mf(nominal, matrix, mode.height).with_range(range),
            };
            tracing::info!(
                "media type: nominal_range={nominal:?} yuv_matrix={matrix:?} -> {} (range setting {range:?})",
                self.color.label()
            );
            // RGB types without an explicit stride are bottom-up by MF convention.
            let fallback = if mode.format == PixFmt::Rgb24 { -(mode.width as i32 * bpp) } else { mode.width as i32 * bpp };
            self.default_stride = mt.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or(fallback);
        }
        self.current = Some((*mode, chroma));
        Ok(())
    }

    /// Blocks until the next frame and converts it to the packed Y + interleaved-UV layout.
    /// Returns `Ok(None)` for stream ticks / gaps.
    fn read_frame(&mut self, pool: &Arc<FramePool>, seq: u64) -> Result<Option<VideoFrame>> {
        let (mode, chroma) = self.current.ok_or_else(|| anyhow!("mode not set"))?;
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
            let copy_started = std::time::Instant::now();
            let mut out = pool.get(super::frame_len(mode.width, mode.height, chroma));

            // Prefer the 2D interface: it gives the real pitch and the top row even for
            // bottom-up images. Otherwise fall back to the media type's default stride.
            if let Ok(buf2d) = windows::core::Interface::cast::<IMF2DBuffer>(&buffer) {
                let mut scan0: *mut u8 = std::ptr::null_mut();
                let mut pitch = 0i32;
                buf2d.Lock2D(&mut scan0, &mut pitch)?;
                let res = convert_raw(mode.format, chroma, scan0, pitch as isize, None, w, h, out.as_mut_slice());
                let _ = buf2d.Unlock2D();
                res?;
            } else {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut cur = 0u32;
                buffer.Lock(&mut ptr, None, Some(&mut cur))?;
                let stride = self.default_stride as isize;
                let top = if stride < 0 { ptr.offset(-stride * (h as isize - 1)) } else { ptr };
                let res = convert_raw(mode.format, chroma, top, stride, Some(cur as usize), w, h, out.as_mut_slice());
                let _ = buffer.Unlock();
                res?;
            }
            crate::perf::CAPTURE_COPY.add(copy_started.elapsed());

            Ok(Some(VideoFrame {
                width: mode.width,
                height: mode.height,
                chroma,
                color: self.color,
                data: out,
                pts_us,
                seq,
                gpu: Default::default(),
            }))
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


impl super::Device for VideoDevice {
    fn open(name_pref: Option<&str>) -> Result<Self> {
        VideoDevice::open(name_pref)
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn modes(&self) -> &[VideoMode] {
        &self.modes
    }
    fn set_mode(&mut self, mode: &VideoMode, chroma: Chroma, range: RangePref) -> Result<()> {
        VideoDevice::set_mode(self, mode, chroma, range)
    }
    fn current(&self) -> Option<(VideoMode, Chroma)> {
        self.current
    }
    fn color(&self) -> ColorInfo {
        self.color
    }
    fn read_frame(&mut self, pool: &Arc<FramePool>, seq: u64) -> Result<Option<VideoFrame>> {
        VideoDevice::read_frame(self, pool, seq)
    }
}

/// Capture loop with automatic reconnect (see [`super::run_capture_loop`]), on a thread with
/// COM and Media Foundation started.
pub fn run_capture(
    device: Option<String>,
    pref: Option<ModePref>,
    chroma: Chroma,
    range: RangePref,
    stop: Arc<AtomicBool>,
    mut on_open: impl FnMut(Result<(String, Vec<VideoMode>, VideoMode, Chroma, ColorInfo)>),
    on_frame: impl FnMut(VideoFrame),
) {
    let _com = match super::ComGuard::new() {
        Ok(g) => g,
        Err(e) => return on_open(Err(e)),
    };
    let _mf = match MfGuard::new() {
        Ok(g) => g,
        Err(e) => return on_open(Err(e)),
    };
    super::run_capture_loop::<VideoDevice>(device, pref, chroma, range, stop, on_open, on_frame);
}
