//! Runtime binding to NVIDIA's Video Effects SDK (Maxine VFX) and its Video Frame Generation
//! effect. Nothing is linked at build time: the DLLs are loaded from the user's SDK install
//! when frame generation is first turned on, so Passthrify runs fine without them.
//!
//! Written against NVIDIA's public C API (nvVideoEffects.h / nvCVImage.h) and the MIT-licensed
//! VideoFrameGenerationEffectApp sample. `NvCVImage` is only ever handled through pointers
//! the SDK allocates (NvCVImage_Create), so its struct layout isn't needed here.

use std::ffi::{c_char, c_void, CStr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use windows::core::{HSTRING, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, SetDllDirectoryW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

use crate::capture::{Chroma, FramePool, VideoFrame};
use crate::color::{ColorInfo, Matrix};

/// The three DLLs a usable install has, in one folder.
pub const CORE_DLL: &str = "NVVideoEffects.dll";
pub const IMAGE_DLL: &str = "NVCVImage.dll";
pub const FEATURE_DLL: &str = "nvVFXVideoFrameGeneration.dll";

// nvCVImage.h values.
const NVCV_RGBA: u32 = 6;
const NVCV_YUV420: u32 = 10;
const NVCV_YUV422: u32 = 11;
const NVCV_YUV444: u32 = 12;
const NVCV_U8: u32 = 1;
const NVCV_INTERLEAVED: u32 = 0;
const NVCV_CPU: u32 = 0;
const NVCV_GPU: u32 = 1;
const NVCV_601: u32 = 0x00;
const NVCV_709: u32 = 0x01;
const NVCV_FULL_RANGE: u32 = 0x04;

type Status = i32;
type Handle = *mut c_void;
/// Opaque `NvCVImage*`.
type Image = c_void;

#[allow(non_snake_case)]
struct Api {
    GetVersion: unsafe extern "C" fn(*mut u32) -> Status,
    CreateEffect: unsafe extern "C" fn(*const c_char, *mut Handle) -> Status,
    DestroyEffect: unsafe extern "C" fn(Handle) -> Status,
    SetU32: unsafe extern "C" fn(Handle, *const c_char, u32) -> Status,
    SetImage: unsafe extern "C" fn(Handle, *const c_char, *mut Image) -> Status,
    SetCudaStream: unsafe extern "C" fn(Handle, *const c_char, *mut c_void) -> Status,
    Load: unsafe extern "C" fn(Handle) -> Status,
    Run: unsafe extern "C" fn(Handle, i32) -> Status,
    ImageCreate: unsafe extern "C" fn(u32, u32, u32, u32, u32, u32, u32, *mut *mut Image) -> Status,
    ImageDestroy: unsafe extern "C" fn(*mut Image),
    #[allow(clippy::type_complexity)]
    TransferFromYUV: unsafe extern "C" fn(
        *const c_void, // y
        i32,           // yPixBytes
        i32,           // yPitch
        *const c_void, // u
        *const c_void, // v
        i32,           // uvPixBytes
        i32,           // uvPitch
        u32,           // yuvFormat
        u32,           // yuvType
        u32,           // yuvColorSpace
        u32,           // yuvMemSpace
        *mut Image,    // dst
        *const c_void, // dstRect (null = all)
        f32,           // scale
        *mut c_void,   // stream
        *mut Image,    // tmp
    ) -> Status,
    #[allow(clippy::type_complexity)]
    TransferToYUV: unsafe extern "C" fn(
        *const Image,  // src
        *const c_void, // srcRect (null = all)
        *const c_void, // y
        i32,           // yPixBytes
        i32,           // yPitch
        *const c_void, // u
        *const c_void, // v
        i32,           // uvPixBytes
        i32,           // uvPitch
        u32,           // yuvFormat
        u32,           // yuvType
        u32,           // yuvColorSpace
        u32,           // yuvMemSpace
        f32,           // scale
        *mut c_void,   // stream
        *mut Image,    // tmp
    ) -> Status,
    ErrorString: Option<unsafe extern "C" fn(Status) -> *const c_char>,
}

/// The few CUDA runtime calls needed to move raw frames to and from the GPU, from the
/// cudart64_*.dll that ships next to the SDK. Converting YUV <-> RGBA is only fast GPU to GPU;
/// NvCVImage converts on the CPU when one side is in system memory.
#[allow(non_snake_case)]
struct Cuda {
    Malloc: unsafe extern "C" fn(*mut *mut c_void, usize) -> i32,
    Free: unsafe extern "C" fn(*mut c_void) -> i32,
    Memcpy: unsafe extern "C" fn(*mut c_void, *const c_void, usize, i32) -> i32,
}

const CUDA_HOST_TO_DEVICE: i32 = 1;
const CUDA_DEVICE_TO_HOST: i32 = 2;

/// Raw CUDA memory for one frame in its capture layout.
struct DeviceBuf {
    free: unsafe extern "C" fn(*mut c_void) -> i32,
    ptr: *mut c_void,
}

impl Drop for DeviceBuf {
    fn drop(&mut self) {
        unsafe { (self.free)(self.ptr) };
    }
}

/// The loaded SDK. Its libraries stay loaded for the rest of the process.
pub struct Sdk {
    api: Api,
    /// None: no cudart next to the SDK, frames are converted on the CPU (slow at 4K).
    cuda: Option<Cuda>,
    pub dir: PathBuf,
}

// The function pointers are plain C entry points.
unsafe impl Send for Sdk {}
unsafe impl Sync for Sdk {}

/// Folders to look in, in order. The default is a `vfg` folder next to the exe, holding either
/// the runtime download (DLLs at its top level) or a copied SDK (`vfg\bin` + `vfg\features`).
pub fn candidate_dirs(configured: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(exe_dir.join("vfg"));
        dirs.push(exe_dir.join("vfg").join("bin"));
    }
    if let Some(p) = configured {
        dirs.push(p.to_path_buf());
        dirs.push(p.join("bin"));
    }
    for var in ["PASSTHRIFY_VFG_DIR", "NV_VIDEO_EFFECTS_PATH", "VFXSDK_ROOT"] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            let p = PathBuf::from(v);
            dirs.push(p.join("bin"));
            dirs.push(p);
        }
    }
    dirs.push(super::runtime::local_dir());
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        let base = PathBuf::from(pf).join("NVIDIA Corporation");
        for name in ["NVIDIA Video Effects", "NVIDIA VFX SDK", "VFX SDK"] {
            dirs.push(base.join(name).join("bin"));
            dirs.push(base.join(name));
        }
    }
    dirs
}

