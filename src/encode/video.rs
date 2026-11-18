//! NVENC video encoder thread (FFmpeg `hevc_nvenc` / `h264_nvenc` / `av1_nvenc`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Receiver;
use ffmpeg_next as ff;

use crate::capture::video_mf::VideoMode;
use crate::capture::VideoFrame;
use crate::config::{Chroma, Codec};
use crate::packets::{EncodedPacket, SendParams, StreamKind, StreamParams};

/// Microsecond time base: packets come out on the session clock directly.
const TB_US: ff::Rational = ff::Rational(1, 1_000_000);

pub struct VideoEncoderConfig {
    pub codec: Codec,
    pub mode: VideoMode,
    pub chroma: Chroma,
    pub bitrate: u64,
}

/// Encoder input format per chroma: NV12 (4:2:0), NV16 (4:2:2), planar YUV444 (4:4:4).
fn pixel_format(chroma: Chroma) -> ff::format::Pixel {
    match chroma {
        Chroma::S420 => ff::format::Pixel::NV12,
        Chroma::S422 => ff::format::Pixel::NV16,
        Chroma::S444 => ff::format::Pixel::YUV444P,
    }
}

pub fn open(cfg: &VideoEncoderConfig) -> Result<ff::encoder::Video> {
    if !cfg.codec.supports(cfg.chroma) {
        anyhow::bail!("{} can't encode {} on NVENC; pick HEVC or H.264", cfg.codec.short_label(), cfg.chroma.label());
    }
    let name = cfg.codec.nvenc_name();
    let codec = ff::encoder::find_by_name(name).ok_or_else(|| anyhow!("FFmpeg has no '{name}' encoder"))?;
    let mut enc = ff::codec::context::Context::new_with_codec(codec).encoder().video()?;
    let m = &cfg.mode;
    enc.set_width(m.width);
    enc.set_height(m.height);
    enc.set_format(pixel_format(cfg.chroma));
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
        (*ctx).color_range = ff::ffi::AVColorRange::AVCOL_RANGE_MPEG;
        (*ctx).colorspace = ff::ffi::AVColorSpace::AVCOL_SPC_BT709;
        (*ctx).color_primaries = ff::ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*ctx).color_trc = ff::ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        (*ctx).rc_buffer_size = cfg.bitrate as i32 * 2;
    }

    let mut opts = ff::Dictionary::new();
    opts.set("preset", "p5");
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
    let mut enc = open(&cfg)?;
    *params.video.lock() = Some(SendParams(super::params_of(&enc)));
    tracing::info!(
        "video encoder {} {} {} @ {:.1} Mbps",
        cfg.codec.nvenc_name(),
        cfg.mode,
        cfg.chroma.label(),
        cfg.bitrate as f64 / 1e6
    );

    let (w, h) = (cfg.mode.width, cfg.mode.height);
    let mut frame = ff::frame::Video::new(pixel_format(cfg.chroma), w, h);
    let mut last_pts = i64::MIN;
    let mut pkt = ff::Packet::empty();

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

        let pts = vf.pts_us.max(last_pts + 1);
        last_pts = pts;
        frame.set_pts(Some(pts));
        let idr = force_idr.swap(false, Ordering::AcqRel);
        frame.set_kind(if idr { ff::picture::Type::I } else { ff::picture::Type::None });

        enc.send_frame(&frame).context("send_frame")?;
        drain(&mut enc, &mut pkt, &mut sink);
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
