pub mod audio;
pub mod video;

use ffmpeg_next as ff;

/// Copies an opened encoder's parameters (incl. global headers) for later muxing.
pub(crate) fn params_of(enc: &ff::codec::Context) -> ff::codec::Parameters {
    let mut p = ff::codec::Parameters::new();
    unsafe {
        ff::ffi::avcodec_parameters_from_context(p.as_mut_ptr(), enc.as_ptr());
    }
    p
}
