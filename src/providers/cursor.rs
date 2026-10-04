//! Cursor: plan usage for the current billing cycle from `cursor.com/api/usage-summary`.

use super::{decode_secret, read_secret_file, EnvFn, Provider, ProviderError};
use crate::config::Config;
use crate::format::{from_unix_secs, parse_rfc3339};
use crate::http;
use crate::model::{ProviderSnapshot, Window};
use crate::paths::Paths;
use base64::Engine;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

pub const API_BASE: &str = "https://cursor.com";

const POLL_INTERVAL: Duration = Duration::from_secs(300);
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const TOKEN_KEY: &str = "cursorAuth/accessToken";

#[derive(Debug, Clone, PartialEq)]
pub struct JwtInfo {
    pub user_id: String,
    pub expires_at: Option<SystemTime>,
}

#[derive(Deserialize)]
struct Claims {
    sub: Option<String>,
    exp: Option<i64>,
}

/// Reads `sub` (user id = text after the last `|`) and `exp` from a JWT payload. Signature is not checked.
pub fn parse_jwt(token: &str) -> Option<JwtInfo> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: Claims = serde_json::from_slice(&bytes).ok()?;
    let sub = claims.sub?;
    let user_id = sub.rsplit('|').next().unwrap_or_default().to_string();
    (!user_id.is_empty()).then(|| JwtInfo {
        user_id,
        expires_at: claims.exp.and_then(from_unix_secs),
    })
}

#[derive(Deserialize)]
struct AgentAuth<'a> {
    #[serde(rename = "accessToken", borrow)]
    access_token: Option<&'a str>,
}

pub fn parse_agent_auth(text: &str) -> Option<Zeroizing<String>> {
    let auth: AgentAuth = serde_json::from_str(text).ok()?;
    let token = auth.access_token?.trim();
    (!token.is_empty()).then(|| Zeroizing::new(token.to_string()))
}

/// Read-only, immutable SQLite URI: Cursor keeps the database open, so we must not lock or write it.
pub fn sqlite_uri(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        text.insert(0, '/');
    }
    let escaped: String = text
        .chars()
        .map(|c| match c {
            '%' => "%25".to_string(),
            ' ' => "%20".to_string(),
            '?' => "%3f".to_string(),
            '#' => "%23".to_string(),
            other => other.to_string(),
        })
        .collect();
    format!("file:{escaped}?mode=ro&immutable=1")
}

pub fn read_app_token(db: &Path) -> Option<Zeroizing<String>> {
    use rusqlite::types::Value;
    use rusqlite::{Connection, OpenFlags};
    if !db.is_file() {
        return None;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(sqlite_uri(db), flags).ok()?;
    let value: Value = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [TOKEN_KEY],
            |row| row.get(0),
        )
        .ok()?;
    let text = match value {
        Value::Text(text) => Zeroizing::new(text),
        Value::Blob(bytes) => decode_secret(&Zeroizing::new(bytes))?,
        _ => return None,
    };
    let token = text.trim().trim_matches('"');
    (!token.is_empty()).then(|| Zeroizing::new(token.to_string()))
}

fn agent_auth_file(env: EnvFn, config_home: &Path) -> PathBuf {
    env("CURSOR_CLI_AUTH_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            config_home
                .join(if cfg!(windows) { "Cursor" } else { "cursor" })
                .join("auth.json")
        })
}

fn app_db(config_home: &Path) -> PathBuf {
    config_home
        .join("Cursor")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb")
}

/// cursor-agent's auth file first, then the desktop app's database.
/// Prefers the first login that is not expired, so a stale `cursor-agent` token cannot hide a fresh app login.
/// If all are expired, the first decodable one is returned so the card can show the expired state.
pub fn resolve_token(env: EnvFn, config_home: &Path) -> Option<Zeroizing<String>> {
    let agent = read_secret_file(&agent_auth_file(env, config_home))
        .and_then(|text| parse_agent_auth(&text));
    let app = read_app_token(&app_db(config_home));
    let now = SystemTime::now();
    let mut decodable: Vec<(Zeroizing<String>, JwtInfo)> = [agent, app]
        .into_iter()
        .flatten()
        .filter_map(|token| parse_jwt(&token).map(|info| (token, info)))
        .collect();
    let fresh = decodable
        .iter()
        .position(|(_, info)| !is_expired(info, now));
    let index = fresh.unwrap_or(0);
    (index < decodable.len()).then(|| decodable.swap_remove(index).0)
}

