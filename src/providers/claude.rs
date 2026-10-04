//! Claude: OAuth usage endpoint + Claude Code session logs.

use super::jsonl::{self, FileInfo};
use super::{read_secret_file, Provider, ProviderError};
use crate::accounts::Account;
use crate::config::Config;
use crate::format::parse_rfc3339;
use crate::http;
use crate::model::{ProviderSnapshot, Session, Window};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

pub const API_BASE: &str = "https://api.anthropic.com";

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_CONTEXT: u64 = 200_000;
const LONG_CONTEXT: u64 = 1_000_000;
const SYNTHETIC_MODEL: &str = "<synthetic>";

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

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

pub fn parse_usage(body: &str) -> Result<Vec<Window>, ProviderError> {
    let resp: UsageResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let windows: Vec<Window> = [("5h", resp.five_hour), ("7d", resp.seven_day)]
        .into_iter()
        .filter_map(|(label, w)| {
            let w = w?;
            let pct = w.utilization?;
            Some(Window::new(
                label,
                pct,
                w.resets_at.as_deref().and_then(parse_rfc3339),
            ))
        })
        .collect();
    if windows.is_empty() {
        return Err(ProviderError::Parse("no usage windows in response".into()));
    }
    Ok(windows)
}

pub fn context_limit(model: &str, limit_override: Option<u64>) -> u64 {
    limit_override.unwrap_or(if model.contains("[1m]") {
        LONG_CONTEXT
    } else {
        DEFAULT_CONTEXT
    })
}

#[derive(Deserialize)]
struct LogLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    effort: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<LogMessage>,
}

#[derive(Deserialize)]
struct LogMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<LogUsage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
}

/// Aggregates a Claude Code session log; `None` when it has no assistant usage.
pub fn scan_session(text: &str, limit_override: Option<u64>) -> Option<Session> {
    let mut s = Session {
        model: String::new(),
        effort: None,
        input: 0,
        output: 0,
        cache_create: 0,
        cache_read: 0,
        requests: 0,
        context_tokens: 0,
        context_limit: 0,
    };
    let mut seen = std::collections::HashSet::new();
    for line in text.lines().filter(|l| l.contains("\"assistant\"")) {
        let Ok(parsed) = serde_json::from_str::<LogLine>(line) else {
            continue;
        };
        if parsed.kind.as_deref() != Some("assistant") {
            continue;
        }
        let Some(msg) = parsed.message else { continue };
        let model = msg.model.unwrap_or_default();
        if model == SYNTHETIC_MODEL {
            continue;
        }
        let Some(u) = msg.usage else { continue };
        // One API response spans several lines (one per content block) with identical usage.
        if msg.id.is_some() && !seen.insert((msg.id, parsed.request_id)) {
            continue;
        }
        s.input += u.input_tokens;
        s.output += u.output_tokens;
        s.cache_create += u.cache_creation_input_tokens;
        s.cache_read += u.cache_read_input_tokens;
        s.requests += 1;
        s.context_tokens =
            u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
        if !model.is_empty() {
            s.model = model;
        }
        if parsed.effort.is_some() {
            s.effort = parsed.effort;
        }
    }
    if s.requests == 0 {
        return None;
    }
    // Model ids do not always carry "[1m]"; a context above the default proves the long window.
    let limit = context_limit(&s.model, limit_override);
    s.context_limit = if limit_override.is_none() && s.context_tokens > limit {
        LONG_CONTEXT
    } else {
        limit
    };
    Some(s)
}

struct SessionCache {
    file: FileInfo,
    session: Option<Session>,
}

pub struct Claude {
    id: String,
    name: String,
    claude_dir: PathBuf,
    api_base: String,
    limit_override: Option<u64>,
    cache: Option<SessionCache>,
    last_usage: Option<(Vec<Window>, SystemTime)>,
}

