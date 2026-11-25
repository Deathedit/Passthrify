//! Self-updater backed by GitHub Releases.
//!
//! 1. `check`: list the repo's releases, pick the newest one allowed by the channel
//!    (Stable = normal releases only, Pre-release = also pre-releases) that is newer than us.
//! 2. `download_and_stage`: download `Passthrify-<ver>.zip`, verify it against the release's
//!    `.sha256` asset, extract it to a temp folder.
//! 3. `launch_installer`: run the *new* exe from the temp folder with `--apply-update`; it
//!    waits for this process to exit, copies itself over the install folder and relaunches.
//!    No separate updater binary, and nothing ever overwrites a running exe.
//!
//! Linux ([`InstallKind`]): the release tarball (`Passthrify-<ver>-linux-x86_64.tar.gz`)
//! goes through the same staged flow; an AppImage (`Passthrify-<ver>-x86_64.AppImage`) is
//! downloaded next to itself, renamed over `$APPIMAGE` and relaunched; a Nix install is
//! updated through its flake, not from inside the app.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Repo baked in by the release workflow (`owner/name`); `update_repo` in config overrides it.
const BUILT_IN_REPO: Option<&str> = option_env!("PASSTHRIFY_REPO");

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum UpdateChannel {
    /// Normal releases only.
    #[default]
    #[serde(rename = "stable")]
    Stable,
    /// Also pre-releases (betas, release candidates).
    #[serde(rename = "prerelease")]
    PreRelease,
}

impl UpdateChannel {
    pub fn label(self) -> &'static str {
        match self {
            UpdateChannel::Stable => "Stable",
            UpdateChannel::PreRelease => "Pre-release",
        }
    }
}

/// The app's executable inside an install folder or package.
pub const EXE: &str = if cfg!(windows) { "passthrify.exe" } else { "passthrify" };

/// How this copy was installed, which decides the release asset and how it's replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// Windows: the zip (or the setup, which installs the same files).
    Zip,
    /// Linux: the release tarball, unpacked anywhere writable.
    Tarball,
    /// Linux: the single-file AppImage (`$APPIMAGE`).
    AppImage,
    /// Linux: the Nix flake package (read-only store path).
    Nix,
}

impl InstallKind {
    pub fn current() -> Self {
        Self::detect(std::env::current_exe().ok().as_deref(), std::env::var_os("APPIMAGE").as_deref())
    }

    fn detect(exe: Option<&Path>, appimage: Option<&OsStr>) -> Self {
        if cfg!(windows) {
            InstallKind::Zip
        } else if appimage.is_some_and(|a| !a.is_empty()) {
            InstallKind::AppImage
        } else if exe.is_some_and(|e| e.starts_with("/nix/store")) {
            InstallKind::Nix
        } else {
            InstallKind::Tarball
        }
    }

    /// The release asset for version `ver` (its checksum is the same name + `.sha256`).
    pub fn package_name(self, ver: &str) -> String {
        match self {
            InstallKind::Zip => format!("Passthrify-{ver}.zip"),
            InstallKind::Tarball | InstallKind::Nix => format!("Passthrify-{ver}-linux-x86_64.tar.gz"),
            InstallKind::AppImage => format!("Passthrify-{ver}-x86_64.AppImage"),
        }
    }
}

pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("valid crate version")
}

/// `owner/name` to check, if any.
pub fn repo(config_override: Option<&str>) -> Option<String> {
    config_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(BUILT_IN_REPO.filter(|s| !s.is_empty()))
        .map(|s| s.trim_start_matches("https://github.com/").trim_matches('/').to_string())
}

// --- GitHub API ----------------------------------------------------------------------------

#[derive(Deserialize, Clone, Debug)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// An installable release.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub notes: String,
    pub url: String,
    pub prerelease: bool,
    pub kind: InstallKind,
    pub package_name: String,
    pub package_url: String,
    pub package_size: u64,
    pub sha_url: String,
}

fn version_of(tag: &str) -> Option<Version> {
    Version::parse(tag.trim().trim_start_matches(['v', 'V'])).ok()
}

