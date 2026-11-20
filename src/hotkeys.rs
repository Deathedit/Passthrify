//! System-wide hotkeys. The manager must live on the UI (event-loop) thread; key presses are
//! forwarded straight to the pipeline from a listener thread, so they work while the window is
//! hidden or a game has focus.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use crossbeam_channel::Sender;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use parking_lot::Mutex;

use crate::config::Config;
use crate::pipeline::{Action, Command};

pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    registered: Vec<HotKey>,
    map: Arc<Mutex<HashMap<u32, Action>>>,
}

impl Hotkeys {
    pub fn new(cmd: Sender<Command>) -> Result<Self> {
        let manager = GlobalHotKeyManager::new()?;
        let map: Arc<Mutex<HashMap<u32, Action>>> = Arc::default();
        let m = map.clone();
        std::thread::Builder::new().name("hotkeys".into()).spawn(move || {
            for ev in GlobalHotKeyEvent::receiver().iter() {
                if ev.state != HotKeyState::Pressed {
                    continue;
                }
                let action = m.lock().get(&ev.id).copied();
                if let Some(a) = action {
                    if cmd.send(a.into()).is_err() {
                        break;
                    }
                }
            }
        })?;
        Ok(Self { manager, registered: Vec::new(), map })
    }

    /// (Re)registers the hotkeys from `cfg`. Returns human-readable problems, if any.
    pub fn apply(&mut self, cfg: &Config) -> Vec<String> {
        let _ = self.manager.unregister_all(&self.registered);
        self.registered.clear();
        self.map.lock().clear();
        let mut errors = Vec::new();
        for (text, action) in [
            (&cfg.hotkey_save_replay, Action::SaveReplay),
            (&cfg.hotkey_toggle_record, Action::ToggleRecord),
            (&cfg.hotkey_screenshot, Action::Screenshot),
            (&cfg.hotkey_toggle_stream, Action::ToggleStream),
        ] {
            if text.trim().is_empty() {
                continue;
            }
            match HotKey::from_str(text) {
                Ok(hk) => match self.manager.register(hk) {
                    Ok(()) => {
                        self.map.lock().insert(hk.id(), action);
                        self.registered.push(hk);
                    }
                    Err(e) => errors.push(format!("{text}: {e} (in use by another app?)")),
                },
                Err(e) => errors.push(format!("'{text}': {e}")),
            }
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hotkeys_parse() {
        let c = Config::default();
        for s in [&c.hotkey_save_replay, &c.hotkey_toggle_record, &c.hotkey_screenshot] {
            HotKey::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        }
    }
}
