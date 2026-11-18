#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod capture;
mod clock;
mod config;
mod encode;
mod hotkeys;
mod mux;
mod notify;
mod packets;
mod pipeline;
mod recorder;
mod replay;
mod screenshot;
mod svg_raster;
mod tray;
mod ui;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use eframe::{egui, egui_wgpu, wgpu};

use config::{Config, PresentMode};
use pipeline::{Command, Pipeline, Shared};

const USAGE: &str = "\
Passthrify - Elgato capture viewer with instant replay

USAGE:
  passthrify                    start the app
  passthrify --list-devices     list capture devices, their modes and audio endpoints
  passthrify --selftest [SECS] [--out DIR] [--mode WxH@FPS] [--chroma 420|422|444]
             [--codec hevc|h264|av1] [--cycle-modes]
                                run capture+encode headless for SECS (default 15), then
                                save a replay, a screenshot and a short recording
";

fn main() -> Result<()> {
    clock::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        attach_console();
        print!("{USAGE}");
        return Ok(());
    }
    if args.iter().any(|a| a == "--list-devices") {
        attach_console();
        return list_devices();
    }
    init_logging(!args.is_empty());
    install_panic_hook();
    ffmpeg_next::init()?;
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Warning);

    if let Some(i) = args.iter().position(|a| a == "--selftest") {
        attach_console();
        let secs = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(15);
        let out = args.iter().position(|a| a == "--out").and_then(|j| args.get(j + 1)).cloned();
        let cycle = args.iter().any(|a| a == "--cycle-modes");
        let arg = |name: &str| args.iter().position(|a| a == name).and_then(|j| args.get(j + 1)).cloned();
        let overrides = SelftestOverrides {
            chroma: arg("--chroma").map(|c| match c.as_str() {
                "444" | "4:4:4" => config::Chroma::S444,
                "422" | "4:2:2" => config::Chroma::S422,
                _ => config::Chroma::S420,
            }),
            mode: arg("--mode").and_then(|m| {
                // WxH@FPS, e.g. 2560x1440@60
                let (res, fps) = m.split_once('@')?;
                let (w, h) = res.split_once('x')?;
                Some(config::ModePref { width: w.parse().ok()?, height: h.parse().ok()?, fps: fps.parse().ok()? })
            }),
            codec: arg("--codec").map(|c| match c.to_lowercase().as_str() {
                "h264" => config::Codec::H264,
                "av1" => config::Codec::Av1,
                _ => config::Codec::Hevc,
            }),
        };
        return selftest(secs, out, cycle, overrides);
    }
    run_gui()
}

fn run_gui() -> Result<()> {
    let cfg = Config::load();
    let shared = Arc::new(Shared::new(&cfg));
    let pipeline = Pipeline::start(cfg.clone(), shared.clone());

    let present_mode = match cfg.present_mode {
        PresentMode::Mailbox => wgpu::PresentMode::Mailbox,
        PresentMode::Immediate => wgpu::PresentMode::Immediate,
        PresentMode::Fifo => wgpu::PresentMode::Fifo,
    };
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("Passthrify")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([480.0, 300.0])
            .with_icon(Arc::new(egui::IconData { rgba: tray::app_icon_rgba(128), width: 128, height: 128 })),
        wgpu_options: egui_wgpu::WgpuConfiguration {
            present_mode,
            desired_maximum_frame_latency: Some(1),
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "Passthrify",
        options,
        Box::new(move |cc| Ok(Box::new(ui::app::App::new(cc, cfg, shared, pipeline)))),
    )
    .map_err(|e| anyhow!("UI error: {e}"))
}

fn init_logging(to_stderr: bool) {
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,wgpu_core=warn,wgpu_hal=error,naga=warn,eframe=warn,egui_wgpu=warn"));
    let dir = Config::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::File::create(dir.join("passthrify.log")).ok();
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false);
    match (file, to_stderr || cfg!(debug_assertions)) {
        (Some(f), true) => builder.with_writer(std::io::stderr.and(std::sync::Mutex::new(f))).init(),
        (Some(f), false) => builder.with_writer(std::sync::Mutex::new(f)).init(),
        (None, _) => builder.with_writer(std::io::stderr).init(),
    }
}

/// Release builds have no console, so make panics visible in the log file.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!("PANIC in thread '{thread}': {info}\n{bt}");
        default(info);
    }));
}

