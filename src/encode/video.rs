//! NVENC video encoder thread (FFmpeg `hevc_nvenc` / `h264_nvenc` / `av1_nvenc`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Receiver;
use ffmpeg_next as ff;

use crate::capture::video_mf::VideoMode;
use crate::capture::VideoFrame;
use crate::color::{ColorInfo, Matrix};
use crate::config::{Chroma, Codec};
use crate::gpu::CudaFrames;
use crate::packets::{EncodedPacket, SendParams, StreamKind, StreamParams};

/// Microsecond time base: packets come out on the session clock directly.
const TB_US: ff::Rational = ff::Rational(1, 1_000_000);

pub struct VideoEncoderConfig {
    pub codec: Codec,
    pub mode: VideoMode,
    pub chroma: Chroma,
    /// Range + matrix of the incoming frames; written into the stream's VUI so players decode
    /// it correctly (full-range NV12 tagged as limited looks crushed/dark in strict players).
    pub color: ColorInfo,
    pub bitrate: u64,
    /// Encode from these CUDA frames (frames arrive uploaded by the capture thread, or are
    /// uploaded here). None = system-memory frames.
    pub gpu: Option<Arc<CudaFrames>>,
}

/// Encoder input format per chroma: NV12 (4:2:0), NV16 (4:2:2), planar YUV444 (4:4:4).
fn pixel_format(chroma: Chroma) -> ff::format::Pixel {
    match chroma {
        Chroma::S420 => ff::format::Pixel::NV12,
        Chroma::S422 => ff::format::Pixel::NV16,
        Chroma::S444 => ff::format::Pixel::YUV444P,
    }
}

/// NVENC preset for a mode. p5 is the quality sweet spot up to 4K60, but NVENC can't sustain it
/// at higher pixel rates (4K144 dropped ~20 fps at p5), so step down for high-refresh modes.
pub fn preset_for(m: &VideoMode) -> &'static str {
    let pixel_rate = m.width as f64 * m.height as f64 * m.fps();
    let uhd60 = 3840.0 * 2160.0 * 61.0;
    if pixel_rate > uhd60 * 1.7 {
        "p2"
    } else if pixel_rate > uhd60 {
        "p3"
    } else {
        "p5"
    }
}

pub fn open(cfg: &VideoEncoderConfig, gpu: Option<&CudaFrames>) -> Result<ff::encoder::Video> {
    if !cfg.codec.supports(cfg.chroma) {
        anyhow::bail!("{} can't encode {} on NVENC; pick HEVC or H.264", cfg.codec.short_label(), cfg.chroma.label());
    }
    let name = cfg.codec.nvenc_name();
    let codec = ff::encoder::find_by_name(name).ok_or_else(|| anyhow!("FFmpeg has no '{name}' encoder"))?;
    let mut enc = ff::codec::context::Context::new_with_codec(codec).encoder().video()?;
    let m = &cfg.mode;
    enc.set_width(m.width);
    enc.set_height(m.height);
    match gpu {
        // CUDA frames: NVENC reads them in place (registered device memory), no upload.
        Some(g) => unsafe {
            enc.set_format(ff::format::Pixel::CUDA);
            (*enc.as_mut_ptr()).hw_frames_ctx = ff::ffi::av_buffer_ref(g.frames_ctx());
        },
        None => enc.set_format(pixel_format(cfg.chroma)),
    }
    enc.set_time_base(TB_US);
    enc.set_frame_rate(Some(ff::Rational(m.fps_num as i32, m.fps_den as i32)));
    enc.set_bit_rate(cfg.bitrate as usize);
    enc.set_max_bit_rate((cfg.bitrate * 3 / 2) as usize);
    // 1 s GOP: replay clips start at most 1 s before the requested window.
    enc.set_gop(m.fps().round().max(1.0) as u32);
    enc.set_max_b_frames(if cfg.codec == Codec::Av1 { 0 } else { 2 });
    enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
    unsafe {
        let ctx = enc.as_mut_ptr();
        (*ctx).color_range = if cfg.color.full_range {
            ff::ffi::AVColorRange::AVCOL_RANGE_JPEG
        } else {
            ff::ffi::AVColorRange::AVCOL_RANGE_MPEG
        };
        (*ctx).colorspace = match cfg.color.matrix {
            Matrix::Bt709 => ff::ffi::AVColorSpace::AVCOL_SPC_BT709,
            Matrix::Bt601 => ff::ffi::AVColorSpace::AVCOL_SPC_SMPTE170M,
        };
        // Primaries / transfer stay BT.709: HDMI SDR content, only the YCbCr matrix differs.
        (*ctx).color_primaries = ff::ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*ctx).color_trc = ff::ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        (*ctx).rc_buffer_size = cfg.bitrate as i32 * 2;
    }

    let mut opts = ff::Dictionary::new();
    opts.set("preset", preset_for(m));
    opts.set("tune", "hq");
    opts.set("rc", "vbr");
    opts.set("forced-idr", "1");
    opts.set("spatial-aq", "1");
    if cfg.codec != Codec::Av1 {
        opts.set("b_ref_mode", "middle");
    }
    enc.open_with(opts).with_context(|| {
        format!(
            "opening {name} {} at {} (4:2:2 needs an RTX 50-series GPU; is the NVIDIA driver up to date?)",
            cfg.chroma.label(),
            cfg.mode
        )
    })
}

