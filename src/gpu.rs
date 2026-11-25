//! CUDA frames: each captured frame is uploaded to the GPU once, and NVENC and frame
//! generation both read that copy (instead of NVENC copying and uploading the frame itself,
//! and frame generation uploading it again).
//!
//! Frames come from an FFmpeg CUDA frames pool, so NVENC takes them as `AV_PIX_FMT_CUDA` input.
//! The CUDA context is the device's primary context: the same one the CUDA runtime in the
//! frame generation SDK uses, so its device pointers are valid there too. CUDA comes from
//! nvcuda.dll (Windows) or libcuda.so.1 (Linux), which ship with every NVIDIA driver.

use std::ffi::{c_void, CStr};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, Result};
use ffmpeg_next::ffi as ff;

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
    import_ext_mem: unsafe extern "system" fn(*mut CuExtMem, *const ExtMemHandleDesc) -> CuResult,
    ext_mem_buffer: unsafe extern "system" fn(*mut u64, CuExtMem, *const ExtMemBufferDesc) -> CuResult,
    destroy_ext_mem: unsafe extern "system" fn(CuExtMem) -> CuResult,
    mem_free: unsafe extern "system" fn(u64) -> CuResult,
}

/// The CUDA driver library. Never unloaded: the driver lives for the whole process.
struct CudaLib(*mut c_void);

#[cfg(windows)]
impl CudaLib {
    const NAME: &'static str = "nvcuda.dll";

    unsafe fn open() -> Result<Self> {
        use windows::core::w;
        use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
        let lib = LoadLibraryExW(w!("nvcuda.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| anyhow!("{} not found (NVIDIA driver): {e}", Self::NAME))?;
        Ok(Self(lib.0))
    }

    fn sym(&self, name: &CStr) -> Result<*mut c_void> {
        use windows::core::PCSTR;
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::System::LibraryLoader::GetProcAddress;
        let p = unsafe { GetProcAddress(HMODULE(self.0), PCSTR(name.as_ptr().cast())) };
        p.map(|f| f as *mut c_void).ok_or_else(|| anyhow!("{} missing from {}", name.to_string_lossy(), Self::NAME))
    }
}

#[cfg(target_os = "linux")]
impl CudaLib {
    const NAME: &'static str = "libcuda.so.1";

    unsafe fn open() -> Result<Self> {
        let lib = libc::dlopen(c"libcuda.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if lib.is_null() {
            let err = libc::dlerror();
            let msg = if err.is_null() { "unknown error".into() } else { CStr::from_ptr(err).to_string_lossy() };
            bail!("{} not found (NVIDIA driver): {msg}", Self::NAME);
        }
        Ok(Self(lib))
    }

    fn sym(&self, name: &CStr) -> Result<*mut c_void> {
        let p = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        if p.is_null() {
            bail!("{} missing from {}", name.to_string_lossy(), Self::NAME);
        }
        Ok(p)
    }
}

fn driver() -> Result<&'static Driver> {
    static DRIVER: OnceLock<std::result::Result<Driver, String>> = OnceLock::new();
    DRIVER.get_or_init(|| unsafe { Driver::open() }.map_err(|e| format!("{e:#}"))).as_ref().map_err(|e| anyhow!("{e}"))
}

impl Driver {
    unsafe fn open() -> Result<Self> {
        let lib = CudaLib::open()?;
        let sym = |name: &CStr| lib.sym(name);
        unsafe fn cast<T>(p: Result<*mut c_void>) -> Result<T> {
            let p = p?;
            Ok(std::mem::transmute_copy(&p))
        }
        let init: unsafe extern "system" fn(u32) -> CuResult = cast(sym(c"cuInit"))?;
        let device_get: unsafe extern "system" fn(*mut i32, i32) -> CuResult = cast(sym(c"cuDeviceGet"))?;
        let retain: unsafe extern "system" fn(*mut CuContext, i32) -> CuResult =
            cast(sym(c"cuDevicePrimaryCtxRetain"))?;
        let stream_create: unsafe extern "system" fn(*mut CuStream, u32) -> CuResult =
            cast(sym(c"cuStreamCreate"))?;
        let push: unsafe extern "system" fn(CuContext) -> CuResult = cast(sym(c"cuCtxPushCurrent_v2"))?;
        let pop: unsafe extern "system" fn(*mut CuContext) -> CuResult = cast(sym(c"cuCtxPopCurrent_v2"))?;
        let event_create: unsafe extern "system" fn(*mut CuEvent, u32) -> CuResult = cast(sym(c"cuEventCreate"))?;
        let event_record = cast(sym(c"cuEventRecord"))?;
        let event_sync = cast(sym(c"cuEventSynchronize"))?;
        let host_alloc = cast(sym(c"cuMemHostAlloc"))?;
        let host_free = cast(sym(c"cuMemFreeHost"))?;
        let memcpy2d_async = cast(sym(c"cuMemcpy2DAsync_v2"))?;
        let import_ext_mem = cast(sym(c"cuImportExternalMemory"))?;
        let ext_mem_buffer = cast(sym(c"cuExternalMemoryGetMappedBuffer"))?;
        let destroy_ext_mem = cast(sym(c"cuDestroyExternalMemory"))?;
        let mem_free = cast(sym(c"cuMemFree_v2"))?;

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
            import_ext_mem,
            ext_mem_buffer,
            destroy_ext_mem,
            mem_free,
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
        self.copy_planes(planes, false)
    }

