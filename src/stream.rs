//! Live streaming to Twitch, YouTube or any RTMP(S) server.
//!
//! The stream has its own H.264 NVENC encode (Twitch takes H.264 up to 1080p60 at ~6 Mbps,
//! far below the replay stream). Captured CUDA frames are scaled on the GPU (`scale_cuda`);
//! frames without a CUDA copy are scaled on the CPU (swscale). Audio reuses the session's AAC
//! packets. Packets go out as FLV over RTMP; a dropped connection reconnects with backoff.

use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{bounded, select, Receiver, Sender};
use ffmpeg_next as ff;
use ff::ffi;
use parking_lot::Mutex;

use crate::capture::{Chroma, VideoFrame};
use crate::cuda_filter::CudaFilter;
use crate::color::Matrix;
use crate::packets::{EncodedPacket, SendParams, StreamKind};

const TB_US: ffi::AVRational = ffi::AVRational { num: 1, den: 1_000_000 };

/// What to stream where.
#[derive(Clone)]
pub struct StreamTarget {
    /// Publish URL including the stream key.
    pub url: String,
    /// Output height (keeps the source's aspect ratio); 0 or above the source = source size.
    pub height: u32,
    pub max_fps: u32,
    pub bitrate_kbps: u32,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum StreamState {
    #[default]
    Off,
    Connecting,
    Live {
        since: Instant,
    },
    Reconnecting {
        error: String,
    },
}

/// Shared with the UI.
#[derive(Default)]
pub struct StreamStatus {
    pub state: Mutex<StreamState>,
    /// Bits per second actually sent, averaged over the last second.
    pub kbps: AtomicU32,
    /// Captured frames the stream couldn't take (its queue was full).
    pub dropped: AtomicU64,
}

/// A running stream: feed it frames and audio packets; stop it with [`Streamer::stop`].
pub struct Streamer {
    frames: Sender<Arc<VideoFrame>>,
    audio: Sender<Arc<EncodedPacket>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Streamer {
    /// `notify(text, error)` reports going live, reconnects and failures.
    pub fn start(target: StreamTarget, status: Arc<StreamStatus>, notify: impl Fn(String, bool) + Send + 'static) -> Self {
        let (frames, frames_rx) = bounded::<Arc<VideoFrame>>(4);
        let (audio, audio_rx) = bounded::<Arc<EncodedPacket>>(256);
        let stop = Arc::new(AtomicBool::new(false));
        *status.state.lock() = StreamState::Connecting;
        let join = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("stream".into())
                .spawn(move || {
                    if let Err(e) = run(&target, frames_rx, audio_rx, &stop, &status, &notify) {
                        notify(format!("Stream stopped: {e:#}"), true);
                    }
                    *status.state.lock() = StreamState::Off;
                    status.kbps.store(0, Ordering::Relaxed);
                })
                .expect("spawn stream thread")
        };
        Self { frames, audio, stop, join: Some(join) }
    }

    pub fn frame_sender(&self) -> Sender<Arc<VideoFrame>> {
        self.frames.clone()
    }

    pub fn audio_sender(&self) -> Sender<Arc<EncodedPacket>> {
        self.audio.clone()
    }

    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(|j| j.is_finished())
    }

