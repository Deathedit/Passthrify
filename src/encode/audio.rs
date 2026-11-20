//! AAC encoder thread. Timestamps are anchored to the session clock and kept continuous; if the
//! card's audio clock drifts more than 100 ms from QPC we re-anchor (gap) or drop samples.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Receiver;
use ffmpeg_next as ff;

use crate::capture::{AudioChunk, AUDIO_CHANNELS, AUDIO_RATE};
use crate::packets::{EncodedPacket, SendParams, StreamKind, StreamParams};

const MAX_DRIFT_SAMPLES: i64 = AUDIO_RATE as i64 / 10;

const SAMPLE_FORMAT: ff::format::Sample = ff::format::Sample::F32(ff::format::sample::Type::Planar);

/// The AAC encoder every audio stream uses (48 kHz stereo, 192 kbps, global header).
fn open_encoder() -> Result<ff::encoder::Audio> {
    let codec = ff::encoder::find(ff::codec::Id::AAC).ok_or_else(|| anyhow!("no AAC encoder"))?;
    let mut enc = ff::codec::context::Context::new_with_codec(codec).encoder().audio()?;
    enc.set_rate(AUDIO_RATE as i32);
    enc.set_channel_layout(ff::ChannelLayout::STEREO);
    enc.set_format(SAMPLE_FORMAT);
    enc.set_bit_rate(192_000);
    enc.set_time_base(ff::Rational(1, AUDIO_RATE as i32));
    enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
    enc.open_with(ff::Dictionary::new()).context("opening AAC encoder")
}

/// Codec parameters of the AAC stream `run` produces, for muxers that start later (the live
/// stream): the same encoder settings always give the same parameters.
pub fn params() -> Result<SendParams> {
    let enc = open_encoder()?;
    Ok(SendParams(super::params_of(&enc)))
}

pub fn run(rx: Receiver<AudioChunk>, params: Arc<StreamParams>, mut sink: impl FnMut(EncodedPacket)) -> Result<()> {
    let mut enc = open_encoder()?;
    let fmt = SAMPLE_FORMAT;
    *params.audio.lock() = Some(SendParams(super::params_of(&enc)));

    let frame_size = enc.frame_size().max(1024) as usize;
    let mut left: VecDeque<f32> = VecDeque::new();
    let mut right: VecDeque<f32> = VecDeque::new();
    // pts (in samples) of the first sample in `left`/`right`.
    let mut next_pts: Option<i64> = None;
    let mut frame = ff::frame::Audio::new(fmt, frame_size, ff::ChannelLayout::STEREO);
    frame.set_rate(AUDIO_RATE);
    let mut pkt = ff::Packet::empty();

    for chunk in rx.iter() {
        let ideal = chunk.pts_us * AUDIO_RATE as i64 / 1_000_000;
        let mut samples = &chunk.samples[..];
        match next_pts {
            None => next_pts = Some(ideal),
            Some(p) => {
                let actual = p + left.len() as i64;
                let diff = ideal - actual;
                if diff > MAX_DRIFT_SAMPLES || (chunk.discontinuity && diff > 0) {
                    // Audio fell behind the clock (or the device skipped): leave a gap.
                    flush_partial(&mut left, &mut right, &mut next_pts, frame_size);
                    next_pts = Some(next_pts.unwrap().max(ideal - left.len() as i64));
                } else if diff < -MAX_DRIFT_SAMPLES {
                    // Audio ran ahead of the clock: drop samples to resync.
                    let drop = ((-diff) as usize * AUDIO_CHANNELS).min(samples.len());
                    samples = &samples[drop..];
                }
            }
        }
        for s in samples.chunks_exact(AUDIO_CHANNELS) {
            left.push_back(s[0]);
            right.push_back(s[1]);
        }

        while left.len() >= frame_size {
            unsafe {
                ff::ffi::av_frame_make_writable(frame.as_mut_ptr());
            }
            for (d, s) in frame.plane_mut::<f32>(0).iter_mut().zip(left.drain(..frame_size)) {
                *d = s;
            }
            for (d, s) in frame.plane_mut::<f32>(1).iter_mut().zip(right.drain(..frame_size)) {
                *d = s;
            }
            let pts = next_pts.unwrap();
            frame.set_pts(Some(pts));
            next_pts = Some(pts + frame_size as i64);
            enc.send_frame(&frame).context("aac send_frame")?;
            drain(&mut enc, &mut pkt, &mut sink);
        }
    }
    let _ = enc.send_eof();
    drain(&mut enc, &mut pkt, &mut sink);
    Ok(())
}

/// Discards a partial frame so a new anchor can start cleanly.
fn flush_partial(l: &mut VecDeque<f32>, r: &mut VecDeque<f32>, next: &mut Option<i64>, _fs: usize) {
    if let Some(p) = next.as_mut() {
        *p += l.len() as i64;
    }
    l.clear();
    r.clear();
}

fn samples_to_us(s: i64) -> i64 {
    s * 1_000_000 / AUDIO_RATE as i64
}

fn drain(enc: &mut ff::encoder::Audio, pkt: &mut ff::Packet, sink: &mut impl FnMut(EncodedPacket)) {
    while enc.receive_packet(pkt).is_ok() {
        let Some(data) = pkt.data() else { continue };
        let pts = pkt.pts().unwrap_or(0);
        sink(EncodedPacket {
            kind: StreamKind::Audio,
            data: data.to_vec(),
            pts_us: samples_to_us(pts),
            dts_us: samples_to_us(pkt.dts().unwrap_or(pts)),
            duration_us: samples_to_us(pkt.duration()),
            key: true,
        });
    }
}