fn is_expired(info: &JwtInfo, now: SystemTime) -> bool {
    info.expires_at
        .is_some_and(|exp| exp <= now + EXPIRY_MARGIN)
}

#[derive(Debug, Clone, PartialEq)]
pub struct CursorUsage {
    pub windows: Vec<Window>,
    pub plan: Option<String>,
    pub note: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    billing_cycle_end: Option<String>,
    membership_type: Option<String>,
    individual_usage: Option<Individual>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Individual {
    plan: Option<PlanUsage>,
    on_demand: Option<Amount>,
    overall: Option<Amount>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanUsage {
    total_percent_used: Option<f64>,
    api_percent_used: Option<f64>,
}

#[derive(Deserialize)]
struct Amount {
    used: Option<f64>,
    limit: Option<f64>,
}

pub fn parse_usage(body: &str) -> Result<CursorUsage, ProviderError> {
    let summary: Summary =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let reset = summary.billing_cycle_end.as_deref().and_then(parse_rfc3339);
    let usage = summary.individual_usage;
    let mut windows = Vec::new();
    if let Some(plan) = usage.as_ref().and_then(|u| u.plan.as_ref()) {
        if let Some(total) = plan.total_percent_used {
            windows.push(Window::new("Plan", total, reset));
        }
        if let Some(api) = plan.api_percent_used {
            windows.push(Window::new("API", api, reset));
        }
    }
    if windows.is_empty() {
        if let Some(Amount {
            used: Some(used),
            limit: Some(limit),
        }) = usage.as_ref().and_then(|u| u.overall.as_ref())
        {
            if *limit > 0.0 {
                windows.push(Window::new("Overall", used / limit * 100.0, reset));
            }
        }
    }
    if windows.is_empty() {
        return Err(ProviderError::Parse(
            "no Cursor plan usage in response".into(),
        ));
    }
    let note = usage
        .as_ref()
        .and_then(|u| u.on_demand.as_ref())
        .and_then(|d| d.used)
        .filter(|cents| *cents > 0.0)
        .map(|cents| format!("on-demand ${:.2}", cents / 100.0));
    Ok(CursorUsage {
        windows,
        plan: summary.membership_type,
        note,
    })
}

pub struct Cursor {
    config_home: PathBuf,
    api_base: String,
    env: EnvFn,
}

impl Cursor {
    pub fn detect(cfg: &Config, paths: &Paths) -> Option<Self> {
        if !cfg.cursor.enabled {
            return None;
        }
        let provider = Self::with_sources(paths, super::real_env);
        let token = resolve_token(provider.env, &provider.config_home)?;
        parse_jwt(&token)?;
        Some(provider)
    }

    pub fn with_sources(paths: &Paths, env: EnvFn) -> Self {
        Self {
            config_home: paths.config_home.clone(),
            api_base: API_BASE.to_string(),
            env,
        }
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }
}

impl Provider for Cursor {
    fn id(&self) -> &str {
        "cursor"
    }

    fn display_name(&self) -> &str {
        "Cursor"
    }

    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn poll(&mut self) -> Result<ProviderSnapshot, ProviderError> {
        let token =
            resolve_token(self.env, &self.config_home).ok_or(ProviderError::NoCredentials)?;
        let jwt = parse_jwt(&token).ok_or(ProviderError::NoCredentials)?;
        if is_expired(&jwt, SystemTime::now()) {
            return Err(ProviderError::Auth);
        }
        let cookie = Zeroizing::new(format!(
            "WorkosCursorSessionToken={}%3A%3A{}",
            jwt.user_id,
            token.as_str()
        ));
        let resp = http::get(
            &format!("{}/api/usage-summary", self.api_base),
            &[("Accept", "application/json"), ("Cookie", cookie.as_str())],
        )?;
        http::check_status(resp.status)?;
        let usage = parse_usage(&resp.body)?;
        let mut snapshot = ProviderSnapshot::new(usage.windows);
        snapshot.plan = usage.plan;
        snapshot.note = usage.note;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::fs;

    const USAGE: &str = include_str!("../../tests/fixtures/cursor/usage.json");

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn jwt(sub: &str, exp: i64) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = engine.encode(format!(r#"{{"sub":"{sub}","exp":{exp}}}"#));
        format!("eyJhbGciOiJIUzI1NiJ9.{payload}.sig")
    }

    fn future() -> i64 {
        (SystemTime::now() + Duration::from_secs(3600))
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn write_agent_auth(home: &Path, token: &str) {
        let dir = home
            .join(".config")
            .join(if cfg!(windows) { "Cursor" } else { "cursor" });
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("auth.json"),
            format!(r#"{{"accessToken":"{token}","refreshToken":"r"}}"#),
        )
        .unwrap();
    }

    fn write_app_db(path: &Path, value: rusqlite::types::Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES ('cursorAuth/accessToken', ?1)",
            [value],
        )
        .unwrap();
    }

    #[test]
    fn parse_jwt_extracts_user_and_expiry() {
        let info = parse_jwt(&jwt("auth0|user_01ABC", 2_000_000_000)).unwrap();
        assert_eq!(info.user_id, "user_01ABC");
        assert_eq!(info.expires_at, from_unix_secs(2_000_000_000));
        assert_eq!(
            parse_jwt(&jwt("plainid", 2_000_000_000)).unwrap().user_id,
            "plainid"
        );
    }

    #[test]
    fn parse_jwt_rejects_garbage() {
        assert!(parse_jwt("not-a-jwt").is_none());
        assert!(parse_jwt("a.!!!.c").is_none());
        assert!(parse_jwt(&jwt("", 1)).is_none());
    }

    #[test]
    fn agent_auth_parsing() {
        assert_eq!(
            parse_agent_auth(r#"{"accessToken":" tok "}"#)
                .unwrap()
                .as_str(),
            "tok"
        );
        assert!(parse_agent_auth(r#"{"accessToken":""}"#).is_none());
        assert!(parse_agent_auth("x").is_none());
    }

    #[test]
    fn app_db_token_text_utf8_and_utf16_blob() {
        use rusqlite::types::Value;
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            Value::Text("\"tok-text\"".into()),
            Value::Blob(b"tok-utf8".to_vec()),
            Value::Blob(
                "tok-utf16"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ),
        ];
        let expected = ["tok-text", "tok-utf8", "tok-utf16"];
        for (i, (value, want)) in cases.into_iter().zip(expected).enumerate() {
            let db = dir.path().join(format!("case {i}/state.vscdb"));
            write_app_db(&db, value);
            assert_eq!(read_app_token(&db).unwrap().as_str(), want);
        }
        assert!(read_app_token(&dir.path().join("missing.vscdb")).is_none());
    }

    #[test]
    fn sqlite_uri_escapes_special_characters() {
        assert_eq!(
            sqlite_uri(Path::new("/a b/c#d?e%f")),
            "file:/a%20b/c%23d%3fe%25f?mode=ro&immutable=1"
        );
    }

    #[test]
    fn agent_auth_preferred_over_app_db() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config");
        write_app_db(
            &config.join("Cursor/User/globalStorage/state.vscdb"),
            rusqlite::types::Value::Text(jwt("auth0|app", future())),
        );
        assert_eq!(
            parse_jwt(&resolve_token(no_env, &config).unwrap())
                .unwrap()
                .user_id,
            "app"
        );
        write_agent_auth(home.path(), &jwt("auth0|agent", future()));
        assert_eq!(
            parse_jwt(&resolve_token(no_env, &config).unwrap())
                .unwrap()
                .user_id,
            "agent"
        );
    }

    #[test]
    fn fresh_app_login_beats_stale_or_broken_agent_login() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config");
        let fresh = jwt("auth0|app", future());
        write_app_db(
            &config.join("Cursor/User/globalStorage/state.vscdb"),
            rusqlite::types::Value::Text(fresh.clone()),
        );
        write_agent_auth(home.path(), &jwt("auth0|agent", 1_000));
        assert_eq!(resolve_token(no_env, &config).unwrap().as_str(), fresh);
        write_agent_auth(home.path(), "not-a-jwt");
        assert_eq!(resolve_token(no_env, &config).unwrap().as_str(), fresh);
    }

    #[test]
    fn expired_logins_still_resolve_for_auth_state() {
        let home = tempfile::tempdir().unwrap();
        let stale = jwt("auth0|agent", 1_000);
        write_agent_auth(home.path(), &stale);
        assert_eq!(
            resolve_token(no_env, &home.path().join(".config"))
                .unwrap()
                .as_str(),
            stale
        );
    }

    #[test]
    fn parse_usage_plan_api_and_on_demand() {
        let usage = parse_usage(USAGE).unwrap();
        assert_eq!(usage.plan.as_deref(), Some("pro"));
        let labels: Vec<&str> = usage.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, vec!["Plan", "API"]);
        assert_eq!(usage.windows[0].used_pct, 61.7);
        assert_eq!(
            usage.windows[0].resets_at,
            parse_rfc3339("2026-10-15T00:00:00.000Z")
        );
        assert_eq!(usage.note.as_deref(), Some("on-demand $4.20"));
    }

    #[test]
    fn parse_usage_overall_fallback_and_errors() {
        let body = r#"{"billingCycleEnd":"2026-10-15T00:00:00Z","membershipType":"enterprise","individualUsage":{"overall":{"used":250,"limit":1000}}}"#;
        let usage = parse_usage(body).unwrap();
        assert_eq!(usage.windows[0].label, "Overall");
        assert_eq!(usage.windows[0].used_pct, 25.0);
        assert!(usage.note.is_none());
        assert!(matches!(
            parse_usage(r#"{"individualUsage":{}}"#),
            Err(ProviderError::Parse(_))
        ));
        assert!(matches!(
            parse_usage("<html>"),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn poll_sends_session_cookie() {
        let home = tempfile::tempdir().unwrap();
        let token = jwt("auth0|user_9", future());
        write_agent_auth(home.path(), &token);
        let server = MockServer::start();
        let cookie = format!("WorkosCursorSessionToken=user_9%3A%3A{token}");
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/api/usage-summary")
                .header("cookie", cookie.as_str());
            then.status(200).body(USAGE);
        });
        let mut p = Cursor::with_sources(&Paths::for_home(home.path()), no_env)
            .with_api_base(server.base_url());
        let snap = p.poll().unwrap();
        m.assert();
        assert_eq!(snap.windows.len(), 2);
        assert_eq!((p.id(), p.display_name()), ("cursor", "Cursor"));
    }

    #[test]
    fn expired_token_is_auth() {
        let home = tempfile::tempdir().unwrap();
        write_agent_auth(home.path(), &jwt("auth0|u", 1_000));
        let mut p = Cursor::with_sources(&Paths::for_home(home.path()), no_env)
            .with_api_base("http://127.0.0.1:9");
        assert_eq!(p.poll(), Err(ProviderError::Auth));
    }

    #[test]
    fn poll_maps_401_and_missing_login() {
        let home = tempfile::tempdir().unwrap();
        let mut none = Cursor::with_sources(&Paths::for_home(home.path()), no_env);
        assert_eq!(none.poll(), Err(ProviderError::NoCredentials));
        write_agent_auth(home.path(), &jwt("auth0|u", future()));
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/usage-summary");
            then.status(401);
        });
        let mut p = Cursor::with_sources(&Paths::for_home(home.path()), no_env)
            .with_api_base(server.base_url());
        assert_eq!(p.poll(), Err(ProviderError::Auth));
    }
}
