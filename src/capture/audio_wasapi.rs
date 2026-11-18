//! WASAPI shared-mode capture (capture card audio) and render (passthrough to speakers).
//!
//! Both directions run at a fixed 48 kHz stereo f32 format; `AUTOCONVERTPCM` lets Windows
//! resample from/to each endpoint's mix format.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::{AUDIO_CHANNELS, AUDIO_RATE};
use crate::audio::PassRing;
use crate::clock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Capture,
    Render,
}

impl Flow {
    fn edataflow(self) -> EDataFlow {
        match self {
            Flow::Capture => eCapture,
            Flow::Render => eRender,
        }
    }
}

unsafe fn enumerator() -> Result<IMMDeviceEnumerator> {
    Ok(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?)
}

unsafe fn device_name(dev: &IMMDevice) -> Result<String> {
    let store = dev.OpenPropertyStore(STGM_READ)?;
    let pv = store.GetValue(&PKEY_Device_FriendlyName)?;
    let p = PropVariantToStringAlloc(&pv)?;
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    Ok(s)
}

unsafe fn endpoints(flow: Flow) -> Result<Vec<(String, IMMDevice)>> {
    let coll = enumerator()?.EnumAudioEndpoints(flow.edataflow(), DEVICE_STATE_ACTIVE)?;
    let mut out = Vec::new();
    for i in 0..coll.GetCount()? {
        let dev = coll.Item(i)?;
        out.push((device_name(&dev).unwrap_or_else(|_| format!("Device {i}")), dev));
    }
    Ok(out)
}

/// Lists active endpoint names. Requires COM on the calling thread.
pub fn list_endpoints(flow: Flow) -> Result<Vec<String>> {
    unsafe { Ok(endpoints(flow)?.into_iter().map(|(n, _)| n).collect()) }
}

/// Finds an endpoint whose name contains `pref`. With no preference: capture picks the first
/// Elgato-looking endpoint (or one matching `video_device`), render uses the default device.
unsafe fn find_endpoint(flow: Flow, pref: Option<&str>, video_device: Option<&str>) -> Result<(String, IMMDevice)> {
    let all = endpoints(flow)?;
    if let Some(p) = pref {
        let p = p.to_lowercase();
        return all
            .into_iter()
            .find(|(n, _)| n.to_lowercase().contains(&p))
            .ok_or_else(|| anyhow!("audio device '{p}' not found"));
    }
    match flow {
        Flow::Render => {
            let dev = enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?;
            Ok((device_name(&dev).unwrap_or_else(|_| "Default output".into()), dev))
        }
        Flow::Capture => {
            let vd = video_device.map(|v| v.to_lowercase());
            all.into_iter()
                .find(|(n, _)| {
                    super::looks_like_elgato(n) || vd.as_deref().is_some_and(|v| n.to_lowercase().contains(v))
                })
                .ok_or_else(|| anyhow!("no capture-card audio endpoint found; pick one in Settings"))
        }
    }
}

fn wave_format() -> WAVEFORMATEXTENSIBLE {
    let block = (AUDIO_CHANNELS * 4) as u16;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: AUDIO_CHANNELS as u16,
            nSamplesPerSec: AUDIO_RATE,
            nAvgBytesPerSec: AUDIO_RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: 0x3, // FRONT_LEFT | FRONT_RIGHT
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

struct Event(HANDLE);
impl Event {
    fn new() -> Result<Self> {
        Ok(Self(unsafe { CreateEventW(None, false, false, None)? }))
    }
}
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

unsafe fn init_client(dev: &IMMDevice, buffer_hns: i64) -> Result<(IAudioClient, Event)> {
    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
    let fmt = wave_format();
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            buffer_hns,
            0,
            &fmt as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX,
            None,
        )
        .context("IAudioClient::Initialize")?;
    let ev = Event::new()?;
    client.SetEventHandle(ev.0)?;
    Ok((client, ev))
}

