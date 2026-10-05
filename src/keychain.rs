//! Read-only lookup of a secret another app stored in the OS keychain. Never prompts.

use zeroize::Zeroizing;

/// Upper bound for one keychain lookup (a hung keyring daemon must not block startup).
pub const LOOKUP_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Reads the keychain secret stored under a service name; injectable so tests never touch the real keychain.
pub type KeychainFn = fn(&'static str) -> Option<Zeroizing<String>>;

/// Secret another app saved under `service` (e.g. `copilot-cli`, `gh:github.com`), bounded by [`LOOKUP_LIMIT`].
pub fn lookup(service: &str) -> Option<Zeroizing<String>> {
    let service = service.to_owned();
    crate::accounts::with_deadline(LOOKUP_LIMIT, move || find(&service)).flatten()
}

#[cfg(target_os = "linux")]
fn find(service: &str) -> Option<Zeroizing<String>> {
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
fn find(service: &str) -> Option<Zeroizing<String>> {
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

/// Secret saved under `service` and `account` (macOS login keychain), bounded by [`LOOKUP_LIMIT`].
#[cfg(target_os = "macos")]
pub fn lookup_account(service: &str, account: &str) -> Option<Zeroizing<String>> {
    if !ITEMS.contains(&(service.to_owned(), account.to_owned())) {
        return None;
    }
    let (service, account) = (service.to_owned(), account.to_owned());
    crate::accounts::with_deadline(LOOKUP_LIMIT, move || {
        security_find(&["-s", &service, "-a", &account])
    })
    .flatten()
}

#[cfg(target_os = "macos")]
fn find(service: &str) -> Option<Zeroizing<String>> {
    if !ITEMS.iter().any(|(s, _)| s == service) {
        return None;
    }
    security_find(&["-s", service])
}

/// (service, account) of every generic password in the login keychain, read once.
/// Listing reads no secrets and never prompts. Without it, scanning home would start
/// one `security` process per folder and hit the scan deadline.
#[cfg(target_os = "macos")]
static ITEMS: std::sync::LazyLock<std::collections::HashSet<(String, String)>> =
    std::sync::LazyLock::new(|| {
        let out = std::process::Command::new("/usr/bin/security")
            .arg("dump-keychain")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        out.map(|o| parse_item_list(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default()
    });

/// Parses `security dump-keychain` output: one block per item, starting at `keychain:`,
/// with `"acct"<blob>="..."` and `"svce"<blob>="..."` attribute lines.
#[cfg(target_os = "macos")]
fn parse_item_list(dump: &str) -> std::collections::HashSet<(String, String)> {
    let attr = |line: &str, key: &str| {
        let value = line.trim().strip_prefix(key)?.strip_prefix("<blob>=\"")?;
        value.strip_suffix('"').map(str::to_owned)
    };
    let mut found = std::collections::HashSet::new();
    let (mut acct, mut svce) = (String::new(), None);
    for line in dump.lines().chain(std::iter::once("keychain: end")) {
        if line.starts_with("keychain:") {
            if let Some(service) = svce.take() {
                found.insert((service, std::mem::take(&mut acct)));
            }
            acct.clear();
        } else if let Some(v) = attr(line, "\"acct\"") {
            acct = v;
        } else if let Some(v) = attr(line, "\"svce\"") {
            svce = Some(v);
        }
    }
    found
}

/// Runs `security find-generic-password <query> -w`. `security` is the tool Claude Code
/// and gh store their items with, so those items' access lists trust it.
#[cfg(target_os = "macos")]
fn security_find(query: &[&str]) -> Option<Zeroizing<String>> {
    let out = std::process::Command::new("/usr/bin/security")
        .arg("find-generic-password")
        .args(query)
        .arg("-w")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let secret = Zeroizing::new(out.stdout);
    if !out.status.success() {
        return None;
    }
    crate::providers::decode_secret(&secret)
}

/// First `bytes` bytes of SHA-256 of `path`, as lowercase hex. CLIs use it to name
/// the keychain item of a non-default config folder.
#[cfg(target_os = "macos")]
pub fn path_hash(path: &std::path::Path, bytes: usize) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(path.to_string_lossy().as_bytes());
    hash[..bytes].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
fn find(_service: &str) -> Option<Zeroizing<String>> {
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

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_item_list_pairs_service_with_account_of_the_same_item() {
        // Shapes seen in real `security dump-keychain` output; no secret values.
        let dump = r#"keychain: "/Users/u/Library/Keychains/login.keychain-db"
version: 512
class: "genp"
attributes:
    0x00000007 <blob>="Claude Code-credentials"
    "acct"<blob>="u"
    "cdat"<timedate>=0x32303236  "2026"
    "svce"<blob>="Claude Code-credentials"
keychain: "/Users/u/Library/Keychains/login.keychain-db"
version: 512
class: "inet"
attributes:
    "acct"<blob>="someone"
    "srvr"<blob>="example.com"
keychain: "/Users/u/Library/Keychains/login.keychain-db"
version: 512
class: "genp"
attributes:
    "acct"<blob>=<NULL>
    "svce"<blob>="gh:github.com"
keychain: "/Users/u/Library/Keychains/login.keychain-db"
version: 512
class: "genp"
attributes:
    "acct"<blob>="orphan"
    "svce"<blob>=<NULL>
keychain: "/Users/u/Library/Keychains/login.keychain-db"
version: 512
class: "genp"
attributes:
    "acct"<blob>="cli|67c0c8bd8543f756"
    "svce"<blob>="Codex Auth""#;
        let mut items: Vec<(String, String)> = parse_item_list(dump).into_iter().collect();
        items.sort();
        let pair = |s: &str, a: &str| (s.to_string(), a.to_string());
        assert_eq!(
            items,
            vec![
                pair("Claude Code-credentials", "u"),
                // The last item has no following `keychain:` line and must still count.
                pair("Codex Auth", "cli|67c0c8bd8543f756"),
                // A <NULL> account must not inherit the previous item's account.
                pair("gh:github.com", ""),
            ]
        );
    }
}
