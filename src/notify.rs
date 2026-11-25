//! Desktop notifications when a clip is saved (shown even while a game has focus).

/// Windows toast notification.
#[cfg(windows)]
pub fn toast(text: &str) {
    use tauri_winrt_notification::{Duration, Toast};
    let res = Toast::new(Toast::POWERSHELL_APP_ID)
        .title("Passthrify")
        .text1(text)
        .sound(None)
        .duration(Duration::Short)
        .show();
    if let Err(e) = res {
        tracing::debug!("toast failed: {e:?}");
    }
}

/// Linux: not wired up yet (Phase 2: freedesktop notifications).
#[cfg(target_os = "linux")]
pub fn toast(text: &str) {
    tracing::info!("notification: {text}");
}