/// Newest release allowed by `channel` that is newer than `current` and ships this install's
/// package + checksum assets. Releases without them are skipped (e.g. a botched upload).
pub fn pick_release(releases: &[GhRelease], channel: UpdateChannel, current: &Version) -> Option<Release> {
    pick_release_for(releases, channel, current, InstallKind::current())
}

fn pick_release_for(releases: &[GhRelease], channel: UpdateChannel, current: &Version, kind: InstallKind) -> Option<Release> {
    releases
        .iter()
        .filter(|r| !r.draft)
        .filter_map(|r| version_of(&r.tag_name).map(|v| (v, r)))
        .filter(|(v, r)| match channel {
            UpdateChannel::Stable => !r.prerelease && v.pre.is_empty(),
            UpdateChannel::PreRelease => true,
        })
        .filter(|(v, _)| v > current)
        .filter_map(|(v, r)| {
            let package_name = kind.package_name(r.tag_name.trim_start_matches(['v', 'V']));
            let package = r.assets.iter().find(|a| a.name == package_name)?;
            let sha = r.assets.iter().find(|a| a.name == format!("{package_name}.sha256"))?;
            let prerelease = r.prerelease || !v.pre.is_empty();
            Some(Release {
                version: v,
                tag: r.tag_name.clone(),
                notes: r.body.clone().unwrap_or_default(),
                url: r.html_url.clone(),
                prerelease,
                kind,
                package_name,
                package_url: package.browser_download_url.clone(),
                package_size: package.size,
                sha_url: sha.browser_download_url.clone(),
            })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
}

pub fn agent() -> Result<ureq::Agent> {
    let builder = ureq::AgentBuilder::new();
    // Windows: the system's TLS (Schannel). Linux: rustls, so no OpenSSL version to match.
    #[cfg(windows)]
    let builder = builder.tls_connector(Arc::new(native_tls::TlsConnector::new().context("initializing TLS")?));
    Ok(builder
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .user_agent(&format!("Passthrify/{} (+updater)", env!("CARGO_PKG_VERSION")))
        .build())
}

pub fn fetch_releases(repo: &str) -> Result<Vec<GhRelease>> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    match agent()?.get(&url).set("Accept", "application/vnd.github+json").call() {
        Ok(resp) => resp.into_json().context("parsing GitHub releases"),
        Err(ureq::Error::Status(404, _)) => bail!("repository '{repo}' not found (or private)"),
        Err(ureq::Error::Status(403, _)) => bail!("GitHub rate limit reached; try again later"),
        Err(e) => Err(anyhow!("contacting GitHub: {e}")),
    }
}

/// Ok(None) = up to date.
pub fn check(repo: &str, channel: UpdateChannel) -> Result<Option<Release>> {
    let releases = fetch_releases(repo)?;
    Ok(pick_release(&releases, channel, &current_version()))
}

// --- Download + stage ----------------------------------------------------------------------

fn staging_root() -> PathBuf {
    std::env::temp_dir().join("passthrify-update")
}

/// Parses "<64 hex>  filename" (or just the hash).
fn parse_sha256(text: &str) -> Result<String> {
    let hash = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("checksum file is malformed");
    }
    Ok(hash)
}

/// Finds the folder holding the exe: the extraction root or its single subfolder (the release
/// archives contain a top-level `Passthrify/` folder).
fn find_app_dir(root: &Path) -> Result<PathBuf> {
    if root.join(EXE).is_file() {
        return Ok(root.to_path_buf());
    }
    for entry in std::fs::read_dir(root)?.flatten() {
        let p = entry.path();
        if p.is_dir() && p.join(EXE).is_file() {
            return Ok(p);
        }
    }
    bail!("update package doesn't contain {EXE}")
}