/// Encodes frames from `rx` until the channel closes. Publishes codec parameters once opened.
pub fn run(
    cfg: VideoEncoderConfig,
    rx: Receiver<Arc<VideoFrame>>,
    force_idr: Arc<AtomicBool>,
    params: Arc<StreamParams>,
    mut sink: impl FnMut(EncodedPacket),
) -> Result<()> {
    // The CUDA path falls back to system-memory frames if NVENC won't open with it.
    let (mut enc, gpu) = match cfg.gpu.as_deref() {
        Some(g) => match open(&cfg, Some(g)) {
            Ok(enc) => (enc, cfg.gpu.clone()),
            Err(e) => {
                tracing::warn!("NVENC with CUDA frames failed, using system memory: {e:#}");
                (open(&cfg, None)?, None)
            }
        },
        None => (open(&cfg, None)?, None),
    };
    *params.video.lock() = Some(SendParams(super::params_of(&enc)));
    tracing::info!(
        "video encoder {} {} {} {} @ {:.1} Mbps (preset {}, {} frames)",
        cfg.codec.nvenc_name(),
        cfg.mode,
        cfg.chroma.label(),
        cfg.color.label(),
        cfg.bitrate as f64 / 1e6,
        preset_for(&cfg.mode),
        if gpu.is_some() { "CUDA" } else { "system memory" }
    );

    let (w, h) = (cfg.mode.width, cfg.mode.height);
    let mut frame = ff::frame::Video::new(pixel_format(cfg.chroma), w, h);
    let mut last_pts = i64::MIN;
    let mut pkt = ff::Packet::empty();
    let mut scratch = Vec::new();

    for vf in rx.iter() {
        if vf.width != w || vf.height != h || vf.chroma != cfg.chroma {
            tracing::warn!(
                "dropping {}x{} {} frame (encoder is {w}x{h} {})",
                vf.width,
                vf.height,
                vf.chroma.label(),
                cfg.chroma.label()
            );
            continue;
        }
        let idr = force_idr.swap(false, Ordering::AcqRel);
        let kind = if idr { ff::picture::Type::I } else { ff::picture::Type::None };
        let pts = vf.pts_us.max(last_pts + 1);
        last_pts = pts;

        if let Some(cuda) = gpu.as_deref() {
            // A new reference to the CUDA frame (uploaded by the capture thread, or here if
            // that upload didn't happen); NVENC keeps it until it has read the frame.
            let prepared = crate::perf::ENCODE_PREP.time(|| -> Result<ff::frame::Video> {
                let uploaded;
                let src = match vf.gpu.get() {
                    Some(g) => g,
                    None => {
                        uploaded = cuda.upload(&vf, &mut scratch)?;
                        &uploaded
                    }
                };
                let f = unsafe { ff::ffi::av_frame_clone(src.as_ptr()) };
                if f.is_null() {
                    anyhow::bail!("av_frame_clone failed");
                }
                Ok(unsafe { ff::frame::Video::wrap(f) })
            });
            let mut f = prepared?;
            f.set_pts(Some(pts));
            f.set_kind(kind);
            crate::perf::ENCODE_SEND.time(|| -> Result<()> {
                enc.send_frame(&f).context("send_frame")?;
                drain(&mut enc, &mut pkt, &mut sink);
                Ok(())
            })?;
            continue;
        }

        let copy_started = std::time::Instant::now();
        unsafe {
            ff::ffi::av_frame_make_writable(frame.as_mut_ptr());
        }
        let (cw, ch) = vf.chroma_size();
        copy_plane(vf.y_plane(), w as usize, h as usize, frame.stride(0), frame.data_mut(0));
        match cfg.chroma {
            // NV12 / NV16: interleaved UV plane, `cw * 2` bytes per row.
            Chroma::S420 | Chroma::S422 => {
                copy_plane(vf.uv_plane(), cw as usize * 2, ch as usize, frame.stride(1), frame.data_mut(1))
            }
            // NV24 -> planar U and V.
            Chroma::S444 => {
                let (us, vs) = (frame.stride(1), frame.stride(2));
                let uv = vf.uv_plane();
                let row = cw as usize * 2;
                for r in 0..ch as usize {
                    let src = &uv[r * row..(r + 1) * row];
                    let u = &mut frame.data_mut(1)[r * us..r * us + cw as usize];
                    for (d, s) in u.iter_mut().zip(src.chunks_exact(2)) {
                        *d = s[0];
                    }
                    let v = &mut frame.data_mut(2)[r * vs..r * vs + cw as usize];
                    for (d, s) in v.iter_mut().zip(src.chunks_exact(2)) {
                        *d = s[1];
                    }
                }
            }
        }

        crate::perf::ENCODE_PREP.add(copy_started.elapsed());
        frame.set_pts(Some(pts));
        frame.set_kind(kind);

        crate::perf::ENCODE_SEND.time(|| -> Result<()> {
            enc.send_frame(&frame).context("send_frame")?;
            drain(&mut enc, &mut pkt, &mut sink);
            Ok(())
        })?;
    }

    let _ = enc.send_eof();
    drain(&mut enc, &mut pkt, &mut sink);
    Ok(())
}

