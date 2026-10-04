//! Discovery of CLI account folders: one config folder per logged-in account.

use crate::config::AccountConfig;
use crate::providers::read_secret_file;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SEPARATORS: &[char] = &['-', '_', '.'];
const DEFAULT_ALIAS: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Empty for the CLI's default folder.
    pub name: String,
    pub dir: PathBuf,
}

impl Account {
    pub fn new(name: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            dir: dir.into(),
        }
    }

    /// Provider id and display name, e.g. ("claude:work", "Claude · work").
    pub fn identity(&self, kind: &str, label: &str) -> (String, String) {
        if self.name.is_empty() {
            (kind.to_string(), label.to_string())
        } else {
            (
                format!("{kind}:{}", self.name),
                format!("{label} · {}", self.name),
            )
        }
    }
}

/// What makes a folder an account for one CLI.
pub struct Rules<'a> {
    pub cred_file: &'a str,
    pub is_valid: fn(&str) -> bool,
    /// Word removed from folder names to derive account names ("claude", "codex").
    pub strip: &'a str,
}

/// Account name from a folder name: ".claude-personal" → "personal", "acme-claude" → "acme".
pub fn derive_name(folder: &str, strip: &str) -> String {
    let base = folder.strip_prefix('.').unwrap_or(folder);
    let Some(start) = base.to_ascii_lowercase().find(&strip.to_ascii_lowercase()) else {
        return base.to_string();
    };
    let left = base[..start].trim_end_matches(SEPARATORS);
    let right = base[start + strip.len()..].trim_start_matches(SEPARATORS);
    let name = [left, right]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if name.is_empty() {
        base.to_string()
    } else {
        name
    }
}

pub fn expand_home(dir: &str, home: &Path) -> PathBuf {
    if dir == "~" {
        home.to_path_buf()
    } else if let Some(rest) = dir.strip_prefix("~/").or_else(|| dir.strip_prefix("~\\")) {
        home.join(rest)
    } else {
        PathBuf::from(dir)
    }
}

/// Runs `work` on a worker thread; `None` if it takes longer than `limit`.
/// Used for home scans, which can block on dead network mounts. A timed-out worker is left to finish on its own.
pub fn with_deadline<T: Send + 'static>(
    limit: std::time::Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(limit).ok()
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn has_valid_credentials(dir: &Path, rules: &Rules) -> bool {
    read_secret_file(&dir.join(rules.cred_file)).is_some_and(|text| (rules.is_valid)(&text))
}

/// Direct subfolders of `home` (symlinks followed), sorted by folder name.
fn home_folders(home: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut folders: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().into_string().ok()?;
            path.is_dir().then_some((name, path))
        })
        .collect();
    folders.sort();
    folders
}

fn push_unique(found: &mut Vec<(PathBuf, Account)>, name: String, dir: PathBuf) {
    let key = canonical(&dir);
    if !found.iter().any(|(existing, _)| *existing == key) {
        found.push((key, Account::new(name, dir)));
    }
}

fn is_hidden(account: &Account, hide: &[String]) -> bool {
    hide.iter()
        .any(|h| *h == account.name || (account.name.is_empty() && h == DEFAULT_ALIAS))
}

fn dedupe_names(accounts: Vec<Account>) -> Vec<Account> {
    let mut used = HashSet::new();
    accounts
        .into_iter()
        .map(|account| {
            let mut name = account.name.clone();
            let mut n = 2;
            while !used.insert(name.clone()) {
                name = format!("{}-{n}", account.name);
                n += 1;
            }
            Account { name, ..account }
        })
        .collect()
}

/// Name of a folder found on disk. The CLI's stock folder (`~/.claude`) is the unnamed
/// default; every other folder is named after itself.
fn folder_name(dir: &Path, stock: &Path, rules: &Rules) -> String {
    if canonical(dir) == canonical(stock) {
        return String::new();
    }
    dir.file_name()
        .and_then(|f| f.to_str())
        .map(|f| derive_name(f, rules.strip))
        .unwrap_or_default()
}

