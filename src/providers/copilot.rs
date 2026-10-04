//! GitHub Copilot: monthly premium-request / AI-credit quota from `copilot_internal/user`.

use super::{read_secret_file, EnvFn, Provider, ProviderError};
use crate::config::Config;
use crate::format::parse_rfc3339;
use crate::http;
use crate::keychain::{self, KeychainFn};
use crate::model::{ProviderSnapshot, Window};
use crate::paths::Paths;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

pub const API_BASE: &str = "https://api.github.com";

const POLL_INTERVAL: Duration = Duration::from_secs(300);
const ENV_VARS: [&str; 3] = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];
const GITHUB_HOST: &str = "github.com";
const CLASSIC_PAT_PREFIX: &str = "ghp_";
const COPILOT_CLI_SERVICE: &str = "copilot-cli";
const GH_SERVICE: &str = "gh:github.com";

fn non_empty(text: &str) -> Option<Zeroizing<String>> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| Zeroizing::new(trimmed.to_string()))
}

#[derive(Deserialize)]
struct VimEntry<'a> {
    #[serde(borrow)]
    oauth_token: Option<&'a str>,
}

/// copilot.vim/lua `apps.json` / `hosts.json`: `{"github.com:<client>": {"oauth_token": ...}}`.
pub fn parse_vim_token(text: &str) -> Option<Zeroizing<String>> {
    let entries: BTreeMap<String, VimEntry> = serde_json::from_str(text).ok()?;
    entries
        .iter()
        .filter(|(key, _)| key.starts_with(GITHUB_HOST))
        .find_map(|(_, entry)| entry.oauth_token.and_then(non_empty))
}

/// gh CLI `hosts.yml`, `github.com:` block only: the active account's `oauth_token` (direct child),
/// else `users.<user>.oauth_token` for the active `user:`, else the first user's token.
pub fn parse_gh_hosts(text: &str) -> Option<Zeroizing<String>> {
    let mut in_github = false;
    let mut child_indent = None;
    let mut user_indent = None;
    let mut in_users = false;
    let mut current_user: Option<String> = None;
    let mut direct = None;
    let mut active: Option<String> = None;
    let mut per_user: Vec<(String, Zeroizing<String>)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            in_github = trimmed.trim_end_matches(':').trim_matches(['"', '\'']) == GITHUB_HOST;
            child_indent = None;
            in_users = false;
            continue;
        }
        if !in_github {
            continue;
        }
        let (key, value) = trimmed.split_once(':').unwrap_or((trimmed, ""));
        let value = value.trim().trim_matches(['"', '\'']);
        if indent == *child_indent.get_or_insert(indent) {
            in_users = key == "users";
            user_indent = None;
            current_user = None;
            match key {
                "oauth_token" if direct.is_none() => direct = non_empty(value),
                "user" => active = Some(value.to_string()),
                _ => {}
            }
        } else if in_users {
            if indent == *user_indent.get_or_insert(indent) {
                current_user = Some(key.to_string());
            } else if key == "oauth_token" {
                if let (Some(user), Some(token)) = (&current_user, non_empty(value)) {
                    per_user.push((user.clone(), token));
                }
            }
        }
    }
    if direct.is_some() {
        return direct;
    }
    let active_index = active.and_then(|a| per_user.iter().position(|(user, _)| *user == a));
    let index = active_index.unwrap_or(0);
    (index < per_user.len()).then(|| per_user.swap_remove(index).1)
}

