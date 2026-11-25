//! Audio capture and passthrough playback through PipeWire (Linux), with the same interface as
//! the WASAPI backend.
//!
//! The capture card's audio is a PipeWire source node (e.g. "Elgato 4K X Analog Stereo"). Both
//! streams ask for interleaved stereo f32 at 48 kHz and PipeWire converts from whatever the
//! device runs at. Each stream runs its own PipeWire loop on the calling thread, iterated with a
//! short timeout so the stop flag is honored; the process callbacks run on that same thread.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use pipewire as pw;
use pw::loop_::Timeout;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamState};

use super::{sleep_unless_stopped, AUDIO_CHANNELS, AUDIO_RATE};
use crate::audio::PassRing;
use crate::clock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Capture,
    Render,
}

impl Flow {
    fn matches(self, media_class: &str) -> bool {
        match self {
            Flow::Capture => media_class.starts_with("Audio/Source"),
            Flow::Render => media_class == "Audio/Sink",
        }
    }
}

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
}

/// An audio node: its human-readable description (what the UI shows and config.toml stores)
/// and its node.name (what a stream targets).
struct Node {
    description: String,
    name: String,
}

/// Lists the audio nodes for `flow` with one registry roundtrip.
fn nodes(flow: Flow) -> Result<Vec<Node>> {
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).map_err(|e| anyhow!("can't connect to PipeWire: {e}"))?;
    let registry = core.get_registry_rc()?;
    let found: Rc<RefCell<Vec<Node>>> = Rc::default();
    let done = Rc::new(std::cell::Cell::new(false));

    let pending = core.sync(0)?;
    let _core_listener = core
        .add_listener_local()
        .done({
            let done = done.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                }
            }
        })
        .register();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let found = found.clone();
            move |g| {
                if g.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(props) = g.props else { return };
                if !props.get("media.class").is_some_and(|c| flow.matches(c)) {
                    return;
                }
                let Some(name) = props.get("node.name") else { return };
                let description = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .unwrap_or(name)
                    .to_string();
                found.borrow_mut().push(Node { description, name: name.to_string() });
            }
        })
        .register();

    let started = Instant::now();
    while !done.get() {
        if started.elapsed() > Duration::from_secs(3) {
            bail!("PipeWire didn't answer");
        }
        mainloop.loop_().iterate(Timeout::Finite(Duration::from_millis(100)));
    }
    let mut out = std::mem::take(&mut *found.borrow_mut());
    out.sort_by(|a, b| a.description.cmp(&b.description));
    Ok(out)
}

/// Lists audio device names (node descriptions).
pub fn list_endpoints(flow: Flow) -> Result<Vec<String>> {
    Ok(nodes(flow)?.into_iter().map(|n| n.description).collect())
}

/// The node whose description contains `pref`. With no preference: capture picks the first
/// Elgato-looking source (or one matching `video_device`); render returns `None` for the
/// default output, which PipeWire follows when the user changes it.
fn find_node(flow: Flow, pref: Option<&str>, video_device: Option<&str>) -> Result<Option<Node>> {
    let all = nodes(flow)?;
    if let Some(p) = pref {
        let p = p.to_lowercase();
        return all
            .into_iter()
            .find(|n| n.description.to_lowercase().contains(&p))
            .map(Some)
            .ok_or_else(|| anyhow!("audio device '{p}' not found"));
    }
    match flow {
        Flow::Render => Ok(None),
        Flow::Capture => {
            let vd = video_device.map(|v| v.to_lowercase());
            all.into_iter()
                .find(|n| {
                    super::looks_like_elgato(&n.description)
                        || vd.as_deref().is_some_and(|v| n.description.to_lowercase().contains(v))
                })
                .map(Some)
                .ok_or_else(|| anyhow!("no capture-card audio device found; pick one in Settings"))
        }
    }
}

/// EnumFormat pod: interleaved stereo F32LE at 48 kHz (FL, FR).
fn format_pod() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(AUDIO_RATE);
    info.set_channels(AUDIO_CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .expect("serializing the audio format")
    .0
    .into_inner()
}