/// Downloads `url` to `dest`, checking it against the "<sha256>  name" file at `sha_url`.
/// `size_hint` is used for progress when the server sends no Content-Length.
pub fn download_verified(
    agent: &ureq::Agent,
    url: &str,
    sha_url: &str,
    size_hint: u64,
    dest: &Path,
    progress: impl Fn(f32),
) -> Result<()> {
    let expected = {
        let text = agent.get(sha_url).call().map_err(|e| anyhow!("downloading checksum: {e}"))?.into_string()?;
        parse_sha256(&text)?
    };
    let resp = agent.get(url).call().map_err(|e| anyhow!("downloading: {e}"))?;
    let total = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok()).unwrap_or(size_hint).max(1);
    let mut file = std::fs::File::create(dest)?;
    let mut reader = resp.into_reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf).context("downloading")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        done += n as u64;
        progress((done as f32 / total as f32).min(1.0));
    }
    file.flush()?;
    drop(file);
    if format!("{:x}", hasher.finalize()) != expected {
        let _ = std::fs::remove_file(dest);
        bail!("download is corrupted (checksum mismatch)");
    }
    Ok(())
}

/// Downloads, verifies and extracts `rel`. Returns the folder containing the new exe, or for
/// an AppImage the downloaded file (next to the running one, ready to rename over it).
pub fn download_and_stage(rel: &Release, progress: impl Fn(f32)) -> Result<PathBuf> {
    let agent = agent()?;
    if rel.kind == InstallKind::AppImage {
        let target = appimage_path()?;
        let part = target.with_file_name(format!(".{}.part", rel.package_name));
        if let Err(e) = download_verified(&agent, &rel.package_url, &rel.sha_url, rel.package_size, &part, progress) {
            let _ = std::fs::remove_file(&part);
            return Err(e.context("nothing was installed"));
        }
        return Ok(part);
    }

    let dir = staging_root().join(rel.version.to_string());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let package = dir.join(&rel.package_name);
    if let Err(e) = download_verified(&agent, &rel.package_url, &rel.sha_url, rel.package_size, &package, progress) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e.context("nothing was installed"));
    }

    let extract = dir.join("extracted");
    match rel.kind {
        InstallKind::Zip => {
            let mut archive = zip::ZipArchive::new(std::fs::File::open(&package)?).context("opening update package")?;
            archive.extract(&extract).context("extracting update package")?;
        }
        #[cfg(target_os = "linux")]
        InstallKind::Tarball | InstallKind::Nix => {
            // Keeps the files' modes, so the exe stays executable.
            let gz = flate2::read::GzDecoder::new(std::fs::File::open(&package)?);
            tar::Archive::new(gz).unpack(&extract).context("extracting update package")?;
        }
        other => bail!("can't unpack a {other:?} package here"),
    }
    find_app_dir(&extract)
}

/// The running AppImage file.
fn appimage_path() -> Result<PathBuf> {
    std::env::var_os("APPIMAGE")
        .filter(|a| !a.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("not running from an AppImage"))
}

/// Puts the verified download in place of the AppImage. The running copy keeps its (now
/// unlinked) file, so replacing it underneath is safe; the rename is atomic.
fn install_appimage(downloaded: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(downloaded, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(downloaded, target).with_context(|| format!("replacing {}", target.display()))
}

/// Why self-install isn't possible here, if it isn't.
pub fn install_blocker() -> Option<String> {
    if cfg!(debug_assertions) {
        return Some("Development build: install releases from GitHub instead.".into());
    }
    let folder = match InstallKind::current() {
        InstallKind::Nix => return Some("Installed with Nix: update your flake (nix flake update) instead.".into()),
        InstallKind::AppImage => appimage_path().ok().and_then(|p| p.parent().map(Path::to_path_buf)),
        InstallKind::Zip | InstallKind::Tarball => {
            std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))
        }
    };
    let Some(dir) = folder else {
        return Some("Can't determine the install folder.".into());
    };
    let probe = dir.join(".passthrify-write-test");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            None
        }
        Err(_) => Some(format!("{} isn't writable; update manually or move Passthrify to a user folder.", dir.display())),
    }
}