    /// Ends the stream (aborts a blocked network write) and waits for the thread.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Output size for a source: `height` with the source's aspect ratio (even sizes), never
/// larger than the source.
pub fn output_size(src_w: u32, src_h: u32, height: u32) -> (u32, u32) {
    let even = |v: u32| (v / 2 * 2).max(2);
    if height == 0 || height >= src_h {
        return (even(src_w), even(src_h));
    }
    let w = (src_w as f64 * height as f64 / src_h as f64).round() as u32;
    (even(w), even(height))
}

const MIN_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

fn run(
    target: &StreamTarget,
    frames: Receiver<Arc<VideoFrame>>,
    audio: Receiver<Arc<EncodedPacket>>,
    stop: &Arc<AtomicBool>,
    status: &StreamStatus,
    notify: &dyn Fn(String, bool),
) -> Result<()> {
    let audio_params = crate::encode::audio::params().ok();
    let interval_us = 1_000_000 / target.max_fps.max(1) as i64;
    let mut video: Option<VideoPath> = None;
    let mut gpu_failed = false;
    let mut out: Option<Output> = None;
    let mut retry_at: Option<Instant> = None;
    let mut backoff = MIN_BACKOFF;
    let mut last_kept = i64::MIN;
    let (mut bytes, mut bytes_since) = (0u64, Instant::now());
    let mut was_live = false;

    let set_state = |s: StreamState| *status.state.lock() = s;
    // A failed connection or write: drop the connection and retry later.
    let fail = |out: &mut Option<Output>, retry_at: &mut Option<Instant>, backoff: &mut Duration, was_live: &mut bool, e: anyhow::Error| {
        out.take();
        let msg = format!("{e:#}");
        tracing::warn!("stream: {msg}; retrying in {} s", backoff.as_secs());
        if *was_live {
            notify(format!("Stream connection lost, reconnecting: {msg}"), true);
        } else if *backoff == MIN_BACKOFF {
            notify(format!("Can't reach the stream server, retrying: {msg}"), true);
        }
        *was_live = false;
        set_state(StreamState::Reconnecting { error: msg });
        *retry_at = Some(Instant::now() + *backoff);
        *backoff = (*backoff * 2).min(MAX_BACKOFF);
    };

    while !stop.load(Ordering::Relaxed) {
        if bytes_since.elapsed() >= Duration::from_secs(1) {
            let secs = bytes_since.elapsed().as_secs_f64();
            status.kbps.store((bytes as f64 * 8.0 / 1000.0 / secs) as u32, Ordering::Relaxed);
            (bytes, bytes_since) = (0, Instant::now());
        }
        select! {
            recv(frames) -> f => {
                let Ok(f) = f else { break };
                // Down to the stream's frame rate (a 144 Hz signal streams at 60).
                if last_kept != i64::MIN && f.pts_us - last_kept < interval_us * 9 / 10 {
                    continue;
                }
                last_kept = f.pts_us;

                let key = VideoKey::of(&f, !gpu_failed);
                if video.as_ref().map(|v| v.key) != Some(key) {
                    let built = match VideoPath::new(&f, key, target) {
                        Err(e) if key.gpu => {
                            tracing::warn!("stream: GPU scaling unavailable, scaling on the CPU: {e:#}");
                            gpu_failed = true;
                            VideoPath::new(&f, VideoKey::of(&f, false), target)
                        }
                        other => other,
                    };
                    let v = built.context("setting up the stream encoder")?;
                    tracing::info!("stream: {}x{} -> {}x{} H.264 {} kbps ({} scaling)", f.width, f.height, v.width, v.height, target.bitrate_kbps, if v.key.gpu { "GPU" } else { "CPU" });
                    // New encoder parameters need a new connection (new stream header).
                    if out.is_some() {
                        out = None;
                        retry_at = None;
                    }
                    video = Some(v);
                }
                let v = video.as_mut().unwrap();

                if out.is_none() && retry_at.is_none_or(|t| Instant::now() >= t) {
                    if !matches!(*status.state.lock(), StreamState::Reconnecting { .. }) {
                        set_state(StreamState::Connecting);
                    }
                    match Output::open(&target.url, &v.params, audio_params.as_ref(), stop) {
                        Ok(o) => {
                            out = Some(o);
                            retry_at = None;
                            v.force_idr = true; // the server needs a keyframe to start
                        }
                        Err(e) => fail(&mut out, &mut retry_at, &mut backoff, &mut was_live, e),
                    }
                }
                let Some(o) = out.as_mut() else { continue };

                let packets = v.encode(&f).context("encoding the stream")?;
                for p in packets {
                    match o.write(&p) {
                        Ok(n) => {
                            bytes += n as u64;
                            if n > 0 && !was_live {
                                was_live = true;
                                backoff = MIN_BACKOFF;
                                set_state(StreamState::Live { since: Instant::now() });
                                status.dropped.store(0, Ordering::Relaxed);
                                notify("Stream is live".into(), false);
                            }
                        }
                        Err(e) => {
                            fail(&mut out, &mut retry_at, &mut backoff, &mut was_live, e);
                            break;
                        }
                    }
                }
            },
            recv(audio) -> p => {
                let Ok(p) = p else { break };
                if let Some(o) = out.as_mut() {
                    match o.write(&p) {
                        Ok(n) => bytes += n as u64,
                        Err(e) => fail(&mut out, &mut retry_at, &mut backoff, &mut was_live, e),
                    }
                }
            },
            default(Duration::from_millis(100)) => {},
        }
    }
    if let Some(o) = out.take() {
        o.finish();
    }
    if was_live {
        notify("Stream ended".into(), false);
    }
    Ok(())
}

/// What the scaler and encoder were built for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct VideoKey {
    width: u32,
    height: u32,
    chroma: Chroma,
    /// Scale CUDA frames on the GPU.
    gpu: bool,
}

