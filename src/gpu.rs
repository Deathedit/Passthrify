//! CUDA frames: each captured frame is uploaded to the GPU once, and NVENC and frame
//! generation both read that copy (instead of NVENC copying and uploading the frame itself,
//! and frame generation uploading it again).
//!
//! Frames come from an FFmpeg CUDA frames pool, so NVENC takes them as `AV_PIX_FMT_CUDA` input.
//! The CUDA context is the device's primary context: the same one the CUDA runtime in the
//! frame generation SDK uses, so its device pointers are valid there too. CUDA comes from
//! nvcuda.dll, which ships with every NVIDIA driver.

use std::ffi::c_void;
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, Result};
use ffmpeg_next::ffi as ff;
use windows::core::{s, w};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

use crate::capture::{Chroma, VideoFrame};

type CuResult = i32;
type CuContext = *mut c_void;
type CuStream = *mut c_void;
type CuEvent = *mut c_void;
const CU_STREAM_NON_BLOCKING: u32 = 1;
const CU_EVENT_BLOCKING_SYNC: u32 = 1;
const CU_EVENT_DISABLE_TIMING: u32 = 2;

/// The CUDA driver: the primary context of device 0 and two non-blocking streams, all living
/// for the whole process (retained once, never released). Uploads get a stream of their own:
/// NVENC queues its input work on FFmpeg's device stream, and waiting on that stream after an
/// upload also waited for the previous frame's encode (6 ms instead of 0.5 ms at 4K).
struct Driver {
    ctx: usize,
    /// Our uploads only.
    upload_stream: usize,
    /// FFmpeg's CUDA device (and so NVENC's input/output) stream.
    ffmpeg_stream: usize,
    /// Recorded after each upload. Created with blocking sync, so waiting on it sleeps instead
    /// of spinning a CPU core (the default stream sync spins).
    done: usize,
    memcpy2d_async: unsafe extern "system" fn(*const CudaMemcpy2d, CuStream) -> CuResult,
    push: unsafe extern "system" fn(CuContext) -> CuResult,
    pop: unsafe extern "system" fn(*mut CuContext) -> CuResult,
    event_record: unsafe extern "system" fn(CuEvent, CuStream) -> CuResult,
    event_sync: unsafe extern "system" fn(CuEvent) -> CuResult,
    host_alloc: unsafe extern "system" fn(*mut *mut c_void, usize, u32) -> CuResult,
    host_free: unsafe extern "system" fn(*mut c_void) -> CuResult,
}

fn driver() -> Result<&'static Driver> {
    static DRIVER: OnceLock<std::result::Result<Driver, String>> = OnceLock::new();
    DRIVER.get_or_init(|| unsafe { Driver::open() }.map_err(|e| format!("{e:#}"))).as_ref().map_err(|e| anyhow!("{e}"))
}