/// The frame generation feature DLL for an SDK `bin` folder: next to the core DLLs (the
/// runtime download), or where NVIDIA's install_feature.ps1 puts it
/// (`<sdk>\features\nvvfxvideoframegeneration\...`).
pub fn feature_dll(dir: &Path) -> Option<PathBuf> {
    let flat = dir.join(FEATURE_DLL);
    if flat.is_file() {
        return Some(flat);
    }
    fn search(dir: &Path, depth: u32) -> Option<PathBuf> {
        let entries: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
        let hit = |p: &&PathBuf| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(FEATURE_DLL));
        if let Some(p) = entries.iter().find(hit) {
            return Some(p.clone());
        }
        if depth == 0 {
            return None;
        }
        entries.iter().filter(|p| p.is_dir()).find_map(|p| search(p, depth - 1))
    }
    search(&dir.parent()?.join("features").join("nvvfxvideoframegeneration"), 3)
}

/// First folder holding the core DLLs, with the frame generation feature installed.
pub fn find_runtime(configured: Option<&Path>) -> Option<PathBuf> {
    candidate_dirs(configured)
        .into_iter()
        .find(|d| [CORE_DLL, IMAGE_DLL].iter().all(|f| d.join(f).is_file()) && feature_dll(d).is_some())
}

/// The CUDA runtime DLL in `dir`, if any.
fn cudart_in(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        let n = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        n.starts_with("cudart64_") && n.ends_with(".dll")
    })
}