    /// Like `upload_planes`, with `src_device`: the sources are device pointers too.
    fn copy_planes(&self, planes: &[(*const u8, usize, *mut u8, usize, usize, usize)], src_device: bool) -> Result<()> {
        let stream = self.upload_stream as CuStream;
        self.with_ctx("cuMemcpy2DAsync", || unsafe {
            for &(src, src_pitch, dst, dst_pitch, width, rows) in planes {
                let c = CudaMemcpy2d {
                    src_memory_type: if src_device { CU_MEMORYTYPE_DEVICE } else { CU_MEMORYTYPE_HOST },
                    src_host: if src_device { std::ptr::null() } else { src.cast() },
                    src_device: if src_device { src as u64 } else { 0 },
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

type CuExtMem = *mut c_void;
#[cfg(windows)]
const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32: u32 = 2;
#[cfg(target_os = "linux")]
const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: u32 = 1;
const CUDA_EXTERNAL_MEMORY_DEDICATED: u32 = 1;

/// An exported memory handle: an opaque Win32 handle (the caller keeps ownership and closes it)
/// or an opaque POSIX fd (CUDA takes ownership when the import succeeds).
#[cfg(windows)]
pub type ExtMemHandle = *mut c_void;
#[cfg(target_os = "linux")]
pub type ExtMemHandle = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct Win32Handle {
    handle: *mut c_void,
    name: *const c_void,
}

/// The handle union of `CUDA_EXTERNAL_MEMORY_HANDLE_DESC` (16 bytes, 8-byte aligned).
#[repr(C)]
union ExtMemHandleUnion {
    fd: i32,
    win32: Win32Handle,
}

/// `CUDA_EXTERNAL_MEMORY_HANDLE_DESC` from cuda.h.
#[repr(C)]
struct ExtMemHandleDesc {
    handle_type: u32,
    handle: ExtMemHandleUnion,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}

impl ExtMemHandleDesc {
    fn new(handle: ExtMemHandle, size: u64) -> Self {
        #[cfg(windows)]
        let (handle_type, handle) = (
            CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32,
            ExtMemHandleUnion { win32: Win32Handle { handle, name: std::ptr::null() } },
        );
        #[cfg(target_os = "linux")]
        let (handle_type, handle) = {
            // Zero the whole union first so the bytes after the fd are defined.
            let mut u = ExtMemHandleUnion { win32: Win32Handle { handle: std::ptr::null_mut(), name: std::ptr::null() } };
            u.fd = handle;
            (CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD, u)
        };
        Self { handle_type, handle, size, flags: CUDA_EXTERNAL_MEMORY_DEDICATED, reserved: [0; 16] }
    }
}

/// `CUDA_EXTERNAL_MEMORY_BUFFER_DESC` from cuda.h.
#[repr(C)]
struct ExtMemBufferDesc {
    offset: u64,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}

const SLOT_FREE: u8 = 0;
const SLOT_WRITING: u8 = 1;
const SLOT_READY: u8 = 2;
const SLOT_READING: u8 = 3;

struct Slot {
    state: std::sync::atomic::AtomicU8,
    seq: std::sync::atomic::AtomicU64,
}

/// Number of frame slots in the preview buffer: one being written, one being read by the
/// GPU, one ready.
pub const PREVIEW_SLOTS: usize = 3;

/// The preview's frame buffer in GPU memory, allocated by the UI's graphics API (Vulkan) and
/// imported into CUDA. The capture thread copies each CUDA frame into a free slot on the GPU;
/// the preview copies a ready slot into its textures, so the frame never goes back through
/// the CPU. Each slot holds the Y plane, then the interleaved UV plane, with 256-byte pitches.
pub struct PreviewTarget {
    pub width: u32,
    pub height: u32,
    pub chroma: Chroma,
    pub y_pitch: usize,
    pub uv_pitch: usize,
    pub slot_size: usize,
    dev_ptr: u64,
    ext_mem: usize,
    slots: [Slot; PREVIEW_SLOTS],
}

unsafe impl Send for PreviewTarget {}
unsafe impl Sync for PreviewTarget {}

impl PreviewTarget {
    /// Row pitches and slot size for a frame shape (pitches 256-byte aligned for wgpu copies).
    pub fn layout(width: u32, height: u32, chroma: Chroma) -> (usize, usize, usize) {
        let align = |n: usize| n.div_ceil(256) * 256;
        let (cw, ch) = crate::capture::chroma_size(width, height, chroma);
        let (y_pitch, uv_pitch) = (align(width as usize), align(cw as usize * 2));
        (y_pitch, uv_pitch, align(y_pitch * height as usize + uv_pitch * ch as usize))
    }

    /// Only interleaved-UV frames (NV12, NV16); 4:4:4 CUDA frames are planar.
    pub fn supports(chroma: Chroma) -> bool {
        chroma != Chroma::S444
    }

    /// Imports `memory_size` bytes of exported Vulkan memory for `PREVIEW_SLOTS` slots of this
    /// frame shape. Windows: the caller still owns and closes the handle. Linux: CUDA owns the fd
    /// once the import succeeds; on an error the caller still owns (and closes) it.
    pub fn import(handle: ExtMemHandle, memory_size: u64, width: u32, height: u32, chroma: Chroma) -> Result<Self> {
        let d = driver()?;
        let (y_pitch, uv_pitch, slot_size) = Self::layout(width, height, chroma);
        let size = (slot_size * PREVIEW_SLOTS) as u64;
        if size > memory_size {
            bail!("preview memory too small ({memory_size} < {size})");
        }
        let desc = ExtMemHandleDesc::new(handle, memory_size);
        let mut ext_mem = std::ptr::null_mut();
        d.with_ctx("cuImportExternalMemory", || unsafe { (d.import_ext_mem)(&mut ext_mem, &desc) })?;
        let buf = ExtMemBufferDesc { offset: 0, size, flags: 0, reserved: [0; 16] };
        let mut dev_ptr = 0u64;
        if let Err(e) = d.with_ctx("cuExternalMemoryGetMappedBuffer", || unsafe { (d.ext_mem_buffer)(&mut dev_ptr, ext_mem, &buf) }) {
            let _ = d.with_ctx("cuDestroyExternalMemory", || unsafe { (d.destroy_ext_mem)(ext_mem) });
            return Err(e);
        }
        let slot = || Slot { state: SLOT_FREE.into(), seq: 0.into() };
        Ok(Self {
            width,
            height,
            chroma,
            y_pitch,
            uv_pitch,
            slot_size,
            dev_ptr,
            ext_mem: ext_mem as usize,
            slots: [slot(), slot(), slot()],
        })
    }

    pub fn matches(&self, f: &VideoFrame) -> bool {
        f.width == self.width && f.height == self.height && f.chroma == self.chroma
    }

    /// Byte offsets of slot `i`'s Y and UV planes in the buffer.
    pub fn plane_offsets(&self, i: usize) -> (u64, u64) {
        let y = (i * self.slot_size) as u64;
        (y, y + (self.y_pitch * self.height as usize) as u64)
    }

    /// Capture thread: copies `g` (a CUDA frame of this shape) into a free slot on the GPU and
    /// marks it ready as frame `seq`. False when no slot is free (the preview then uploads that
    /// frame from the CPU) or the copy failed.
    pub fn write(&self, g: &GpuFrame, seq: u64) -> bool {
        use std::sync::atomic::Ordering::*;
        // A free slot, else the oldest ready one (never one the preview is reading).
        let pick = (0..PREVIEW_SLOTS)
            .filter(|&i| self.slots[i].state.load(Acquire) == SLOT_FREE)
            .chain(
                (0..PREVIEW_SLOTS)
                    .filter(|&i| self.slots[i].state.load(Acquire) == SLOT_READY)
                    .min_by_key(|&i| self.slots[i].seq.load(Acquire)),
            )
            .find(|&i| {
                let s = &self.slots[i].state;
                s.compare_exchange(SLOT_FREE, SLOT_WRITING, AcqRel, Acquire).is_ok()
                    || s.compare_exchange(SLOT_READY, SLOT_WRITING, AcqRel, Acquire).is_ok()
            });
        let Some(i) = pick else { return false };
        let p = g.planes();
        let (yo, uvo) = self.plane_offsets(i);
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = crate::capture::chroma_size(self.width, self.height, self.chroma);
        let base = self.dev_ptr as usize;
        let ok = driver()
            .and_then(|d| {
                d.copy_planes(
                    &[
                        (p.y, p.y_pitch as usize, (base + yo as usize) as *mut u8, self.y_pitch, w, h),
                        (p.u, p.uv_pitch as usize, (base + uvo as usize) as *mut u8, self.uv_pitch, cw as usize * 2, ch as usize),
                    ],
                    true,
                )
            })
            .is_ok();
        let slot = &self.slots[i];
        if ok {
            slot.seq.store(seq, Release);
            slot.state.store(SLOT_READY, Release);
        } else {
            slot.state.store(SLOT_FREE, Release);
        }
        ok
    }

    /// Preview: claims the ready slot holding frame `seq` for reading.
    pub fn take(&self, seq: u64) -> Option<usize> {
        use std::sync::atomic::Ordering::*;
        (0..PREVIEW_SLOTS).find(|&i| {
            let s = &self.slots[i];
            s.state.load(Acquire) == SLOT_READY
                && s.seq.load(Acquire) == seq
                && s.state.compare_exchange(SLOT_READY, SLOT_READING, AcqRel, Acquire).is_ok()
                // Rewritten between the checks: give it back.
                && (s.seq.load(Acquire) == seq || {
                    s.state.store(SLOT_READY, Release);
                    false
                })
        })
    }

    /// Preview: the GPU has finished reading slot `i`.
    pub fn release(&self, i: usize) {
        self.slots[i].state.store(SLOT_FREE, std::sync::atomic::Ordering::Release);
    }
}

impl Drop for PreviewTarget {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            let _ = d.with_ctx("cuMemFree", || unsafe { (d.mem_free)(self.dev_ptr) });
            let _ = d.with_ctx("cuDestroyExternalMemory", || unsafe { (d.destroy_ext_mem)(self.ext_mem as CuExtMem) });
        }
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
pub fn self_test(vfg_dir: Option<&std::path::Path>, range_only: bool) -> Result<()> {
    use crate::capture::FramePool;
    use crate::capture::{PixFmt, VideoMode};
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
    if !range_only {
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

    }

    // Full-range input (what the 4K X sends) must come out as limited range, on both paths.
    // Writes MP4s to the temp dir for checking with ffprobe (signalstats).
    let full = crate::color::ColorInfo { full_range: true, matrix: crate::color::Matrix::Bt601 };
    for use_gpu in [true, false] {
        let (w, h) = (1920, 1080);
        let mode = VideoMode { index: 0, width: w, height: h, fps_num: 60, fps_den: 1, format: PixFmt::Nv12 };
        let cuda = if use_gpu { Some(CudaFrames::new(w, h, Chroma::S420)?) } else { None };
        let cfg = crate::encode::video::VideoEncoderConfig {
            codec: Codec::Hevc,
            mode,
            chroma: Chroma::S420,
            color: full,
            bitrate: 20_000_000,
            gpu: cuda.clone(),
        };
        let params: Arc<crate::packets::StreamParams> = Arc::default();
        let (tx, rx) = crossbeam_channel::bounded::<Arc<VideoFrame>>(4);
        let packets = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let enc = {
            let (packets, params) = (packets.clone(), params.clone());
            std::thread::spawn(move || {
                crate::encode::video::run(cfg, rx, Arc::new(AtomicBool::new(false)), params, |p| packets.lock().push(p))
            })
        };
        let mut scratch = Vec::new();
        for k in 0..60 {
            let mut f = make(w, h, Chroma::S420, k);
            f.color = full;
            if let Some(c) = &cuda {
                let _ = f.gpu.set(c.upload(&f, &mut scratch)?);
            }
            tx.send(Arc::new(f)).ok();
        }
        drop(tx);
        enc.join().map_err(|_| anyhow!("encoder thread panicked"))??;
        let path = std::env::temp_dir().join(format!("passthrify-range-{}.mp4", if use_gpu { "cuda" } else { "cpu" }));
        let video = params.video.lock().clone().ok_or_else(|| anyhow!("no video params"))?;
        let mut writer = crate::mux::LiveWriter::create(&path, &video, None)?;
        for p in packets.lock().iter() {
            writer.write(p)?;
        }
        writer.finish()?;
        println!("range check ({}): {}", if use_gpu { "CUDA" } else { "CPU" }, path.display());
    }
    if range_only {
        return Ok(());
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
#[cfg(target_os = "linux")]
pub fn process_cpu_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts);
    }
    ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1_000_000.0
}

#[cfg(windows)]
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
