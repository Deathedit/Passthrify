//! Stream keys in Windows Credential Manager (one per service), so they never land in
//! config.toml. Visible under Control Panel › Credential Manager › Windows Credentials.

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

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
    fn saves_reads_and_deletes_a_key() {
        // Its own name, so a real stream key is never touched.
        let name = format!("Passthrify/Test/{}", std::process::id());
        if let Err(e) = write(&name, "  live_abc123  ") {
            // Some CI accounts have no credential store; nothing to test there.
            eprintln!("skipped: Credential Manager unavailable ({e})");
            return;
        }
        assert_eq!(read(&name).as_deref(), Some("live_abc123"));
        write(&name, "live_new").unwrap();
        assert_eq!(read(&name).as_deref(), Some("live_new"));
        write(&name, "").unwrap();
        assert_eq!(read(&name), None);
    }
}