/// Why a stream stopped working (set from its state callback).
type Failure = Rc<RefCell<Option<String>>>;

fn watch_state(failure: &Failure) -> impl FnMut(&pw::stream::Stream, &mut (), StreamState, StreamState) + 'static {
    let failure = failure.clone();
    move |_, _, old, new| {
        tracing::debug!("pipewire stream: {old:?} -> {new:?}");
        match new {
            StreamState::Error(e) => *failure.borrow_mut() = Some(e),
            // The target node went away (card unplugged): we asked not to be moved elsewhere.
            StreamState::Unconnected if !matches!(old, StreamState::Unconnected) => {
                *failure.borrow_mut() = Some("the audio device went away".into())
            }
            _ => {}
        }
    }
}

/// Iterates `mainloop` until `stop` or a stream failure.
fn run_loop(mainloop: &pw::main_loop::MainLoopRc, stop: &AtomicBool, failure: &Failure, mut each: impl FnMut()) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        mainloop.loop_().iterate(Timeout::Finite(Duration::from_millis(100)));
        each();
        if let Some(e) = failure.borrow_mut().take() {
            bail!("{e}");
        }
    }
    Ok(())
}

/// Captures from the card until `stop`. `on_open` reports the device name (or error) on each
/// (re)open; `on_samples(interleaved, pts_us, discontinuity)` gets every buffer.
pub fn run_capture(
    pref: Option<String>,
    video_device: Option<String>,
    stop: Arc<AtomicBool>,
    mut on_open: impl FnMut(Result<String>),
    mut on_samples: impl FnMut(&[f32], i64, bool),
) {
    init();
    while !stop.load(Ordering::Relaxed) {
        let res = (|| -> Result<()> {
            let node = find_node(Flow::Capture, pref.as_deref(), video_device.as_deref())?
                .ok_or_else(|| anyhow!("no capture-card audio device"))?;
            let mainloop = pw::main_loop::MainLoopRc::new(None)?;
            let context = pw::context::ContextRc::new(&mainloop, None)?;
            let core = context.connect_rc(None).map_err(|e| anyhow!("can't connect to PipeWire: {e}"))?;
            let stream = pw::stream::StreamBox::new(
                &core,
                "passthrify-capture",
                properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE => "Game",
                    *pw::keys::APP_NAME => "Passthrify",
                    *pw::keys::NODE_NAME => "passthrify-capture",
                    *pw::keys::TARGET_OBJECT => node.name.as_str(),
                    *pw::keys::NODE_LATENCY => "256/48000",
                    // Stay on the card: never fall back or move to another source.
                    "node.dont-fallback" => "true",
                    "node.dont-reconnect" => "true",
                },
            )?;
            // Filled by the process callback, drained on this thread by the loop below.
            let queue: Rc<RefCell<VecDeque<(Vec<f32>, i64)>>> = Rc::default();
            let failure: Failure = Rc::default();
            let _listener = stream
                .add_local_listener::<()>()
                .state_changed(watch_state(&failure))
                .process({
                    let queue = queue.clone();
                    move |stream, _| {
                        let Some(mut buffer) = stream.dequeue_buffer() else { return };
                        let Some(data) = buffer.datas_mut().first_mut() else { return };
                        let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                        let Some(bytes) = data.data() else { return };
                        let Some(bytes) = bytes.get(offset..offset + size) else { return };
                        let samples: Vec<f32> =
                            bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
                        // The buffer just completed: its first sample is its duration ago.
                        let frames = (samples.len() / AUDIO_CHANNELS) as i64;
                        let pts = clock::now_us() - frames * 1_000_000 / AUDIO_RATE as i64;
                        queue.borrow_mut().push_back((samples, pts));
                    }
                })
                .register()?;
            let pod = format_pod();
            let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad format pod"))?];
            stream.connect(
                spa::utils::Direction::Input,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut params,
            )?;
            tracing::info!("audio capture from '{}' ({})", node.description, node.name);
            on_open(Ok(node.description.clone()));
            let mut first = true;
            let mut next_pts: Option<i64> = None;
            run_loop(&mainloop, &stop, &failure, || {
                for (samples, pts) in queue.borrow_mut().drain(..) {
                    // A gap of more than 20 ms against where the last buffer ended is a
                    // discontinuity (an xrun or the graph being suspended).
                    let disc = first || next_pts.is_some_and(|n| (pts - n).abs() > 20_000);
                    on_samples(&samples, pts, disc);
                    next_pts = Some(pts + (samples.len() / AUDIO_CHANNELS) as i64 * 1_000_000 / AUDIO_RATE as i64);
                    first = false;
                }
            })?;
            let _ = stream.disconnect();
            Ok(())
        })();
        if let Err(e) = res {
            if !stop.load(Ordering::Relaxed) {
                tracing::warn!("audio capture: {e:#}");
                on_open(Err(e));
                sleep_unless_stopped(&stop, 3000);
            }
        }
    }
}

