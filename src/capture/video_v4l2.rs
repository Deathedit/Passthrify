//! Video capture through V4L2 (Linux): any UVC capture card (Elgato 4K X / 4K60 S+, Cam Link,
//! generic HDMI dongles) shows up as `/dev/videoN`.
//!
//! Device and mode queries go through the `v4l` crate; streaming uses mmap buffers driven by
//! raw ioctls, so a dequeue can wait with a timeout (the stop flag is checked between frames)
//! and an unplugged card ends in an error instead of a panic.

use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use v4l::framesize::FrameSizeEnum;
use v4l::frameinterval::FrameIntervalEnum;
use v4l::v4l_sys as sys;
use v4l::video::Capture;
use v4l::FourCC;

use super::modes::{convert_raw, PixFmt, VideoDeviceInfo, VideoMode};
use super::{looks_like_elgato, Chroma, FramePool, VideoFrame};
use crate::clock;
use crate::color::{ColorInfo, RangePref};
use crate::config::ModePref;

const BUF_TYPE: u32 = sys::v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_CAPTURE;
const MEMORY: u32 = sys::v4l2_memory_V4L2_MEMORY_MMAP;
/// Driver buffers. 4 keeps a frame or two of slack without adding latency (we always take the
/// oldest, and capture never falls behind by more than a frame).
const BUFFERS: u32 = 4;
/// How long a dequeue waits before returning to check the stop flag.
const POLL_MS: i32 = 200;
/// No frame for this long: the signal or the device is gone; reopen.
const STALL_MS: u128 = 3000;

/// `/dev/video*` nodes that capture video (skips metadata nodes such as the 4K X's second one),
/// with their card names. Duplicate names get " (2)", " (3)" so each stays selectable.
fn capture_nodes() -> Vec<(PathBuf, String)> {
    let mut nodes: Vec<(PathBuf, String)> = Vec::new();
    let mut paths: Vec<PathBuf> = v4l::context::enum_devices().into_iter().map(|n| n.path().to_path_buf()).collect();
    paths.sort_by_key(|p| node_index(p));
    for path in paths {
        let Ok(dev) = v4l::Device::with_path(&path) else { continue };
        let Ok(caps) = dev.query_caps() else { continue };
        if caps.capabilities.bits() & sys::V4L2_CAP_VIDEO_CAPTURE == 0 {
            continue;
        }
        let base = card_name(&caps.card);
        let mut name = base.clone();
        let mut n = 2;
        while nodes.iter().any(|(_, existing)| *existing == name) {
            name = format!("{base} ({n})");
            n += 1;
        }
        nodes.push((path, name));
    }
    nodes
}

/// uvcvideo names cards "<interface>: <product>", which for capture cards is usually the same
/// string twice ("Elgato 4K X: Elgato 4K X"); keep one.
fn card_name(card: &str) -> String {
    let card = card.trim();
    match card.split_once(": ") {
        Some((a, b)) if a.trim() == b.trim() => a.trim().to_string(),
        _ => card.to_string(),
    }
}

fn node_index(p: &Path) -> u32 {
    p.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.trim_start_matches("video").parse().ok())
        .unwrap_or(u32::MAX)
}

/// Lists video capture devices.
pub fn list_devices() -> Result<Vec<VideoDeviceInfo>> {
    Ok(capture_nodes().into_iter().map(|(_, name)| VideoDeviceInfo { name }).collect())
}

pub struct VideoDevice {
    pub name: String,
    pub modes: Vec<VideoMode>,
    path: PathBuf,
    /// Declared before `dev`: fields drop in order, and the buffers must be unmapped and
    /// released before the device fd closes.
    stream: Option<MmapStream>,
    dev: v4l::Device,
    current: Option<(VideoMode, Chroma)>,
    /// bytesperline of the current format.
    pitch: usize,
    color: ColorInfo,
    last_frame: std::time::Instant,
}

