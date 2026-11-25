//! Capture modes and pixel formats shared by the capture backends, and the conversions from
//! what the device delivers (NV12 / YUY2 / BGR24, any pitch) into the packed Y + interleaved-UV
//! layout of [`VideoFrame`](super::VideoFrame).

use std::fmt;

use anyhow::{bail, Result};

use super::Chroma;
use crate::config::ModePref;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixFmt {
    Nv12,
    Yuy2,
    /// Packed B,G,R bytes (MFVideoFormat_RGB24, usually bottom-up; V4L2 BGR3). Full 4:4:4 color.
    Rgb24,
    P010,
    Mjpg,
    /// Unhandled subtype; holds the FourCC (GUID.Data1 for MF video subtypes, which can also be
    /// a D3DFORMAT number).
    Other(u32),
}


impl PixFmt {
    /// From a V4L2 / FourCC pixel format code.
    #[allow(dead_code)]
    pub fn from_fourcc(code: u32) -> Self {
        match &code.to_le_bytes() {
            b"NV12" => PixFmt::Nv12,
            b"YUYV" | b"YUY2" => PixFmt::Yuy2,
            b"BGR3" => PixFmt::Rgb24,
            b"P010" => PixFmt::P010,
            b"MJPG" => PixFmt::Mjpg,
            _ => PixFmt::Other(code),
        }
    }

    pub fn name(self) -> String {
        match self {
            PixFmt::Other(code) => {
                let b = code.to_le_bytes();
                if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
                    String::from_utf8_lossy(&b).trim().to_string()
                } else {
                    format!("D3DFMT {code}")
                }
            }
            f => format!("{f:?}").to_uppercase(),
        }
    }

    pub fn supported(self) -> bool {
        matches!(self, PixFmt::Nv12 | PixFmt::Yuy2 | PixFmt::Rgb24)
    }

    /// Chroma resolution the device delivers in this format.
    pub fn native_chroma(self) -> Chroma {
        match self {
            PixFmt::Yuy2 => Chroma::S422,
            PixFmt::Rgb24 => Chroma::S444,
            _ => Chroma::S420,
        }
    }

    /// Preference when `desired` chroma is wanted (lower is better); None = never use.
    /// RGB24 is only worth its bandwidth when 4:4:4 is wanted.
    pub(crate) fn rank_for(self, desired: Chroma) -> Option<u32> {
        match (desired, self) {
            (Chroma::S420, PixFmt::Nv12) => Some(0),
            (Chroma::S420, PixFmt::Yuy2) => Some(1),
            (Chroma::S422, PixFmt::Yuy2) => Some(0),
            (Chroma::S422, PixFmt::Nv12) => Some(1),
            (Chroma::S444, PixFmt::Rgb24) => Some(0),
            (Chroma::S444, PixFmt::Yuy2) => Some(1),
            (Chroma::S444, PixFmt::Nv12) => Some(2),
            _ => None,
        }
    }
}

/// Chroma we produce when capturing `fmt` while `desired` is wanted.
pub fn output_chroma(fmt: PixFmt, desired: Chroma) -> Chroma {
    fmt.native_chroma().min(desired)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoMode {
    /// Index into the reader's native media types.
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub format: PixFmt,
}

impl VideoMode {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den.max(1) as f64
    }

    pub fn matches(&self, p: &ModePref) -> bool {
        self.width == p.width && self.height == p.height && (self.fps() - p.fps).abs() < 0.5
    }

    pub fn same_shape(&self, o: &VideoMode) -> bool {
        self.width == o.width && self.height == o.height && self.fps_num == o.fps_num && self.fps_den == o.fps_den
    }
}

impl fmt::Display for VideoMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{} @ {:.2} {}", self.width, self.height, self.fps(), self.format.name())
    }
}

#[derive(Clone, Debug)]
pub struct VideoDeviceInfo {
    pub name: String,
}

