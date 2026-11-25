//! YCbCr color description of captured frames (range + matrix) and the math to turn it into RGB.
//!
//! The Elgato 4K X, for example, delivers NV12 as *full range BT.601* but YUY2 as limited
//! BT.709, so this has to travel with every frame: the preview shader, screenshots and the
//! encoder's VUI tags all depend on it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt709,
    Bt601,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    /// true = 0-255 ("PC"/JPEG), false = 16-235 ("TV"/MPEG).
    pub full_range: bool,
    pub matrix: Matrix,
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self { full_range: false, matrix: Matrix::Bt709 }
    }
}

/// User override for the input range (some sources mislabel themselves).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RangePref {
    /// Use what the capture device reports.
    #[default]
    Auto,
    Limited,
    Full,
}

impl RangePref {
    pub const ALL: [RangePref; 3] = [RangePref::Auto, RangePref::Limited, RangePref::Full];
    pub fn label(self) -> &'static str {
        match self {
            RangePref::Auto => "Auto (from device)",
            RangePref::Limited => "Limited (16-235)",
            RangePref::Full => "Full (0-255)",
        }
    }
}

/// Uniform block for the preview shader (matches `ColorParams` in nv12.wgsl).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShaderParams {
    /// y_offset, y_scale, c_scale, unused
    pub levels: [f32; 4],
    /// r_v, g_u, g_v, b_u
    pub coefs: [f32; 4],
}

impl ColorInfo {
    pub fn label(&self) -> String {
        format!(
            "{} {}",
            match self.matrix {
                Matrix::Bt709 => "BT.709",
                Matrix::Bt601 => "BT.601",
            },
            if self.full_range { "full" } else { "limited" }
        )
    }

    /// From Media Foundation attributes: MF_MT_VIDEO_NOMINAL_RANGE (1 = 0-255, 2 = 16-235) and
    /// MF_MT_YUV_MATRIX (1 = BT.709, 2 = BT.601). Missing values default to limited, and to
    /// BT.709 for HD / BT.601 for SD.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn from_mf(nominal_range: Option<u32>, yuv_matrix: Option<u32>, height: u32) -> Self {
        let matrix = match yuv_matrix {
            Some(2) => Matrix::Bt601,
            Some(1) => Matrix::Bt709,
            _ if height < 720 => Matrix::Bt601,
            _ => Matrix::Bt709,
        };
        Self { full_range: nominal_range == Some(1), matrix }
    }

    /// From a V4L2 pixel format: `colorspace` (1 = SMPTE 170M, 3 = Rec. 709, 7 = JPEG,
    /// 8 = sRGB), `ycbcr_enc` (1 = BT.601, 2 = BT.709, 3/4 = their xvYCC variants, 0 = derive
    /// from the colorspace) and `quantization` (1 = full, 2 = limited, 0 = derive: full only for
    /// the JPEG colorspace), as the V4L2 spec defines the defaults.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn from_v4l2(colorspace: u32, ycbcr_enc: u32, quantization: u32, height: u32) -> Self {
        let matrix = match (ycbcr_enc, colorspace) {
            (1 | 3, _) => Matrix::Bt601,
            (2 | 4, _) => Matrix::Bt709,
            (_, 3) => Matrix::Bt709,
            (_, 1 | 7 | 8) => Matrix::Bt601,
            _ if height < 720 => Matrix::Bt601,
            _ => Matrix::Bt709,
        };
        let full_range = match quantization {
            1 => true,
            2 => false,
            _ => colorspace == 7,
        };
        Self { full_range, matrix }
    }

    pub fn with_range(mut self, pref: RangePref) -> Self {
        match pref {
            RangePref::Auto => {}
            RangePref::Limited => self.full_range = false,
            RangePref::Full => self.full_range = true,
        }
        self
    }

    /// (r_v, g_u, g_v, b_u) for R = Y + r_v V, G = Y - g_u U - g_v V, B = Y + b_u U.
    fn coefs(&self) -> [f32; 4] {
        let (kr, kb): (f32, f32) = match self.matrix {
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt601 => (0.299, 0.114),
        };
        let kg = 1.0 - kr - kb;
        let rv = 2.0 * (1.0 - kr);
        let bu = 2.0 * (1.0 - kb);
        [rv, bu * kb / kg, rv * kr / kg, bu]
    }

    /// (y_offset, y_scale, c_scale) on normalized 0..1 samples.
    fn levels(&self) -> (f32, f32, f32) {
        if self.full_range {
            (0.0, 1.0, 1.0)
        } else {
            (16.0 / 255.0, 255.0 / 219.0, 255.0 / 224.0)
        }
    }

    pub fn shader_params(&self) -> ShaderParams {
        let (yo, ys, cs) = self.levels();
        ShaderParams { levels: [yo, ys, cs, 0.0], coefs: self.coefs() }
    }

    /// 8-bit Y, Cb, Cr -> 8-bit gamma-encoded RGB.
    pub fn to_rgb(&self, y: u8, u: u8, v: u8) -> [u8; 3] {
        let (yo, ys, cs) = self.levels();
        let [rv, gu, gv, bu] = self.coefs();
        let y = (y as f32 / 255.0 - yo) * ys;
        let u = (u as f32 / 255.0 - 128.0 / 255.0) * cs;
        let v = (v as f32 / 255.0 - 128.0 / 255.0) * cs;
        let px = |c: f32| (c * 255.0).round().clamp(0.0, 255.0) as u8;
        [px(y + rv * v), px(y - gu * u - gv * v), px(y + bu * u)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIM709: ColorInfo = ColorInfo { full_range: false, matrix: Matrix::Bt709 };
    const FULL601: ColorInfo = ColorInfo { full_range: true, matrix: Matrix::Bt601 };

    #[test]
    fn black_and_white_levels() {
        assert_eq!(LIM709.to_rgb(16, 128, 128), [0, 0, 0]);
        assert_eq!(LIM709.to_rgb(235, 128, 128), [255, 255, 255]);
        assert_eq!(FULL601.to_rgb(0, 128, 128), [0, 0, 0]);
        assert_eq!(FULL601.to_rgb(255, 128, 128), [255, 255, 255]);
        // The bug this module fixes: full-range black read as limited is crushed below 0,
        // and Y=16 (dark grey in full range) is shown as pure black.
        assert_eq!(FULL601.to_rgb(16, 128, 128), [16, 16, 16]);
    }

    #[test]
    fn primaries_round_trip() {
        // Pure red encoded in full-range BT.601: Y=76, Cb=85, Cr=255.
        let [r, g, b] = FULL601.to_rgb(76, 85, 255);
        assert!(r >= 253 && g <= 2 && b <= 2, "{r} {g} {b}");
        // Pure red in limited BT.709: Y=63, Cb=102, Cr=240.
        let [r, g, b] = LIM709.to_rgb(63, 102, 240);
        assert!(r >= 253 && g <= 3 && b <= 3, "{r} {g} {b}");
    }

    #[test]
    fn mf_attributes() {
        assert_eq!(ColorInfo::from_mf(Some(1), Some(2), 2160), FULL601);
        assert_eq!(ColorInfo::from_mf(Some(2), Some(1), 1440), LIM709);
        assert_eq!(ColorInfo::from_mf(None, None, 1080), LIM709);
        assert_eq!(ColorInfo::from_mf(None, None, 480).matrix, Matrix::Bt601);
        assert!(!FULL601.with_range(RangePref::Limited).full_range);
        assert_eq!(FULL601.with_range(RangePref::Auto), FULL601);
    }
}
