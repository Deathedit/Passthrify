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

/// Linux: a freedesktop notification (org.freedesktop.Notifications), carrying the app icon
/// itself so it shows without an installed icon theme entry.
#[cfg(target_os = "linux")]
pub fn toast(text: &str) {
    tracing::info!("notification: {text}");
    if let Err(e) = linux::notify(text) {
        tracing::debug!("notification failed: {e}");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::OnceLock;

    use zbus::blocking::Connection;
    use zbus::zvariant::{Structure, Value};

    /// Replaces the previous notification instead of stacking one per clip.
    static LAST_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    pub fn notify(text: &str) -> zbus::Result<()> {
        static CONN: OnceLock<Option<Connection>> = OnceLock::new();
        let conn = CONN.get_or_init(|| Connection::session().ok()).as_ref().ok_or(zbus::Error::Unsupported)?;
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("desktop-entry", Value::from("passthrify"));
        hints.insert("urgency", Value::U8(1));
        hints.insert("image-data", icon());
        let replaces = LAST_ID.load(std::sync::atomic::Ordering::Relaxed);
        let reply = conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &("Passthrify", replaces, "passthrify", "Passthrify", text, Vec::<&str>::new(), hints, 4000i32),
        )?;
        if let Ok(id) = reply.body().deserialize::<u32>() {
            LAST_ID.store(id, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    /// `(iiibiiay)`: width, height, rowstride, has alpha, bits per sample, channels, RGBA.
    fn icon() -> Value<'static> {
        const SIZE: u32 = 64;
        static RGBA: OnceLock<Vec<u8>> = OnceLock::new();
        let rgba = RGBA.get_or_init(|| crate::tray::app_icon_rgba(SIZE)).clone();
        Value::from(Structure::from((SIZE as i32, SIZE as i32, (SIZE * 4) as i32, true, 8i32, 4i32, rgba)))
    }
}
