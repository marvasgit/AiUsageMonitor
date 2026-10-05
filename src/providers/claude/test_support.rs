//! Fixtures shared by the Claude tests.

use std::fs;

pub const USAGE: &str = include_str!("../../../tests/fixtures/claude/usage.json");
pub const CREDS: &str = include_str!("../../../tests/fixtures/claude/credentials.json");
pub const SESSION: &str = include_str!("../../../tests/fixtures/claude/session.jsonl");

pub fn home_with_creds() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    fs::create_dir_all(claude.join("projects/p")).unwrap();
    fs::write(claude.join(".credentials.json"), CREDS).unwrap();
    fs::write(claude.join("projects/p/s.jsonl"), SESSION).unwrap();
    home
}
