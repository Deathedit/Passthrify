//! Opening folders and links with the desktop's default handler.

use std::ffi::OsStr;

/// Opens a folder in the file manager (created first if missing).
pub fn open_folder(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(windows)]
    spawn("explorer", [dir.as_os_str()]);
    #[cfg(not(windows))]
    spawn("xdg-open", [dir.as_os_str()]);
}

/// Opens a web link in the default browser.
pub fn open_url(url: &str) {
    #[cfg(windows)]
    spawn("rundll32", [OsStr::new("url.dll,FileProtocolHandler"), OsStr::new(url)]);
    #[cfg(not(windows))]
    spawn("xdg-open", [OsStr::new(url)]);
}

fn spawn<'a>(program: &str, args: impl IntoIterator<Item = &'a OsStr>) {
    if let Err(e) = std::process::Command::new(program).args(args).spawn() {
        tracing::warn!("couldn't run {program}: {e}");
    }
}
