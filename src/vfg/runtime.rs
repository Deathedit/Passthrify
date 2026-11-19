//! Optional download of NVIDIA's frame generation runtime (Video Effects SDK core + the Video
//! Frame Generation feature, about 200 MB), so most users don't carry it in the main package.
//!
//! It's published once as `Passthrify-VFG-runtime-<sdk version>.zip` (+ `.sha256`) on a
//! release tagged [`RELEASE_TAG`] (see scripts/package-vfg-runtime.ps1). The tag isn't a
//! version number, so the app updater never offers it. The zip is installed into a `vfg`
//! folder next to the exe, or in local app data when that folder isn't writable.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use super::sdk;

pub const RELEASE_TAG: &str = "vfg-runtime";
const ASSET_PREFIX: &str = "Passthrify-VFG-runtime";

#[derive(Clone, Debug, PartialEq)]
pub enum DownloadState {
    Idle,
    Downloading(f32),
    Done(PathBuf),
    Error(String),
}

#[derive(Deserialize)]
struct GhRelease {
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

struct Asset {
    name: String,
    url: String,
    sha_url: String,
    size: u64,
}

/// Picks the runtime zip and its checksum from a release's assets.
fn pick_asset(assets: &[GhAsset]) -> Option<Asset> {
    let zip = assets.iter().find(|a| a.name.starts_with(ASSET_PREFIX) && a.name.ends_with(".zip"))?;
    let sha = assets.iter().find(|a| a.name == format!("{}.sha256", zip.name))?;
    Some(Asset { name: zip.name.clone(), url: zip.browser_download_url.clone(), sha_url: sha.browser_download_url.clone(), size: zip.size })
}

/// Where a downloaded runtime goes (and is looked for): `<exe dir>\vfg` when writable,
/// else `%LOCALAPPDATA%\Passthrify\vfg`.
pub fn install_dir() -> PathBuf {
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    if let Some(dir) = exe_dir {
        let probe = dir.join(".passthrify-write-test");
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(probe);
            return dir.join("vfg");
        }
    }
    local_dir()
}

pub fn local_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "Passthrify")
        .map(|d| d.data_local_dir().join("vfg"))
        .unwrap_or_else(|| std::env::temp_dir().join("passthrify-vfg"))
}

/// The folder in `root` (itself or one subfolder) that holds the runtime DLLs.
fn runtime_root(root: &Path) -> Option<PathBuf> {
    let has = |d: &Path| [sdk::CORE_DLL, sdk::IMAGE_DLL, sdk::FEATURE_DLL].iter().all(|f| d.join(f).is_file());
    if has(root) {
        return Some(root.to_path_buf());
    }
    std::fs::read_dir(root).ok()?.flatten().map(|e| e.path()).find(|p| p.is_dir() && has(p))
}

/// Downloads, verifies and installs the runtime from `repo`'s [`RELEASE_TAG`] release.
/// Returns the folder it was installed to.
pub fn download(repo: &str, progress: impl Fn(f32)) -> Result<PathBuf> {
    let agent = crate::updater::agent()?;
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/{RELEASE_TAG}");
    let release: GhRelease = match agent.get(&url).set("Accept", "application/vnd.github+json").call() {
        Ok(resp) => resp.into_json().context("parsing the runtime release")?,
        Err(ureq::Error::Status(404, _)) => bail!("{repo} has no frame generation runtime to download yet"),
        Err(e) => return Err(anyhow!("contacting GitHub: {e}")),
    };
    let asset = pick_asset(&release.assets).ok_or_else(|| anyhow!("the runtime release is missing its zip or checksum"))?;
    install_from(&agent, &asset, &install_dir(), progress)
}

