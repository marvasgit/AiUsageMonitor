//! Claude Code's own login: `.credentials.json`, or the macOS keychain item.

use super::agents::{parse_omo_auth, read_omp_account, read_omp_token};
use super::profile::{fetch_profile, Profile};
use crate::providers::read_secret_file;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use zeroize::Zeroizing;

/// Claude Code's credentials for one config folder: `.credentials.json`, or on macOS
/// the login keychain item Claude Code writes instead of that file.
pub fn read_credentials(dir: &Path) -> Option<Zeroizing<String>> {
    read_secret_file(&dir.join(".credentials.json")).or_else(|| keychain_credentials(dir))
}

#[cfg(target_os = "macos")]
fn keychain_credentials(dir: &Path) -> Option<Zeroizing<String>> {
    let home = dirs::home_dir()?;
    crate::keychain::lookup(&keychain_service(dir, &home))
}

#[cfg(not(target_os = "macos"))]
fn keychain_credentials(_dir: &Path) -> Option<Zeroizing<String>> {
    None
}

/// Keychain service Claude Code uses for `dir`: plain for `~/.claude`, otherwise
/// suffixed with the first 8 hex digits of SHA-256 of the folder path
/// (the value of `CLAUDE_CONFIG_DIR`).
#[cfg(target_os = "macos")]
fn keychain_service(dir: &Path, home: &Path) -> String {
    const SERVICE: &str = "Claude Code-credentials";
    if dir == home.join(".claude") {
        return SERVICE.to_string();
    }
    format!("{SERVICE}-{}", crate::keychain::path_hash(dir, 4))
}

#[derive(Deserialize)]
struct CredFile<'a> {
    #[serde(rename = "claudeAiOauth", borrow)]
    oauth: Option<OAuth<'a>>,
}

#[derive(Deserialize)]
struct OAuth<'a> {
    #[serde(rename = "accessToken", borrow)]
    access_token: Option<&'a str>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// Returns (access token, subscription type); the token is copied into wiping memory.
pub fn parse_credentials(text: &str) -> Option<(Zeroizing<String>, Option<String>)> {
    let file: CredFile = serde_json::from_str(text).ok()?;
    let oauth = file.oauth?;
    let token = oauth.access_token.filter(|t| !t.trim().is_empty())?;
    Some((Zeroizing::new(token.to_string()), oauth.subscription_type))
}

/// Where a Claude card gets its OAuth token.
pub(super) enum Creds {
    /// Claude Code config folder (`.credentials.json` or the macOS keychain).
    ClaudeDir(PathBuf),
    /// omp's credential database (`[omp] db`, default `~/.omp/agent/agent.db`).
    Omp(PathBuf),
    /// omo's credential file (`[omo] auth`, default `~/.omo/agent/auth.json`).
    Omo(PathBuf),
}

impl Creds {
    /// Short name shown on the card: "CC" (Claude Code), "omo", "omp".
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::ClaudeDir(_) => "CC",
            Self::Omo(_) => "omo",
            Self::Omp(_) => "omp",
        }
    }

    /// (access token, subscription type).
    pub(super) fn token(&self) -> Option<(Zeroizing<String>, Option<String>)> {
        match self {
            Self::ClaudeDir(dir) => parse_credentials(&read_credentials(dir)?),
            Self::Omp(db) => read_omp_token(db).map(|token| (token, None)),
            Self::Omo(file) => {
                parse_omo_auth(&read_secret_file(file)?, SystemTime::now()).map(|t| (t, None))
            }
        }
    }

    /// This login's profile, and its account uuid: from the profile endpoint, else omp's
    /// saved id. `None` when it cannot be told (e.g. offline).
    pub(super) fn lookup(&self, api_base: &str) -> (Option<String>, Option<Profile>) {
        let profile = self.token().and_then(|(t, _)| fetch_profile(api_base, t));
        let id = profile
            .as_ref()
            .map(|p| p.uuid.clone())
            .or_else(|| match self {
                Self::Omp(db) => read_omp_account(db),
                Self::ClaudeDir(_) | Self::Omo(_) => None,
            });
        (id, profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::claude::test_support::CREDS;

    #[test]
    fn parse_credentials_extracts_token_and_plan() {
        let (token, plan) = parse_credentials(CREDS).unwrap();
        assert_eq!(token.as_str(), "test-access-token");
        assert_eq!(plan.as_deref(), Some("max"));
    }

    #[test]
    fn parse_credentials_rejects_empty_token() {
        assert!(parse_credentials(r#"{"claudeAiOauth":{"accessToken":""}}"#).is_none());
        assert!(parse_credentials(r#"{"claudeAiOauth":{}}"#).is_none());
        assert!(parse_credentials("garbage").is_none());
    }
}