/// First available token and the name of its source (env var name, "keychain", "copilot.vim", "gh").
pub fn resolve_token(
    env: EnvFn,
    keychain: KeychainFn,
    vim_dir: &Path,
    gh_dir: &Path,
) -> Option<(Zeroizing<String>, &'static str)> {
    for name in ENV_VARS {
        // Copilot does not accept classic personal access tokens; a general `GITHUB_TOKEN` must not hide a real login.
        if let Some(token) = env(name).as_deref().and_then(non_empty) {
            if !token.starts_with(CLASSIC_PAT_PREFIX) {
                return Some((token, name));
            }
        }
    }
    if let Some(token) = keychain(COPILOT_CLI_SERVICE)
        .as_deref()
        .and_then(|t| non_empty(t))
    {
        return Some((token, "keychain"));
    }
    for file in ["apps.json", "hosts.json"] {
        if let Some(token) = read_secret_file(&vim_dir.join(file)).and_then(|t| parse_vim_token(&t))
        {
            return Some((token, "copilot.vim"));
        }
    }
    if let Some(token) =
        read_secret_file(&gh_dir.join("hosts.yml")).and_then(|t| parse_gh_hosts(&t))
    {
        return Some((token, "gh"));
    }
    // gh's default storage since 2.24: the OS keychain, written by go-keyring.
    let stored = keychain(GH_SERVICE)?;
    Some((decode_go_keyring(&stored)?, "gh keychain"))
}

/// go-keyring may store values as `go-keyring-base64:<base64>`; plain values pass through.
fn decode_go_keyring(stored: &str) -> Option<Zeroizing<String>> {
    use base64::Engine;
    match stored.trim().strip_prefix("go-keyring-base64:") {
        Some(encoded) => {
            let bytes = Zeroizing::new(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()?,
            );
            super::decode_secret(&bytes)
        }
        None => non_empty(stored),
    }
}

#[derive(Deserialize)]
struct UserResponse {
    copilot_plan: Option<String>,
    quota_reset_date: Option<String>,
    token_based_billing: Option<bool>,
    quota_snapshots: Option<Snapshots>,
    monthly_quotas: Option<Counts>,
    limited_user_quotas: Option<Counts>,
}

#[derive(Deserialize)]
struct Snapshots {
    premium_interactions: Option<Snapshot>,
    chat: Option<Snapshot>,
    completions: Option<Snapshot>,
}

#[derive(Deserialize)]
struct Snapshot {
    entitlement: Option<f64>,
    remaining: Option<f64>,
    percent_remaining: Option<f64>,
    unlimited: Option<bool>,
}

impl Snapshot {
    /// Used % unless unlimited, an all-zero placeholder, or (if required) without an entitlement.
    fn used_pct(&self, require_entitlement: bool) -> Option<f64> {
        if self.unlimited == Some(true) {
            return None;
        }
        let entitlement = self.entitlement.unwrap_or(0.0);
        let remaining = self.remaining.unwrap_or(0.0);
        let percent_remaining = self.percent_remaining.unwrap_or(0.0);
        if entitlement == 0.0 && remaining == 0.0 && percent_remaining == 0.0 {
            return None;
        }
        if require_entitlement && entitlement <= 0.0 {
            return None;
        }
        Some(100.0 - percent_remaining)
    }
}

#[derive(Deserialize)]
struct Counts {
    chat: Option<f64>,
    completions: Option<f64>,
}

/// `yyyy-MM-dd` (midnight UTC) or RFC 3339.
fn parse_reset(text: &str) -> Option<SystemTime> {
    parse_rfc3339(text).or_else(|| {
        let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
        Some(SystemTime::from(date.and_hms_opt(0, 0, 0)?.and_utc()))
    })
}

