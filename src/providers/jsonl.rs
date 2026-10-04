//! Helpers for CLI session logs stored as JSON Lines.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub len: u64,
}

/// Newest `*.{ext}` files under `dir` (recursive), newest first, at most `limit`.
pub fn newest_files(dir: &Path, ext: &str, limit: usize) -> Vec<FileInfo> {
    let mut found = Vec::new();
    collect(dir, ext, 0, &mut found);
    found.sort_by_key(|f| std::cmp::Reverse(f.modified));
    found.truncate(limit);
    found
}

fn collect(dir: &Path, ext: &str, depth: usize, out: &mut Vec<FileInfo>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let path = entry.path();
        if meta.is_dir() {
            collect(&path, ext, depth + 1, out);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push(FileInfo {
                path,
                modified,
                len: meta.len(),
            });
        }
    }
}

/// Last `max_bytes` of a file (lossy UTF-8). When truncated, the partial first line is dropped.
pub fn read_tail(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start == 0 {
        return Ok(text);
    }
    Ok(match text.find('\n') {
        Some(i) => text[i + 1..].to_string(),
        None => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};

    #[test]
    fn newest_files_recurses_and_sorts_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("p1/a.jsonl");
        let b = dir.path().join("p2/deep/b.jsonl");
        let c = dir.path().join("p2/c.txt");
        for f in [&a, &b, &c] {
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, "{}\n").unwrap();
        }
        set_file_mtime(&a, FileTime::from_unix_time(1_000, 0)).unwrap();
        set_file_mtime(&b, FileTime::from_unix_time(2_000, 0)).unwrap();
        set_file_mtime(&c, FileTime::from_unix_time(3_000, 0)).unwrap();
        let found: Vec<PathBuf> = newest_files(dir.path(), "jsonl", 5)
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(found, vec![b, a]);
        assert_eq!(newest_files(dir.path(), "jsonl", 1).len(), 1);
        assert!(newest_files(&dir.path().join("missing"), "jsonl", 5).is_empty());
    }

    #[test]
    fn read_tail_returns_whole_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.jsonl");
        fs::write(&f, "one\ntwo\n").unwrap();
        assert_eq!(read_tail(&f, 1024).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn read_tail_drops_partial_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.jsonl");
        fs::write(&f, "aaaaaaaaaa\nbbb\nccc\n").unwrap();
        assert_eq!(read_tail(&f, 10).unwrap(), "bbb\nccc\n");
    }
}