fn install_from(agent: &ureq::Agent, asset: &Asset, target: &Path, progress: impl Fn(f32)) -> Result<PathBuf> {
    // One work folder per install target, so two installs (or parallel tests) never share it.
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        target.hash(&mut h);
        h.finish()
    };
    let work = std::env::temp_dir().join(format!("passthrify-vfg-download-{key:016x}"));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let zip_path = work.join(&asset.name);
    crate::updater::download_verified(agent, &asset.url, &asset.sha_url, asset.size, &zip_path, progress)?;

    // Extract next to the target, then swap it in, so a failure never leaves half a runtime.
    let parent = target.parent().ok_or_else(|| anyhow!("bad install folder {}", target.display()))?;
    std::fs::create_dir_all(parent)?;
    let staging = parent.join("vfg.download");
    let _ = std::fs::remove_dir_all(&staging);
    let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip_path)?).context("opening the runtime zip")?;
    archive.extract(&staging).context("extracting the runtime zip")?;
    let _ = std::fs::remove_dir_all(&work);
    let root = runtime_root(&staging).ok_or_else(|| anyhow!("the runtime zip doesn't contain {}", sdk::FEATURE_DLL))?;
    if target.exists() {
        std::fs::remove_dir_all(target).with_context(|| format!("replacing {}", target.display()))?;
    }
    std::fs::rename(&root, target).with_context(|| format!("installing to {}", target.display()))?;
    let _ = std::fs::remove_dir_all(&staging);
    tracing::info!("frame generation runtime installed to {}", target.display());
    Ok(target.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;

    #[test]
    fn picks_the_runtime_zip_and_checksum() {
        let a = |n: &str| GhAsset { name: n.into(), browser_download_url: format!("https://x/{n}"), size: 7 };
        let assets = [
            a("notes.txt"),
            a("Passthrify-VFG-runtime-1.3.0.zip"),
            a("Passthrify-VFG-runtime-1.3.0.zip.sha256"),
        ];
        let got = pick_asset(&assets).unwrap();
        assert_eq!(got.name, "Passthrify-VFG-runtime-1.3.0.zip");
        assert_eq!(got.sha_url, "https://x/Passthrify-VFG-runtime-1.3.0.zip.sha256");
        assert!(pick_asset(&assets[..2]).is_none(), "no checksum, no download");
    }

    /// Serves `files` over HTTP for `requests` requests; returns the base URL.
    fn serve(files: Vec<(&'static str, Vec<u8>)>, requests: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = files.iter().find(|(p, _)| *p == path).map(|(_, b)| b.clone()).unwrap_or_default();
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        base
    }

    fn runtime_zip() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default();
        for name in [sdk::CORE_DLL, sdk::IMAGE_DLL, sdk::FEATURE_DLL, "NOTICE.txt"] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(b"x").unwrap();
        }
        zip.finish().unwrap();
        out.into_inner()
    }

    #[test]
    fn downloads_verifies_and_installs() {
        let zip = runtime_zip();
        let sha = format!("{:x}  rt.zip\n", Sha256::digest(&zip));
        let base = serve(vec![("/rt.zip.sha256", sha.into_bytes()), ("/rt.zip", zip)], 2);
        let asset = Asset {
            name: "Passthrify-VFG-runtime-test.zip".into(),
            url: format!("{base}/rt.zip"),
            sha_url: format!("{base}/rt.zip.sha256"),
            size: 0,
        };
        let root = std::env::temp_dir().join(format!("pt-vfg-test-{}", std::process::id()));
        let target = root.join("vfg");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("stale.dll"), b"old").unwrap(); // an older runtime is replaced
        let agent = crate::updater::agent().unwrap();
        let got = install_from(&agent, &asset, &target, |_| {}).unwrap();
        assert_eq!(got, target);
        assert!(target.join(sdk::FEATURE_DLL).is_file());
        assert!(!target.join("stale.dll").exists());
        assert!(!root.join("vfg.download").exists(), "staging cleaned up");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_a_tampered_runtime() {
        let zip = runtime_zip();
        let base = serve(vec![("/rt.zip.sha256", format!("{}  rt.zip", "0".repeat(64)).into_bytes()), ("/rt.zip", zip)], 2);
        let asset = Asset { name: "rt.zip".into(), url: format!("{base}/rt.zip"), sha_url: format!("{base}/rt.zip.sha256"), size: 0 };
        let target = std::env::temp_dir().join(format!("pt-vfg-bad-{}", std::process::id())).join("vfg");
        let agent = crate::updater::agent().unwrap();
        let err = install_from(&agent, &asset, &target, |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert!(!target.exists());
    }
}