impl Claude {
    pub fn detect(cfg: &Config, account: &Account) -> Option<Self> {
        if !cfg.claude.enabled {
            return None;
        }
        let creds = read_secret_file(&account.dir.join(".credentials.json"))?;
        parse_credentials(&creds)?;
        let (id, name) = account.identity("claude", "Claude");
        Some(Self {
            id,
            name,
            claude_dir: account.dir.clone(),
            api_base: API_BASE.to_string(),
            limit_override: cfg.claude.context_limit,
            cache: None,
            last_usage: None,
        })
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    fn fetch_usage(&self, token: &str) -> Result<Vec<Window>, ProviderError> {
        let bearer = Zeroizing::new(format!("Bearer {token}"));
        let resp = http::get(
            &format!("{}/api/oauth/usage", self.api_base),
            &[
                ("Authorization", bearer.as_str()),
                ("anthropic-beta", "oauth-2025-04-20"),
                ("Accept", "application/json"),
            ],
        )?;
        http::check_status(resp.status)?;
        parse_usage(&resp.body)
    }

    /// Rescans the newest session log only when its path, size or mtime changed.
    fn current_session(&mut self) -> Option<Session> {
        let newest = jsonl::newest_files(&self.claude_dir.join("projects"), "jsonl", 1).pop()?;
        if let Some(cache) = &self.cache {
            if cache.file == newest {
                return cache.session.clone();
            }
        }
        let session = std::fs::read_to_string(&newest.path)
            .ok()
            .and_then(|text| scan_session(&text, self.limit_override));
        self.cache = Some(SessionCache {
            file: newest,
            session: session.clone(),
        });
        session
    }
}

impl Provider for Claude {
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
        let creds = read_secret_file(&self.claude_dir.join(".credentials.json"))
            .ok_or(ProviderError::NoCredentials)?;
        let (token, plan) = parse_credentials(&creds).ok_or(ProviderError::NoCredentials)?;
        let usage = self.fetch_usage(&token);
        let session = self.current_session();
        let mut snapshot = match (usage, &self.last_usage) {
            (Ok(windows), _) => {
                self.last_usage = Some((windows.clone(), SystemTime::now()));
                ProviderSnapshot::new(windows)
            }
            // The session is local and always readable: keep it live while the API is down.
            (
                Err(e @ (ProviderError::Network(_) | ProviderError::Parse(_))),
                Some((windows, at)),
            ) => {
                let at: chrono::DateTime<chrono::Local> = (*at).into();
                let mut snap = ProviderSnapshot::new(windows.clone());
                snap.note = Some(format!("usage as of {} · {e}", at.format("%H:%M")));
                snap
            }
            (Err(e), _) => return Err(e),
        };
        snapshot.plan = plan;
        snapshot.session = session;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::fs;
    use std::time::UNIX_EPOCH;

    const USAGE: &str = include_str!("../../tests/fixtures/claude/usage.json");
    const CREDS: &str = include_str!("../../tests/fixtures/claude/credentials.json");
    const SESSION: &str = include_str!("../../tests/fixtures/claude/session.jsonl");

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

