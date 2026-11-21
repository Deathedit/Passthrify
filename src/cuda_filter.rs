//! A chain of FFmpeg CUDA filters (scale_cuda, colorspace_cuda, …) over CUDA frames: frames
//! stay on the GPU, and the output comes from the chain's own frame pool (`out_frames`, for
//! the encoder's `hw_frames_ctx`).

use std::ffi::CString;

use anyhow::{anyhow, bail, Result};
use ffmpeg_next as ff;
use ff::ffi;

use crate::color::{ColorInfo, Matrix};

const TB_US: ffi::AVRational = ffi::AVRational { num: 1, den: 1_000_000 };

pub struct CudaFilter {
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    /// Frames context of the output (owned by the sink, freed with the graph).
    pub out_frames: *mut ffi::AVBufferRef,
    color: ColorInfo,
}

unsafe impl Send for CudaFilter {}

fn av_err(e: i32) -> String {
    ff::Error::from(e).to_string()
}

fn range_of(c: ColorInfo) -> ffi::AVColorRange {
    if c.full_range {
        ffi::AVColorRange::AVCOL_RANGE_JPEG
    } else {
        ffi::AVColorRange::AVCOL_RANGE_MPEG
    }
}

fn space_of(c: ColorInfo) -> ffi::AVColorSpace {
    match c.matrix {
        Matrix::Bt709 => ffi::AVColorSpace::AVCOL_SPC_BT709,
        Matrix::Bt601 => ffi::AVColorSpace::AVCOL_SPC_SMPTE170M,
    }
}

/// The filter that turns full-range frames into limited range (16-235), which every player
/// shows correctly; many ignore the full-range flag and crush the shadows.
pub fn to_limited_range() -> (&'static str, String) {
    ("colorspace_cuda", "range=tv".into())
}

impl CudaFilter {
    /// `frames`: the input frames context; `color`: the input's range and matrix;
    /// `chain`: (filter name, options) in order.
    pub fn new(frames: *mut ffi::AVBufferRef, width: u32, height: u32, color: ColorInfo, chain: &[(&str, String)]) -> Result<Self> {
        if chain.is_empty() {
            bail!("empty filter chain");
        }
        unsafe {
            let graph = ffi::avfilter_graph_alloc();
            if graph.is_null() {
                bail!("avfilter_graph_alloc failed");
            }
            let mut s = Self { graph, src: std::ptr::null_mut(), sink: std::ptr::null_mut(), out_frames: std::ptr::null_mut(), color };
            let filter = |name: &str| {
                let c = CString::new(name).unwrap();
                let f = ffi::avfilter_get_by_name(c.as_ptr());
                if f.is_null() {
                    Err(anyhow!("FFmpeg has no {name} filter"))
                } else {
                    Ok(f)
                }
            };
            s.src = ffi::avfilter_graph_alloc_filter(graph, filter("buffer")?, c"in".as_ptr());
            if s.src.is_null() {
                bail!("creating the buffer source failed");
            }
            let par = ffi::av_buffersrc_parameters_alloc();
            (*par).format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA as i32;
            (*par).width = width as i32;
            (*par).height = height as i32;
            (*par).time_base = TB_US;
            (*par).hw_frames_ctx = frames;
            (*par).color_range = range_of(color);
            (*par).color_space = space_of(color);
            let e = ffi::av_buffersrc_parameters_set(s.src, par);
            ffi::av_free(par.cast());
            if e < 0 {
                bail!("buffer source parameters: {}", av_err(e));
            }
            let e = ffi::avfilter_init_str(s.src, std::ptr::null());
            if e < 0 {
                bail!("buffer source init: {}", av_err(e));
            }
            let mut prev = s.src;
            for (i, (name, args)) in chain.iter().enumerate() {
                let mut f = std::ptr::null_mut();
                let label = CString::new(format!("f{i}")).unwrap();
                let args = CString::new(args.as_str()).unwrap();
                let e = ffi::avfilter_graph_create_filter(&mut f, filter(name)?, label.as_ptr(), args.as_ptr(), std::ptr::null_mut(), graph);
                if e < 0 {
                    bail!("{name}: {}", av_err(e));
                }
                let e = ffi::avfilter_link(prev, 0, f, 0);
                if e < 0 {
                    bail!("linking {name}: {}", av_err(e));
                }
                prev = f;
            }
            let e = ffi::avfilter_graph_create_filter(&mut s.sink, filter("buffersink")?, c"out".as_ptr(), std::ptr::null(), std::ptr::null_mut(), graph);
            if e < 0 {
                bail!("buffer sink: {}", av_err(e));
            }
            let e = ffi::avfilter_link(prev, 0, s.sink, 0);
            if e < 0 {
                bail!("linking the sink: {}", av_err(e));
            }
            let e = ffi::avfilter_graph_config(graph, std::ptr::null_mut());
            if e < 0 {
                bail!("configuring the CUDA filters: {}", av_err(e));
            }
            s.out_frames = ffi::av_buffersink_get_hw_frames_ctx(s.sink);
            if s.out_frames.is_null() {
                bail!("the CUDA filters produced no CUDA frames");
            }
            Ok(s)
        }
    }

    /// Filters one CUDA frame; None if the chain wants more input first.
    pub fn run(&mut self, frame: *const ffi::AVFrame, pts: i64) -> Result<Option<ff::frame::Video>> {
        unsafe {
            let mut input = ffi::av_frame_clone(frame);
            if input.is_null() {
                bail!("av_frame_clone failed");
            }
            (*input).pts = pts;
            // Our CUDA frames carry no color tags; the filters need them (colorspace_cuda reads
            // the input range from the frame).
            (*input).color_range = range_of(self.color);
            (*input).colorspace = space_of(self.color);
            let e = ffi::av_buffersrc_add_frame_flags(self.src, input, 0);
            ffi::av_frame_free(&mut input);
            if e < 0 {
                bail!("CUDA filter input: {}", av_err(e));
            }
            let mut out = ff::frame::Video::empty();
            let e = ffi::av_buffersink_get_frame(self.sink, out.as_mut_ptr());
            if e == ffi::AVERROR(ffi::EAGAIN) {
                return Ok(None);
            }
            if e < 0 {
                bail!("CUDA filter output: {}", av_err(e));
            }
            Ok(Some(out))
        }
    }
}

impl Drop for CudaFilter {
    fn drop(&mut self) {
        unsafe { ffi::avfilter_graph_free(&mut self.graph) };
    }
}
