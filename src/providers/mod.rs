//! Provider abstraction. To add a provider: new module here + one line in `registry::all_providers`.

pub mod claude;
pub mod copilot;
pub mod cursor;
pub mod jsonl;
pub mod minimax;
pub mod openai;
pub mod usage_cache;

pub use crate::model::ProviderError;
use crate::model::ProviderSnapshot;
use std::io::Read;
use std::path::Path;
use std::time::Duration;
use zeroize::Zeroizing;

const MAX_SECRET_FILE: u64 = 1024 * 1024;

/// Reads an environment variable; injectable so tests never depend on the real environment.
pub type EnvFn = fn(&str) -> Option<String>;

pub fn real_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Secret bytes from a keychain or database: UTF-8, else UTF-16LE. Trimmed (also of NULs); `None` when empty.
/// UTF-16LE ASCII is also valid UTF-8 (with NULs between letters), so UTF-8 is only accepted without NULs.
pub fn decode_secret(bytes: &[u8]) -> Option<Zeroizing<String>> {
    // Trailing NULs (C-string terminators) are ignored for the UTF-8 reading.
    let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let text = match std::str::from_utf8(&bytes[..end]) {
        Ok(text) if !text.contains('\0') => Zeroizing::new(text.to_string()),
        _ if bytes.len().is_multiple_of(2) => {
            let (pairs, _) = bytes.as_chunks::<2>();
            let units: Vec<u16> = pairs.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
            Zeroizing::new(String::from_utf16(&units).ok()?)
        }
        _ => return None,
    };
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    (!trimmed.is_empty()).then(|| Zeroizing::new(trimmed.to_string()))
}

pub trait Provider: Send {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn poll_interval(&self) -> Duration;
    fn poll(&mut self) -> Result<ProviderSnapshot, ProviderError>;
}

/// Reads a small credential file into memory that is wiped on drop.
pub fn read_secret_file(path: &Path) -> Option<Zeroizing<String>> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_SECRET_FILE {
        return None;
    }
    let mut text = Zeroizing::new(String::new());
    file.take(MAX_SECRET_FILE).read_to_string(&mut text).ok()?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_secret_utf8_utf16_and_garbage() {
        assert_eq!(decode_secret(b"  gho_abc \n").unwrap().as_str(), "gho_abc");
        let utf16: Vec<u8> = "gho_xyz"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode_secret(&utf16).unwrap().as_str(), "gho_xyz");
        assert!(decode_secret(b"   ").is_none());
        assert!(decode_secret(&[0xff, 0xfe, 0xfd]).is_none());
    }

    #[test]
    fn decode_secret_handles_nul_terminated_utf8() {
        assert_eq!(decode_secret(b"gho_abc\0").unwrap().as_str(), "gho_abc");
        assert_eq!(decode_secret(b"gho_ab\0").unwrap().as_str(), "gho_ab");
    }

    #[test]
    fn read_secret_file_reads_small_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("a.json");
        std::fs::write(&small, "{\"k\":1}").unwrap();
        assert_eq!(read_secret_file(&small).unwrap().as_str(), "{\"k\":1}");
        let big = dir.path().join("b.json");
        std::fs::write(&big, vec![b'x'; MAX_SECRET_FILE as usize + 1]).unwrap();
        assert!(read_secret_file(&big).is_none());
        assert!(read_secret_file(&dir.path().join("missing")).is_none());
    }
}