/// Loads (once per folder) the SDK from the first folder that has it.
pub fn load(configured: Option<&Path>) -> Result<Arc<Sdk>> {
    static LOADED: OnceLock<Mutex<Option<Arc<Sdk>>>> = OnceLock::new();
    let cell = LOADED.get_or_init(|| Mutex::new(None));
    let mut slot = cell.lock();
    let dir = find_runtime(configured).ok_or_else(|| {
        anyhow!("NVIDIA Video Effects SDK with Video Frame Generation not found (needs {CORE_DLL}, {IMAGE_DLL} and {FEATURE_DLL})")
    })?;
    if let Some(sdk) = slot.as_ref() {
        if sdk.dir != dir {
            tracing::warn!("VFG runtime moved to {}; still using {} until restart", dir.display(), sdk.dir.display());
        }
        return Ok(sdk.clone());
    }
    let sdk = Arc::new(unsafe { Sdk::open(&dir) }?);
    *slot = Some(sdk.clone());
    Ok(sdk)
}

unsafe fn open_lib(path: &Path) -> Result<HMODULE> {
    LoadLibraryExW(&HSTRING::from(path), None, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS)
        .with_context(|| format!("loading {}", path.display()))
}

unsafe fn sym<T>(lib: HMODULE, name: &str) -> Result<T> {
    let cname = format!("{name}\0");
    let p = GetProcAddress(lib, PCSTR(cname.as_ptr())).ok_or_else(|| anyhow!("{name} missing from the SDK"))?;
    Ok(std::mem::transmute_copy(&p))
}

impl Sdk {
    unsafe fn open(dir: &Path) -> Result<Self> {
        // The core DLL loads the feature DLL and its CUDA/TensorRT dependencies by name.
        let _ = SetDllDirectoryW(&HSTRING::from(dir));
        let image = open_lib(&dir.join(IMAGE_DLL))?;
        let core = open_lib(&dir.join(CORE_DLL))?;
        let feature_path = feature_dll(dir).ok_or_else(|| anyhow!("{FEATURE_DLL} not found for {}", dir.display()))?;
        let _feature = open_lib(&feature_path)?;
        let api = Api {
            GetVersion: sym(core, "NvVFX_GetVersion")?,
            CreateEffect: sym(core, "NvVFX_CreateEffect")?,
            DestroyEffect: sym(core, "NvVFX_DestroyEffect")?,
            SetU32: sym(core, "NvVFX_SetU32")?,
            SetImage: sym(core, "NvVFX_SetImage")?,
            SetCudaStream: sym(core, "NvVFX_SetCudaStream")?,
            Load: sym(core, "NvVFX_Load")?,
            Run: sym(core, "NvVFX_Run")?,
            ImageCreate: sym(image, "NvCVImage_Create")?,
            ImageDestroy: sym(image, "NvCVImage_Destroy")?,
            TransferFromYUV: sym(image, "NvCVImage_TransferFromYUV")?,
            TransferToYUV: sym(image, "NvCVImage_TransferToYUV")?,
            ErrorString: sym(image, "NvCV_GetErrorStringFromCode").or_else(|_| sym(core, "NvCV_GetErrorStringFromCode")).ok(),
        };
        // Any cudart works: device memory from the runtime API lives in the GPU's primary
        // context, which the SDK uses too.
        let cuda = cudart_in(dir)
            .or_else(|| feature_path.parent().and_then(cudart_in))
            .and_then(|p| open_lib(&p).ok())
            .and_then(|lib| {
                Some(Cuda {
                    Malloc: sym(lib, "cudaMalloc").ok()?,
                    Free: sym(lib, "cudaFree").ok()?,
                    Memcpy: sym(lib, "cudaMemcpy").ok()?,
                })
            });
        if cuda.is_none() {
            tracing::warn!("no CUDA runtime next to the VFX SDK; frame conversion falls back to the CPU");
        }
        let mut version = 0;
        let sdk = Sdk { api, cuda, dir: dir.to_path_buf() };
        sdk.check((sdk.api.GetVersion)(&mut version), "NvVFX_GetVersion")?;
        tracing::info!("VFX SDK {} loaded from {}", version_string(version), dir.display());
        Ok(sdk)
    }

