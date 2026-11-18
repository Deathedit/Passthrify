//! Session clock based on QueryPerformanceCounter.
//!
//! Every timestamp in the pipeline (video frames, audio chunks, encoded packets) is expressed in
//! microseconds since the session epoch, so audio and video line up without any cross-device
//! clock negotiation.

use std::sync::OnceLock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

static FREQ: OnceLock<i64> = OnceLock::new();
static EPOCH: OnceLock<i64> = OnceLock::new();

fn freq() -> i64 {
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    })
}

fn epoch() -> i64 {
    *EPOCH.get_or_init(qpc_now)
}

/// Fix the epoch. Call once at startup, before any capture thread runs.
pub fn init() {
    freq();
    epoch();
}

pub fn qpc_now() -> i64 {
    let mut c = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    c
}

/// Microseconds since the session epoch.
pub fn now_us() -> i64 {
    qpc_to_us(qpc_now())
}

pub fn qpc_to_us(qpc: i64) -> i64 {
    ((qpc - epoch()) as i128 * 1_000_000 / freq() as i128) as i64
}

/// WASAPI reports `u64QPCPosition` in 100 ns units (QPC already scaled to 10 MHz).
pub fn hns_to_us(hns: u64) -> i64 {
    let epoch_hns = (epoch() as i128 * 10_000_000 / freq() as i128) as i64;
    (hns as i64 - epoch_hns) / 10
}