/// Captures from the card until `stop`. `on_open` reports the endpoint name (or error) on each
/// (re)open; `on_samples(interleaved, pts_us, discontinuity)` gets every packet.
pub fn run_capture(
    pref: Option<String>,
    video_device: Option<String>,
    stop: Arc<AtomicBool>,
    mut on_open: impl FnMut(Result<String>),
    mut on_samples: impl FnMut(&[f32], i64, bool),
) {
    let _com = match super::ComGuard::new() {
        Ok(g) => g,
        Err(e) => return on_open(Err(e)),
    };
    let mut silence = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let res = unsafe {
            (|| -> Result<()> {
                let (name, dev) = find_endpoint(Flow::Capture, pref.as_deref(), video_device.as_deref())?;
                let (client, ev) = init_client(&dev, 200_000)?; // 20 ms
                let capture: IAudioCaptureClient = client.GetService()?;
                client.Start()?;
                tracing::info!("audio capture from '{name}'");
                on_open(Ok(name));
                let mut first = true;
                while !stop.load(Ordering::Relaxed) {
                    if WaitForSingleObject(ev.0, 200) != WAIT_OBJECT_0 {
                        continue;
                    }
                    loop {
                        if capture.GetNextPacketSize()? == 0 {
                            break;
                        }
                        let mut data: *mut u8 = std::ptr::null_mut();
                        let mut frames = 0u32;
                        let mut flags = 0u32;
                        let mut qpc = 0u64;
                        capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                        let n = frames as usize * AUDIO_CHANNELS;
                        let pts = if qpc != 0 { clock::hns_to_us(qpc) } else { clock::now_us() };
                        let disc = first || flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
                        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                            silence.clear();
                            silence.resize(n, 0.0);
                            on_samples(&silence, pts, disc);
                        } else {
                            on_samples(std::slice::from_raw_parts(data as *const f32, n), pts, disc);
                        }
                        first = false;
                        capture.ReleaseBuffer(frames)?;
                    }
                }
                let _ = client.Stop();
                Ok(())
            })()
        };
        if let Err(e) = res {
            if !stop.load(Ordering::Relaxed) {
                tracing::warn!("audio capture: {e:#}");
                on_open(Err(e));
                sleep_unless_stopped(&stop, 3000);
            }
        }
    }
}

/// Plays the passthrough ring on the chosen output until `stop`.
pub fn run_render(pref: Option<String>, stop: Arc<AtomicBool>, ring: Arc<PassRing>) {
    let Ok(_com) = super::ComGuard::new() else { return };
    let mut scratch: Vec<f32> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let res = unsafe {
            (|| -> Result<()> {
                let (name, dev) = find_endpoint(Flow::Render, pref.as_deref(), None)?;
                let (client, ev) = init_client(&dev, 100_000)?; // 10 ms
                let render: IAudioRenderClient = client.GetService()?;
                let buf_frames = client.GetBufferSize()?;
                // Pre-roll silence so the first period doesn't glitch.
                render.GetBuffer(buf_frames)?;
                render.ReleaseBuffer(buf_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                client.Start()?;
                tracing::info!("audio passthrough to '{name}' ({buf_frames} frame buffer)");
                ring.reset();
                while !stop.load(Ordering::Relaxed) {
                    if WaitForSingleObject(ev.0, 200) != WAIT_OBJECT_0 {
                        continue;
                    }
                    let padding = client.GetCurrentPadding()?;
                    let avail = buf_frames.saturating_sub(padding);
                    if avail == 0 {
                        continue;
                    }
                    let n = avail as usize * AUDIO_CHANNELS;
                    scratch.resize(n, 0.0);
                    ring.pop_into(&mut scratch);
                    let ptr = render.GetBuffer(avail)? as *mut f32;
                    std::ptr::copy_nonoverlapping(scratch.as_ptr(), ptr, n);
                    render.ReleaseBuffer(avail, 0)?;
                }
                let _ = client.Stop();
                Ok(())
            })()
        };
        if let Err(e) = res {
            if !stop.load(Ordering::Relaxed) {
                tracing::warn!("audio render: {e:#}");
                sleep_unless_stopped(&stop, 3000);
            }
        }
    }
}

fn sleep_unless_stopped(stop: &AtomicBool, ms: u64) {
    let mut left = ms;
    while left > 0 && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(left.min(100)));
        left = left.saturating_sub(100);
    }
}