/// Starts the staged exe as installer (AppImage: puts the new file in place and starts it).
/// The caller must then exit promptly.
pub fn launch_installer(staged: &Path, rel: &Release) -> Result<()> {
    let version = &rel.version;
    if rel.kind == InstallKind::AppImage {
        let target = appimage_path()?;
        if let Err(e) = install_appimage(staged, &target) {
            let _ = std::fs::remove_file(staged);
            return Err(e);
        }
        write_result("ok", &format!("Updated to Passthrify {version}"));
        // The new copy waits for this one to exit; otherwise it would find this instance
        // running (control.rs) and just bring it to the front.
        std::process::Command::new(&target)
            .arg("--wait-pid")
            .arg(std::process::id().to_string())
            .env_remove("APPDIR")
            .env_remove("ARGV0")
            .spawn()
            .context("starting the new version")?;
        return Ok(());
    }
    let install_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow!("no install folder"))?
        .to_path_buf();
    std::process::Command::new(staged.join(EXE))
        .arg("--apply-update")
        .arg("--from")
        .arg(staged)
        .arg("--to")
        .arg(&install_dir)
        .arg("--pid")
        .arg(std::process::id().to_string())
        .arg("--version")
        .arg(version.to_string())
        .current_dir(staged)
        .spawn()
        .context("starting the installer")?;
    Ok(())
}

// --- Installer mode (runs from the staged copy) ---------------------------------------------

fn result_file() -> PathBuf {
    crate::config::Config::data_dir().join("update-result.txt")
}

/// Leaves the outcome for the relaunched app to show ([`take_update_result`]).
fn write_result(status: &str, msg: &str) {
    let _ = std::fs::create_dir_all(crate::config::Config::data_dir());
    let _ = std::fs::write(result_file(), format!("{status}\n{msg}\n"));
}

/// After a restart: Some((ok, message)) describing the last update attempt, once.
pub fn take_update_result() -> Option<(bool, String)> {
    let path = result_file();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let (status, msg) = text.split_once('\n').unwrap_or((text.as_str(), ""));
    Some((status.trim() == "ok", msg.trim().to_string()))
}

/// Waits (up to `timeout`) for process `pid` to end.
#[cfg(target_os = "linux")]
pub fn wait_for_exit(pid: u32, timeout: Duration) {
    let started = std::time::Instant::now();
    while Path::new(&format!("/proc/{pid}")).exists() && started.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(windows)]
pub fn wait_for_exit(pid: u32, timeout: Duration) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(h, timeout.as_millis() as u32);
            let _ = CloseHandle(h);
        }
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<usize> {
    let mut n = 0;
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            n += copy_tree(&src, &dst)?;
            continue;
        }
        // Antivirus scanners or a slow exit can hold files briefly: retry for ~10 s.
        let mut attempt = 0;
        loop {
            match replace_file(&src, &dst) {
                Ok(()) => break,
                Err(e) if attempt < 40 => {
                    attempt += 1;
                    if attempt == 1 {
                        tracing::warn!("copy {} failed ({e}); retrying", dst.display());
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => return Err(e).with_context(|| format!("replacing {}", dst.display())),
            }
        }
        n += 1;
    }
    Ok(n)
}

/// Windows: copies over the file in place (the old process has exited).
#[cfg(windows)]
fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

/// Unix: copies next to it, then renames over it. Atomic, keeps the mode, and works even if
/// something still runs the old binary (writing into it would fail with ETXTBSY).
#[cfg(unix)]
fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dst.with_file_name(format!(".{name}.update-tmp"));
    std::fs::copy(src, &tmp)?;
    std::fs::rename(&tmp, dst).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// If Passthrify was installed with the setup (installer/passthrify.iss, per-user), keeps its
/// Apps & features entry at the version just installed in place. A no-op for zip installs.
#[cfg(not(windows))]
fn set_installed_version(_version: &str) {}

#[cfg(windows)]
fn set_installed_version(version: &str) {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ,
    };
    // `AppId` from installer/passthrify.iss, plus Inno Setup's "_is1" suffix.
    let key_path = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{6BA996A8-2252-4F77-AFFE-17E5D77FE070}_is1");
    let data: Vec<u8> = version.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, key_path, 0, KEY_SET_VALUE, &mut key) != ERROR_SUCCESS {
            return;
        }
        if RegSetValueExW(key, w!("DisplayVersion"), 0, REG_SZ, Some(&data)) != ERROR_SUCCESS {
            tracing::warn!("could not update the installed version in Apps & features");
        }
        let _ = RegCloseKey(key);
    }
}