    #[test]
    fn parse_usage_maps_windows() {
        let w = parse_usage(USAGE).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].label, "5h");
        assert_eq!(w[0].used_pct, 3.0);
        assert_eq!(
            w[0].resets_at
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1_791_039_600
        );
        assert_eq!(w[1].label, "7d");
        assert_eq!(w[1].used_pct, 68.0);
    }

    #[test]
    fn parse_usage_skips_null_windows_and_fails_when_none() {
        let w = parse_usage(r#"{"five_hour":null,"seven_day":{"utilization":5,"resets_at":null}}"#)
            .unwrap();
        assert_eq!(w, vec![Window::new("7d", 5.0, None)]);
        assert!(matches!(
            parse_usage(r#"{"five_hour":null}"#),
            Err(ProviderError::Parse(_))
        ));
        assert!(matches!(
            parse_usage("<html>"),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn scan_session_sums_usage_and_tracks_last_context() {
        let s = scan_session(SESSION, None).unwrap();
        assert_eq!(s.requests, 2);
        assert_eq!(s.input, 180);
        assert_eq!(s.output, 1_800);
        assert_eq!(s.cache_create, 2_200);
        assert_eq!(s.cache_read, 101_920);
        assert_eq!(s.context_tokens, 80 + 200 + 71_920);
        assert_eq!(s.context_limit, 200_000);
        assert_eq!(s.model, "claude-sonnet-5");
        assert_eq!(s.effort.as_deref(), Some("high"));
    }

    #[test]
    fn scan_session_ignores_synthetic_entries() {
        let text = concat!(
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":4}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            "\n"
        );
        let s = scan_session(text, None).unwrap();
        assert_eq!(s.model, "claude-opus-5-5");
        assert_eq!(s.context_tokens, 8);
        assert_eq!(s.requests, 1);
    }

    #[test]
    fn scan_session_without_assistant_usage_is_none() {
        assert!(scan_session("not json\n{\"type\":\"user\"}\n", None).is_none());
    }

    #[test]
    fn scan_session_counts_each_api_response_once() {
        // Claude Code writes one line per content block, repeating id, requestId and usage.
        let block = r#"{"type":"assistant","requestId":"req_1","message":{"id":"msg_1","model":"claude-sonnet-5","usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":30,"cache_read_input_tokens":40}}}"#;
        let other = r#"{"type":"assistant","requestId":"req_2","message":{"id":"msg_2","model":"claude-sonnet-5","usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":4}}}"#;
        let text = [block, block, block, other].join("\n");
        let s = scan_session(&text, None).unwrap();
        assert_eq!(s.requests, 2);
        assert_eq!(
            (s.input, s.output, s.cache_create, s.cache_read),
            (11, 22, 33, 44)
        );
        assert_eq!(s.context_tokens, 1 + 3 + 4);
    }

    #[test]
    fn scan_session_infers_long_context_when_over_default() {
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":379000}}}"#;
        let s = scan_session(line, None).unwrap();
        assert_eq!(s.context_limit, 1_000_000);
        assert!(s.context_pct() < 100.0);
    }

    #[test]
    fn context_limit_rules() {
        assert_eq!(context_limit("claude-sonnet-5", None), 200_000);
        assert_eq!(context_limit("claude-sonnet-5[1m]", None), 1_000_000);
        assert_eq!(context_limit("claude-sonnet-5[1m]", Some(500_000)), 500_000);
    }

    fn home_with_creds() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        fs::create_dir_all(claude.join("projects/p")).unwrap();
        fs::write(claude.join(".credentials.json"), CREDS).unwrap();
        fs::write(claude.join("projects/p/s.jsonl"), SESSION).unwrap();
        home
    }

    #[test]
    fn detect_requires_enabled_and_credentials() {
        let home = home_with_creds();
        let paths = Account::new("", home.path().join(".claude"));
        assert!(Claude::detect(&Config::default(), &paths).is_some());
        let mut cfg = Config::default();
        cfg.claude.enabled = false;
        assert!(Claude::detect(&cfg, &paths).is_none());
        let empty = tempfile::tempdir().unwrap();
        assert!(Claude::detect(
            &Config::default(),
            &Account::new("", empty.path().join(".claude"))
        )
        .is_none());
    }

    #[test]
    fn poll_returns_windows_plan_and_session() {
        let home = home_with_creds();
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/api/oauth/usage")
                .header("authorization", "Bearer test-access-token")
                .header("anthropic-beta", "oauth-2025-04-20");
            then.status(200).body(USAGE);
        });
        let mut p = Claude::detect(
            &Config::default(),
            &Account::new("", home.path().join(".claude")),
        )
        .unwrap()
        .with_api_base(server.base_url());
        let snap = p.poll().unwrap();
        m.assert();
        assert_eq!(snap.plan.as_deref(), Some("max"));
        assert_eq!(snap.windows.len(), 2);
        assert_eq!(snap.session.unwrap().requests, 2);
    }

    #[test]
    fn poll_maps_401_to_auth_and_500_to_network() {
        let home = home_with_creds();
        let server = MockServer::start();
        let mut unauthorized = server.mock(|when, then| {
            when.method(GET).path("/api/oauth/usage");
            then.status(401);
        });
        let mut p = Claude::detect(
            &Config::default(),
            &Account::new("", home.path().join(".claude")),
        )
        .unwrap()
        .with_api_base(server.base_url());
        assert_eq!(p.poll(), Err(ProviderError::Auth));
        unauthorized.delete();
        server.mock(|when, then| {
            when.method(GET).path("/api/oauth/usage");
            then.status(500);
        });
        assert_eq!(p.poll(), Err(ProviderError::Network("HTTP 500".into())));
    }

    #[test]
    fn poll_keeps_session_fresh_when_usage_call_fails() {
        let home = home_with_creds();
        let server = MockServer::start();
        let mut ok = server.mock(|when, then| {
            when.method(GET).path("/api/oauth/usage");
            then.status(200).body(USAGE);
        });
        let mut p = Claude::detect(
            &Config::default(),
            &Account::new("", home.path().join(".claude")),
        )
        .unwrap()
        .with_api_base(server.base_url());
        p.poll().unwrap();
        ok.delete();
        let mut failing = server.mock(|when, then| {
            when.method(GET).path("/api/oauth/usage");
            then.status(500);
        });
        let log = home.path().join(".claude/projects/p/s2.jsonl");
        fs::write(
            &log,
            r#"{"type":"assistant","message":{"model":"claude-new","usage":{"input_tokens":7,"output_tokens":1}}}"#,
        )
        .unwrap();
        filetime::set_file_mtime(&log, filetime::FileTime::from_unix_time(4_000_000_000, 0))
            .unwrap();

        let snap = p.poll().unwrap();
        assert_eq!(snap.windows.len(), 2, "cached windows kept");
        assert_eq!(
            snap.session.unwrap().model,
            "claude-new",
            "session rescanned"
        );
        assert!(snap.note.unwrap().contains("HTTP 500"));

        failing.delete();
        server.mock(|when, then| {
            when.method(GET).path("/api/oauth/usage");
            then.status(401);
        });
        assert_eq!(p.poll(), Err(ProviderError::Auth));
    }

    #[test]
    fn named_account_has_own_identity() {
        let home = home_with_creds();
        let dir = home.path().join(".claude");
        let named =
            Claude::detect(&Config::default(), &Account::new("personal", dir.clone())).unwrap();
        assert_eq!(
            (named.id(), named.display_name()),
            ("claude:personal", "Claude · personal")
        );
        let default = Claude::detect(&Config::default(), &Account::new("", dir)).unwrap();
        assert_eq!((default.id(), default.display_name()), ("claude", "Claude"));
    }

    #[test]
    fn poll_without_credentials_is_no_credentials() {
        let home = home_with_creds();
        let mut p = Claude::detect(
            &Config::default(),
            &Account::new("", home.path().join(".claude")),
        )
        .unwrap();
        fs::remove_file(home.path().join(".claude/.credentials.json")).unwrap();
        assert_eq!(p.poll(), Err(ProviderError::NoCredentials));
    }
}