fn copy_plane(src: &[u8], row_bytes: usize, rows: usize, dst_stride: usize, dst: &mut [u8]) {
    for r in 0..rows {
        dst[r * dst_stride..r * dst_stride + row_bytes].copy_from_slice(&src[r * row_bytes..(r + 1) * row_bytes]);
    }
}

fn drain(enc: &mut ff::encoder::Video, pkt: &mut ff::Packet, sink: &mut impl FnMut(EncodedPacket)) {
    while enc.receive_packet(pkt).is_ok() {
        let Some(data) = pkt.data() else { continue };
        let pts = pkt.pts().unwrap_or(0);
        sink(EncodedPacket {
            kind: StreamKind::Video,
            data: data.to_vec(),
            pts_us: pts,
            dts_us: pkt.dts().unwrap_or(pts),
            duration_us: pkt.duration(),
            key: pkt.is_key(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::video_mf::PixFmt;

    fn mode(w: u32, h: u32, fps: u32) -> VideoMode {
        VideoMode { index: 0, width: w, height: h, fps_num: fps, fps_den: 1, format: PixFmt::Nv12 }
    }

    #[test]
    fn faster_presets_for_high_pixel_rates() {
        assert_eq!(preset_for(&mode(3840, 2160, 60)), "p5");
        assert_eq!(preset_for(&mode(2560, 1440, 144)), "p3"); // 530 Mpx/s, just above 4K60
        assert_eq!(preset_for(&mode(3840, 2160, 120)), "p2");
        assert_eq!(preset_for(&mode(3840, 2160, 144)), "p2");
        assert_eq!(preset_for(&mode(1920, 1080, 240)), "p5");
        assert_eq!(preset_for(&mode(3440, 1440, 144)), "p3");
    }
}