/// Entry point for `passthrify --apply-update --from DIR --to DIR --pid N --version V`.
pub fn apply_update_main(args: &[String]) -> Result<()> {
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let from = PathBuf::from(arg("--from").ok_or_else(|| anyhow!("--from missing"))?);
    let to = PathBuf::from(arg("--to").ok_or_else(|| anyhow!("--to missing"))?);
    let pid: u32 = arg("--pid").and_then(|p| p.parse().ok()).unwrap_or(0);
    let version = arg("--version").unwrap_or_default();

    if from == to {
        bail!("refusing to install over the staging folder");
    }
    if pid != 0 {
        wait_for_exit(pid, Duration::from_secs(60));
    }
    let result = copy_tree(&from, &to);
    let (status, msg) = match &result {
        Ok(n) => {
            tracing::info!("update {version}: replaced {n} files in {}", to.display());
            set_installed_version(&version);
            ("ok", format!("Updated to Passthrify {version}"))
        }
        Err(e) => {
            tracing::error!("update {version} failed: {e:#}");
            ("error", format!("Update to {version} failed: {e:#}"))
        }
    };
    write_result(status, &msg);

    // Relaunch whatever is in the install folder now (new on success, old on failure).
    std::process::Command::new(to.join(EXE))
        .current_dir(&to)
        .spawn()
        .context("relaunching Passthrify")?;
    result.map(|_| ())
}

// --- Background state for the UI ------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate { checked: String },
    Available(Release),
    Downloading { release: Release, progress: f32 },
    /// Installer launched; the app should close.
    Restarting,
    Error(String),
}

pub struct Updater {
    state: Mutex<UpdateState>,
    repaint: Box<dyn Fn() + Send + Sync>,
}