impl VideoDevice {
    pub fn open(name_pref: Option<&str>) -> Result<Self> {
        let nodes = capture_nodes();
        if nodes.is_empty() {
            bail!("no video capture devices found (is the card plugged in, and are you in the 'video' group?)");
        }
        let names: Vec<&str> = nodes.iter().map(|(_, n)| n.as_str()).collect();
        let idx = match name_pref {
            Some(p) => {
                let p = p.to_lowercase();
                // Exact name first (so "Card (2)" doesn't match "Card"), then a substring.
                names
                    .iter()
                    .position(|n| n.to_lowercase() == p)
                    .or_else(|| names.iter().position(|n| n.to_lowercase().contains(&p)))
                    .ok_or_else(|| anyhow!("capture device '{p}' not found (have: {names:?})"))?
            }
            None => names.iter().position(|n| looks_like_elgato(n)).unwrap_or(0),
        };
        let (path, name) = nodes[idx].clone();
        let dev = v4l::Device::with_path(&path).with_context(|| format!("opening {} ('{name}')", path.display()))?;
        let modes = enumerate_modes(&dev).with_context(|| format!("listing modes of '{name}'"))?;
        Ok(Self {
            name,
            modes,
            path,
            stream: None,
            dev,
            current: None,
            pitch: 0,
            color: ColorInfo::default(),
            last_frame: std::time::Instant::now(),
        })
    }

    pub fn set_mode(&mut self, mode: &VideoMode, chroma: Chroma, range: RangePref) -> Result<()> {
        self.stream = None; // STREAMOFF + unmap before the format can change
        let fd = self.dev.handle().fd();
        let fourcc = match mode.format {
            PixFmt::Nv12 => *b"NV12",
            PixFmt::Yuy2 => *b"YUYV",
            PixFmt::Rgb24 => *b"BGR3",
            other => bail!("unsupported pixel format {other:?}"),
        };
        let pix = unsafe {
            let mut f: sys::v4l2_format = std::mem::zeroed();
            f.type_ = BUF_TYPE;
            f.fmt.pix.width = mode.width;
            f.fmt.pix.height = mode.height;
            f.fmt.pix.pixelformat = u32::from_le_bytes(fourcc);
            f.fmt.pix.field = sys::v4l2_field_V4L2_FIELD_NONE;
            ioctl(fd, v4l::v4l2::vidioc::VIDIOC_S_FMT, &mut f).map_err(|e| busy_hint(e, &self.name))?;
            f.fmt.pix
        };
        if (pix.width, pix.height, pix.pixelformat) != (mode.width, mode.height, u32::from_le_bytes(fourcc)) {
            bail!(
                "'{}' gave {}x{} {} instead of {mode}",
                self.name,
                pix.width,
                pix.height,
                PixFmt::from_fourcc(pix.pixelformat).name()
            );
        }
        unsafe {
            let mut p: sys::v4l2_streamparm = std::mem::zeroed();
            p.type_ = BUF_TYPE;
            // Frame interval = 1 / fps.
            p.parm.capture.timeperframe = sys::v4l2_fract { numerator: mode.fps_den, denominator: mode.fps_num };
            if let Err(e) = ioctl(fd, v4l::v4l2::vidioc::VIDIOC_S_PARM, &mut p) {
                tracing::warn!("setting {:.2} fps on '{}': {e}", mode.fps(), self.name);
            }
        }
        let bpp = match mode.format {
            PixFmt::Rgb24 => 3,
            PixFmt::Yuy2 => 2,
            _ => 1,
        };
        self.pitch = (pix.bytesperline as usize).max(mode.width as usize * bpp);
        let ycbcr_enc = unsafe { pix.__bindgen_anon_1.ycbcr_enc };
        self.color = match mode.format {
            // We convert full-range RGB to limited BT.709 ourselves.
            PixFmt::Rgb24 => ColorInfo::default(),
            _ => ColorInfo::from_v4l2(pix.colorspace, ycbcr_enc, pix.quantization, mode.height).with_range(range),
        };
        tracing::info!(
            "v4l2 format: colorspace={} ycbcr_enc={ycbcr_enc} quantization={} pitch={} -> {} (range setting {range:?})",
            pix.colorspace,
            pix.quantization,
            self.pitch,
            self.color.label()
        );
        self.stream = Some(MmapStream::start(fd, BUFFERS).map_err(|e| busy_hint(e, &self.name))?);
        self.current = Some((*mode, chroma));
        self.last_frame = std::time::Instant::now();
        Ok(())
    }