pub fn parse_usage(body: &str) -> Result<(Vec<Window>, Option<String>), ProviderError> {
    let resp: UserResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let reset = resp.quota_reset_date.as_deref().and_then(parse_reset);
    let mut windows = Vec::new();
    if let Some(snaps) = &resp.quota_snapshots {
        let premium_label = if resp.token_based_billing == Some(true) {
            "AI credits"
        } else {
            "Premium"
        };
        let rows = [
            (premium_label, &snaps.premium_interactions, false),
            ("Chat", &snaps.chat, true),
            ("Completions", &snaps.completions, true),
        ];
        for (label, snapshot, require_entitlement) in rows {
            if let Some(used) = snapshot
                .as_ref()
                .and_then(|s| s.used_pct(require_entitlement))
            {
                windows.push(Window::new(label, used, reset));
            }
        }
    } else if let (Some(monthly), Some(left)) = (&resp.monthly_quotas, &resp.limited_user_quotas) {
        let rows = [
            ("Chat", monthly.chat, left.chat),
            ("Completions", monthly.completions, left.completions),
        ];
        for (label, total, remaining) in rows {
            if let (Some(total), Some(remaining)) = (total, remaining) {
                if total > 0.0 {
                    windows.push(Window::new(label, (1.0 - remaining / total) * 100.0, reset));
                }
            }
        }
    }
    if windows.is_empty() {
        return Err(ProviderError::Parse("no limited Copilot quota".into()));
    }
    Ok((windows, resp.copilot_plan))
}

pub struct Copilot {
    vim_dir: PathBuf,
    gh_dir: PathBuf,
    api_base: String,
    env: EnvFn,
    keychain: KeychainFn,
}

impl Copilot {
    pub fn detect(cfg: &Config, paths: &Paths) -> Option<Self> {
        if !cfg.copilot.enabled {
            return None;
        }
        let provider = Self::with_sources(paths, super::real_env, keychain::lookup);
        let (_, source) = provider.token()?;
        log::info!("copilot: token from {source}");
        Some(provider)
    }

    pub fn with_sources(paths: &Paths, env: EnvFn, keychain: KeychainFn) -> Self {
        let gh_dir = env("GH_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| {
            paths
                .config_home
                .join(if cfg!(windows) { "GitHub CLI" } else { "gh" })
        });
        Self {
            vim_dir: paths.local_config_home.join("github-copilot"),
            gh_dir,
            api_base: API_BASE.to_string(),
            env,
            keychain,
        }
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    fn token(&self) -> Option<(Zeroizing<String>, &'static str)> {
        resolve_token(self.env, self.keychain, &self.vim_dir, &self.gh_dir)
    }
}

impl Provider for Copilot {
    fn id(&self) -> &str {
        "copilot"
    }

    fn display_name(&self) -> &str {
        "Copilot"
    }

    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn poll(&mut self) -> Result<ProviderSnapshot, ProviderError> {
        let (token, _) = self.token().ok_or(ProviderError::NoCredentials)?;
        let auth = Zeroizing::new(format!("token {}", token.as_str()));
        let resp = http::get(
            &format!("{}/copilot_internal/user", self.api_base),
            &[
                ("Authorization", auth.as_str()),
                ("Accept", "application/json"),
                ("Editor-Version", "vscode/1.96.2"),
                ("Editor-Plugin-Version", "copilot-chat/0.26.7"),
                ("User-Agent", "GitHubCopilotChat/0.26.7"),
                ("X-Github-Api-Version", "2025-04-01"),
            ],
        )?;
        if resp.status == 404 {
            return Err(ProviderError::Parse(
                "no Copilot subscription for this account".into(),
            ));
        }
        resp.check()?;
        let (windows, plan) = parse_usage(&resp.body)?;
        let mut snapshot = ProviderSnapshot::new(windows);
        snapshot.plan = plan;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::fs;
    use std::time::UNIX_EPOCH;

    const USER: &str = include_str!("../../tests/fixtures/copilot/user.json");
    const LEGACY: &str = include_str!("../../tests/fixtures/copilot/user_legacy.json");

    fn no_env(_: &str) -> Option<String> {
        None
    }
    fn gh_env(name: &str) -> Option<String> {
        (name == "GH_TOKEN").then(|| "env-token".to_string())
    }
    fn no_keychain(_: &'static str) -> Option<Zeroizing<String>> {
        None
    }
    fn keychain_token(service: &'static str) -> Option<Zeroizing<String>> {
        (service == "copilot-cli").then(|| Zeroizing::new("kc-token".to_string()))
    }
    fn gh_keychain_only(service: &'static str) -> Option<Zeroizing<String>> {
        // go-keyring may store the token base64-encoded with this prefix.
        (service == "gh:github.com")
            .then(|| Zeroizing::new("go-keyring-base64:Z2hvX2twX3Rva2Vu".to_string()))
    }

    #[test]
    fn parse_usage_modern_snapshot() {
        let (windows, plan) = parse_usage(USER).unwrap();
        assert_eq!(plan.as_deref(), Some("individual_pro"));
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "Premium");
        assert_eq!(windows[0].used_pct, 76.0);
        let reset = windows[0]
            .resets_at
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(reset, 1_793_491_200); // 2026-11-01T00:00:00Z
    }