/// Builds row slices from a locked buffer (top row pointer + signed pitch) and converts.
/// `len` is the locked length for sanity checks when known.
#[allow(clippy::too_many_arguments)]
pub unsafe fn convert_raw(
    fmt: PixFmt,
    chroma: Chroma,
    top: *const u8,
    pitch: isize,
    len: Option<usize>,
    w: usize,
    h: usize,
    out: &mut [u8],
) -> Result<()> {
    if top.is_null() || pitch == 0 {
        bail!("empty frame buffer");
    }
    let row_bytes = match fmt {
        PixFmt::Rgb24 => w * 3,
        PixFmt::Yuy2 => w * 2,
        _ => w,
    };
    if (pitch.unsigned_abs()) < row_bytes {
        bail!("pitch {pitch} smaller than a row ({row_bytes} bytes)");
    }
    match fmt {
        PixFmt::Nv12 | PixFmt::Yuy2 => {
            if pitch < 0 {
                bail!("bottom-up {fmt:?} is not supported");
            }
            let pitch = pitch as usize;
            let need = match fmt {
                PixFmt::Nv12 => pitch * h * 3 / 2,
                _ => pitch * h,
            };
            if len.is_some_and(|l| l < need) {
                bail!("short {fmt:?} buffer ({} < {need})", len.unwrap());
            }
            let src = std::slice::from_raw_parts(top, need);
            convert(fmt, chroma, src, pitch, w, h, out)
        }
        PixFmt::Rgb24 => {
            if len.is_some_and(|l| l < pitch.unsigned_abs() * (h - 1) + row_bytes) {
                bail!("short RGB24 buffer");
            }
            let rows: Vec<&[u8]> =
                (0..h).map(|r| std::slice::from_raw_parts(top.offset(r as isize * pitch), row_bytes)).collect();
            bgr24_to_nv24(&rows, w, out);
            Ok(())
        }
        other => bail!("unsupported pixel format {other:?}"),
    }
}

/// Picks a capture mode and the chroma it yields.
///
/// With a resolution/fps preference: the matching mode in the format that best delivers
/// `chroma` (e.g. RGB24 for 4:4:4, YUY2 for 4:2:2), falling back to lower chroma when that
/// mode doesn't offer it. Without one: the largest mode up to 4K / 60 fps, preferring modes
/// that actually deliver the desired chroma.
pub fn choose_mode(modes: &[VideoMode], pref: Option<&ModePref>, chroma: Chroma) -> Option<(VideoMode, Chroma)> {
    let usable = || {
        modes
            .iter()
            .filter(|m| m.format.supported() && m.width % 2 == 0 && m.height % 2 == 0)
            .filter(move |m| m.format.rank_for(chroma).is_some())
    };
    let rank = |m: &VideoMode| m.format.rank_for(chroma).unwrap_or(9);
    let with_chroma = |m: &VideoMode| (*m, output_chroma(m.format, chroma));

    if let Some(p) = pref {
        if let Some(m) = usable().filter(|m| m.matches(p)).min_by_key(|m| rank(m)) {
            return Some(with_chroma(m));
        }
    }
    // Default: up to 4K, at most ~60 fps (high-refresh modes are opt-in).
    let default_ok = |m: &&VideoMode| m.width <= 3840 && m.height <= 2160 && m.fps() <= 60.5;
    let pool: Vec<&VideoMode> =
        if usable().any(|m| default_ok(&m)) { usable().filter(default_ok).collect() } else { usable().collect() };
    let key = |m: &VideoMode| {
        (
            output_chroma(m.format, chroma) == chroma,
            m.width * m.height,
            (m.fps() * 100.0) as u64,
            std::cmp::Reverse(rank(m)),
        )
    };
    pool.into_iter().max_by(|a, b| key(a).cmp(&key(b))).map(|m| with_chroma(m))
}

/// Converts a locked NV12/YUY2 buffer (top-down, positive pitch) into the packed layout.
pub fn convert(fmt: PixFmt, chroma: Chroma, src: &[u8], pitch: usize, w: usize, h: usize, out: &mut [u8]) -> Result<()> {
    match fmt {
        PixFmt::Yuy2 if chroma == Chroma::S422 => {
            if src.len() < pitch * h {
                bail!("short YUY2 buffer");
            }
            yuy2_to_nv16(src, pitch, w, h, out);
            Ok(())
        }
        PixFmt::Nv12 => {
            if src.len() < pitch * h * 3 / 2 {
                bail!("short NV12 buffer ({} < {})", src.len(), pitch * h * 3 / 2);
            }
            let (y_out, uv_out) = out.split_at_mut(w * h);
            for row in 0..h {
                y_out[row * w..(row + 1) * w].copy_from_slice(&src[row * pitch..row * pitch + w]);
            }
            let uv = &src[pitch * h..];
            for row in 0..h / 2 {
                uv_out[row * w..(row + 1) * w].copy_from_slice(&uv[row * pitch..row * pitch + w]);
            }
            Ok(())
        }
        PixFmt::Yuy2 => {
            if src.len() < pitch * h {
                bail!("short YUY2 buffer");
            }
            yuy2_to_nv12(src, pitch, w, h, out);
            Ok(())
        }
        other => bail!("unsupported pixel format {other:?}"),
    }
}

