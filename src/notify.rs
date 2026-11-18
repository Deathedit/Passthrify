//! Windows toast notifications (shown even while a game has focus).

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
