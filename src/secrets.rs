//! Stream keys in the system's credential store (one per service), so they never land in
//! config.toml. Windows: Credential Manager (Control Panel › Credential Manager › Windows
//! Credentials). Linux: the Secret Service keyring (GNOME Keyring, KWallet, KeePassXC), as
//! service "Passthrify", username "StreamKey/<service>"; check with
//! `secret-tool search service Passthrify`.

use crate::config::StreamService;

fn target(service: StreamService) -> String {
    format!("Passthrify/StreamKey/{service:?}")
}

/// The saved stream key for `service`, if any.
pub fn stream_key(service: StreamService) -> Option<String> {
    read(&target(service))
}

/// Saves (or, when `key` is empty, deletes) the stream key for `service`.
pub fn set_stream_key(service: StreamService, key: &str) -> anyhow::Result<()> {
    write(&target(service), key)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;

    use anyhow::{anyhow, Result};
    use secret_service::blocking::SecretService;
    use secret_service::EncryptionType;

    /// Attributes in the layout the `keyring` crate and libsecret tools expect.
    fn attributes(name: &str) -> HashMap<&str, &str> {
        let user = name.strip_prefix("Passthrify/").unwrap_or(name);
        HashMap::from([("service", "Passthrify"), ("username", user)])
    }

    fn connect() -> Result<SecretService<'static>> {
        SecretService::connect(EncryptionType::Dh).map_err(|e| {
            anyhow!("no system keyring found ({e}); install and start GNOME Keyring, KWallet or KeePassXC")
        })
    }

    pub fn read(name: &str) -> Option<String> {
        let ss = connect().ok()?;
        let found = ss.search_items(attributes(name)).ok()?;
        let item = match found.unlocked.into_iter().next() {
            Some(i) => i,
            None => {
                let i = found.locked.into_iter().next()?;
                i.unlock().ok()?; // the keyring may ask for its password
                i
            }
        };
        let secret = item.get_secret().ok()?;
        String::from_utf8(secret).ok().filter(|k| !k.is_empty())
    }

    pub fn write(name: &str, key: &str) -> Result<()> {
        let key = key.trim();
        let ss = connect()?;
        let attrs = attributes(name);
        if key.is_empty() {
            let found = ss.search_items(attrs)?;
            for item in found.unlocked.iter().chain(&found.locked) {
                item.delete()?;
            }
            return Ok(());
        }
        let collection = ss.get_default_collection()?;
        if collection.is_locked()? {
            collection.unlock()?;
        }
        let user = attrs["username"];
        let label = format!("Passthrify stream key ({})", user.trim_start_matches("StreamKey/"));
        collection.create_item(&label, attrs, key.as_bytes(), true, "text/plain")?;
        Ok(())
    }
}
#[cfg(target_os = "linux")]
use linux::{read, write};

#[cfg(windows)]
use windows::core::{HSTRING, PWSTR};
#[cfg(windows)]
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

#[cfg(windows)]
fn read(name: &str) -> Option<String> {
    unsafe {
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        CredReadW(&HSTRING::from(name), CRED_TYPE_GENERIC, 0, &mut cred).ok()?;
        let c = &*cred;
        let blob = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
        let key = String::from_utf8(blob.to_vec()).ok();
        CredFree(cred as *const _);
        key.filter(|k| !k.is_empty())
    }
}

#[cfg(windows)]
fn write(name: &str, key: &str) -> anyhow::Result<()> {
    let key = key.trim();
    let name = HSTRING::from(name);
    unsafe {
        if key.is_empty() {
            let _ = CredDeleteW(&name, CRED_TYPE_GENERIC, 0);
            return Ok(());
        }
        let mut blob = key.as_bytes().to_vec();
        let cred = CREDENTIALW {
            Flags: CRED_FLAGS(0),
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(name.as_ptr() as *mut u16),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        CredWriteW(&cred, 0)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(target_os = "linux", ignore = "uses the desktop keyring, which may ask for its password")]
    fn saves_reads_and_deletes_a_key() {
        // Its own name, so a real stream key is never touched.
        let name = format!("Passthrify/Test/{}", std::process::id());
        if let Err(e) = write(&name, "  live_abc123  ") {
            // Some CI accounts have no credential store; nothing to test there.
            eprintln!("skipped: no credential store ({e})");
            return;
        }
        assert_eq!(read(&name).as_deref(), Some("live_abc123"));
        write(&name, "live_new").unwrap();
        assert_eq!(read(&name).as_deref(), Some("live_new"));
        write(&name, "").unwrap();
        assert_eq!(read(&name), None);
    }
}