/// Accounts for one CLI: the stock folder, the env-selected folder, any home subfolder
/// with valid credentials, then config entries. Names never depend on which folder the
/// environment selects, so launching from a shell with `CLAUDE_CONFIG_DIR` set changes nothing.
pub fn discover(
    home: &Path,
    default_dir: &Path,
    rules: &Rules,
    extra: &[AccountConfig],
    hide: &[String],
) -> Vec<Account> {
    let stock = home.join(format!(".{}", rules.strip));
    let mut found: Vec<(PathBuf, Account)> = Vec::new();
    for dir in [stock.as_path(), default_dir] {
        if has_valid_credentials(dir, rules) {
            push_unique(
                &mut found,
                folder_name(dir, &stock, rules),
                dir.to_path_buf(),
            );
        }
    }
    for (_, dir) in home_folders(home) {
        if has_valid_credentials(&dir, rules) {
            let name = folder_name(&dir, &stock, rules);
            push_unique(&mut found, name, dir);
        }
    }
    for entry in extra {
        let dir = expand_home(&entry.dir, home);
        let key = canonical(&dir);
        if let Some((_, account)) = found.iter_mut().find(|(existing, _)| *existing == key) {
            account.name = entry.name.clone();
        } else if has_valid_credentials(&dir, rules) {
            found.push((key, Account::new(entry.name.clone(), dir)));
        } else {
            log::warn!(
                "account {:?}: no valid {} in {}",
                entry.name,
                rules.cred_file,
                dir.display()
            );
        }
    }
    // Hide by the names shown on cards, i.e. after collision suffixes are applied.
    let named = dedupe_names(found.into_iter().map(|(_, account)| account).collect());
    named.into_iter().filter(|a| !is_hidden(a, hide)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn valid(text: &str) -> bool {
        text.contains("valid-creds")
    }

    const RULES: Rules<'static> = Rules {
        cred_file: "creds.json",
        is_valid: valid,
        strip: "claude",
    };

    fn account_dir(root: &Path, folder: &str, content: &str) -> PathBuf {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("creds.json"), content).unwrap();
        dir
    }

    fn names(accounts: &[Account]) -> Vec<&str> {
        accounts.iter().map(|a| a.name.as_str()).collect()
    }

    #[test]
    fn derive_name_cases() {
        assert_eq!(derive_name(".claude-personal", "claude"), "personal");
        assert_eq!(derive_name(".claude_work", "claude"), "work");
        assert_eq!(derive_name("acme-claude", "claude"), "acme");
        assert_eq!(derive_name(".claude2", "claude"), "2");
        assert_eq!(derive_name("my-claude-work", "claude"), "my-work");
        assert_eq!(derive_name(".Claude-Home", "claude"), "Home");
        assert_eq!(derive_name(".claude", "claude"), "claude");
        assert_eq!(derive_name("work-acc", "claude"), "work-acc");
    }

    #[test]
    fn identity_for_default_and_named() {
        assert_eq!(
            Account::new("", "/x").identity("claude", "Claude"),
            ("claude".into(), "Claude".into())
        );
        assert_eq!(
            Account::new("work", "/x").identity("openai", "OpenAI"),
            ("openai:work".into(), "OpenAI · work".into())
        );
    }

    #[test]
    fn expand_home_handles_tilde() {
        let home = Path::new("/h");
        assert_eq!(expand_home("~", home), PathBuf::from("/h"));
        assert_eq!(expand_home("~/a/b", home), PathBuf::from("/h/a/b"));
        assert_eq!(expand_home("/abs", home), PathBuf::from("/abs"));
    }

    #[test]
    fn discovers_default_and_any_named_folders() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        account_dir(h, ".claude", "valid-creds");
        account_dir(h, ".claude-personal", "valid-creds");
        account_dir(h, ".claude_work", "valid-creds");
        account_dir(h, "acme-claude", "valid-creds");
        account_dir(h, ".claude-broken", "garbage");
        fs::create_dir_all(h.join("notes")).unwrap();
        fs::write(h.join(".claude-file"), "valid-creds").unwrap();
        let found = discover(h, &h.join(".claude"), &RULES, &[], &[]);
        assert_eq!(names(&found), vec!["", "personal", "work", "acme"]);
        assert_eq!(found[0].dir, h.join(".claude"));
    }

    #[test]
    fn default_dir_override_keeps_home_claude_once() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        account_dir(h, ".claude", "valid-creds");
        let personal = account_dir(h, ".claude-personal", "valid-creds");
        let found = discover(h, &personal, &RULES, &[], &[]);
        assert_eq!(names(&found), vec!["", "personal"]);
        assert_eq!(found[0].dir, h.join(".claude"));
        assert_eq!(found[1].dir, personal);
    }

    #[test]
    fn default_dir_outside_home_is_named_after_its_folder() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let work = account_dir(elsewhere.path(), ".claude-work", "valid-creds");
        let found = discover(home.path(), &work, &RULES, &[], &[]);
        assert_eq!(names(&found), vec!["work"]);
    }

    #[test]
    fn config_extra_added_renamed_and_missing_skipped() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let h = home.path();
        account_dir(h, ".claude", "valid-creds");
        account_dir(h, ".claude-personal", "valid-creds");
        let far = account_dir(elsewhere.path(), "deep/claude", "valid-creds");
        let extra = vec![
            AccountConfig {
                name: "home".into(),
                dir: "~/.claude-personal/".into(),
            },
            AccountConfig {
                name: "far".into(),
                dir: far.to_string_lossy().into_owned(),
            },
            AccountConfig {
                name: "ghost".into(),
                dir: "/does/not/exist".into(),
            },
        ];
        let found = discover(h, &h.join(".claude"), &RULES, &extra, &[]);
        assert_eq!(names(&found), vec!["", "home", "far"]);
    }

    #[test]
    fn hide_by_name_and_default() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        account_dir(h, ".claude", "valid-creds");
        account_dir(h, ".claude-personal", "valid-creds");
        account_dir(h, ".claude-work", "valid-creds");
        let hide = vec!["default".to_string(), "work".to_string()];
        assert_eq!(
            names(&discover(h, &h.join(".claude"), &RULES, &[], &hide)),
            vec!["personal"]
        );
    }

    #[test]
    fn duplicate_names_get_suffix() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let h = home.path();
        account_dir(h, ".claude-work", "valid-creds");
        let other = account_dir(elsewhere.path(), "x", "valid-creds");
        let extra = vec![AccountConfig {
            name: "work".into(),
            dir: other.to_string_lossy().into_owned(),
        }];
        assert_eq!(
            names(&discover(h, &h.join(".claude"), &RULES, &extra, &[])),
            vec!["work", "work-2"]
        );
    }

    #[test]
    fn with_deadline_returns_fast_results_and_gives_up_on_slow_ones() {
        let limit = std::time::Duration::from_millis(200);
        assert_eq!(with_deadline(limit, || 7), Some(7));
        let slow = with_deadline(std::time::Duration::from_millis(20), || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            7
        });
        assert_eq!(slow, None);
    }

    #[test]
    fn hide_matches_suffixed_names_shown_on_cards() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        account_dir(h, ".claude-work", "valid-creds");
        account_dir(h, "claude-work", "valid-creds");
        let hide = vec!["work-2".to_string()];
        assert_eq!(
            names(&discover(h, &h.join(".claude"), &RULES, &[], &hide)),
            vec!["work"]
        );
    }

    #[test]
    fn expand_home_accepts_windows_separator() {
        assert_eq!(
            expand_home("~\\.codex-work", Path::new("/h")),
            Path::new("/h").join(".codex-work")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_same_folder_listed_once() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let personal = account_dir(h, ".claude-personal", "valid-creds");
        std::os::unix::fs::symlink(&personal, h.join(".claude-x")).unwrap();
        assert_eq!(
            names(&discover(h, &h.join(".claude"), &RULES, &[], &[])),
            vec!["personal"]
        );
    }
}
