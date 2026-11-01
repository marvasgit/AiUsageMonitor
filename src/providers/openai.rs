//! OpenAI ChatGPT plan via Codex CLI: live usage endpoint + local session logs.

mod agents;

use super::jsonl;
use super::{read_secret_file, Provider, ProviderError};
use crate::accounts::Account;
use crate::backoff::Backoff;
use crate::config::Config;
use crate::format::{from_unix_secs, parse_rfc3339, window_label_from_minutes};
use crate::http;
use crate::model::{ProviderSnapshot, Window};
use agents::{Credentials, Source};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

/// Codex CLI's login for one Codex home: `auth.json`, or on macOS the `Codex Auth`
/// keychain item (account `cli|<first 16 hex of SHA-256 of the folder path>`) that
/// Codex writes when `cli_auth_credentials_store` is `keyring` or `auto`.
pub fn read_auth(dir: &Path) -> Option<Zeroizing<String>> {
    read_secret_file(&dir.join("auth.json")).or_else(|| keychain_auth(dir))
}

#[cfg(target_os = "macos")]
fn keychain_auth(dir: &Path) -> Option<Zeroizing<String>> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let account = format!("cli|{}", crate::keychain::path_hash(&dir, 8));
    crate::keychain::lookup_account("Codex Auth", &account)
}

#[cfg(not(target_os = "macos"))]
fn keychain_auth(_dir: &Path) -> Option<Zeroizing<String>> {
    None
}

pub const API_BASE: &str = "https://chatgpt.com";

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const LOCAL_FILES: usize = 5;
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Deserialize)]
struct AuthFile<'a> {
    #[serde(borrow)]
    tokens: Option<AuthTokens<'a>>,
}

#[derive(Deserialize)]
struct AuthTokens<'a> {
    #[serde(borrow)]
    access_token: Option<&'a str>,
    account_id: Option<String>,
}

