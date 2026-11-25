//! Remote control of the running app over a Unix socket (Linux): `passthrify --action NAME`
//! sends one action and exits. Wayland has no global hotkeys an app can grab, so this is what
//! a compositor binding runs (niri: `Alt+F10 { spawn "passthrify" "--action" "save-replay"; }`).
//!
//! The socket also keeps a single instance: a second `passthrify` asks the first to show its
//! window and exits.
//!
//! Protocol: one line with an action name; the reply is `ok` or `error: <reason>`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::Sender;

use crate::pipeline::{Action, Command};

/// Action names accepted by `--action`, with what they do (for `--help` and Settings).
pub const ACTIONS: [(&str, &str); 5] = [
    ("save-replay", "save the instant replay"),
    ("toggle-record", "start / stop recording"),
    ("screenshot", "take a screenshot"),
    ("toggle-stream", "go live / end the stream"),
    ("show", "show and focus the window"),
];

enum Request {
    Action(Action),
    Show,
}

fn parse(name: &str) -> Option<Request> {
    Some(match name.trim() {
        "save-replay" => Request::Action(Action::SaveReplay),
        "toggle-record" => Request::Action(Action::ToggleRecord),
        "screenshot" => Request::Action(Action::Screenshot),
        "toggle-stream" => Request::Action(Action::ToggleStream),
        "show" => Request::Show,
        _ => return None,
    })
}

/// `$XDG_RUNTIME_DIR/passthrify.sock` (private to the user), else a per-user name in /tmp.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("passthrify.sock"),
        _ => std::env::temp_dir().join(format!("passthrify-{}.sock", unsafe { libc::getuid() })),
    }
}

/// Sends `action` to the running app. Ok(()) once it confirmed.
pub fn send(action: &str) -> Result<()> {
    if parse(action).is_none() {
        let names: Vec<&str> = ACTIONS.iter().map(|(n, _)| *n).collect();
        bail!("unknown action '{action}' (one of: {})", names.join(", "));
    }
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|_| anyhow!("Passthrify isn't running"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{action}")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).context("no reply from Passthrify")?;
    match reply.trim() {
        "ok" => Ok(()),
        other => bail!("{}", other.strip_prefix("error: ").unwrap_or(other)),
    }
}

static LISTENER: Mutex<Option<UnixListener>> = Mutex::new(None);
/// This instance bound the socket (and so removes it on exit).
static OWNED: AtomicBool = AtomicBool::new(false);

/// Claims the socket for this instance. Returns false when another instance is running (it
/// was asked to show its window); this one should then exit.
pub fn claim() -> Result<bool> {
    let path = socket_path();
    if send("show").is_ok() {
        return Ok(false);
    }
    // Nobody answered: a leftover socket from a crash, or none at all.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    *LISTENER.lock().unwrap() = Some(listener);
    OWNED.store(true, Ordering::Relaxed);
    Ok(true)
}

/// Serves the claimed socket on a background thread: actions go to the pipeline, `show` to
/// `show_window`. Does nothing if [`claim`] wasn't called (or failed).
pub fn serve(cmd: Sender<Command>, show_window: impl Fn() + Send + 'static) {
    let Some(listener) = LISTENER.lock().unwrap().take() else { return };
    let spawned = std::thread::Builder::new().name("control".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            if BufReader::new(&conn).read_line(&mut line).is_err() {
                continue;
            }
            let reply = match parse(&line) {
                Some(Request::Action(a)) => {
                    tracing::info!("control: {}", line.trim());
                    match cmd.send(a.into()) {
                        Ok(()) => "ok".to_string(),
                        Err(_) => "error: Passthrify is shutting down".to_string(),
                    }
                }
                Some(Request::Show) => {
                    show_window();
                    "ok".to_string()
                }
                None => format!("error: unknown action '{}'", line.trim()),
            };
            let _ = writeln!(conn, "{reply}");
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("control socket: {e}");
    }
}

/// Removes the socket on exit (a crash leaves it behind; [`claim`] cleans that up).
pub fn release() {
    if OWNED.swap(false, Ordering::Relaxed) {
        let _ = std::fs::remove_file(socket_path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_action_parses() {
        for (name, _) in ACTIONS {
            assert!(parse(name).is_some(), "{name}");
        }
        assert!(parse("save-replay\n").is_some());
        assert!(parse("reboot").is_none());
    }
}