/// YUY2 (Y0 U Y1 V) -> NV12. Chroma is taken from the even row of each pair.
pub fn yuy2_to_nv12(src: &[u8], pitch: usize, w: usize, h: usize, out: &mut [u8]) {
    let (y_out, uv_out) = out.split_at_mut(w * h);
    for row in 0..h {
        let s = &src[row * pitch..row * pitch + w * 2];
        let y = &mut y_out[row * w..(row + 1) * w];
        for (dst, px) in y.chunks_exact_mut(2).zip(s.chunks_exact(4)) {
            dst[0] = px[0];
            dst[1] = px[2];
        }
        if row % 2 == 0 {
            let uv = &mut uv_out[(row / 2) * w..(row / 2 + 1) * w];
            for (dst, px) in uv.chunks_exact_mut(2).zip(s.chunks_exact(4)) {
                dst[0] = px[1];
                dst[1] = px[3];
            }
        }
    }
}

/// YUY2 (Y0 U Y1 V) -> NV16 (4:2:2: full-height interleaved UV, no chroma loss).
pub fn yuy2_to_nv16(src: &[u8], pitch: usize, w: usize, h: usize, out: &mut [u8]) {
    let (y_out, uv_out) = out.split_at_mut(w * h);
    for row in 0..h {
        let s = &src[row * pitch..row * pitch + w * 2];
        let y = &mut y_out[row * w..(row + 1) * w];
        let uv = &mut uv_out[row * w..(row + 1) * w];
        for ((dy, duv), px) in y.chunks_exact_mut(2).zip(uv.chunks_exact_mut(2)).zip(s.chunks_exact(4)) {
            dy[0] = px[0];
            dy[1] = px[2];
            duv[0] = px[1];
            duv[1] = px[3];
        }
    }
}