    fn check(&self, status: Status, what: &str) -> Result<()> {
        if status == 0 {
            return Ok(());
        }
        let text = self
            .api
            .ErrorString
            .map(|f| unsafe { f(status) })
            .filter(|p| !p.is_null())
            .map(|p| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
            .unwrap_or_default();
        bail!("{what} failed: {} ({status})", if text.is_empty() { "error" } else { &text })
    }

    fn gpu_rgba(&self, width: u32, height: u32) -> Result<GpuImage> {
        let mut p = std::ptr::null_mut();
        self.check(
            unsafe { (self.api.ImageCreate)(width, height, NVCV_RGBA, NVCV_U8, NVCV_INTERLEAVED, NVCV_GPU, 32, &mut p) },
            "NvCVImage_Create",
        )?;
        Ok(GpuImage { destroy: self.api.ImageDestroy, ptr: p })
    }

    fn device_buf(&self, len: usize) -> Result<Option<DeviceBuf>> {
        let Some(c) = &self.cuda else { return Ok(None) };
        let mut p = std::ptr::null_mut();
        let e = unsafe { (c.Malloc)(&mut p, len) };
        if e != 0 {
            bail!("cudaMalloc({len}) failed ({e})");
        }
        Ok(Some(DeviceBuf { free: c.Free, ptr: p }))
    }

    fn memcpy(&self, dst: *mut c_void, src: *const c_void, len: usize, kind: i32) -> Result<()> {
        let c = self.cuda.as_ref().expect("device buffers imply CUDA");
        let e = unsafe { (c.Memcpy)(dst, src, len, kind) };
        if e != 0 {
            bail!("cudaMemcpy failed ({e})");
        }
        Ok(())
    }
}

/// "1.3.0.0" from the packed 0xMMmmppbb version.
pub fn version_string(v: u32) -> String {
    format!("{}.{}.{}.{}", v >> 24, (v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF)
}

struct GpuImage {
    destroy: unsafe extern "C" fn(*mut Image),
    ptr: *mut Image,
}

impl Drop for GpuImage {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.destroy)(self.ptr) };
        }
    }
}

/// Speed/quality model of the effect (the SDK's Mode).
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Quality {
    Low,
    #[default]
    Medium,
    High,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Low, Quality::Medium, Quality::High];
    pub fn label(self) -> &'static str {
        match self {
            Quality::Low => "Fast",
            Quality::Medium => "Balanced",
            Quality::High => "Quality",
        }
    }
    fn mode(self) -> u32 {
        match self {
            Quality::Low => 0,
            Quality::Medium => 1,
            Quality::High => 2,
        }
    }
}

/// What an [`Interpolator`] was built for; a change means building a new one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    pub width: u32,
    pub height: u32,
    pub chroma: Chroma,
    pub color: ColorInfo,
    pub quality: Quality,
    pub multiplier: u32,
}

impl Shape {
    pub fn of(f: &VideoFrame, quality: Quality, multiplier: u32) -> Self {
        Self { width: f.width, height: f.height, chroma: f.chroma, color: f.color, quality, multiplier }
    }
}

/// One loaded Video Frame Generation effect plus its GPU buffers. Not thread-safe: the frame
/// generation worker owns it. Everything runs on CUDA's default stream, so a transfer back to
/// CPU memory has finished when the call returns.
pub struct Interpolator {
    sdk: Arc<Sdk>,
    pub shape: Shape,
    effect: Handle,
    inputs: [GpuImage; 2],
    output: GpuImage,
    tmp: GpuImage,
    /// Index into `inputs` the next frame goes to.
    next: usize,
    /// `inputs[1 - next]` holds the previous frame.
    have_prev: bool,
    pool: Arc<FramePool>,
    /// GPU copy of a frame in its capture layout, for GPU-side YUV <-> RGBA conversion.
    staging: Option<DeviceBuf>,
    /// Last push: upload, generate, download (ms).
    pub timings: [f64; 3],
}

const SEL_EFFECT: &CStr = c"VideoFrameGeneration";