impl VideoKey {
    fn of(f: &VideoFrame, gpu_ok: bool) -> Self {
        Self { width: f.width, height: f.height, chroma: f.chroma, gpu: gpu_ok && f.gpu.get().is_some() }
    }
}

/// Scaler + H.264 encoder for one input shape.
struct VideoPath {
    key: VideoKey,
    width: u32,
    height: u32,
    scaler: Scaler,
    enc: ff::encoder::Video,
    params: SendParams,
    force_idr: bool,
    last_pts: i64,
}

enum Scaler {
    Gpu(CudaFilter),
    Cpu(CpuScaler),
}

impl VideoPath {
    fn new(f: &VideoFrame, key: VideoKey, t: &StreamTarget) -> Result<Self> {
        let (width, height) = output_size(f.width, f.height, t.height);
        let scaler = if key.gpu {
            let g = f.gpu.get().ok_or_else(|| anyhow!("no CUDA frame"))?;
            // Scale on the GPU, and bring full range down to limited (see `to_limited_range`).
            let mut chain = vec![("scale_cuda", format!("w={width}:h={height}:format=nv12"))];
            if f.color.full_range {
                chain.push(crate::cuda_filter::to_limited_range());
            }
            let frames = unsafe { (*g.as_ptr()).hw_frames_ctx };
            Scaler::Gpu(CudaFilter::new(frames, f.width, f.height, f.color, &chain)?)
        } else {
            Scaler::Cpu(CpuScaler::new(f.width, f.height, f.chroma, f.color, width, height)?)
        };
        let enc = open_encoder(f, width, height, t, &scaler)?;
        let params = SendParams(crate::encode::params_of(&enc));
        Ok(Self { key, width, height, scaler, enc, params, force_idr: true, last_pts: i64::MIN })
    }

    fn encode(&mut self, f: &VideoFrame) -> Result<Vec<EncodedPacket>> {
        let pts = f.pts_us.max(self.last_pts + 1);
        self.last_pts = pts;
        let mut frame = match &mut self.scaler {
            Scaler::Gpu(s) => {
                let Some(g) = f.gpu.get() else { return Ok(Vec::new()) };
                match s.run(g.as_ptr(), pts)? {
                    Some(fr) => fr,
                    None => return Ok(Vec::new()),
                }
            }
            Scaler::Cpu(s) => s.scale(f)?,
        };
        frame.set_pts(Some(pts));
        let idr = std::mem::take(&mut self.force_idr);
        frame.set_kind(if idr { ff::picture::Type::I } else { ff::picture::Type::None });
        self.enc.send_frame(&frame).context("send_frame")?;
        let mut out = Vec::new();
        let mut pkt = ff::Packet::empty();
        while self.enc.receive_packet(&mut pkt).is_ok() {
            let Some(data) = pkt.data() else { continue };
            let pts = pkt.pts().unwrap_or(0);
            out.push(EncodedPacket {
                kind: StreamKind::Video,
                data: data.to_vec(),
                pts_us: pts,
                dts_us: pkt.dts().unwrap_or(pts),
                duration_us: pkt.duration(),
                key: pkt.is_key(),
            });
        }
        Ok(out)
    }
}