/// Packed BGR24 rows (top to bottom) -> NV24 (4:4:4), BT.709 limited range, fixed point.
/// Split across a few threads: 1440p60 is ~220 Mpixel/s.
pub fn bgr24_to_nv24(rows: &[&[u8]], w: usize, out: &mut [u8]) {
    let h = rows.len();
    let (y_out, uv_out) = out.split_at_mut(w * h);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 6);
    let band = h.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for ((rows, y), uv) in rows.chunks(band).zip(y_out.chunks_mut(band * w)).zip(uv_out.chunks_mut(band * w * 2)) {
            s.spawn(move || {
                for (r, src) in rows.iter().enumerate() {
                    let y = &mut y[r * w..(r + 1) * w];
                    let uv = &mut uv[r * w * 2..(r + 1) * w * 2];
                    for ((px, dy), duv) in src.chunks_exact(3).zip(y.iter_mut()).zip(uv.chunks_exact_mut(2)) {
                        let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
                        *dy = ((47 * r + 157 * g + 16 * b + 128) >> 8) as u8 + 16;
                        duv[0] = ((-26 * r - 86 * g + 112 * b + 128) >> 8).wrapping_add(128).clamp(16, 240) as u8;
                        duv[1] = ((112 * r - 102 * g - 10 * b + 128) >> 8).wrapping_add(128).clamp(16, 240) as u8;
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(index: u32, w: u32, h: u32, fps: u32, format: PixFmt) -> VideoMode {
        VideoMode { index, width: w, height: h, fps_num: fps, fps_den: 1, format }
    }

    #[test]
    fn picks_best_mode() {
        let modes = [
            m(0, 1920, 1080, 60, PixFmt::Yuy2),
            m(1, 3840, 2160, 30, PixFmt::Nv12),
            m(2, 3840, 2160, 60, PixFmt::Yuy2),
            m(3, 3840, 2160, 60, PixFmt::Nv12),
            m(4, 3840, 2160, 60, PixFmt::P010),
            m(5, 7680, 4320, 30, PixFmt::Nv12),
            m(6, 3840, 2160, 144, PixFmt::Nv12),
        ];
        let pick = |p: Option<&ModePref>| choose_mode(&modes, p, Chroma::S420).unwrap();
        assert_eq!(pick(None), (modes[3], Chroma::S420));
        let pref = ModePref { width: 1920, height: 1080, fps: 60.0 };
        assert_eq!(pick(Some(&pref)), (modes[0], Chroma::S420)); // YUY2 downsampled to 4:2:0
        let missing = ModePref { width: 1280, height: 720, fps: 60.0 };
        assert_eq!(pick(Some(&missing)).0.index, 3);
        // High refresh is only used when asked for explicitly.
        let hfr = ModePref { width: 3840, height: 2160, fps: 144.0 };
        assert_eq!(pick(Some(&hfr)).0.index, 6);
    }

    #[test]
    fn picks_format_for_chroma() {
        let modes = [
            m(0, 3840, 2160, 60, PixFmt::Nv12),
            m(1, 2560, 1440, 60, PixFmt::Nv12),
            m(2, 2560, 1440, 60, PixFmt::Yuy2),
            m(3, 2560, 1440, 60, PixFmt::Rgb24),
            m(4, 3840, 2160, 30, PixFmt::Yuy2),
        ];
        let p1440 = ModePref { width: 2560, height: 1440, fps: 60.0 };
        let p4k60 = ModePref { width: 3840, height: 2160, fps: 60.0 };
        assert_eq!(choose_mode(&modes, Some(&p1440), Chroma::S420).unwrap(), (modes[1], Chroma::S420));
        assert_eq!(choose_mode(&modes, Some(&p1440), Chroma::S422).unwrap(), (modes[2], Chroma::S422));
        assert_eq!(choose_mode(&modes, Some(&p1440), Chroma::S444).unwrap(), (modes[3], Chroma::S444));
        // 4K60 has no 4:4:4 here: fall back to what that mode offers.
        assert_eq!(choose_mode(&modes, Some(&p4k60), Chroma::S444).unwrap(), (modes[0], Chroma::S420));
        // Auto + 4:4:4 prefers a mode that really delivers 4:4:4 over a bigger 4:2:0 one.
        assert_eq!(choose_mode(&modes, None, Chroma::S444).unwrap(), (modes[3], Chroma::S444));
        // Auto + 4:2:2: 4K30 YUY2 beats 1440p60 YUY2 on size.
        assert_eq!(choose_mode(&modes, None, Chroma::S422).unwrap(), (modes[4], Chroma::S422));
    }

    #[test]
    fn yuy2_to_422_keeps_all_chroma() {
        let src = [
            10, 100, 20, 200, 30, 101, 40, 201, //
            50, 1, 60, 2, 70, 3, 80, 4,
        ];
        let mut out = vec![0u8; 4 * 2 * 2];
        yuy2_to_nv16(&src, 8, 4, 2, &mut out);
        assert_eq!(&out[..8], &[10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(&out[8..], &[100, 200, 101, 201, 1, 2, 3, 4]);
    }

    #[test]
    fn bgr_to_444_reference_colors() {
        // white, black, pure red, pure blue (BGR byte order)
        let row0: &[u8] = &[255, 255, 255, 0, 0, 0];
        let row1: &[u8] = &[0, 0, 255, 255, 0, 0];
        let mut out = vec![0u8; 2 * 2 * 3];
        bgr24_to_nv24(&[row0, row1], 2, &mut out);
        let (y, uv) = out.split_at(4);
        assert_eq!(y[0], 235); // white
        assert_eq!(y[1], 16); // black
        assert_eq!(&uv[0..4], &[128, 128, 128, 128]); // neutral chroma
        // BT.709 red: Y≈63, Cb≈102, Cr=240 ; blue: Y≈32, Cb=240, Cr≈118
        assert!((62..=64).contains(&y[2]), "{}", y[2]);
        assert!((101..=103).contains(&uv[4]) && uv[5] == 240, "{:?}", &uv[4..6]);
        assert!((31..=33).contains(&y[3]), "{}", y[3]);
        assert!(uv[6] == 240 && (117..=119).contains(&uv[7]), "{:?}", &uv[6..8]);
    }

    #[test]
    fn bottom_up_rgb_is_flipped() {
        // Memory holds the bottom row first (pitch negative): top row = red, bottom = blue.
        let mem: [u8; 6] = [255, 0, 0, /* bottom: blue */ 0, 0, 255 /* top: red */];
        let mut out = vec![0u8; 1 * 2 * 3];
        unsafe {
            let top = mem.as_ptr().add(3);
            convert_raw(PixFmt::Rgb24, Chroma::S444, top, -3, None, 1, 2, &mut out).unwrap();
        }
        assert!((62..=64).contains(&out[0]), "top row should be red: {out:?}");
        assert!((31..=33).contains(&out[1]), "bottom row should be blue: {out:?}");
    }

    #[test]
    fn yuy2_conversion() {
        // 4x2 image: row0 = Y 10,20,30,40 U 100 V 200 / U 101 V 201; row1 = Y 50..80
        let src = [
            10, 100, 20, 200, 30, 101, 40, 201, //
            50, 1, 60, 2, 70, 3, 80, 4,
        ];
        let mut out = vec![0u8; 4 * 2 * 3 / 2];
        yuy2_to_nv12(&src, 8, 4, 2, &mut out);
        assert_eq!(&out[..8], &[10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(&out[8..], &[100, 200, 101, 201]);
    }

    #[test]
    fn nv12_compaction_drops_padding() {
        // 2x2 image with pitch 4.
        let src = [1, 2, 0, 0, 3, 4, 0, 0, 9, 8, 0, 0];
        let mut out = vec![0u8; 6];
        convert(PixFmt::Nv12, Chroma::S420, &src, 4, 2, 2, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 9, 8]);
    }
}