impl Interpolator {
    pub fn new(sdk: Arc<Sdk>, shape: Shape) -> Result<Self> {
        let (w, h) = (shape.width, shape.height);
        let inputs = [sdk.gpu_rgba(w, h)?, sdk.gpu_rgba(w, h)?];
        let output = sdk.gpu_rgba(w, h)?;
        let tmp = sdk.gpu_rgba(w, h)?;
        let staging = sdk.device_buf(crate::capture::frame_len(w, h, shape.chroma))?;
        let mut effect: Handle = std::ptr::null_mut();
        sdk.check(unsafe { (sdk.api.CreateEffect)(SEL_EFFECT.as_ptr(), &mut effect) }, "NvVFX_CreateEffect")?;
        // From here on `me` owns the effect, so an error below still destroys it.
        let me = Self {
            sdk,
            shape,
            effect,
            inputs,
            output,
            tmp,
            next: 0,
            have_prev: false,
            pool: Arc::default(),
            staging,
            timings: [0.0; 3],
        };
        let sdk = &me.sdk;
        me.set_u32(c"InputWidth", w)?;
        me.set_u32(c"InputHeight", h)?;
        sdk.check(
            unsafe { (sdk.api.SetCudaStream)(effect, c"CudaStream".as_ptr(), std::ptr::null_mut()) },
            "NvVFX_SetCudaStream",
        )?;
        me.set_u32(c"Mode", shape.quality.mode())?;
        me.set_u32(c"AutomaticShotChangeDetectionEnabled", 1)?;
        let t = std::time::Instant::now();
        sdk.check(unsafe { (sdk.api.Load)(effect) }, "NvVFX_Load")?;
        tracing::info!("VFG model loaded in {:.0} ms for {shape:?}", t.elapsed().as_secs_f64() * 1e3);
        me.set_image(c"DstImage0", me.output.ptr)?;
        me.set_u32(c"FrameMultiplier", shape.multiplier)?;
        Ok(me)
    }

    fn set_u32(&self, sel: &CStr, v: u32) -> Result<()> {
        self.sdk.check(unsafe { (self.sdk.api.SetU32)(self.effect, sel.as_ptr(), v) }, &format!("set {sel:?}"))
    }

    fn set_image(&self, sel: &CStr, img: *mut Image) -> Result<()> {
        self.sdk.check(unsafe { (self.sdk.api.SetImage)(self.effect, sel.as_ptr(), img) }, &format!("set {sel:?}"))
    }

    fn yuv_format(&self) -> u32 {
        match self.shape.chroma {
            Chroma::S420 => NVCV_YUV420,
            Chroma::S422 => NVCV_YUV422,
            Chroma::S444 => NVCV_YUV444,
        }
    }

    fn colorspace(&self) -> u32 {
        let c = self.shape.color;
        (if c.matrix == Matrix::Bt601 { NVCV_601 } else { NVCV_709 }) | if c.full_range { NVCV_FULL_RANGE } else { 0 }
    }

    /// Forget the previous frame (after a stall or a cut): the next frame starts a new pair.
    pub fn reset(&mut self) {
        self.have_prev = false;
    }

    /// Uploads `frame` and returns the `multiplier - 1` frames between the previous one and it
    /// (none for the first frame after creation or a reset).
    pub fn push(&mut self, frame: &VideoFrame) -> Result<Vec<VideoFrame>> {
        let t0 = std::time::Instant::now();
        self.timings = [0.0; 3];
        let w = frame.width;
        let (cw, _) = frame.chroma_size();
        let cur = self.next;
        let (mut y, mut uv) = (frame.y_plane().as_ptr(), frame.uv_plane().as_ptr());
        let mut space = NVCV_CPU;
        if let Some(dev) = &self.staging {
            self.sdk.memcpy(dev.ptr, frame.data.as_ptr().cast(), frame.data.len(), CUDA_HOST_TO_DEVICE)?;
            y = dev.ptr.cast();
            uv = unsafe { y.add((w * frame.height) as usize) };
            space = NVCV_GPU;
        }
        self.sdk.check(
            unsafe {
                (self.sdk.api.TransferFromYUV)(
                    y.cast(),
                    1,
                    w as i32,
                    uv.cast(),
                    uv.add(1).cast(),
                    2,
                    (cw * 2) as i32,
                    self.yuv_format(),
                    NVCV_U8,
                    self.colorspace(),
                    space,
                    self.inputs[cur].ptr,
                    std::ptr::null(),
                    1.0,
                    std::ptr::null_mut(),
                    self.tmp.ptr,
                )
            },
            "NvCVImage_TransferFromYUV",
        )?;
        self.next = 1 - cur;
        self.timings[0] = t0.elapsed().as_secs_f64() * 1e3;
        let had_prev = std::mem::replace(&mut self.have_prev, true);
        if !had_prev {
            return Ok(Vec::new());
        }
        // A new pair needs both inputs set again, even when the buffers didn't change.
        self.set_image(c"SrcImage0", self.inputs[1 - cur].ptr)?;
        self.set_image(c"SrcImage1", self.inputs[cur].ptr)?;
        self.set_u32(c"ShotChange", 0)?;
        let mut out = Vec::with_capacity(self.shape.multiplier as usize - 1);
        for k in 1..self.shape.multiplier {
            self.set_u32(c"FrameIndex", k)?;
            let t = std::time::Instant::now();
            self.sdk.check(unsafe { (self.sdk.api.Run)(self.effect, 0) }, "NvVFX_Run")?;
            let t1 = std::time::Instant::now();
            out.push(self.download(frame)?);
            self.timings[1] += (t1 - t).as_secs_f64() * 1e3;
            self.timings[2] += t1.elapsed().as_secs_f64() * 1e3;
        }
        Ok(out)
    }