    #[test]
    fn token_based_billing_is_labelled_ai_credits() {
        let body = USER.replace(
            "\"token_based_billing\":false",
            "\"token_based_billing\":true",
        );
        assert_eq!(parse_usage(&body).unwrap().0[0].label, "AI credits");
    }

    #[test]
    fn unlimited_and_placeholder_snapshots_skipped() {
        let body = r#"{"quota_snapshots":{"premium_interactions":{"entitlement":0,"remaining":0,"percent_remaining":0,"unlimited":false},"chat":{"unlimited":true}}}"#;
        assert!(matches!(parse_usage(body), Err(ProviderError::Parse(_))));
    }

    #[test]
    fn limited_chat_and_completions_are_shown() {
        let body = r#"{"quota_reset_date":"2026-11-01","quota_snapshots":{"chat":{"entitlement":50,"remaining":40,"percent_remaining":80,"unlimited":false},"completions":{"entitlement":2000,"remaining":500,"percent_remaining":25,"unlimited":false}}}"#;
        let labels: Vec<String> = parse_usage(body)
            .unwrap()
            .0
            .into_iter()
            .map(|w| w.label)
            .collect();
        assert_eq!(labels, vec!["Chat", "Completions"]);
    }

    #[test]
    fn legacy_quota_shape() {
        let (windows, plan) = parse_usage(LEGACY).unwrap();
        assert_eq!(plan.as_deref(), Some("free"));
        assert_eq!(windows[0].label, "Chat");
        assert_eq!(windows[0].used_pct, 80.0);
        assert_eq!(windows[1].label, "Completions");
        assert_eq!(windows[1].used_pct, 25.0);
        assert!(windows[0].resets_at.is_some());
    }

