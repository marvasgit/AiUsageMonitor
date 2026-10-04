//! Read-only lookup of a secret another app stored in the OS keychain. Never prompts.

use zeroize::Zeroizing;

/// Upper bound for one keychain lookup (a hung keyring daemon must not block startup).
pub const LOOKUP_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Reads the keychain secret stored under a service name; injectable so tests never touch the real keychain.
pub type KeychainFn = fn(&'static str) -> Option<Zeroizing<String>>;

/// Secret another app saved under `service` (e.g. `copilot-cli`, `gh:github.com`), bounded by [`LOOKUP_LIMIT`].
pub fn lookup(service: &'static str) -> Option<Zeroizing<String>> {
    crate::accounts::with_deadline(LOOKUP_LIMIT, move || find(service)).flatten()
}

#[cfg(target_os = "linux")]
fn find(service: &'static str) -> Option<Zeroizing<String>> {
    use dbus_secret_service::{EncryptionType, SecretService};
    let ss = SecretService::connect(EncryptionType::Dh).ok()?;
    let found = ss
        .search_items(std::collections::HashMap::from([("service", service)]))
        .ok()?;
    // Locked items are skipped on purpose: unlocking would show a password prompt.
    let item = found.unlocked.first()?;
    let secret = Zeroizing::new(item.get_secret().ok()?);
    crate::providers::decode_secret(&secret)
}

#[cfg(windows)]
fn find(service: &'static str) -> Option<Zeroizing<String>> {
    use windows_sys::Win32::Security::Credentials::{CredEnumerateW, CredFree, CREDENTIALW};
    let filter: Vec<u16> = format!("{service}*")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut count: u32 = 0;
    let mut creds: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `filter` is a NUL-terminated UTF-16 string; `count` and `creds` are valid out-pointers.
    let ok = unsafe { CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut creds) };
    if ok == 0 || creds.is_null() {
        return None;
    }
    // SAFETY: on success `creds` points to `count` credential pointers that stay valid until CredFree.
    let list = unsafe { std::slice::from_raw_parts(creds, count as usize) };
    let token = list.iter().find_map(|&cred| {
        // SAFETY: each entry is a valid CREDENTIALW owned by the enumeration buffer.
        let cred = unsafe { &*cred };
        if cred.CredentialBlob.is_null() || cred.CredentialBlobSize == 0 {
            return None;
        }
        // SAFETY: `CredentialBlob` points to `CredentialBlobSize` readable bytes.
        let blob = unsafe {
            std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize)
        };
        crate::providers::decode_secret(blob)
    });
    // SAFETY: `creds` was allocated by CredEnumerateW and is freed exactly once.
    unsafe { CredFree(creds as *const core::ffi::c_void) };
    token
}

#[cfg(not(any(target_os = "linux", windows)))]
fn find(_service: &'static str) -> Option<Zeroizing<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_returns_within_deadline() {
        // Real keychain on the dev/CI machine: only checks that the call returns (some or none) without hanging.
        let start = std::time::Instant::now();
        let _ = lookup("copilot-cli");
        assert!(start.elapsed() < LOOKUP_LIMIT + std::time::Duration::from_secs(1));
    }
}