/// Plays the passthrough ring on the chosen output (or the default one) until `stop`.
pub fn run_render(pref: Option<String>, stop: Arc<AtomicBool>, ring: Arc<PassRing>) {
    init();
    while !stop.load(Ordering::Relaxed) {
        let res = (|| -> Result<()> {
            let node = find_node(Flow::Render, pref.as_deref(), None)?;
            let mainloop = pw::main_loop::MainLoopRc::new(None)?;
            let context = pw::context::ContextRc::new(&mainloop, None)?;
            let core = context.connect_rc(None).map_err(|e| anyhow!("can't connect to PipeWire: {e}"))?;
            let mut props = properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Game",
                *pw::keys::APP_NAME => "Passthrify",
                *pw::keys::NODE_NAME => "passthrify-passthrough",
                *pw::keys::NODE_DESCRIPTION => "Passthrify (capture card audio)",
                *pw::keys::NODE_LATENCY => "256/48000",
            };
            if let Some(n) = &node {
                props.insert(*pw::keys::TARGET_OBJECT, n.name.as_str());
            }
            let stream = pw::stream::StreamBox::new(&core, "passthrify-passthrough", props)?;
            let failure: Failure = Rc::default();
            let _listener = stream
                .add_local_listener::<()>()
                .state_changed(watch_state(&failure))
                .process({
                    let ring = ring.clone();
                    let mut scratch: Vec<f32> = Vec::new();
                    move |stream, _| {
                        let Some(mut buffer) = stream.dequeue_buffer() else { return };
                        // How much the graph wants this cycle (requested), else fill the buffer.
                        let requested = buffer.requested() as usize;
                        let Some(data) = buffer.datas_mut().first_mut() else { return };
                        let stride = 4 * AUDIO_CHANNELS;
                        let n_frames = {
                            let Some(bytes) = data.data() else { return };
                            let max = bytes.len() / stride;
                            let n = if requested > 0 { requested.min(max) } else { max };
                            scratch.resize(n * AUDIO_CHANNELS, 0.0);
                            ring.pop_into(&mut scratch);
                            for (dst, s) in bytes.chunks_exact_mut(4).zip(&scratch) {
                                dst.copy_from_slice(&s.to_le_bytes());
                            }
                            n
                        };
                        let chunk = data.chunk_mut();
                        *chunk.offset_mut() = 0;
                        *chunk.stride_mut() = stride as i32;
                        *chunk.size_mut() = (n_frames * stride) as u32;
                    }
                })
                .register()?;
            let pod = format_pod();
            let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad format pod"))?];
            ring.reset();
            stream.connect(
                spa::utils::Direction::Output,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut params,
            )?;
            let name = node.as_ref().map(|n| n.description.as_str()).unwrap_or("default output");
            tracing::info!("audio passthrough to '{name}'");
            run_loop(&mainloop, &stop, &failure, || {})?;
            let _ = stream.disconnect();
            Ok(())
        })();
        if let Err(e) = res {
            if !stop.load(Ordering::Relaxed) {
                tracing::warn!("audio render: {e:#}");
                sleep_unless_stopped(&stop, 3000);
            }
        }
    }
}