fn open_encoder(f: &VideoFrame, width: u32, height: u32, t: &StreamTarget, scaler: &Scaler) -> Result<ff::encoder::Video> {
    let codec = ff::encoder::find_by_name("h264_nvenc").ok_or_else(|| anyhow!("FFmpeg has no h264_nvenc encoder"))?;
    let mut enc = ff::codec::context::Context::new_with_codec(codec).encoder().video()?;
    let bitrate = t.bitrate_kbps as usize * 1000;
    let fps = t.max_fps.max(1);
    enc.set_width(width);
    enc.set_height(height);
    enc.set_time_base(ff::Rational(1, 1_000_000));
    enc.set_frame_rate(Some(ff::Rational(fps as i32, 1)));
    enc.set_bit_rate(bitrate);
    enc.set_max_bit_rate(bitrate);
    // 2 s keyframe interval: what Twitch and YouTube ask for.
    enc.set_gop(fps * 2);
    enc.set_max_b_frames(2);
    enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
    unsafe {
        let ctx = enc.as_mut_ptr();
        (*ctx).rc_buffer_size = bitrate as i32;
        // Both scalers output limited range.
        (*ctx).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        (*ctx).colorspace = match f.color.matrix {
            Matrix::Bt709 => ffi::AVColorSpace::AVCOL_SPC_BT709,
            Matrix::Bt601 => ffi::AVColorSpace::AVCOL_SPC_SMPTE170M,
        };
        (*ctx).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*ctx).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        match scaler {
            Scaler::Gpu(s) => {
                (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
                (*ctx).hw_frames_ctx = ffi::av_buffer_ref(s.out_frames);
            }
            Scaler::Cpu(_) => (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_NV12,
        }
    }
    let mut opts = ff::Dictionary::new();
    opts.set("preset", "p5");
    opts.set("tune", "hq");
    opts.set("rc", "cbr");
    opts.set("profile", "high");
    opts.set("forced-idr", "1");
    opts.set("spatial-aq", "1");
    enc.open_with(opts).context("opening h264_nvenc for the stream")
}

fn av_err(e: i32) -> String {
    ff::Error::from(e).to_string()
}

/// swscale from our packed layouts (NV12 / NV16 / NV24) to limited-range NV12 at the output size.
struct CpuScaler {
    sws: *mut ffi::SwsContext,
    frame: ff::frame::Video,
    src_w: u32,
    src_h: u32,
    chroma: Chroma,
}

unsafe impl Send for CpuScaler {}

impl CpuScaler {
    fn new(src_w: u32, src_h: u32, chroma: Chroma, color: crate::color::ColorInfo, w: u32, h: u32) -> Result<Self> {
        let src_fmt = match chroma {
            Chroma::S420 => ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            Chroma::S422 => ffi::AVPixelFormat::AV_PIX_FMT_NV16,
            Chroma::S444 => ffi::AVPixelFormat::AV_PIX_FMT_NV24,
        };
        let sws = unsafe {
            ffi::sws_getContext(
                src_w as i32,
                src_h as i32,
                src_fmt,
                w as i32,
                h as i32,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                ffi::SwsFlags::SWS_BILINEAR as i32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if sws.is_null() {
            bail!("sws_getContext {src_w}x{src_h} -> {w}x{h} failed");
        }
        unsafe {
            // Same matrix, full range in -> limited range out.
            let cs = if color.matrix == Matrix::Bt709 { ffi::SWS_CS_ITU709 } else { ffi::SWS_CS_ITU601 } as i32;
            let table = ffi::sws_getCoefficients(cs);
            ffi::sws_setColorspaceDetails(sws, table, color.full_range as i32, table, 0, 0, 1 << 16, 1 << 16);
        }
        Ok(Self { sws, frame: ff::frame::Video::new(ff::format::Pixel::NV12, w, h), src_w, src_h, chroma })
    }

    fn scale(&mut self, f: &VideoFrame) -> Result<ff::frame::Video> {
        if (f.width, f.height, f.chroma) != (self.src_w, self.src_h, self.chroma) {
            bail!("frame shape changed");
        }
        let (cw, _) = f.chroma_size();
        unsafe {
            let dst = self.frame.as_mut_ptr();
            ffi::av_frame_make_writable(dst);
            let src = [f.y_plane().as_ptr(), f.uv_plane().as_ptr(), std::ptr::null(), std::ptr::null()];
            let stride = [f.width as i32, (cw * 2) as i32, 0, 0];
            ffi::sws_scale(self.sws, src.as_ptr(), stride.as_ptr(), 0, f.height as i32, (*dst).data.as_ptr(), (*dst).linesize.as_ptr());
            // A new reference for the encoder, so the next scale can write while NVENC reads.
            let out = ffi::av_frame_clone(dst);
            if out.is_null() {
                bail!("av_frame_clone failed");
            }
            Ok(ff::frame::Video::wrap(out))
        }
    }
}

impl Drop for CpuScaler {
    fn drop(&mut self) {
        unsafe { ffi::sws_freeContext(self.sws) };
    }
}

/// FLV over RTMP(S). Starts at the first video keyframe; earlier packets are skipped.
struct Output {
    ctx: *mut ffi::AVFormatContext,
    video_idx: i32,
    audio_idx: Option<i32>,
    tbs: [ffi::AVRational; 2],
    offset: Option<i64>,
    start_pts: i64,
    last_dts: [i64; 2],
    /// Keeps the interrupt callback's flag alive.
    _stop: Arc<AtomicBool>,
}

unsafe impl Send for Output {}

/// Aborts blocking network calls once the stream is stopped.
unsafe extern "C" fn interrupted(opaque: *mut c_void) -> i32 {
    (*(opaque as *const AtomicBool)).load(Ordering::Relaxed) as i32
}

impl Output {
    fn open(url: &str, video: &SendParams, audio: Option<&SendParams>, stop: &Arc<AtomicBool>) -> Result<Self> {
        let curl = CString::new(url).context("stream URL")?;
        unsafe {
            let mut ctx = std::ptr::null_mut();
            let e = ffi::avformat_alloc_output_context2(&mut ctx, std::ptr::null(), c"flv".as_ptr(), curl.as_ptr());
            if e < 0 || ctx.is_null() {
                bail!("creating the FLV muxer: {}", av_err(e));
            }
            let mut o = Self {
                ctx,
                video_idx: 0,
                audio_idx: None,
                tbs: [TB_US; 2],
                offset: None,
                start_pts: 0,
                last_dts: [i64::MIN; 2],
                _stop: stop.clone(),
            };
            (*ctx).interrupt_callback = ffi::AVIOInterruptCB { callback: Some(interrupted), opaque: Arc::as_ptr(stop) as *mut c_void };
            // Don't hold video back for long waiting on audio (or the other way round).
            (*ctx).max_interleave_delta = 500_000;
            let add = |p: &SendParams| -> Result<i32> {
                let st = ffi::avformat_new_stream(ctx, std::ptr::null());
                if st.is_null() {
                    bail!("avformat_new_stream failed");
                }
                ffi::avcodec_parameters_copy((*st).codecpar, p.0.as_ptr());
                (*(*st).codecpar).codec_tag = 0;
                Ok((*st).index)
            };
            o.video_idx = add(video)?;
            o.audio_idx = audio.map(add).transpose()?;

            let mut opts: *mut ffi::AVDictionary = std::ptr::null_mut();
            // Give up on a dead connection after 10 s instead of hanging.
            ffi::av_dict_set(&mut opts, c"rw_timeout".as_ptr(), c"10000000".as_ptr(), 0);
            ffi::av_dict_set(&mut opts, c"tcp_nodelay".as_ptr(), c"1".as_ptr(), 0);
            let e = ffi::avio_open2(&mut (*ctx).pb, curl.as_ptr(), ffi::AVIO_FLAG_WRITE, &(*ctx).interrupt_callback, &mut opts);
            ffi::av_dict_free(&mut opts);
            if e < 0 {
                bail!("connecting: {}", av_err(e));
            }
            let mut hdr: *mut ffi::AVDictionary = std::ptr::null_mut();
            ffi::av_dict_set(&mut hdr, c"flvflags".as_ptr(), c"no_duration_filesize".as_ptr(), 0);
            let e = ffi::avformat_write_header(ctx, &mut hdr);
            ffi::av_dict_free(&mut hdr);
            if e < 0 {
                bail!("starting the stream: {}", av_err(e));
            }
            let tb = |i: i32| (**(*ctx).streams.add(i as usize)).time_base;
            o.tbs[0] = tb(o.video_idx);
            if let Some(a) = o.audio_idx {
                o.tbs[1] = tb(a);
            }
            Ok(o)
        }
    }

    /// Writes one packet; returns the bytes written (0 when skipped).
    fn write(&mut self, p: &EncodedPacket) -> Result<usize> {
        let (idx, slot) = match p.kind {
            StreamKind::Video => (self.video_idx, 0),
            StreamKind::Audio => match self.audio_idx {
                Some(i) => (i, 1),
                None => return Ok(0),
            },
        };
        let offset = match self.offset {
            Some(o) => o,
            None if p.kind == StreamKind::Video && p.key => {
                self.start_pts = p.pts_us;
                *self.offset.insert(p.dts_us.min(p.pts_us))
            }
            None => return Ok(0),
        };
        if p.kind == StreamKind::Audio && p.pts_us < self.start_pts {
            return Ok(0);
        }
        let tb = self.tbs[slot];
        let rescale = |us: i64| unsafe { ffi::av_rescale_q(us, TB_US, tb) };
        let mut dts = rescale(p.dts_us - offset);
        if dts <= self.last_dts[slot] {
            dts = self.last_dts[slot] + 1;
        }
        let pts = rescale(p.pts_us - offset).max(dts);
        self.last_dts[slot] = dts;
        unsafe {
            let mut pkt = ffi::av_packet_alloc();
            if pkt.is_null() || ffi::av_new_packet(pkt, p.data.len() as i32) < 0 {
                ffi::av_packet_free(&mut pkt);
                bail!("av_new_packet failed");
            }
            std::ptr::copy_nonoverlapping(p.data.as_ptr(), (*pkt).data, p.data.len());
            (*pkt).stream_index = idx;
            (*pkt).dts = dts;
            (*pkt).pts = pts;
            (*pkt).duration = rescale(p.duration_us);
            if p.key {
                (*pkt).flags |= ffi::AV_PKT_FLAG_KEY as i32;
            }
            let e = ffi::av_interleaved_write_frame(self.ctx, pkt);
            ffi::av_packet_free(&mut pkt);
            if e < 0 {
                bail!("the server closed the connection ({})", av_err(e));
            }
        }
        Ok(p.data.len())
    }

    /// Ends the stream cleanly.
    fn finish(self) {
        unsafe {
            ffi::av_write_trailer(self.ctx);
        }
        // Drop closes the connection.
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            ffi::avio_closep(&mut (*self.ctx).pb);
            ffi::avformat_free_context(self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StreamService;

    #[test]
    fn output_keeps_aspect_and_never_upscales() {
        assert_eq!(output_size(3840, 2160, 1080), (1920, 1080));
        assert_eq!(output_size(2560, 1080, 720), (1706, 720)); // 21:9, even width
        assert_eq!(output_size(1280, 720, 1080), (1280, 720));
        assert_eq!(output_size(3840, 2160, 0), (3840, 2160));
    }

    #[test]
    fn publish_urls() {
        assert_eq!(StreamService::Twitch.url("", " live_123 "), "rtmp://live.twitch.tv/app/live_123");
        assert_eq!(StreamService::YouTube.url("", "abcd"), "rtmps://a.rtmps.youtube.com:443/live2/abcd");
        assert_eq!(StreamService::Custom.url("rtmp://host/live/", "k"), "rtmp://host/live/k");
        assert_eq!(StreamService::Custom.url("rtmp://host/live/key", ""), "rtmp://host/live/key");
    }
}
