//! Session clock: QueryPerformanceCounter on Windows, CLOCK_MONOTONIC on Linux.
//!
//! Every timestamp in the pipeline (video frames, audio chunks, encoded packets) is expressed in
//! microseconds since the session epoch, so audio and video line up without any cross-device
//! clock negotiation.

use std::sync::OnceLock;

static EPOCH: OnceLock<i64> = OnceLock::new();

fn epoch() -> i64 {
    *EPOCH.get_or_init(sys::ticks)
}

/// Fix the epoch. Call once at startup, before any capture thread runs.
pub fn init() {
    sys::init();
    epoch();
}

/// Microseconds since the session epoch.
pub fn now_us() -> i64 {
    sys::ticks_to_us(sys::ticks() - epoch())
}

#[cfg(windows)]
mod sys {
    use std::sync::OnceLock;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    static FREQ: OnceLock<i64> = OnceLock::new();

    pub fn freq() -> i64 {
        *FREQ.get_or_init(|| {
            let mut f = 0i64;
            unsafe {
                let _ = QueryPerformanceFrequency(&mut f);
            }
            f.max(1)
        })
    }

    pub fn init() {
        freq();
    }

    /// QPC ticks.
    pub fn ticks() -> i64 {
        let mut c = 0i64;
        unsafe {
            let _ = QueryPerformanceCounter(&mut c);
        }
        c
    }

    pub fn ticks_to_us(ticks: i64) -> i64 {
        (ticks as i128 * 1_000_000 / freq() as i128) as i64
    }
}

/// WASAPI reports `u64QPCPosition` in 100 ns units (QPC already scaled to 10 MHz).
#[cfg(windows)]
pub fn hns_to_us(hns: u64) -> i64 {
    let epoch_hns = (epoch() as i128 * 10_000_000 / sys::freq() as i128) as i64;
    (hns as i64 - epoch_hns) / 10
}

#[cfg(target_os = "linux")]
mod sys {
    pub fn init() {}

    /// CLOCK_MONOTONIC nanoseconds (the clock V4L2 and PipeWire timestamps use).
    pub fn ticks() -> i64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        }
        ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
    }

    pub fn ticks_to_us(ticks: i64) -> i64 {
        ticks / 1_000
    }
}

/// Converts a CLOCK_MONOTONIC timestamp in nanoseconds (V4L2 buffers, PipeWire) to session µs.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub fn mono_ns_to_us(ns: i64) -> i64 {
    (ns - epoch()) / 1_000
}
