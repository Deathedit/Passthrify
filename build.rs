//! Windows: embeds the icon and copies the FFmpeg runtime DLLs next to the built executable
//! (target/<profile>), so the exe runs when launched directly, not only via `cargo run`.
//! Linux: sets an `$ORIGIN/lib` rpath for bundled builds (PASSTHRIFY_BUNDLE_LIBS).

use std::{env, fs, path::PathBuf};

#[path = "src/svg_raster.rs"]
mod svg_raster;

const ICON_SVG: &str = "assets/logo/passthrify-mark.svg";

/// Renders the logo into a multi-size .ico and embeds it (plus version info) in the exe.
fn embed_icon() {
    println!("cargo:rerun-if-changed={ICON_SVG}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let svg = match fs::read(ICON_SVG) {
        Ok(s) => s,
        Err(e) => return println!("cargo:warning=icon: can't read {ICON_SVG}: {e}"),
    };
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 20, 24, 32, 40, 48, 64, 128, 256] {
        match svg_raster::rasterize(&svg, size, size) {
            Ok(rgba) => {
                let img = ico::IconImage::from_rgba_data(size, size, rgba);
                match ico::IconDirEntry::encode(&img) {
                    Ok(entry) => dir.add_entry(entry),
                    Err(e) => println!("cargo:warning=icon {size}px: {e}"),
                }
            }
            Err(e) => println!("cargo:warning=icon {size}px: {e}"),
        }
    }
    let ico_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("passthrify.ico");
    if let Err(e) = fs::File::create(&ico_path).and_then(|f| dir.write(f)) {
        return println!("cargo:warning=icon: writing {}: {e}", ico_path.display());
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().unwrap());
    res.set("ProductName", "Passthrify");
    res.set("FileDescription", "Passthrify - capture viewer with instant replay");
    if let Err(e) = res.compile() {
        println!("cargo:warning=icon: embedding resources failed ({e}); exe will use the default icon");
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/svg_raster.rs");
    embed_icon();

    // Linux tarball / AppImage: the binary finds the bundled FFmpeg in lib/ next to it.
    println!("cargo:rerun-if-env-changed=PASSTHRIFY_BUNDLE_LIBS");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        if env::var_os("PASSTHRIFY_BUNDLE_LIBS").is_some() {
            println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/lib");
        }
        // The DLL copy below is for Windows; Linux links against FFMPEG_DIR/lib (or pkg-config).
        return;
    }

    let Ok(ffmpeg_dir) = env::var("FFMPEG_DIR") else {
        println!("cargo:warning=FFMPEG_DIR not set; FFmpeg DLLs will not be copied");
        return;
    };
    let bin = PathBuf::from(&ffmpeg_dir).join("bin");
    println!("cargo:rerun-if-changed={}", bin.display());

    // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out (classic layout) or
    // target/<profile>/build/<pkg>/<hash>/out (newer build-dir layout): find target/<profile>.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let Some(profile_dir) = out_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == profile.as_str()) && p.join("build").is_dir())
    else {
        println!("cargo:warning=could not locate target/{profile} from {}", out_dir.display());
        return;
    };

    let Ok(entries) = fs::read_dir(&bin) else {
        println!("cargo:warning=cannot read {}", bin.display());
        return;
    };
    // (`cargo test`/`cargo run` also put the link search dirs on PATH, so test binaries in the
    // per-unit build dirs find the DLLs without copies.)
    let targets = [profile_dir.to_path_buf()];
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("dll")) {
            for dir in &targets {
                let _ = fs::create_dir_all(dir);
                let dest = dir.join(path.file_name().unwrap());
                let stale = match (fs::metadata(&path), fs::metadata(&dest)) {
                    (Ok(a), Ok(b)) => a.len() != b.len(),
                    _ => true,
                };
                if stale {
                    if let Err(e) = fs::copy(&path, &dest) {
                        println!("cargo:warning=copy {} failed: {e}", dest.display());
                    }
                }
            }
        }
    }
}