/// Lets a GUI-subsystem build print to the terminal it was started from.
fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn list_devices() -> Result<()> {
    let _com = capture::ComGuard::new()?;
    let _mf = capture::video_mf::MfGuard::new()?;
    println!("Video capture devices:");
    for d in capture::video_mf::list_devices()? {
        println!("  {}", d.name);
        match capture::video_mf::VideoDevice::open(Some(&d.name)) {
            Ok(dev) => {
                let best = dev.choose_mode(None, config::Chroma::S420).map(|(m, _)| m);
                for m in &dev.modes {
                    let mark = if Some(*m) == best { "  <- default" } else { "" };
                    let chroma = if m.format.supported() { m.format.native_chroma().label() } else { "-" };
                    println!("      [{:>3}] {m} ({chroma}){mark}", m.index);
                }
            }
            Err(e) => println!("      (can't open: {e:#})"),
        }
    }
    for (title, flow) in [
        ("Audio inputs", capture::audio_wasapi::Flow::Capture),
        ("Audio outputs", capture::audio_wasapi::Flow::Render),
    ] {
        println!("{title}:");
        for n in capture::audio_wasapi::list_endpoints(flow)? {
            let mark = if flow == capture::audio_wasapi::Flow::Capture && capture::looks_like_elgato(&n) {
                "  <- auto"
            } else {
                ""
            };
            println!("  {n}{mark}");
        }
    }
    Ok(())
}

/// Headless end-to-end check against real hardware.
struct SelftestOverrides {
    chroma: Option<config::Chroma>,
    mode: Option<config::ModePref>,
    codec: Option<config::Codec>,
}

fn selftest(secs: u64, out: Option<String>, cycle: bool, ov: SelftestOverrides) -> Result<()> {
    let mut cfg = Config::load();
    if let Some(o) = out {
        cfg.output_dir = o.into();
    }
    if let Some(c) = ov.chroma {
        cfg.chroma = c;
    }
    if ov.mode.is_some() {
        cfg.video_mode = ov.mode;
    }
    if let Some(c) = ov.codec {
        cfg.codec = c;
    }
    cfg.windows_notifications = false;
    cfg.replay_seconds = cfg.replay_seconds.min(secs.saturating_sub(5).max(5) as u32);
    println!("output dir: {}", cfg.output_dir.display());
    let shared = Arc::new(Shared::new(&cfg));
    let pipeline = Pipeline::start(cfg.clone(), shared.clone());
    let st = &shared.status;

    let start = Instant::now();
    let mut recorded = false;
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        let t = start.elapsed().as_secs();
        if cycle && t % 5 == 0 {
            // Exercise the Settings -> Apply path with a different capture mode each time.
            const MODES: [(u32, u32, f64); 6] =
                [(1920, 1080, 60.0), (2560, 1440, 120.0), (3840, 2160, 60.0), (1280, 720, 60.0), (3840, 2160, 144.0), (1920, 1080, 240.0)];
            let (w, h, fps) = MODES[(t / 5) as usize % MODES.len()];
            let mut c = cfg.clone();
            c.video_mode = Some(config::ModePref { width: w, height: h, fps });
            println!("--- applying {w}x{h} @ {fps}");
            pipeline.send(Command::Apply(Box::new(c)));
        }
        if t == 3 && !recorded {
            pipeline.send(Command::ToggleRecord);
            recorded = true;
        }
        println!(
            "[{t:>3}s] {} | {:?} | {:.2} fps | dropped {} | replay {:.1}s {:.1} MB | {} | audio: {} | err: {:?}",
            st.device.lock(),
            st.mode.lock().map(|m| m.to_string()),
            st.fps_x100.load(Ordering::Relaxed) as f64 / 100.0,
            st.dropped_frames.load(Ordering::Relaxed),
            st.replay_ms.load(Ordering::Relaxed) as f64 / 1000.0,
            st.replay_bytes.load(Ordering::Relaxed) as f64 / 1_048_576.0,
            st.encoder.lock(),
            st.audio_in.lock(),
            st.error.lock(),
        );
    }
    pipeline.send(Command::ToggleRecord);
    pipeline.send(Command::SaveReplay);
    pipeline.send(Command::Screenshot);
    std::thread::sleep(Duration::from_secs(4));
    for (text, error, _) in shared.recent_toasts(Duration::from_secs(3600)) {
        println!("{} {text}", if error { "ERROR" } else { "OK   " });
    }
    drop(pipeline);
    Ok(())
}