    #[test]
    fn garbage_is_parse_error() {
        assert!(matches!(
            parse_usage("<html>"),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn vim_token_prefers_github_com_entry() {
        let text = r#"{"ghe.corp:Iv1":{"oauth_token":"ghe"},"github.com:Iv1.b507a08c87ecfe98":{"user":"u","oauth_token":"gho_vim"}}"#;
        assert_eq!(parse_vim_token(text).unwrap().as_str(), "gho_vim");
        assert!(parse_vim_token(r#"{"github.com:x":{"oauth_token":""}}"#).is_none());
        assert!(parse_vim_token("nope").is_none());
    }

    #[test]
    fn gh_hosts_reads_github_com_block_only() {
        let text = "ghe.corp:\n    oauth_token: ghe_token\ngithub.com:\n    users:\n        me:\n            oauth_token: gho_user\n    git_protocol: https\n    user: me\n";
        assert_eq!(parse_gh_hosts(text).unwrap().as_str(), "gho_user");
        assert_eq!(
            parse_gh_hosts("github.com:\n    oauth_token: \"gho_q\"\n")
                .unwrap()
                .as_str(),
            "gho_q"
        );
        assert!(parse_gh_hosts("ghe.corp:\n    oauth_token: x\n").is_none());
    }

    #[test]
    fn gh_hosts_prefers_active_account() {
        let both = "github.com:\n    users:\n        alice:\n            oauth_token: gho_alice\n        bob:\n            oauth_token: gho_bob\n    git_protocol: https\n    oauth_token: gho_active\n    user: bob\n";
        assert_eq!(parse_gh_hosts(both).unwrap().as_str(), "gho_active");
        let users_only = "github.com:\n    users:\n        alice:\n            oauth_token: gho_alice\n        bob:\n            oauth_token: gho_bob\n    user: bob\n";
        assert_eq!(parse_gh_hosts(users_only).unwrap().as_str(), "gho_bob");
    }

    fn classic_pat_env(name: &str) -> Option<String> {
        (name == "GITHUB_TOKEN").then(|| "ghp_classic".to_string())
    }

    #[test]
    fn classic_pat_in_env_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let found = resolve_token(classic_pat_env, keychain_token, dir.path(), dir.path());
        assert_eq!(
            found.map(|(t, s)| (t.as_str().to_string(), s)),
            Some(("kc-token".into(), "keychain"))
        );
    }

    #[test]
    fn token_sources_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let vim = dir.path().join("github-copilot");
        let gh = dir.path().join("gh");
        fs::create_dir_all(&vim).unwrap();
        fs::create_dir_all(&gh).unwrap();
        fs::write(
            vim.join("apps.json"),
            r#"{"github.com:a":{"oauth_token":"from-vim"}}"#,
        )
        .unwrap();
        fs::write(
            gh.join("hosts.yml"),
            "github.com:\n    oauth_token: from-gh\n",
        )
        .unwrap();
        let src = |env: EnvFn, kc: KeychainFn| {
            resolve_token(env, kc, &vim, &gh).map(|(t, s)| (t.as_str().to_string(), s))
        };
        assert_eq!(
            src(gh_env, keychain_token),
            Some(("env-token".into(), "GH_TOKEN"))
        );
        assert_eq!(
            src(no_env, keychain_token),
            Some(("kc-token".into(), "keychain"))
        );
        assert_eq!(
            src(no_env, no_keychain),
            Some(("from-vim".into(), "copilot.vim"))
        );
        fs::remove_file(vim.join("apps.json")).unwrap();
        assert_eq!(src(no_env, no_keychain), Some(("from-gh".into(), "gh")));
        assert_eq!(
            src(no_env, gh_keychain_only),
            Some(("from-gh".into(), "gh"))
        );
        fs::remove_file(gh.join("hosts.yml")).unwrap();
        assert_eq!(
            src(no_env, gh_keychain_only),
            Some(("gho_kp_token".into(), "gh keychain"))
        );
        assert_eq!(src(no_env, no_keychain), None);
    }

    #[test]
    fn poll_sends_token_and_editor_headers() {
        let home = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/copilot_internal/user")
                .header("authorization", "token env-token")
                .header("editor-version", "vscode/1.96.2");
            then.status(200).body(USER);
        });
        let mut p = Copilot::with_sources(&Paths::for_home(home.path()), gh_env, no_keychain)
            .with_api_base(server.base_url());
        let snap = p.poll().unwrap();
        m.assert();
        assert_eq!(snap.plan.as_deref(), Some("individual_pro"));
        assert_eq!((p.id(), p.display_name()), ("copilot", "Copilot"));
    }

    #[test]
    fn poll_maps_status_codes() {
        let home = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut unauthorized = server.mock(|when, then| {
            when.method(GET).path("/copilot_internal/user");
            then.status(401);
        });
        let mut p = Copilot::with_sources(&Paths::for_home(home.path()), gh_env, no_keychain)
            .with_api_base(server.base_url());
        assert_eq!(p.poll(), Err(ProviderError::Auth));
        unauthorized.delete();
        server.mock(|when, then| {
            when.method(GET).path("/copilot_internal/user");
            then.status(404);
        });
        assert!(matches!(p.poll(), Err(ProviderError::Parse(_))));
        let mut none = Copilot::with_sources(&Paths::for_home(home.path()), no_env, no_keychain);
        assert_eq!(none.poll(), Err(ProviderError::NoCredentials));
    }
}
