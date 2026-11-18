//! Copies the FFmpeg runtime DLLs next to the built executable (target/<profile>), so the exe
//! runs when launched directly, not only via `cargo run`.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-changed=build.rs");

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