    fn read_frame(&mut self, pool: &Arc<FramePool>, seq: u64) -> Result<Option<VideoFrame>> {
        let (mode, chroma) = self.current.ok_or_else(|| anyhow!("mode not set"))?;
        let stream = self.stream.as_mut().ok_or_else(|| anyhow!("not streaming"))?;
        let Some(buf) = stream.dequeue(POLL_MS).with_context(|| format!("reading from {}", self.path.display()))? else {
            if self.last_frame.elapsed().as_millis() > STALL_MS {
                bail!("no frames for {} s (signal lost?)", STALL_MS / 1000);
            }
            return Ok(None);
        };
        let pts_us = clock::now_us();
        self.last_frame = std::time::Instant::now();
        let (data, bytesused, flags) = (stream.bufs[buf.index].as_ptr(), buf.bytesused as usize, buf.flags);
        // A corrupted frame (USB hiccup): drop it, keep the stream.
        let result = if flags & sys::V4L2_BUF_FLAG_ERROR != 0 || bytesused == 0 {
            Ok(None)
        } else {
            let (w, h) = (mode.width as usize, mode.height as usize);
            let copy_started = std::time::Instant::now();
            let mut out = pool.get(super::frame_len(mode.width, mode.height, chroma));
            // V4L2 images are always top-down with a positive pitch.
            let res = unsafe { convert_raw(mode.format, chroma, data, self.pitch as isize, Some(bytesused), w, h, out.as_mut_slice()) };
            crate::perf::CAPTURE_COPY.add(copy_started.elapsed());
            res.map(|()| {
                Some(VideoFrame {
                    width: mode.width,
                    height: mode.height,
                    chroma,
                    color: self.color,
                    data: out,
                    pts_us,
                    seq,
                    gpu: Default::default(),
                })
            })
        };
        stream.queue(buf.index).with_context(|| format!("requeueing a buffer on {}", self.path.display()))?;
        result
    }
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

/// Every (format, size, frame rate) the device offers. Stepwise sizes/intervals (rare on capture
/// cards) contribute their largest size and fastest rate.
fn enumerate_modes(dev: &v4l::Device) -> Result<Vec<VideoMode>> {
    let mut modes = Vec::new();
    for f in dev.enum_formats()? {
        let format = PixFmt::from_fourcc(u32::from_le_bytes(f.fourcc.repr));
        for size in dev.enum_framesizes(f.fourcc).unwrap_or_default() {
            let (width, height) = match size.size {
                FrameSizeEnum::Discrete(d) => (d.width, d.height),
                FrameSizeEnum::Stepwise(s) => (s.max_width, s.max_height),
            };
            for iv in dev.enum_frameintervals(FourCC::new(&f.fourcc.repr), width, height).unwrap_or_default() {
                let frac = match iv.interval {
                    FrameIntervalEnum::Discrete(d) => d,
                    FrameIntervalEnum::Stepwise(s) => s.min,
                };
                if frac.numerator == 0 || frac.denominator == 0 {
                    continue;
                }
                modes.push(VideoMode {
                    index: modes.len() as u32,
                    width,
                    height,
                    fps_num: frac.denominator,
                    fps_den: frac.numerator,
                    format,
                });
            }
        }
    }
    Ok(modes)
}

fn busy_hint(e: std::io::Error, name: &str) -> anyhow::Error {
    if e.raw_os_error() == Some(libc::EBUSY) {
        anyhow!("'{name}' is in use by another app (close OBS or other capture apps)")
    } else {
        anyhow!(e)
    }
}

unsafe fn ioctl<T>(fd: i32, request: v4l::v4l2::vidioc::_IOC_TYPE, arg: &mut T) -> std::io::Result<()> {
    v4l::v4l2::ioctl(fd, request, arg as *mut T as *mut c_void)
}

/// A dequeued buffer.
struct Dequeued {
    index: usize,
    bytesused: u32,
    flags: u32,
}

/// mmap streaming: `BUFFERS` driver buffers, all queued except the one being read.
struct MmapStream {
    fd: i32,
    bufs: Vec<&'static [u8]>,
}

impl MmapStream {
    fn start(fd: i32, count: u32) -> std::io::Result<Self> {
        let mut req: sys::v4l2_requestbuffers = unsafe { std::mem::zeroed() };
        req.count = count;
        req.type_ = BUF_TYPE;
        req.memory = MEMORY;
        unsafe { ioctl(fd, v4l::v4l2::vidioc::VIDIOC_REQBUFS, &mut req)? };
        let mut stream = Self { fd, bufs: Vec::new() };
        for index in 0..req.count {
            let mut b = stream.desc(index as usize);
            unsafe { ioctl(fd, v4l::v4l2::vidioc::VIDIOC_QUERYBUF, &mut b)? };
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    b.length as usize,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    fd,
                    b.m.offset as libc::off_t,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            stream.bufs.push(unsafe { std::slice::from_raw_parts(ptr as *const u8, b.length as usize) });
        }
        for index in 0..stream.bufs.len() {
            stream.queue(index)?;
        }
        let mut typ = BUF_TYPE;
        unsafe { ioctl(fd, v4l::v4l2::vidioc::VIDIOC_STREAMON, &mut typ)? };
        Ok(stream)
    }