impl Updater {
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(UpdateState::Idle), repaint: Box::new(repaint) })
    }

    pub fn state(&self) -> UpdateState {
        self.state.lock().clone()
    }

    fn set(&self, s: UpdateState) {
        *self.state.lock() = s;
        (self.repaint)();
    }

    pub fn busy(&self) -> bool {
        matches!(*self.state.lock(), UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Restarting)
    }

    /// Checks in the background (after `delay`).
    pub fn start_check(self: &Arc<Self>, repo: Option<String>, channel: UpdateChannel, delay: Duration) {
        if self.busy() {
            return;
        }
        let Some(repo) = repo else {
            return; // self-built copy without an update source: nothing to check
        };
        self.set(UpdateState::Checking);
        let me = self.clone();
        std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                std::thread::sleep(delay);
                let next = match check(&repo, channel) {
                    Ok(Some(rel)) => {
                        tracing::info!("update available: {} ({:?} channel)", rel.tag, channel);
                        UpdateState::Available(rel)
                    }
                    Ok(None) => UpdateState::UpToDate { checked: chrono::Local::now().format("%H:%M").to_string() },
                    Err(e) => {
                        tracing::warn!("update check failed: {e:#}");
                        UpdateState::Error(format!("{e:#}"))
                    }
                };
                me.set(next);
            })
            .ok();
    }

    /// Downloads, verifies, stages and launches the installer in the background.
    pub fn start_install(self: &Arc<Self>, rel: Release) {
        if self.busy() {
            return;
        }
        self.set(UpdateState::Downloading { release: rel.clone(), progress: 0.0 });
        let me = self.clone();
        std::thread::Builder::new()
            .name("update-install".into())
            .spawn(move || {
                let staged = download_and_stage(&rel, |p| {
                    if let UpdateState::Downloading { progress, .. } = &mut *me.state.lock() {
                        *progress = p;
                    }
                    (me.repaint)();
                });
                let res = staged.and_then(|dir| launch_installer(&dir, &rel));
                match res {
                    Ok(()) => me.set(UpdateState::Restarting),
                    Err(e) => {
                        tracing::error!("update install failed: {e:#}");
                        me.set(UpdateState::Error(format!("{e:#}")));
                    }
                }
            })
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows release picker (these tests run on every platform).
    fn pick_release(releases: &[GhRelease], channel: UpdateChannel, current: &Version) -> Option<Release> {
        pick_release_for(releases, channel, current, InstallKind::Zip)
    }

    fn rel(tag: &str, prerelease: bool, with_assets: bool) -> GhRelease {
        let ver = tag.trim_start_matches('v');
        let assets = if with_assets {
            vec![
                GhAsset { name: format!("Passthrify-{ver}.zip"), browser_download_url: format!("https://x/{ver}.zip"), size: 10 },
                GhAsset {
                    name: format!("Passthrify-{ver}.zip.sha256"),
                    browser_download_url: format!("https://x/{ver}.sha256"),
                    size: 1,
                },
            ]
        } else {
            vec![]
        };
        GhRelease {
            tag_name: tag.into(),
            body: Some(format!("notes {tag}")),
            prerelease,
            draft: false,
            html_url: format!("https://github.com/o/r/releases/tag/{tag}"),
            assets,
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn channels_pick_the_right_release() {
        let releases = [
            rel("v0.3.0-beta.1", true, true),
            rel("v0.2.0", false, true),
            rel("v0.1.0", false, true),
        ];
        let stable = pick_release(&releases, UpdateChannel::Stable, &v("0.1.0")).unwrap();
        assert_eq!(stable.tag, "v0.2.0");
        assert!(!stable.prerelease);
        let pre = pick_release(&releases, UpdateChannel::PreRelease, &v("0.1.0")).unwrap();
        assert_eq!(pre.tag, "v0.3.0-beta.1");
        assert!(pre.prerelease);
        assert_eq!(pre.package_name, "Passthrify-0.3.0-beta.1.zip");
        assert_eq!(pre.sha_url, "https://x/0.3.0-beta.1.sha256");
    }

    #[test]
    fn never_offers_downgrades_or_same_version() {
        let releases = [rel("v0.2.0", false, true), rel("v0.1.0", false, true)];
        assert!(pick_release(&releases, UpdateChannel::Stable, &v("0.2.0")).is_none());
        // On a beta newer than the latest stable: stable channel offers nothing.
        assert!(pick_release(&releases, UpdateChannel::Stable, &v("0.3.0-beta.1")).is_none());
        // ...but the final release of that beta is an upgrade.
        let releases = [rel("v0.3.0", false, true)];
        assert_eq!(pick_release(&releases, UpdateChannel::Stable, &v("0.3.0-beta.1")).unwrap().tag, "v0.3.0");
    }

    #[test]
    fn skips_drafts_mislabelled_and_incomplete_releases() {
        let mut draft = rel("v0.5.0", false, true);
        draft.draft = true;
        let unflagged_beta = rel("v0.4.0-rc.1", false, true); // semver pre-release, not flagged
        let no_assets = rel("v0.3.0", false, false);
        let garbage = rel("nightly", false, true);
        let ok = rel("v0.2.0", false, true);
        let releases = [draft, unflagged_beta, no_assets, garbage, ok];
        assert_eq!(pick_release(&releases, UpdateChannel::Stable, &v("0.1.0")).unwrap().tag, "v0.2.0");
        assert_eq!(pick_release(&releases, UpdateChannel::PreRelease, &v("0.1.0")).unwrap().tag, "v0.4.0-rc.1");
    }

    #[test]
    fn parses_checksums_and_repos() {
        let h = "a".repeat(64);
        assert_eq!(parse_sha256(&format!("{h}  Passthrify-0.1.0.zip\r\n")).unwrap(), h);
        assert!(parse_sha256("nope").is_err());
        assert_eq!(repo(Some("https://github.com/me/Passthrify/")).as_deref(), Some("me/Passthrify"));
        assert_eq!(repo(Some("  ")), repo(None));
    }

    /// Serves `files` (path -> body) over HTTP on localhost for `requests` requests.
    fn serve(files: Vec<(&'static str, Vec<u8>)>, requests: usize) -> String {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                loop {
                    let mut l = String::new();
                    if reader.read_line(&mut l).unwrap() == 0 || l == "\r\n" {
                        break;
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = files.iter().find(|(p, _)| *p == path).map(|(_, b)| b.clone());
                let (status, body) = match body {
                    Some(b) => ("200 OK", b),
                    None => ("404 Not Found", Vec::new()),
                };
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn linux_packages_are_picked_by_install_kind() {
        let mut r = rel("v0.4.0", false, true);
        for name in ["Passthrify-0.4.0-linux-x86_64.tar.gz", "Passthrify-0.4.0-x86_64.AppImage"] {
            r.assets.push(GhAsset { name: name.into(), browser_download_url: format!("https://x/{name}"), size: 5 });
            r.assets.push(GhAsset { name: format!("{name}.sha256"), browser_download_url: format!("https://x/{name}.sha256"), size: 1 });
        }
        let releases = [r];
        let pick = |kind| pick_release_for(&releases, UpdateChannel::Stable, &v("0.3.0"), kind).unwrap();
        assert_eq!(pick(InstallKind::Zip).package_url, "https://x/0.4.0.zip");
        let tar = pick(InstallKind::Tarball);
        assert_eq!(tar.package_name, "Passthrify-0.4.0-linux-x86_64.tar.gz");
        assert_eq!(tar.sha_url, "https://x/Passthrify-0.4.0-linux-x86_64.tar.gz.sha256");
        assert_eq!(pick(InstallKind::AppImage).package_url, "https://x/Passthrify-0.4.0-x86_64.AppImage");
        // A release without the Linux assets isn't offered to Linux installs.
        let windows_only = [rel("v0.5.0", false, true)];
        assert!(pick_release_for(&windows_only, UpdateChannel::Stable, &v("0.3.0"), InstallKind::Tarball).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detects_the_install_kind() {
        let d = |exe: &str, appimage: Option<&str>| InstallKind::detect(Some(Path::new(exe)), appimage.map(OsStr::new));
        assert_eq!(d("/home/me/Passthrify/passthrify", None), InstallKind::Tarball);
        assert_eq!(d("/tmp/.mount_PassthXYZ/usr/bin/passthrify", Some("/home/me/Passthrify.AppImage")), InstallKind::AppImage);
        assert_eq!(d("/nix/store/abc-passthrify-0.1.2/bin/.passthrify-wrapped", None), InstallKind::Nix);
        assert_eq!(d("/home/me/Passthrify/passthrify", Some("")), InstallKind::Tarball);
    }

    fn test_zip() -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            z.add_directory("Passthrify/", opts).unwrap();
            z.start_file(format!("Passthrify/{EXE}"), opts).unwrap();
            z.write_all(b"new exe").unwrap();
            z.start_file("Passthrify/avcodec-62.dll", opts).unwrap();
            z.write_all(&vec![7u8; 100_000]).unwrap();
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    fn release_at(base: &str, ver: &str) -> Release {
        Release {
            version: v(ver),
            tag: format!("v{ver}"),
            notes: String::new(),
            url: String::new(),
            prerelease: false,
            kind: InstallKind::Zip,
            package_name: format!("Passthrify-{ver}.zip"),
            package_url: format!("{base}/pkg.zip"),
            package_size: 0,
            sha_url: format!("{base}/pkg.zip.sha256"),
        }
    }

    #[test]
    fn downloads_verifies_and_extracts() {
        let zip = test_zip();
        let sha = format!("{:x}  Passthrify-9.9.1.zip\n", Sha256::digest(&zip));
        let base = serve(vec![("/pkg.zip.sha256", sha.into_bytes()), ("/pkg.zip", zip)], 2);
        let progress = std::sync::Mutex::new(Vec::new());
        let dir = download_and_stage(&release_at(&base, "9.9.1"), |p| progress.lock().unwrap().push(p)).unwrap();
        assert_eq!(std::fs::read(dir.join(EXE)).unwrap(), b"new exe");
        assert!(dir.join("avcodec-62.dll").is_file());
        assert_eq!(*progress.lock().unwrap().last().unwrap(), 1.0);
        let _ = std::fs::remove_dir_all(staging_root().join("9.9.1"));
    }

    #[test]
    fn rejects_tampered_downloads() {
        let zip = test_zip();
        let wrong = format!("{}  Passthrify-9.9.2.zip\n", "0".repeat(64));
        let base = serve(vec![("/pkg.zip.sha256", wrong.into_bytes()), ("/pkg.zip", zip)], 2);
        let err = download_and_stage(&release_at(&base, "9.9.2"), |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert!(!staging_root().join("9.9.2").exists(), "corrupt download must be deleted");
    }

    /// A release tarball: `Passthrify/passthrify` (executable) and a bundled library.
    #[cfg(target_os = "linux")]
    fn test_tarball() -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut tar = tar::Builder::new(&mut gz);
            let mut add = |path: &str, mode: u32, body: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(mode);
                h.set_cksum();
                tar.append_data(&mut h, path, body).unwrap();
            };
            add("Passthrify/passthrify", 0o755, b"new elf");
            add("Passthrify/lib/libavcodec.so.62", 0o644, &[7u8; 50_000]);
            tar.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tarball_update_keeps_the_exe_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tgz = test_tarball();
        let sha = format!("{:x}  Passthrify-9.9.3-linux-x86_64.tar.gz\n", Sha256::digest(&tgz));
        let base = serve(vec![("/pkg.tgz.sha256", sha.into_bytes()), ("/pkg.tgz", tgz)], 2);
        let mut rel = release_at(&base, "9.9.3");
        rel.kind = InstallKind::Tarball;
        rel.package_name = "Passthrify-9.9.3-linux-x86_64.tar.gz".into();
        rel.package_url = format!("{base}/pkg.tgz");
        rel.sha_url = format!("{base}/pkg.tgz.sha256");
        let dir = download_and_stage(&rel, |_| {}).unwrap();
        let exe = dir.join(EXE);
        assert_eq!(std::fs::read(&exe).unwrap(), b"new elf");
        assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111, 0o111, "exe must stay executable");
        assert!(dir.join("lib/libavcodec.so.62").is_file());

        // Installing over an old copy replaces the file and keeps the mode.
        let install = staging_root().join("9.9.3-install");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join(EXE), b"old elf").unwrap();
        copy_tree(&dir, &install).unwrap();
        assert_eq!(std::fs::read(install.join(EXE)).unwrap(), b"new elf");
        assert_eq!(std::fs::metadata(install.join(EXE)).unwrap().permissions().mode() & 0o111, 0o111);
        let leftovers: Vec<_> = std::fs::read_dir(&install).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("update-tmp")).collect();
        assert!(leftovers.is_empty(), "temp files left behind");
        let _ = std::fs::remove_dir_all(staging_root().join("9.9.3"));
        let _ = std::fs::remove_dir_all(&install);
    }

    #[cfg(unix)]
    #[test]
    fn appimage_is_swapped_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("pt-appimage-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("Passthrify.AppImage");
        std::fs::write(&target, b"old").unwrap();
        let part = base.join(".Passthrify-9.9.4-x86_64.AppImage.part");
        std::fs::write(&part, b"new").unwrap();
        install_appimage(&part, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn copy_tree_and_app_dir() {
        let base = std::env::temp_dir().join(format!("pt-updater-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let pkg = base.join("extracted").join("Passthrify");
        std::fs::create_dir_all(pkg.join("sub")).unwrap();
        std::fs::write(pkg.join(EXE), b"new").unwrap();
        std::fs::write(pkg.join("sub").join("a.txt"), b"x").unwrap();
        let app = find_app_dir(&base.join("extracted")).unwrap();
        assert_eq!(app, pkg);

        let install = base.join("install");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join(EXE), b"old").unwrap();
        assert_eq!(copy_tree(&app, &install).unwrap(), 2);
        assert_eq!(std::fs::read(install.join(EXE)).unwrap(), b"new");
        assert!(install.join("sub").join("a.txt").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }
}
