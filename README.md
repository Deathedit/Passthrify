# Passthrify

Low-latency viewer for Elgato capture cards (4K X / 4K60 S+ / 4K60 Pro mk2) with a
ShadowPlay-style instant replay, manual recording, audio passthrough and screenshots.
Rust, Media Foundation + WASAPI capture, NVENC encoding via FFmpeg, egui/wgpu UI.

## Use

| Action | Default hotkey (global) |
|---|---|
| Save the last N seconds (instant replay) | `Alt+F10` |
| Start / stop recording | `Alt+F9` |
| Screenshot (full-res PNG) | `Alt+F1` |
| Fullscreen | `F11` or double-click the preview, `Esc` to leave |

Clips go to `Videos\Passthrify` by default. Settings, hotkeys and devices are under ⚙ Settings
and are stored in `%APPDATA%\Passthrify\config\config.toml`. The log is `passthrify.log` in the
same folder. The tray icon offers the same actions plus show/hide.

Close **Elgato 4K Capture Utility** / OBS first: the card can only be opened by one app.

### CLI

```
passthrify --list-devices            # capture devices, all native modes, audio endpoints
passthrify --selftest 30 --out DIR   # headless: capture+encode 30 s, record, save replay + screenshot
```

## Build

Prerequisites (Windows 10/11 x64):

- Rust stable, MSVC toolchain (`winget install Rustlang.Rustup`)
- Visual Studio 2022 Build Tools with the C++ workload
- LLVM (libclang, used by `ffmpeg-sys-next` bindgen) at `C:\Program Files\LLVM` (`winget install LLVM.LLVM`)
- FFmpeg 8.1 **lgpl-shared** build extracted to `third_party/ffmpeg`
  (BtbN `ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip`; it includes NVENC)
- NVIDIA driver recent enough for the FFmpeg NVENC SDK

`.cargo/config.toml` points `FFMPEG_DIR` / `LIBCLANG_PATH` at those locations. `build.rs` copies
the FFmpeg DLLs next to the executable.

The easy way is `build.bat` (double-click) or `.\build.ps1`. It checks the prerequisites,
downloads FFmpeg if it's missing, runs the tests, builds the release exe and packages
`dist\Passthrify\` (exe + FFmpeg DLLs + README).

```
.\build.ps1                  # test + release build + dist\Passthrify
.\build.ps1 -Zip             # also dist\Passthrify-<version>.zip
.\build.ps1 -SkipTests -Run  # build, package and launch
.\build.ps1 -Dev -Run        # quick debug build with a log console
.\build.ps1 -Clean           # start from scratch
```

Or with cargo directly:

```
cargo test
cargo run                 # debug build shows a console with logs
cargo build --release     # target/release/passthrify.exe (+ FFmpeg DLLs copied next to it)
```

## How it works

```
MF SourceReader (NV12) ─┬─► latest frame ─► wgpu NV12→RGB shader (preview)
                        └─► hevc_nvenc ─┐
WASAPI capture ─┬─► passthrough ring ─► WASAPI render
                └─► AAC ───────────────┴─► packet bus ─► replay ring (RAM, GOP-aligned)
                                                      └─► recorder (fragmented MP4)
```

- All timestamps are QPC microseconds, so audio and video align without device clock negotiation.
- The replay buffer holds **encoded** packets (4K60 HEVC at 50 Mbps ≈ 375 MB per minute) and
  always starts on a keyframe (1 s GOP). Saving a replay just remuxes, with no re-encode.
- Recordings are fragmented MP4: a crash leaves a playable file.

## Not yet supported

- HDR (P010) capture/encode. Use SDR output, or let the card tone-map.
- AMD/Intel encoders and older DirectShow-only Elgato cards (HD60 S, HD60 Pro).
- Zero-copy GPU path (frames go through system memory: ~750 MB/s at 4K60, fine on modern PCs).