pub fn parse_auth(text: &str) -> Option<(Zeroizing<String>, Option<String>)> {
    let file: AuthFile = serde_json::from_str(text).ok()?;
    let tokens = file.tokens?;
    let token = tokens.access_token.filter(|t| !t.trim().is_empty())?;
    Some((Zeroizing::new(token.to_string()), tokens.account_id))
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveLimits {
    pub windows: Vec<Window>,
    pub plan: Option<String>,
}

#[derive(Deserialize)]
struct LiveResponse {
    plan_type: Option<String>,
    rate_limit: Option<LiveRateLimit>,
}

#[derive(Deserialize)]
struct LiveRateLimit {
    primary_window: Option<LiveWindow>,
    secondary_window: Option<LiveWindow>,
}

#[derive(Deserialize)]
struct LiveWindow {
    used_percent: f64,
    limit_window_seconds: u64,
    reset_at: Option<i64>,
}

impl LiveWindow {
    fn to_window(&self) -> Window {
        Window::new(
            window_label_from_minutes(self.limit_window_seconds / 60),
            self.used_percent,
            self.reset_at.and_then(from_unix_secs),
        )
    }
}

pub fn parse_live(body: &str) -> Result<LiveLimits, ProviderError> {
    let resp: LiveResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let rl = resp
        .rate_limit
        .ok_or_else(|| ProviderError::Parse("no rate_limit in response".into()))?;
    let windows: Vec<Window> = [rl.primary_window, rl.secondary_window]
        .iter()
        .flatten()
        .map(LiveWindow::to_window)
        .collect();
    if windows.is_empty() {
        return Err(ProviderError::Parse(
            "no rate-limit windows in response".into(),
        ));
    }
    Ok(LiveLimits {
        windows,
        plan: resp.plan_type,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalLimits {
    pub windows: Vec<Window>,
    pub plan: Option<String>,
    pub as_of: SystemTime,
}

#[derive(Deserialize)]
struct LocalLine {
    timestamp: Option<String>,
    payload: Option<LocalPayload>,
}

#[derive(Deserialize)]
struct LocalPayload {
    rate_limits: Option<LocalRateLimits>,
}

#[derive(Deserialize)]
struct LocalRateLimits {
    primary: Option<LocalWindow>,
    secondary: Option<LocalWindow>,
    plan_type: Option<String>,
}

#[derive(Deserialize)]
struct LocalWindow {
    used_percent: f64,
    window_minutes: u64,
    resets_at: Option<i64>,
    resets_in_seconds: Option<u64>,
}

impl LocalWindow {
    fn to_window(&self, as_of: SystemTime) -> Window {
        let reset = self.resets_at.and_then(from_unix_secs).or_else(|| {
            self.resets_in_seconds
                .map(|s| as_of + Duration::from_secs(s))
        });
        Window::new(
            window_label_from_minutes(self.window_minutes),
            self.used_percent,
            reset,
        )
    }
}

/// Rate limits from one Codex log line; `None` unless it carries a non-null primary window.
pub fn parse_local_line(line: &str) -> Option<LocalLimits> {
    if !line.contains("\"rate_limits\"") {
        return None;
    }
    let parsed: LocalLine = serde_json::from_str(line).ok()?;
    let rl = parsed.payload?.rate_limits?;
    let primary = rl.primary?;
    let as_of = parsed.timestamp.as_deref().and_then(parse_rfc3339)?;
    let mut windows = vec![primary.to_window(as_of)];
    windows.extend(rl.secondary.map(|w| w.to_window(as_of)));
    Some(LocalLimits {
        windows,
        plan: rl.plan_type,
        as_of,
    })
}

/// Latest recorded limits from the newest few Codex session logs.
pub fn find_local(codex_home: &Path) -> Option<LocalLimits> {
    jsonl::newest_files(&codex_home.join("sessions"), "jsonl", LOCAL_FILES)
        .iter()
        .find_map(|f| {
            let text = jsonl::read_tail(&f.path, TAIL_BYTES).ok()?;
            text.lines().rev().find_map(parse_local_line)
        })
}

pub fn merge(
    live: Option<Result<LiveLimits, ProviderError>>,
    local: Option<LocalLimits>,
    now: SystemTime,
) -> Result<ProviderSnapshot, ProviderError> {
    let live_err = match live {
        Some(Ok(l)) => {
            let mut snap = ProviderSnapshot::new(l.windows);
            snap.plan = l.plan;
            return Ok(snap);
        }
        Some(Err(e)) => Some(e),
        None => None,
    };
    let Some(local) = local else {
        return Err(live_err.unwrap_or_else(|| {
            ProviderError::Parse("no rate limits recorded by Codex yet".into())
        }));
    };
    if let Some(e) = &live_err {
        log::warn!("openai: live usage failed ({e}); showing local Codex data");
    }
    let windows = local
        .windows
        .into_iter()
        .map(|w| match w.resets_at {
            Some(reset) if reset <= now => Window::new(w.label, 0.0, None),
            _ => w,
        })
        .collect();
    let mut snap = ProviderSnapshot::new(windows);
    snap.plan = local.plan;
    let as_of: chrono::DateTime<chrono::Local> = local.as_of.into();
    snap.note = Some(format!("as of {} (local)", as_of.format("%H:%M")));
    Ok(snap)
}

pub struct OpenAi {
    id: String,
    name: String,
    codex_home: Option<PathBuf>,
    sources: Vec<Source>,
    api_base: String,
    live_poll: bool,
    backoff: Backoff,
}

impl OpenAi {
    pub fn detect(cfg: &Config, account: &Account) -> Option<Self> {
        if !cfg.openai.enabled {
            return None;
        }
        let auth = read_auth(&account.dir)?;
        let (_, account_id) = parse_auth(&auth)?;
        let (id, name) = account.identity("openai", "OpenAI");
        Some(Self {
            id,
            name,
            codex_home: Some(account.dir.clone()),
            sources: vec![Source {
                credentials: Credentials::Codex(account.dir.clone()),
                account: account_id,
            }],
            api_base: API_BASE.to_string(),
            live_poll: cfg.openai.live_poll,
            backoff: Backoff::new(POLL_INTERVAL),
        })
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }
}

/// Merge agent tokens into the card for their ChatGPT account, keeping other accounts separate.
pub fn merge_agent_logins(
    cfg: &Config,
    paths: &crate::paths::Paths,
    mut cards: Vec<OpenAi>,
) -> Vec<OpenAi> {
    if !cfg.openai.enabled {
        return cards;
    }
    for (enabled, name, credentials) in [
        (
            cfg.omo.enabled,
            "omo",
            Credentials::Omo(crate::accounts::expand_home(&cfg.omo.auth, &paths.home)),
        ),
        (
            cfg.omp.enabled,
            "omp",
            Credentials::Omp(crate::accounts::expand_home(&cfg.omp.db, &paths.home)),
        ),
    ] {
        if !enabled {
            continue;
        }
        for (_, account) in credentials.logins() {
            let same = account.as_ref().and_then(|id| {
                cards
                    .iter()
                    .position(|card| card.sources.iter().any(|s| s.account.as_ref() == Some(id)))
            });
            let source = Source {
                credentials: credentials.clone(),
                account,
            };
            if let Some(i) = same {
                cards[i].sources.push(source);
            } else {
                let alias = if cards.iter().any(|c| c.id == "openai") {
                    let mut alias = name.to_owned();
                    let mut suffix = 2;
                    while cards.iter().any(|c| c.id == format!("openai:{alias}")) {
                        alias = format!("{name}-{suffix}");
                        suffix += 1;
                    }
                    alias
                } else {
                    String::new()
                };
                let (id, name) = Account::new(alias, "").identity("openai", "OpenAI");
                cards.push(OpenAi {
                    id,
                    name,
                    codex_home: None,
                    sources: vec![source],
                    api_base: API_BASE.to_owned(),
                    live_poll: cfg.openai.live_poll,
                    backoff: Backoff::new(POLL_INTERVAL),
                });
            }
        }
    }
    cards
}

fn poll_live(
    token: &str,
    account: Option<&str>,
    api_base: &str,
) -> Result<LiveLimits, ProviderError> {
    let bearer = Zeroizing::new(format!("Bearer {token}"));
    let mut headers = vec![
        ("Authorization", bearer.as_str()),
        ("Accept", "application/json"),
    ];
    if let Some(account) = account {
        headers.push(("ChatGPT-Account-Id", account));
    }
    let resp = http::get(&format!("{api_base}/backend-api/wham/usage"), &headers)?;
    resp.check()?;
    parse_live(&resp.body)
}

impl Provider for OpenAi {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn poll(&mut self) -> Result<ProviderSnapshot, ProviderError> {
        // Local data stands in while the live call fails, so the poll still succeeds:
        // the live endpoint needs its own backoff.
        let (sources, base) = (&self.sources, &self.api_base);
        let backoff = &mut self.backoff;
        let live = self.live_poll.then(|| {
            backoff.call(|| {
                let mut error = ProviderError::NoCredentials;
                let mut tried: Vec<Zeroizing<String>> = Vec::new();
                for (token, account) in sources.iter().flat_map(Source::logins) {
                    if tried
                        .iter()
                        .any(|previous| previous.as_str() == token.as_str())
                    {
                        continue;
                    }
                    let result = poll_live(&token, account.as_deref(), base);
                    tried.push(token);
                    match result {
                        Err(e @ (ProviderError::Auth | ProviderError::NoCredentials)) => error = e,
                        result => return result,
                    }
                }
                Err(error)
            })
        });
        let local = match live {
            Some(Ok(_)) => None,
            _ => self.codex_home.as_deref().and_then(find_local),
        };
        merge(live, local, SystemTime::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use httpmock::prelude::*;
    use std::fs;
    use std::time::UNIX_EPOCH;

    const LIVE: &str = include_str!("../../tests/fixtures/openai/wham_usage.json");
    const AUTH: &str = include_str!("../../tests/fixtures/openai/auth.json");
    const SESSION: &str = include_str!("../../tests/fixtures/openai/session.jsonl");

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn local_33() -> Option<LocalLimits> {
        parse_local_line(SESSION.lines().nth(2).unwrap())
    }

    #[test]
    fn parse_auth_reads_token_and_account() {
        let (token, account) = parse_auth(AUTH).unwrap();
        assert_eq!(token.as_str(), "test-access");
        assert_eq!(account.as_deref(), Some("acct-test"));
    }

    #[test]
    fn parse_auth_requires_access_token() {
        assert!(parse_auth(r#"{"tokens":{"access_token":""}}"#).is_none());
        assert!(parse_auth(r#"{"OPENAI_API_KEY":"sk-x","tokens":null}"#).is_none());
        assert!(parse_auth("nope").is_none());
    }

    #[test]
    fn parse_live_maps_windows_and_plan() {
        let live = parse_live(LIVE).unwrap();
        assert_eq!(live.plan.as_deref(), Some("plus"));
        assert_eq!(
            live.windows,
            vec![
                Window::new("5h", 31.0, Some(at(1_791_044_262))),
                Window::new("week", 16.0, Some(at(1_791_603_826))),
            ]
        );
    }

    #[test]
    fn parse_live_without_rate_limit_is_parse_error() {
        assert!(matches!(
            parse_live(r#"{"plan_type":"free","rate_limit":null}"#),
            Err(ProviderError::Parse(_))
        ));
        assert!(matches!(parse_live("<html>"), Err(ProviderError::Parse(_))));
    }

    #[test]
    fn parse_local_line_ignores_null_primary() {
        assert!(parse_local_line(SESSION.lines().last().unwrap()).is_none());
        assert!(parse_local_line(SESSION.lines().nth(1).unwrap()).is_none());
        let local = local_33().unwrap();
        assert_eq!(
            local.windows[0],
            Window::new("5h", 33.0, Some(at(1_791_044_262)))
        );
        assert_eq!(local.windows[1].label, "week");
        assert_eq!(local.plan.as_deref(), Some("plus"));
        assert_eq!(
            local.as_of,
            parse_rfc3339("2026-10-03T10:06:00.000Z").unwrap()
        );
    }

    #[test]
    fn parse_local_line_supports_resets_in_seconds() {
        let line = r#"{"timestamp":"2026-10-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":10,"window_minutes":300,"resets_in_seconds":600}}}}"#;
        let local = parse_local_line(line).unwrap();
        assert_eq!(
            local.windows[0].resets_at,
            parse_rfc3339("2026-10-03T10:10:00Z")
        );
    }

    fn write_session(home: &Path, rel: &str, text: &str, mtime: i64) {
        let p = home.join(".codex/sessions").join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        set_file_mtime(&p, FileTime::from_unix_time(mtime, 0)).unwrap();
    }

    #[test]
    fn find_local_takes_latest_line_with_limits() {
        let home = tempfile::tempdir().unwrap();
        write_session(home.path(), "2026/10/03/a.jsonl", SESSION, 1_000);
        assert_eq!(
            find_local(&home.path().join(".codex")).unwrap().windows[0].used_pct,
            33.0
        );
    }

    #[test]
    fn find_local_skips_newer_file_without_limits() {
        let home = tempfile::tempdir().unwrap();
        write_session(home.path(), "2026/10/02/old.jsonl", SESSION, 1_000);
        write_session(
            home.path(),
            "2026/10/03/new.jsonl",
            "{\"type\":\"session_meta\"}\n",
            2_000,
        );
        assert_eq!(
            find_local(&home.path().join(".codex")).unwrap().windows[0].used_pct,
            33.0
        );
    }

    #[test]
    fn merge_prefers_live() {
        let snap = merge(Some(parse_live(LIVE)), local_33(), at(1_791_000_000)).unwrap();
        assert_eq!(snap.windows[0].used_pct, 31.0);
        assert_eq!(snap.plan.as_deref(), Some("plus"));
        assert!(snap.note.is_none());
    }

    #[test]
    fn merge_falls_back_to_local_with_note() {
        let snap = merge(
            Some(Err(ProviderError::Auth)),
            local_33(),
            at(1_791_000_000),
        )
        .unwrap();
        assert_eq!(snap.windows[0].used_pct, 33.0);
        assert!(snap.note.unwrap().contains("(local)"));
    }

    #[test]
    fn merge_local_zeroes_expired_windows() {
        let snap = merge(None, local_33(), at(1_791_100_000)).unwrap();
        assert_eq!(snap.windows[0], Window::new("5h", 0.0, None));
        assert_eq!(snap.windows[1].used_pct, 5.0);
    }

    #[test]
    fn merge_without_any_source_reports_live_error_or_parse() {
        assert_eq!(
            merge(Some(Err(ProviderError::Auth)), None, at(0)),
            Err(ProviderError::Auth)
        );
        assert!(matches!(
            merge(None, None, at(0)),
            Err(ProviderError::Parse(_))
        ));
    }

    fn home_with_auth() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        fs::write(home.path().join(".codex/auth.json"), AUTH).unwrap();
        home
    }

    #[test]
    fn detect_requires_auth_file_and_enabled() {
        let home = home_with_auth();
        let paths = Account::new("", home.path().join(".codex"));
        assert!(OpenAi::detect(&Config::default(), &paths).is_some());
        let mut cfg = Config::default();
        cfg.openai.enabled = false;
        assert!(OpenAi::detect(&cfg, &paths).is_none());
        let empty = tempfile::tempdir().unwrap();
        assert!(OpenAi::detect(
            &Config::default(),
            &Account::new("", empty.path().join(".codex"))
        )
        .is_none());
    }

    #[test]
    fn named_account_has_own_identity() {
        let home = home_with_auth();
        let p = OpenAi::detect(
            &Config::default(),
            &Account::new("work", home.path().join(".codex")),
        )
        .unwrap();
        assert_eq!((p.id(), p.display_name()), ("openai:work", "OpenAI · work"));
    }

    #[test]
    fn poll_uses_live_endpoint_with_account_header() {
        let home = home_with_auth();
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/backend-api/wham/usage")
                .header("authorization", "Bearer test-access")
                .header("chatgpt-account-id", "acct-test");
            then.status(200).body(LIVE);
        });
        let mut p = OpenAi::detect(
            &Config::default(),
            &Account::new("", home.path().join(".codex")),
        )
        .unwrap()
        .with_api_base(server.base_url());
        let snap = p.poll().unwrap();
        m.assert();
        assert_eq!(snap.plan.as_deref(), Some("plus"));
    }

    #[test]
    fn agent_credentials_recover_rejected_codex_login_and_follow_token_rotation() {
        for agent in ["omo", "omp"] {
            let home = home_with_auth();
            let paths = crate::paths::Paths::for_home(home.path());
            let omo = home.path().join(".omo/agent");
            fs::create_dir_all(&omo).unwrap();
            let omp = home.path().join(".omp/agent");
            fs::create_dir_all(&omp).unwrap();
            let payload = base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-test"}}"#,
            );
            let token = format!("h.{payload}.s");
            let rejected_token = format!("h.{payload}.rejected");
            let save = |access: &str| {
                if agent == "omo" {
                    fs::write(
                        omo.join("auth.json"),
                        serde_json::json!({"chatgpt-subscription": {
                            "access": "not-an-oauth-token",
                            "accounts": [
                                {"access": rejected_token, "expires": 4102444800000u64},
                                {"access": access, "expires": 4102444800000u64}
                            ]
                        }})
                        .to_string(),
                    )
                    .unwrap();
                } else {
                    let conn = rusqlite::Connection::open(omp.join("agent.db")).unwrap();
                    conn.execute_batch(
                        "CREATE TABLE IF NOT EXISTS auth_credentials (
                         id INTEGER PRIMARY KEY, provider TEXT, credential_type TEXT,
                         data TEXT, disabled_cause TEXT, updated_at INTEGER);",
                    )
                    .unwrap();
                    conn.execute(
                        "INSERT OR REPLACE INTO auth_credentials VALUES
                         (3, 'openai-codex', 'oauth', ?1, NULL, 3)",
                        [serde_json::json!({
                            "access": rejected_token, "accountId": "acct-test",
                            "expires": 4102444800000u64
                        })
                        .to_string()],
                    )
                    .unwrap();
                    conn.execute(
                        "INSERT OR REPLACE INTO auth_credentials VALUES
                         (1, 'openai-codex', 'oauth', ?1, NULL, 1)",
                        [serde_json::json!({
                            "access": access, "accountId": "acct-test",
                            "expires": 4102444800000u64
                        })
                        .to_string()],
                    )
                    .unwrap();
                    conn.execute(
                        "INSERT OR REPLACE INTO auth_credentials VALUES
                         (2, 'openai-codex', 'oauth', ?1, 'disabled', 2)",
                        [r#"{"access":"disabled-token","accountId":"other"}"#],
                    )
                    .unwrap();
                }
            };
            save(&token);
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET)
                    .path("/backend-api/wham/usage")
                    .header("authorization", "Bearer test-access");
                then.status(401);
            });
            let rejected = server.mock(|when, then| {
                when.method(GET)
                    .path("/backend-api/wham/usage")
                    .header("authorization", format!("Bearer {rejected_token}"));
                then.status(401);
            });
            let mut live = server.mock(|when, then| {
                when.method(GET)
                    .path("/backend-api/wham/usage")
                    .header("authorization", format!("Bearer {token}"))
                    .header("chatgpt-account-id", "acct-test");
                then.status(200).body(LIVE);
            });
            let cfg = Config::default();
            let codex = OpenAi::detect(&cfg, &Account::new("", paths.codex_home.clone())).unwrap();
            let mut cards = merge_agent_logins(&cfg, &paths, vec![codex]);
            assert_eq!(cards.len(), 1, "same account must share one card");
            cards[0].api_base = server.base_url();
            assert_eq!(cards[0].poll().unwrap().windows[0].used_pct, 31.0);
            live.assert_calls(1);
            live.delete();
            let rotated = format!("h.{payload}.rotated");
            save(&rotated);
            let refreshed = server.mock(|when, then| {
                when.method(GET)
                    .path("/backend-api/wham/usage")
                    .header("authorization", format!("Bearer {rotated}"))
                    .header("chatgpt-account-id", "acct-test");
                then.status(200).body(LIVE);
            });
            assert_eq!(cards[0].poll().unwrap().windows[0].used_pct, 31.0);
            refreshed.assert_calls(1);
            rejected.assert_calls(2);
        }
    }

    #[test]
    fn rate_limited_live_call_backs_off_while_local_data_is_shown() {
        let home = home_with_auth();
        write_session(home.path(), "2026/10/03/a.jsonl", SESSION, 1_000);
        let server = MockServer::start();
        let limited = server.mock(|when, then| {
            when.method(GET).path("/backend-api/wham/usage");
            then.status(429);
        });
        let mut p = OpenAi::detect(
            &Config::default(),
            &Account::new("", home.path().join(".codex")),
        )
        .unwrap()
        .with_api_base(server.base_url());
        assert!(p.poll().unwrap().note.is_some());
        assert!(p.poll().unwrap().note.is_some());
        limited.assert_calls(1);
    }

    #[test]
    fn poll_falls_back_to_local_on_401() {
        let home = home_with_auth();
        write_session(home.path(), "2026/10/03/a.jsonl", SESSION, 1_000);
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/backend-api/wham/usage");
            then.status(401);
        });
        let mut p = OpenAi::detect(
            &Config::default(),
            &Account::new("", home.path().join(".codex")),
        )
        .unwrap()
        .with_api_base(server.base_url());
        assert!(p.poll().unwrap().note.is_some());
    }

    #[test]
    fn poll_with_live_disabled_never_calls_network() {
        let home = home_with_auth();
        write_session(home.path(), "2026/10/03/a.jsonl", SESSION, 1_000);
        let mut cfg = Config::default();
        cfg.openai.live_poll = false;
        let mut p = OpenAi::detect(&cfg, &Account::new("", home.path().join(".codex")))
            .unwrap()
            .with_api_base("http://127.0.0.1:9");
        assert!(p.poll().is_ok());
    }
}
