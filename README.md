# Passthrify

Low-latency viewer for Elgato capture cards (4K X / 4K60 S+ / 4K60 Pro mk2) with a
ShadowPlay-style instant replay, manual recording, audio passthrough and screenshots.
Rust, Media Foundation + WASAPI capture, NVENC encoding via FFmpeg, egui/wgpu UI.

## Install

Download from [Releases](https://github.com/Deathedit/Passthrify/releases):

- **`Passthrify-<version>-setup.exe`** (recommended): installs for your user into
  `%LOCALAPPDATA%\Programs\Passthrify`, no admin needed. Adds a Start menu entry (desktop icon
  optional) and an entry in **Settings › Apps** to uninstall. Running a newer setup upgrades in
  place. The installer isn't code-signed, so SmartScreen may warn: **More info → Run anyway**.
- **`Passthrify-<version>.zip`**: portable. Unzip anywhere and run `passthrify.exe`.

Both update themselves from inside the app (Settings › Updates). Uninstalling keeps your
settings in `%APPDATA%\Passthrify`; delete that folder too for a clean slate.

## Use

| Action | Default hotkey (global) |
|---|---|
| Save the last N seconds (instant replay) | `Alt+F10` |
| Start / stop recording | `Alt+F9` |
| Screenshot (full-res PNG) | `Alt+F1` |
| Fullscreen | `F11` or double-click the preview, `Esc` to leave |
| Show / hide the sidebar | `Ctrl+B` (while Passthrify has focus) |

Everything else lives in the **sidebar** on the right: Save replay, Record, Screenshot and Open
folder at the top, quick dropdowns for **chroma**, **resolution**, **frame rate** and **codec**, then
the settings sections, with volume and fullscreen at the bottom. The sidebar floats over the video
and never resizes it; hide it with the panel button at its top-right (or `Ctrl+B`), and bring it
back with the button that appears over the video when you move the mouse. Windows under 1180 px wide start with
it hidden.

| Chroma | Captured as | Encoded as | On a 4K X |
|---|---|---|---|
| 4:2:0 | NV12 | HEVC Main / H.264 High / AV1 | every mode, up to 4K144 |
| 4:2:2 | YUY2 | HEVC Rext / H.264 High 4:2:2 (RTX 50-series) | up to 4K30, 1440p60, 1080p144 |
| 4:4:4 | RGB24 | HEVC Rext / H.264 High 4:4:4 | up to 1440p60, 1080p120 |

Resolution and frame-rate lists are tagged with the best chroma each mode offers. If a mode
can't deliver the chosen chroma, Passthrify falls back to what it can (shown as `4:4:4 → 4:2:0`).
AV1 on NVENC is 4:2:0 only. 4:2:2/4:4:4 files need VLC, mpv, MPC-HC or a pro editor;
Windows' built-in player only decodes 4:2:0.

Clips go to `Videos\Passthrify` by default. Settings, hotkeys and devices are in the sidebar's
settings sections and are stored in `%APPDATA%\Passthrify\config\config.toml`. The log is `passthrify.log` in the
same folder. The tray icon offers the same actions plus show/hide.

Close **Elgato 4K Capture Utility** / OBS first: the card can only be opened by one app.

### Frame generation (experimental)

Settings › Display › **Frame generation** adds AI-generated frames between the game's frames in
the preview, using NVIDIA's Video Frame Generation (VFG) effect: a 30 fps game shows at 60, a
60 fps game at 120 (2x), up to 4x. Only the preview changes; recordings, replays and screenshots
keep the real frames.

- Needs an RTX 40/50 GPU and NVIDIA's frame generation runtime. It isn't in the main download
  (about 200 MB); Settings offers **Download runtime**, which installs it into a `vfg` folder
  next to the exe (or `%LOCALAPPDATA%\Passthrify\vfg`). It's NVIDIA software under NVIDIA's
  license terms.
  An NVIDIA Video Effects SDK install works too. The default place is the `vfg` folder next to
  the exe: copy the SDK's `bin` and `features` folders into it (`vfg\bin`, `vfg\features`).
  After that Passthrify looks in `frame_gen_dir` from config.toml, `PASSTHRIFY_VFG_DIR`,
  `NV_VIDEO_EFFECTS_PATH`, `VFXSDK_ROOT` (and their `bin`), `%LOCALAPPDATA%\Passthrify\vfg` and
  `Program Files\NVIDIA Corporation\NVIDIA Video Effects`.
- Repeated captured frames (a 30 fps game in a 60 Hz signal) are skipped, so it interpolates
  between the game's own frames, and the output is paced from their timing.
- The real frame is shown about half a game frame later than without it (8 ms at 60 fps,
  2x), plus the GPU time. Your monitor needs a refresh rate at least as high as the output.
- The status bar shows the output rate (`→ 120 fps`, hover for the GPU time per frame).
- `passthrify --frame-gen-test` checks the SDK without a capture card: it interpolates a
  moving square at 1080p and 4K, prints timings and writes PNGs.

### CLI

```
passthrify --list-devices            # capture devices, all native modes, audio endpoints
passthrify --selftest 30 --out DIR   # headless: capture+encode 30 s, record, save replay + screenshot
passthrify --gpu-test                # no card needed: encode synthetic frames via system memory and
                                     # CUDA frames, compare CPU time per frame
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
.\build.ps1 -Installer       # also dist\Passthrify-<version>-setup.exe (needs Inno Setup 6:
                             #   winget install JRSoftware.InnoSetup)
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
- Each captured frame lands in page-locked memory and is uploaded to the GPU once (CUDA, about
  0.5 ms at 4K). NVENC and frame generation both read that copy, so the encoder does no CPU copy
  of its own: about 0.2 ms of CPU per 4K frame instead of 1.4 ms. The log shows per-frame timings
  every 10 s. `gpu_frames = false` in config.toml goes back to system-memory encoding.
- The preview reads that copy too: its frame buffer is Vulkan memory shared with CUDA, the
  capture thread copies each frame into it on the GPU, and the preview copies it into its
  textures there (0.04 ms on the UI thread instead of a 0.5 ms upload). Frame generation's
  generated frames and 4:4:4 still upload from the CPU.

## Updates

Settings → **Updates** checks GitHub Releases at startup (can be turned off) or on demand.
- **Stable** offers normal releases only. **Pre-release** also offers betas, release
  candidates (tags like `v0.2.0-beta.1`) and the automatic dev builds of every push to `main`
  (`v0.1.1-dev.12`).
- **Install and restart** downloads `Passthrify-<ver>.zip` and verifies it against the release's
  `.sha256`. It then restarts into the new version, keeping your settings and any other files
  in the folder. For a copy installed with the setup it also updates the version shown in
  Settings › Apps.
- Builds check `Deathedit/Passthrify` (`PASSTHRIFY_REPO` in `.cargo/config.toml`; the workflow
  sets it from the repo it runs in, so a fork points at itself). To use another repo, set
  `update_repo = "owner/name"` in the app's config.toml. To check from the
  command line: `passthrify --check-update [--repo owner/name] [--channel prerelease]`.

**Cutting a release:** bump `version` in Cargo.toml, commit, then `git tag v0.2.0` and push the
tag. A version with a suffix (`0.2.0-beta.1`) is published as a pre-release.

**Frame generation runtime:** the optional download is a zip on a release tagged `vfg-runtime`
(not a version, so the updater ignores it). Build it from your own NVIDIA Video Effects SDK
install (NGC, with the Video Frame Generation feature) and publish it once:
`.\scripts\package-vfg-runtime.ps1 -SdkDir <sdk>\bin`, then the `gh release create` line it
prints.

**Dev builds:** every push to `main` publishes a pre-release `v<next>-dev.<commits>`, where `<next>`
is the Cargo.toml version, or its next patch if that version is already tagged, and
`<commits>` is the commit count of `main` (`git rev-list --count HEAD`). Only the newest 5
dev builds are kept.

## Design

- **Logo:** "Lens P" in `assets/logo/` (SVG). build.rs renders `passthrify-mark.svg` into the
  exe's multi-size icon. The app renders it at runtime for the window/taskbar, and
  `passthrify-tray-16.svg` (pixel-snapped) for the tray.
- **Theme:** `src/ui/theme.rs` holds the palette (signal cyan `#5BD3FF`, rec red `#FF4D5E`,
  surfaces `#08090C`–`#1A1E25`), egui visuals, line icons and styled widgets.
- **Fonts:** Inter (UI) and JetBrains Mono (numbers, timers) in `assets/fonts/`, embedded in
  the binary. Both are SIL OFL 1.1; the licenses ship next to the exe.
- `PASSTHRIFY_SETTINGS=capture|audio|replay|hotkeys|display` opens Settings on launch.

## Not yet supported

- HDR (P010) capture/encode. Use SDR output, or let the card tone-map.
- AMD/Intel encoders and older DirectShow-only Elgato cards (HD60 S, HD60 Pro).
- Full zero-copy: Elgato's driver delivers frames in system memory, so one upload is the minimum.