    fn desc(&self, index: usize) -> sys::v4l2_buffer {
        let mut b: sys::v4l2_buffer = unsafe { std::mem::zeroed() };
        b.index = index as u32;
        b.type_ = BUF_TYPE;
        b.memory = MEMORY;
        b
    }

    fn queue(&mut self, index: usize) -> std::io::Result<()> {
        let mut b = self.desc(index);
        unsafe { ioctl(self.fd, v4l::v4l2::vidioc::VIDIOC_QBUF, &mut b) }
    }

    /// The next filled buffer, or None after `timeout_ms` without one.
    fn dequeue(&mut self, timeout_ms: i32) -> std::io::Result<Option<Dequeued>> {
        let mut pfd = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::Interrupted { Ok(None) } else { Err(e) };
        }
        if n == 0 {
            return Ok(None);
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 && pfd.revents & libc::POLLIN == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "device error (unplugged?)"));
        }
        let mut b = self.desc(0);
        match unsafe { ioctl(self.fd, v4l::v4l2::vidioc::VIDIOC_DQBUF, &mut b) } {
            Ok(()) => Ok(Some(Dequeued { index: b.index as usize, bytesused: b.bytesused, flags: b.flags })),
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Drop for MmapStream {
    fn drop(&mut self) {
        let mut typ = BUF_TYPE;
        // Errors (ENODEV after an unplug) don't matter here: the buffers go either way.
        let _ = unsafe { ioctl(self.fd, v4l::v4l2::vidioc::VIDIOC_STREAMOFF, &mut typ) };
        for b in self.bufs.drain(..) {
            unsafe { libc::munmap(b.as_ptr() as *mut c_void, b.len()) };
        }
        let mut req: sys::v4l2_requestbuffers = unsafe { std::mem::zeroed() };
        req.type_ = BUF_TYPE;
        req.memory = MEMORY;
        let _ = unsafe { ioctl(self.fd, v4l::v4l2::vidioc::VIDIOC_REQBUFS, &mut req) };
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn card_names() {
        assert_eq!(super::card_name("Elgato 4K X: Elgato 4K X"), "Elgato 4K X");
        assert_eq!(super::card_name("USB Video: USB3.0 HD Capture"), "USB Video: USB3.0 HD Capture");
        assert_eq!(super::card_name("Cam Link 4K"), "Cam Link 4K");
    }
}

/// Capture loop with automatic reconnect (see [`super::run_capture_loop`]).
pub fn run_capture(
    device: Option<String>,
    pref: Option<ModePref>,
    chroma: Chroma,
    range: RangePref,
    stop: Arc<AtomicBool>,
    on_open: impl FnMut(Result<(String, Vec<VideoMode>, VideoMode, Chroma, ColorInfo)>),
    on_frame: impl FnMut(VideoFrame),
) {
    super::run_capture_loop::<VideoDevice>(device, pref, chroma, range, stop, on_open, on_frame);
}