    /// The output image back to CPU memory in the frame's own layout (Y + interleaved UV).
    fn download(&self, like: &VideoFrame) -> Result<VideoFrame> {
        let (w, h) = (like.width, like.height);
        let (cw, _) = like.chroma_size();
        let len = like.data.len();
        let mut data = self.pool.get(len);
        let host = data.as_mut_slice().as_mut_ptr();
        // Convert into the GPU staging buffer and copy that down, or straight to the CPU.
        let (y, space) = match &self.staging {
            Some(dev) => (dev.ptr.cast::<u8>(), NVCV_GPU),
            None => (host, NVCV_CPU),
        };
        let uv = unsafe { y.add((w * h) as usize) };
        self.sdk.check(
            unsafe {
                (self.sdk.api.TransferToYUV)(
                    self.output.ptr,
                    std::ptr::null(),
                    y.cast(),
                    1,
                    w as i32,
                    uv.cast(),
                    uv.add(1).cast(),
                    2,
                    (cw * 2) as i32,
                    self.yuv_format(),
                    NVCV_U8,
                    self.colorspace(),
                    space,
                    1.0,
                    std::ptr::null_mut(),
                    self.tmp.ptr,
                )
            },
            "NvCVImage_TransferToYUV",
        )?;
        if let Some(dev) = &self.staging {
            self.sdk.memcpy(host.cast(), dev.ptr, len, CUDA_DEVICE_TO_HOST)?;
        }
        Ok(VideoFrame { width: w, height: h, chroma: like.chroma, color: like.color, data, pts_us: like.pts_us, seq: 0 })
    }
}

impl Drop for Interpolator {
    fn drop(&mut self) {
        if !self.effect.is_null() {
            unsafe { (self.sdk.api.DestroyEffect)(self.effect) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vfg_next_to_the_exe_comes_first() {
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let dirs = candidate_dirs(Some(Path::new(r"D:\elsewhere")));
        assert_eq!(dirs[0], exe_dir.join("vfg"));
        assert_eq!(dirs[1], exe_dir.join("vfg").join("bin"));
        assert_eq!(dirs[2], PathBuf::from(r"D:\elsewhere"), "then the config override");
    }

    #[test]
    fn finds_the_feature_in_an_sdk_layout() {
        let root = std::env::temp_dir().join(format!("pt-sdk-layout-{}", std::process::id()));
        let bin = root.join("bin");
        let feature_dir = root.join("features").join("nvvfxvideoframegeneration").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&feature_dir).unwrap();
        assert_eq!(feature_dll(&bin), None);
        std::fs::write(feature_dir.join(FEATURE_DLL), b"x").unwrap();
        assert_eq!(feature_dll(&bin), Some(feature_dir.join(FEATURE_DLL)));
        std::fs::write(bin.join(FEATURE_DLL), b"x").unwrap();
        assert_eq!(feature_dll(&bin), Some(bin.join(FEATURE_DLL)), "a copy next to the core wins");
        let _ = std::fs::remove_dir_all(&root);
    }
}
