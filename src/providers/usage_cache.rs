//! Last good rate-limit windows on disk, so a restart shows them at once and need
//! not call a (possibly rate-limited) endpoint straight away. Holds no secrets.

use crate::model::Window;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_CACHE_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct StoredWindow {
    label: String,
    used_pct: f64,
    resets_at: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    fetched_at: u64,
    windows: Vec<StoredWindow>,
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn time(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Cache file for a provider id; anything but `[A-Za-z0-9_-]` becomes `_`.
pub fn file_for(dir: &Path, provider_id: &str) -> PathBuf {
    let safe: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("usage-{safe}.json"))
}

pub fn load(path: &Path) -> Option<(Vec<Window>, SystemTime)> {
    if std::fs::metadata(path).ok()?.len() > MAX_CACHE_BYTES {
        return None;
    }
    let stored: Stored = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let windows: Vec<Window> = stored
        .windows
        .into_iter()
        .map(|w| Window::new(w.label, w.used_pct, w.resets_at.map(time)))
        .collect();
    (!windows.is_empty()).then(|| (windows, time(stored.fetched_at)))
}

/// Best effort: a failed write only costs the cache, so it is logged, not returned.
pub fn save(path: &Path, windows: &[Window], fetched_at: SystemTime) {
    let stored = Stored {
        fetched_at: secs(fetched_at),
        windows: windows
            .iter()
            .map(|w| StoredWindow {
                label: w.label.clone(),
                used_pct: w.used_pct,
                resets_at: w.resets_at.map(secs),
            })
            .collect(),
    };
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&stored)?)?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        log::warn!("cannot save usage cache {}: {e}", path.display());
    }
}

/// Windows whose reset time has passed are shown as 0 % with no reset time.
pub fn expire(windows: &[Window], now: SystemTime) -> Vec<Window> {
    windows
        .iter()
        .map(|w| match w.resets_at {
            Some(reset) if reset <= now => Window::new(w.label.clone(), 0.0, None),
            _ => w.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_is_sanitized() {
        let dir = Path::new("/c");
        assert_eq!(
            file_for(dir, "claude:personal"),
            dir.join("usage-claude_personal.json")
        );
        assert_eq!(file_for(dir, "a/../b"), dir.join("usage-a____b.json"));
    }

    #[test]
    fn round_trip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/usage-claude.json");
        assert!(load(&path).is_none());
        let windows = vec![
            Window::new("5h", 12.0, Some(time(2_000_000_000))),
            Window::new("7d", 40.0, None),
        ];
        save(&path, &windows, time(1_900_000_000));
        let (loaded, at) = load(&path).unwrap();
        assert_eq!(loaded, windows);
        assert_eq!(at, time(1_900_000_000));
    }

    #[test]
    fn corrupt_or_empty_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load(&path).is_none());
        std::fs::write(&path, r#"{"fetched_at":1,"windows":[]}"#).unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn expire_zeroes_passed_windows() {
        let now = time(1_000);
        let w = expire(
            &[
                Window::new("5h", 50.0, Some(time(999))),
                Window::new("7d", 60.0, Some(time(2_000))),
            ],
            now,
        );
        assert_eq!(w[0], Window::new("5h", 0.0, None));
        assert_eq!(w[1].used_pct, 60.0);
    }
}
