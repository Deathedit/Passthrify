# Passthrify

Low-latency viewer for Elgato capture cards (4K X / 4K60 S+ / 4K60 Pro mk2) with a
ShadowPlay-style instant replay, manual recording, audio passthrough and screenshots.
Rust, NVENC encoding via FFmpeg, egui/wgpu UI. Windows (Media Foundation + WASAPI capture)
and Linux (V4L2 + PipeWire).

## Install

Download from [Releases](https://github.com/Deathedit/Passthrify/releases):

- **`Passthrify-<version>-setup.exe`** (recommended): installs for your user into
  `%LOCALAPPDATA%\Programs\Passthrify`, no admin needed. Adds a Start menu entry (desktop icon
  optional) and an entry in **Settings › Apps** to uninstall. Running a newer setup upgrades in
  place. The installer isn't code-signed, so SmartScreen may warn: **More info → Run anyway**.
- **`Passthrify-<version>.zip`**: portable. Unzip anywhere and run `passthrify.exe`.

Both update themselves from inside the app (Settings › Updates). Uninstalling keeps your
settings in `%APPDATA%\Passthrify`; delete that folder too for a clean slate.

### Linux

x86_64 with glibc 2.39 or newer (Ubuntu 24.04, Fedora 40, Arch, …), PipeWire, and an NVIDIA
GPU with the proprietary driver. Any V4L2/UVC capture card works; Elgato cards are picked by
default.

- **`Passthrify-<version>-x86_64.AppImage`**: one file. `chmod +x` it and run it. Without FUSE,
  run it with `--appimage-extract-and-run`.
- **`Passthrify-<version>-linux-x86_64.tar.gz`**: unpack anywhere and run
  `Passthrify/passthrify`. For a menu entry, copy `passthrify.desktop` to
  `~/.local/share/applications/` with `Exec=` pointing at the binary.
- **Nix**: `nix run github:Deathedit/Passthrify`, or add the flake to your configuration.

The AppImage and the tarball update themselves; a Nix install updates with your flake.
Settings live in `~/.config/passthrify`.

- **Card access:** your user needs read access to `/dev/video*`, usually through the `video`
  group (`sudo usermod -aG video $USER`, then log in again). A card can be open in one app at
  a time, so close OBS first.
- **Hotkeys:** X11 sessions get the same global hotkeys as Windows. Wayland doesn't let apps
  grab keys, so bind the commands in your compositor instead (Settings › Hotkeys lists them):

  ```kdl
  // niri (~/.config/niri/config.kdl)
  binds {
      Alt+F10 { spawn "passthrify" "--action" "save-replay"; }
      Alt+F9  { spawn "passthrify" "--action" "toggle-record"; }
  }
  ```

  Sway / Hyprland: `bindsym` / `bind` with `exec passthrify --action save-replay`. The actions
  are `save-replay`, `toggle-record`, `screenshot`, `toggle-stream` and `show`. Launching
  Passthrify a second time brings the running window to the front.
- **Title bar:** by default the window manager draws it (none on tiling ones like niri,
  Hyprland or Sway), except on GNOME, where Passthrify draws its own. Settings › Display ›
  Title bar overrides it.
- **Tray, notifications, stream key:** the tray icon needs a StatusNotifierItem host (KDE,
  Waybar, GNOME with the AppIndicator extension). Stream keys are kept in the Secret Service
  keyring (GNOME Keyring, KWallet, KeePassXC); see them with
  `secret-tool search service Passthrify`.

## Use

| Action | Default hotkey (global) |
|---|---|
| Save the last N seconds (instant replay) | `Alt+F10` |
| Start / stop recording | `Alt+F9` |
| Go live / end the stream | `Alt+F8` |
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

### Streaming

**Go live** (under Save replay, `Alt+F8` or the tray) streams the game to Twitch, YouTube or any
RTMP(S) server. Set it up in Settings › **Streaming**: pick the service, paste your stream key
(**Find your key** opens the right dashboard page) and choose the resolution, frame rate and
bitrate. The defaults, 1080p60 at 6000 kbps, suit Twitch; YouTube takes more (about 12000 at
1080p60).

- The key is kept in Windows Credential Manager (`Passthrify/StreamKey/<service>`), never in
  config.toml. Each service has its own.
- The stream is its own H.264 NVENC encode, scaled down on the GPU (keeping the aspect ratio, so
  21:9 stays 21:9) with a 2 s keyframe interval. Recordings and replays keep their own quality.
  Audio is the capture card's.
- If the connection drops, it reconnects by itself (2 s, then up to 30 s between tries). The
  status bar shows `LIVE 00:12:03 · 6.1 Mbps`, or reconnecting, with dropped frames on hover.
- Changing settings while live doesn't end the stream.
- Test without going live: `passthrify --selftest 20 --stream rtmp://127.0.0.1:1935/live/test`
  with a local server such as
  `ffmpeg -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy test.flv`.

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
passthrify --action save-replay      # (Linux) tell the running app to save a replay, etc.
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

### Linux

Needs Rust stable, a C toolchain, clang/libclang, pkg-config and the PipeWire development files
(Debian/Ubuntu: `sudo apt install build-essential clang libclang-dev pkg-config libpipewire-0.3-dev`).
`build.sh` downloads FFmpeg 8.1 (BtbN's LGPL shared build, as on Windows) into
`third_party/ffmpeg-linux`, runs the tests, builds the release binary and packages
`dist/Passthrify/` with FFmpeg in `lib/`:

```
./build.sh                       # test + release build + dist/Passthrify
./build.sh --tarball --appimage  # also the release tarball and AppImage (+ .sha256 each)
./build.sh --skip-tests          # no cargo test
```

`build.sh` finds libclang itself. For plain `cargo build`, set `LIBCLANG_PATH` (for example
`/usr/lib/llvm-18/lib`): `.cargo/config.toml` otherwise points it at the Windows LLVM folder.

On NixOS, `nix develop` gives a shell with everything (FFmpeg from nixpkgs), so
`cargo run` works directly, and `nix build` builds the flake package. `nix develop -c
./build.sh` also works, but its packages only run on NixOS; portable ones come from CI on
Ubuntu 24.04.

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
- Recordings, replays and the stream are written in limited range (16-235). The 4K X sends 4:2:0
  as full range (0-255), and many players ignore the full-range flag and show such files too
  dark, so Passthrify converts it on the GPU (`colorspace_cuda`), or on the CPU without CUDA.
  The preview and screenshots use the full range as captured.
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
- On Linux it downloads the package matching the install: the tarball replaces the files in
  its folder the same way, and an AppImage is replaced by the new AppImage in place. A Nix
  install (in `/nix/store`) is updated with the flake instead.
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
- `PASSTHRIFY_SETTINGS=capture|audio|replay|streaming|hotkeys|display` opens Settings on launch.

## Not yet supported

- HDR (P010) capture/encode. Use SDR output, or let the card tone-map.
- AMD/Intel encoders and older DirectShow-only Elgato cards (HD60 S, HD60 Pro).
- On Linux: frame generation (NVIDIA's SDK is Windows-only) and the 4K60 Pro mk2 (PCIe, no
  Linux driver).
- Full zero-copy: Elgato's driver delivers frames in system memory, so one upload is the minimum.
