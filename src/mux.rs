//! MP4 writing from encoded packets: whole clips (replay) and live fragmented MP4 (recording).

use std::path::Path;

use anyhow::{bail, Context, Result};
use ffmpeg_next as ff;

use crate::packets::{EncodedPacket, SendParams, StreamKind};
use crate::replay::Clip;

const TB_US: ff::Rational = ff::Rational(1, 1_000_000);

struct Writer {
    octx: ff::format::context::Output,
    video_idx: usize,
    audio_idx: Option<usize>,
    video_tb: ff::Rational,
    audio_tb: ff::Rational,
    last_dts: [i64; 2],
}

impl Writer {
    fn create(path: &Path, video: &SendParams, audio: Option<&SendParams>, movflags: &str) -> Result<Self> {
        let mut octx = ff::format::output(&path).with_context(|| format!("creating {}", path.display()))?;
        let video_idx = add_stream(&mut octx, video)?;
        let audio_idx = audio.map(|a| add_stream(&mut octx, a)).transpose()?;
        let mut opts = ff::Dictionary::new();
        opts.set("movflags", movflags);
        octx.write_header_with(opts).context("writing MP4 header")?;
        let video_tb = octx.stream(video_idx).unwrap().time_base();
        let audio_tb = audio_idx.map(|i| octx.stream(i).unwrap().time_base()).unwrap_or(TB_US);
        Ok(Self { octx, video_idx, audio_idx, video_tb, audio_tb, last_dts: [i64::MIN; 2] })
    }

    /// Writes one packet with timestamps shifted by `-offset_us`.
    fn write(&mut self, p: &EncodedPacket, offset_us: i64) -> Result<()> {
        let (idx, tb, slot) = match p.kind {
            StreamKind::Video => (self.video_idx, self.video_tb, 0),
            StreamKind::Audio => match self.audio_idx {
                Some(i) => (i, self.audio_tb, 1),
                None => return Ok(()),
            },
        };
        let mut dts = rescale(p.dts_us - offset_us, tb);
        let mut pts = rescale(p.pts_us - offset_us, tb);
        if dts <= self.last_dts[slot] {
            dts = self.last_dts[slot] + 1;
        }
        pts = pts.max(dts);
        self.last_dts[slot] = dts;

        let mut pkt = ff::Packet::copy(&p.data);
        pkt.set_stream(idx);
        pkt.set_dts(Some(dts));
        pkt.set_pts(Some(pts));
        pkt.set_duration(rescale(p.duration_us, tb));
        if p.key {
            pkt.set_flags(ff::packet::Flags::KEY);
        }
        pkt.write_interleaved(&mut self.octx).context("writing packet")?;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        // Not `octx.write_trailer()`: with fragmented MP4 the mov muxer returns the (positive)
        // size of the `mfra` box it wrote, which ffmpeg-next misreports as an error.
        let ret = unsafe { ff::ffi::av_write_trailer(self.octx.as_mut_ptr()) };
        if ret < 0 {
            bail!("writing MP4 trailer: {}", ff::Error::from(ret));
        }
        Ok(())
    }
}

fn add_stream(octx: &mut ff::format::context::Output, params: &SendParams) -> Result<usize> {
    let mut st = octx.add_stream(ff::encoder::find(ff::codec::Id::None))?;
    st.set_parameters(params.0.clone());
    unsafe {
        (*st.parameters().as_mut_ptr()).codec_tag = 0;
    }
    Ok(st.index())
}

fn rescale(us: i64, tb: ff::Rational) -> i64 {
    unsafe { ff::ffi::av_rescale_q(us, TB_US.into(), tb.into()) }
}

/// Writes a replay clip to `path` (MP4 with `faststart`).
pub fn write_clip(path: &Path, clip: &Clip, video: &SendParams, audio: Option<&SendParams>) -> Result<()> {
    let Some(first) = clip.video.first() else { bail!("clip has no video") };
    let audio = audio.filter(|_| !clip.audio.is_empty());
    let mut w = Writer::create(path, video, audio, "+faststart")?;
    let offset = first.dts_us.min(first.pts_us);

    // Merge both streams in dts order.
    let (mut vi, mut ai) = (0, 0);
    while vi < clip.video.len() || ai < clip.audio.len() {
        let take_video = match (clip.video.get(vi), clip.audio.get(ai)) {
            (Some(v), Some(a)) => v.dts_us <= a.dts_us,
            (Some(_), None) => true,
            _ => false,
        };
        if take_video {
            w.write(&clip.video[vi], offset)?;
            vi += 1;
        } else {
            w.write(&clip.audio[ai], offset)?;
            ai += 1;
        }
    }
    w.finish()
}

/// Live recorder output: fragmented MP4 so the file stays playable if the app crashes.
/// Starts at the first video keyframe it sees; earlier packets are discarded.
pub struct LiveWriter {
    w: Writer,
    offset: Option<i64>,
    start_pts: i64,
}

impl LiveWriter {
    pub fn create(path: &Path, video: &SendParams, audio: Option<&SendParams>) -> Result<Self> {
        let w = Writer::create(path, video, audio, "+frag_keyframe+empty_moov+default_base_moof")?;
        Ok(Self { w, offset: None, start_pts: 0 })
    }

    pub fn write(&mut self, p: &EncodedPacket) -> Result<()> {
        match self.offset {
            None => {
                if p.kind == StreamKind::Video && p.key {
                    self.offset = Some(p.dts_us.min(p.pts_us));
                    self.start_pts = p.pts_us;
                } else {
                    return Ok(());
                }
            }
            Some(_) if p.kind == StreamKind::Audio && p.pts_us < self.start_pts => return Ok(()),
            Some(_) => {}
        }
        self.w.write(p, self.offset.unwrap())
    }

    pub fn finish(self) -> Result<()> {
        self.w.finish()
    }
}