impl Driver {
    unsafe fn open() -> Result<Self> {
        let lib = LoadLibraryExW(w!("nvcuda.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| anyhow!("nvcuda.dll not found (NVIDIA driver): {e}"))?;
        unsafe fn sym<T>(lib: HMODULE, name: windows::core::PCSTR) -> Result<T> {
            let p = GetProcAddress(lib, name).ok_or_else(|| anyhow!("{} missing from nvcuda.dll", name.display()))?;
            Ok(std::mem::transmute_copy(&p))
        }
        let init: unsafe extern "system" fn(u32) -> CuResult = sym(lib, s!("cuInit"))?;
        let device_get: unsafe extern "system" fn(*mut i32, i32) -> CuResult = sym(lib, s!("cuDeviceGet"))?;
        let retain: unsafe extern "system" fn(*mut CuContext, i32) -> CuResult =
            sym(lib, s!("cuDevicePrimaryCtxRetain"))?;
        let stream_create: unsafe extern "system" fn(*mut CuStream, u32) -> CuResult =
            sym(lib, s!("cuStreamCreate"))?;
        let push: unsafe extern "system" fn(CuContext) -> CuResult = sym(lib, s!("cuCtxPushCurrent_v2"))?;
        let pop: unsafe extern "system" fn(*mut CuContext) -> CuResult = sym(lib, s!("cuCtxPopCurrent_v2"))?;
        let event_create: unsafe extern "system" fn(*mut CuEvent, u32) -> CuResult = sym(lib, s!("cuEventCreate"))?;
        let event_record = sym(lib, s!("cuEventRecord"))?;
        let event_sync = sym(lib, s!("cuEventSynchronize"))?;
        let host_alloc = sym(lib, s!("cuMemHostAlloc"))?;
        let host_free = sym(lib, s!("cuMemFreeHost"))?;
        let memcpy2d_async = sym(lib, s!("cuMemcpy2DAsync_v2"))?;

        let check = |r: CuResult, what: &str| if r == 0 { Ok(()) } else { Err(anyhow!("{what} failed (CUDA error {r})")) };
        check(init(0), "cuInit")?;
        let mut dev = 0;
        check(device_get(&mut dev, 0), "cuDeviceGet")?;
        let mut ctx = std::ptr::null_mut();
        check(retain(&mut ctx, dev), "cuDevicePrimaryCtxRetain")?;
        check(push(ctx), "cuCtxPushCurrent")?;
        let (mut upload_stream, mut ffmpeg_stream) = (std::ptr::null_mut(), std::ptr::null_mut());
        let mut created = stream_create(&mut upload_stream, CU_STREAM_NON_BLOCKING);
        if created == 0 {
            created = stream_create(&mut ffmpeg_stream, CU_STREAM_NON_BLOCKING);
        }
        let mut done = std::ptr::null_mut();
        if created == 0 {
            created = event_create(&mut done, CU_EVENT_BLOCKING_SYNC | CU_EVENT_DISABLE_TIMING);
        }
        let mut prev = std::ptr::null_mut();
        pop(&mut prev);
        check(created, "creating CUDA streams")?;
        Ok(Self {
            ctx: ctx as usize,
            upload_stream: upload_stream as usize,
            ffmpeg_stream: ffmpeg_stream as usize,
            done: done as usize,
            memcpy2d_async,
            push,
            pop,
            event_record,
            event_sync,
            host_alloc,
            host_free,
        })
    }

    /// Runs `f` with our context current on this thread.
    fn with_ctx(&self, what: &str, f: impl FnOnce() -> CuResult) -> Result<()> {
        unsafe {
            let r = (self.push)(self.ctx as CuContext);
            if r != 0 {
                bail!("cuCtxPushCurrent failed (CUDA error {r})");
            }
            let r = f();
            let mut prev = std::ptr::null_mut();
            (self.pop)(&mut prev);
            if r != 0 {
                bail!("{what} failed (CUDA error {r})");
            }
        }
        Ok(())
    }

    /// Copies planes (host pointer, host pitch, device pointer, device pitch, row bytes, rows)
    /// to the GPU on the upload stream and waits for them. Pinned sources go by DMA.
    fn upload_planes(&self, planes: &[(*const u8, usize, *mut u8, usize, usize, usize)]) -> Result<()> {
        let stream = self.upload_stream as CuStream;
        self.with_ctx("cuMemcpy2DAsync", || unsafe {
            for &(src, src_pitch, dst, dst_pitch, width, rows) in planes {
                let c = CudaMemcpy2d {
                    src_memory_type: CU_MEMORYTYPE_HOST,
                    src_host: src.cast(),
                    src_pitch,
                    dst_memory_type: CU_MEMORYTYPE_DEVICE,
                    dst_device: dst as u64,
                    dst_pitch,
                    width_in_bytes: width,
                    height: rows,
                    ..Default::default()
                };
                let r = (self.memcpy2d_async)(&c, stream);
                if r != 0 {
                    return r;
                }
            }
            // All uploads share this stream, so the latest record covers every earlier copy,
            // even if another thread records in between.
            (self.event_record)(self.done as CuEvent, stream)
        })?;
        // Wait until the frame is on the GPU, so NVENC and frame generation (on other streams)
        // see all of it and the host buffer can be reused.
        self.with_ctx("cuEventSynchronize", || unsafe { (self.event_sync)(self.done as CuEvent) })
    }
}

/// Page-locked host memory: the GPU copies from it by DMA at full PCIe speed, where pageable
/// memory goes through a driver staging copy. Capture frame buffers live in it.
pub struct PinnedBuf {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for PinnedBuf {}
unsafe impl Sync for PinnedBuf {}

impl PinnedBuf {
    /// None without CUDA (or if the driver refuses), so callers fall back to normal memory.
    pub fn new(len: usize) -> Option<Self> {
        let d = driver().ok()?;
        let mut p = std::ptr::null_mut();
        d.with_ctx("cuMemHostAlloc", || unsafe { (d.host_alloc)(&mut p, len.max(1), 0) }).ok()?;
        Some(Self { ptr: p.cast(), len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for PinnedBuf {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            let _ = d.with_ctx("cuMemFreeHost", || unsafe { (d.host_free)(self.ptr.cast()) });
        }
    }
}

const CU_MEMORYTYPE_HOST: u32 = 1;
const CU_MEMORYTYPE_DEVICE: u32 = 2;

/// `CUDA_MEMCPY2D` from cuda.h.
#[repr(C)]
struct CudaMemcpy2d {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: u64,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: u64,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

impl Default for CudaMemcpy2d {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// `AVCUDADeviceContext` from libavutil/hwcontext_cuda.h (not in the generated bindings).
#[repr(C)]
struct AvCudaDeviceContext {
    cuda_ctx: CuContext,
    stream: CuStream,
    internal: *mut c_void,
}

struct DeviceRef(*mut ff::AVBufferRef);
unsafe impl Send for DeviceRef {}
unsafe impl Sync for DeviceRef {}

/// FFmpeg's CUDA device on our context and stream (created once).
fn device() -> Result<*mut ff::AVBufferRef> {
    static DEVICE: OnceLock<std::result::Result<DeviceRef, String>> = OnceLock::new();
    let dev = DEVICE.get_or_init(|| {
        let open = || -> Result<DeviceRef> {
            let d = driver()?;
            unsafe {
                let r = ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA);
                if r.is_null() {
                    bail!("av_hwdevice_ctx_alloc(CUDA) failed (FFmpeg built without CUDA?)");
                }
                let hw = (*((*r).data as *mut ff::AVHWDeviceContext)).hwctx as *mut AvCudaDeviceContext;
                (*hw).cuda_ctx = d.ctx as CuContext;
                (*hw).stream = d.ffmpeg_stream as CuStream;
                let e = ff::av_hwdevice_ctx_init(r);
                if e < 0 {
                    let mut r = r;
                    ff::av_buffer_unref(&mut r);
                    bail!("av_hwdevice_ctx_init(CUDA) failed ({})", av_err(e));
                }
                Ok(DeviceRef(r))
            }
        };
        open().map_err(|e| format!("{e:#}"))
    });
    dev.as_ref().map(|d| d.0).map_err(|e| anyhow!("{e}"))
}

fn av_err(e: i32) -> String {
    ffmpeg_next::Error::from(e).to_string()
}

/// FFmpeg pixel format of the CUDA frames for a chroma. 4:4:4 is planar (NVENC has no NV24).
fn sw_format(chroma: Chroma) -> ff::AVPixelFormat {
    match chroma {
        Chroma::S420 => ff::AVPixelFormat::AV_PIX_FMT_NV12,
        Chroma::S422 => ff::AVPixelFormat::AV_PIX_FMT_NV16,
        Chroma::S444 => ff::AVPixelFormat::AV_PIX_FMT_YUV444P,
    }
}

/// A pool of CUDA frames for one capture shape.
pub struct CudaFrames {
    frames: *mut ff::AVBufferRef,
    pub width: u32,
    pub height: u32,
    pub chroma: Chroma,
}

unsafe impl Send for CudaFrames {}
unsafe impl Sync for CudaFrames {}

impl CudaFrames {
    pub fn new(width: u32, height: u32, chroma: Chroma) -> Result<Arc<Self>> {
        let dev = device()?;
        unsafe {
            let mut r = ff::av_hwframe_ctx_alloc(dev);
            if r.is_null() {
                bail!("av_hwframe_ctx_alloc failed");
            }
            let fc = (*r).data as *mut ff::AVHWFramesContext;
            (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_CUDA;
            (*fc).sw_format = sw_format(chroma);
            (*fc).width = width as i32;
            (*fc).height = height as i32;
            let e = ff::av_hwframe_ctx_init(r);
            if e < 0 {
                ff::av_buffer_unref(&mut r);
                bail!("CUDA frames {width}x{height} {} not supported ({})", chroma.label(), av_err(e));
            }
            Ok(Arc::new(Self { frames: r, width, height, chroma }))
        }
    }

    /// The frames context, for the encoder's `hw_frames_ctx` (the caller takes its own ref).
    pub fn frames_ctx(&self) -> *mut ff::AVBufferRef {
        self.frames
    }

    pub fn matches(&self, f: &VideoFrame) -> bool {
        f.width == self.width && f.height == self.height && f.chroma == self.chroma
    }

    /// Uploads `f` into a frame from the pool and waits for the copy to finish.
    /// `scratch` holds the planar U and V of a 4:4:4 frame.
    pub fn upload(&self, f: &VideoFrame, scratch: &mut Vec<u8>) -> Result<GpuFrame> {
        if !self.matches(f) {
            bail!("{}x{} {} frame for {}x{} {} CUDA frames", f.width, f.height, f.chroma.label(), self.width, self.height, self.chroma.label());
        }
        let (w, h) = (f.width as usize, f.height as usize);
        let (cw, ch) = f.chroma_size();
        let (cw, ch) = (cw as usize, ch as usize);
        unsafe {
            let dst = GpuFrame(ff::av_frame_alloc());
            if dst.0.is_null() {
                bail!("av_frame_alloc failed");
            }
            let e = ff::av_hwframe_get_buffer(self.frames, dst.0, 0);
            if e < 0 {
                bail!("av_hwframe_get_buffer failed ({})", av_err(e));
            }
            let d = &*dst.0;
            let (dp, dl) = (d.data, d.linesize.map(|l| l as usize));
            let y = f.y_plane().as_ptr();
            match f.chroma {
                Chroma::S420 | Chroma::S422 => {
                    let uv = f.uv_plane().as_ptr();
                    driver()?.upload_planes(&[(y, w, dp[0], dl[0], w, h), (uv, cw * 2, dp[1], dl[1], cw * 2, ch)])?;
                }
                // NV24 -> planar YUV444P (NVENC has no interleaved 4:4:4 input).
                Chroma::S444 => {
                    scratch.resize(cw * ch * 2, 0);
                    let (u, v) = scratch.split_at_mut(cw * ch);
                    for ((pair, u), v) in f.uv_plane().chunks_exact(2).zip(u.iter_mut()).zip(v.iter_mut()) {
                        *u = pair[0];
                        *v = pair[1];
                    }
                    driver()?.upload_planes(&[
                        (y, w, dp[0], dl[0], w, h),
                        (u.as_ptr(), cw, dp[1], dl[1], cw, ch),
                        (v.as_ptr(), cw, dp[2], dl[2], cw, ch),
                    ])?;
                }
            }
            Ok(dst)
        }
    }
}

impl Drop for CudaFrames {
    fn drop(&mut self) {
        unsafe { ff::av_buffer_unref(&mut self.frames) };
    }
}

/// One frame in CUDA memory. Holds a reference to its pool, so it can outlive `CudaFrames`.
pub struct GpuFrame(*mut ff::AVFrame);

unsafe impl Send for GpuFrame {}
unsafe impl Sync for GpuFrame {}

/// Device pointers and pitches of a CUDA frame, in the layout NvCVImage_TransferFromYUV takes.
pub struct Planes {
    pub y: *const u8,
    pub y_pitch: i32,
    pub u: *const u8,
    pub v: *const u8,
    /// Bytes from one U sample to the next: 2 for interleaved UV (NV12/NV16), 1 for planar.
    pub uv_pixel_bytes: i32,
    pub uv_pitch: i32,
}

impl GpuFrame {
    pub fn as_ptr(&self) -> *const ff::AVFrame {
        self.0
    }

    pub fn planes(&self) -> Planes {
        unsafe {
            let f = &*self.0;
            if !f.data[2].is_null() {
                Planes { y: f.data[0], y_pitch: f.linesize[0], u: f.data[1], v: f.data[2], uv_pixel_bytes: 1, uv_pitch: f.linesize[1] }
            } else {
                Planes { y: f.data[0], y_pitch: f.linesize[0], u: f.data[1], v: f.data[1].add(1), uv_pixel_bytes: 2, uv_pitch: f.linesize[1] }
            }
        }
    }
}

impl Drop for GpuFrame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

/// `passthrify --gpu-test`: encodes synthetic frames from system memory and from CUDA frames,
/// prints where each path spends time, and checks that frame generation gives the same picture
/// from a CUDA frame as from a CPU frame. Needs an NVIDIA GPU, no capture card.
pub fn self_test(vfg_dir: Option<&std::path::Path>) -> Result<()> {
    use crate::capture::FramePool;
    use crate::capture::video_mf::{PixFmt, VideoMode};
    use crate::config::Codec;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Instant;

    ffmpeg_next::init()?;
    let pool = Arc::new(FramePool::pinned());
    // A moving luma ramp with a chroma pattern, different every frame.
    let make = |w: u32, h: u32, chroma: Chroma, k: u64| {
        let mut data = pool.get(crate::capture::frame_len(w, h, chroma));
        let buf = data.as_mut_slice();
        let (luma, uv) = buf.split_at_mut((w * h) as usize);
        if k >= 8 && k % 30 != 0 {
            // Recycled buffers keep earlier content; skip refilling so the CPU numbers are
            // the frame path's own (every 30th frame still changes).
            return VideoFrame { width: w, height: h, chroma, color: Default::default(), data, pts_us: k as i64 * 16_667, seq: k, gpu: Default::default() };
        }
        for (r, row) in luma.chunks_exact_mut(w as usize).enumerate() {
            row.fill(((r as u64 + k * 7) % 200 + 20) as u8);
        }
        for (i, b) in uv.iter_mut().enumerate() {
            *b = (96 + (i as u64 / 64 + k) % 64) as u8;
        }
        VideoFrame { width: w, height: h, chroma, color: Default::default(), data, pts_us: k as i64 * 16_667, seq: k, gpu: Default::default() }
    };

    println!("CUDA: {}", match driver() { Ok(_) => "ok".to_string(), Err(e) => format!("{e:#}") });
    // Raw upload speed, nothing else running: pinned (capture buffers) vs pageable memory.
    {
        let (w, h) = (3840, 2160);
        let cuda = CudaFrames::new(w, h, Chroma::S420)?;
        let heap_pool = Arc::new(FramePool::default());
        let mut scratch = Vec::new();
        for (label, pool) in [("pinned", &pool), ("pageable", &heap_pool)] {
            let f = {
                let mut data = pool.get(crate::capture::frame_len(w, h, Chroma::S420));
                data.as_mut_slice().fill(100);
                VideoFrame { width: w, height: h, chroma: Chroma::S420, color: Default::default(), data, pts_us: 0, seq: 0, gpu: Default::default() }
            };
            let pinned = PinnedBuf::new(16).is_some() && label == "pinned";
            cuda.upload(&f, &mut scratch)?; // warm-up
            let runs = 60;
            let t = Instant::now();
            for _ in 0..runs {
                cuda.upload(&f, &mut scratch)?;
            }
            let ms = t.elapsed().as_secs_f64() * 1e3 / runs as f64;
            println!(
                "4K upload from {label} memory{}: {ms:.2} ms ({:.1} GB/s)",
                if label == "pinned" && !pinned { " (pinning unavailable, normal memory)" } else { "" },
                f.data.len() as f64 / ms / 1e6
            );
        }
    }

    let frames = 240u64;
    for (w, h, chroma) in [(3840, 2160, Chroma::S420), (3840, 2160, Chroma::S422), (1920, 1080, Chroma::S444)] {
        let mode = VideoMode { index: 0, width: w, height: h, fps_num: 60, fps_den: 1, format: PixFmt::Nv12 };
        for use_gpu in [false, true] {
            let label = format!("{w}x{h} {} {}", chroma.label(), if use_gpu { "CUDA frames  " } else { "system memory" });
            let cuda = if use_gpu {
                match CudaFrames::new(w, h, chroma) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        println!("{label}: unavailable: {e:#}");
                        continue;
                    }
                }
            } else {
                None
            };
            let cfg = crate::encode::video::VideoEncoderConfig {
                codec: Codec::Hevc,
                mode,
                chroma,
                color: Default::default(),
                bitrate: 50_000_000,
                gpu: cuda.clone(),
            };
            let (tx, rx) = crossbeam_channel::bounded::<Arc<VideoFrame>>(4);
            let (packets, bytes) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
            let enc = {
                let (packets, bytes) = (packets.clone(), bytes.clone());
                std::thread::spawn(move || {
                    crate::encode::video::run(cfg, rx, Arc::new(AtomicBool::new(false)), Arc::default(), |p| {
                        packets.fetch_add(1, Ordering::Relaxed);
                        bytes.fetch_add(p.data.len() as u64, Ordering::Relaxed);
                    })
                })
            };
            let _ = crate::perf::summary(); // reset
            let mut scratch = Vec::new();
            let cpu0 = process_cpu_ms();
            let t = Instant::now();
            for k in 0..frames {
                let f = make(w, h, chroma, k);
                if let Some(c) = &cuda {
                    let _ = f.gpu.set(crate::perf::GPU_UPLOAD.time(|| c.upload(&f, &mut scratch))?);
                }
                if tx.send(Arc::new(f)).is_err() {
                    break;
                }
            }
            drop(tx);
            let res = enc.join().map_err(|_| anyhow!("encoder thread panicked"))?;
            let secs = t.elapsed().as_secs_f64();
            let cpu = (process_cpu_ms() - cpu0) / frames as f64;
            match res {
                Ok(()) => println!(
                    "{label}: {:.0} fps, CPU {cpu:.2} ms/frame, {} packets, {:.1} MB · {}",
                    frames as f64 / secs,
                    packets.load(Ordering::Relaxed),
                    bytes.load(Ordering::Relaxed) as f64 / 1e6,
                    crate::perf::summary().unwrap_or_default()
                ),
                Err(e) => println!("{label}: encoder failed: {e:#}"),
            }
        }
    }

    // Frame generation from a CUDA frame must match the same frame from CPU memory.
    match crate::vfg::sdk::load(vfg_dir) {
        Err(e) => println!("frame generation check skipped: {e:#}"),
        Ok(sdk) => {
            let (w, h) = (3840, 2160);
            let cuda = CudaFrames::new(w, h, Chroma::S420)?;
            let shape = crate::vfg::sdk::Shape {
                width: w,
                height: h,
                chroma: Chroma::S420,
                color: Default::default(),
                quality: crate::vfg::Quality::Medium,
                multiplier: 2,
            };
            let mut scratch = Vec::new();
            let mut run = |gpu: bool| -> Result<(VideoFrame, f64)> {
                let mut interp = crate::vfg::sdk::Interpolator::new(sdk.clone(), shape)?;
                let (a, b) = (make(w, h, Chroma::S420, 1), make(w, h, Chroma::S420, 5));
                if gpu {
                    let _ = a.gpu.set(cuda.upload(&a, &mut scratch)?);
                    let _ = b.gpu.set(cuda.upload(&b, &mut scratch)?);
                }
                interp.push(&a)?;
                let mut out = interp.push(&b)?;
                let upload_ms = interp.timings[0];
                Ok((out.pop().ok_or_else(|| anyhow!("no generated frame"))?, upload_ms))
            };
            let (cpu, cpu_ms) = run(false)?;
            let (gpu, gpu_ms) = run(true)?;
            let diff = cpu.data.iter().zip(gpu.data.iter()).map(|(a, b)| a.abs_diff(*b) as u64).sum::<u64>() as f64
                / cpu.data.len() as f64;
            println!(
                "frame generation 4K: CPU frame input {cpu_ms:.2} ms, CUDA frame input {gpu_ms:.2} ms, mean difference {diff:.3} {}",
                if diff < 0.5 { "(same picture)" } else { "(DIFFERENT)" }
            );
            anyhow::ensure!(diff < 0.5, "frame generation output differs between CPU and CUDA input");
        }
    }
    Ok(())
}

/// CPU time used by this process so far (all threads), in ms.
pub fn process_cpu_ms() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 10_000.0;
    t(k) + t(u)
}
