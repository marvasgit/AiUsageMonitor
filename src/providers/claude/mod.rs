//! Claude: OAuth usage endpoint + Claude Code session logs.
//!
//! `creds` reads Claude Code's login, `agents` adds omo/omp logins, `profile` names the
//! account, `session` reads the session logs; this module polls and builds the card.

mod agents;
mod creds;
mod profile;
mod session;
#[cfg(test)]
mod test_support;

pub use agents::{
    agent_logins, merge_agent_logins, parse_omo_auth, read_omp_token, read_omp_usage, AgentLogin,
};
pub use creds::{parse_credentials, read_credentials};
pub use profile::mask_email;
pub use session::{context_limit, scan_session};

use super::jsonl;
use super::usage_cache;
use super::{Provider, ProviderError};
use crate::accounts::Account;
use crate::backoff::Backoff;
use crate::config::Config;
use crate::format::parse_rfc3339;
use crate::http;
use crate::model::{ProviderSnapshot, Session, Window};
use creds::Creds;
use profile::{fetch_profile, Profile};
use serde::Deserialize;
use session::SessionCache;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

pub const API_BASE: &str = "https://api.anthropic.com";

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

/// One token source of a card, with its own backoff: a 429 on one token does not hold
/// off the others.
struct Source {
    creds: Creds,
    backoff: Backoff,
}

/// One card per Claude account. `sources` are tried in order (Claude Code, omo, omp);
/// the first that answers wins.
pub struct Claude {
    id: String,
    name: String,
    sources: Vec<Source>,
    /// Claude Code folder whose `projects/` logs feed the session block.
    sessions: Option<PathBuf>,
    api_base: String,
    limit_override: Option<u64>,
    interval: Duration,
    cache: Option<SessionCache>,
    last_usage: Option<(Vec<Window>, SystemTime)>,
    usage_file: Option<PathBuf>,
    plan: Option<String>,
    /// Login whose token produced `last_usage`; `None` for a reading loaded at startup.
    last_from: Option<&'static str>,
    /// Account of the card, asked once from the profile endpoint.
    profile: Option<Profile>,
    profile_tried: bool,
}

impl Source {
    fn new(creds: Creds, interval: Duration) -> Self {
        Self {
            creds,
            backoff: Backoff::new(interval),
        }
    }
}

impl Claude {
    pub fn detect(cfg: &Config, account: &Account) -> Option<Self> {
        if !cfg.claude.enabled {
            return None;
        }
        let creds = Creds::ClaudeDir(account.dir.clone());
        creds.token()?;
        let (id, name) = account.identity("claude", "Claude");
        Some(Self::new(cfg, id, name, creds, Some(account.dir.clone())))
    }

    fn new(
        cfg: &Config,
        id: String,
        name: String,
        creds: Creds,
        sessions: Option<PathBuf>,
    ) -> Self {
        let interval = cfg.claude.poll_interval();
        Self {
            id,
            name,
            sources: vec![Source::new(creds, interval)],
            sessions,
            api_base: API_BASE.to_string(),
            limit_override: cfg.claude.context_limit,
            interval,
            cache: None,
            last_usage: None,
            usage_file: None,
            plan: None,
            last_from: None,
            profile: None,
            profile_tried: false,
        }
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// Keeps the last good usage in `dir` and starts from what is already there.
    pub fn with_cache_dir(mut self, dir: &Path) -> Self {
        let file = usage_cache::file_for(dir, &self.id);
        self.last_usage = usage_cache::load(&file);
        self.usage_file = Some(file);
        self
    }

    /// Source text for the app's own stored reading: "omo · cached", or just "cached"
    /// when it was loaded from the cache file at startup.
    fn cached_source(&self) -> String {
        match self.last_from {
            Some(label) => format!("{label} · cached"),
            None => "cached".to_string(),
        }
    }

    /// Stored usage young enough to show without calling the endpoint (e.g. after a restart).
    fn recent_usage(&self, now: SystemTime) -> Option<(Vec<Window>, SystemTime)> {
        let (windows, at) = self.last_usage.as_ref()?;
        let age = now.duration_since(*at).ok()?;
        (age < self.interval / 2).then(|| (windows.clone(), *at))
    }

    /// Asks the sources in order; the first answer wins. When all fail, the most useful
    /// error is returned: a transient one (stored data stays on screen), else a rejected
    /// token, else missing credentials.
    /// Returns the windows and where they came from ("omo · live", "CC · cached").
    fn usage(&mut self, now: SystemTime) -> Result<(Vec<Window>, String), ProviderError> {
        if let Some((windows, _)) = self.recent_usage(now) {
            return Ok((windows, self.cached_source()));
        }
        let rank = |e: &ProviderError| match e {
            e if e.is_transient() => 2,
            ProviderError::Auth => 1,
            _ => 0,
        };
        let mut error = ProviderError::NoCredentials;
        let api_base = &self.api_base;
        for source in &mut self.sources {
            let Some((token, plan)) = source.creds.token() else {
                continue;
            };
            if plan.is_some() {
                self.plan = plan;
            }
            match source.backoff.call(|| fetch_usage(api_base, &token)) {
                Ok(windows) => {
                    if let Some(file) = &self.usage_file {
                        usage_cache::save(file, &windows, now);
                    }
                    self.last_usage = Some((windows.clone(), now));
                    let label = source.creds.label();
                    self.last_from = Some(label);
                    return Ok((windows, format!("{label} · live")));
                }
                Err(e) if rank(&e) >= rank(&error) => error = e,
                Err(_) => {}
            }
        }
        Err(error)
    }

    /// What to show while the usage call fails transiently: the newest stored windows
    /// (this app's own, or for the omp card also the usage omp last fetched), else just
    /// the local session.
    fn fallback(
        &self,
        err: ProviderError,
        has_session: bool,
        now: SystemTime,
    ) -> Result<ProviderSnapshot, ProviderError> {
        let omp = self.sources.iter().find_map(|s| match &s.creds {
            Creds::Omp(db) => read_omp_usage(db),
            Creds::ClaudeDir(_) | Creds::Omo(_) => None,
        });
        let stored = match (self.last_usage.clone(), omp) {
            (Some(own), Some(omp)) if omp.1 > own.1 => Some((omp, "omp saved".to_string())),
            (Some(own), _) => Some((own, self.cached_source())),
            (None, Some(omp)) => Some((omp, "omp saved".to_string())),
            (None, None) => None,
        };
        match stored {
            Some(((windows, at), source)) => {
                let at: chrono::DateTime<chrono::Local> = at.into();
                let mut snap = ProviderSnapshot::new(usage_cache::expire(&windows, now));
                snap.note = Some(format!("usage as of {} · {err}", at.format("%H:%M")));
                snap.source = Some(source);
                Ok(snap)
            }
            None if has_session => {
                let mut snap = ProviderSnapshot::new(Vec::new());
                snap.note = Some(format!("usage unavailable · {err}"));
                Ok(snap)
            }
            None => Err(err),
        }
    }

    /// Rescans the newest session log only when its path, size or mtime changed.
    fn current_session(&mut self) -> Option<Session> {
        let dir = self.sessions.as_ref()?;
        let newest = jsonl::newest_files(&dir.join("projects"), "jsonl", 1).pop()?;
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

fn fetch_usage(api_base: &str, token: &str) -> Result<Vec<Window>, ProviderError> {
    let bearer = Zeroizing::new(format!("Bearer {token}"));
    let resp = http::get(
        &format!("{api_base}/api/oauth/usage"),
        &[
            ("Authorization", bearer.as_str()),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("Accept", "application/json"),
        ],
    )?;
    resp.check()?;
    parse_usage(&resp.body)
}

impl Provider for Claude {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    fn poll_interval(&self) -> Duration {
        self.interval
    }

    fn poll(&mut self) -> Result<ProviderSnapshot, ProviderError> {
        if !self.profile_tried {
            self.profile_tried = true;
            let api_base = &self.api_base;
            self.profile = self
                .sources
                .iter()
                .find_map(|s| fetch_profile(api_base, s.creds.token()?.0));
        }
        if self.plan.is_none() {
            let own = self.sources.iter().find_map(|s| s.creds.token()?.1);
            self.plan = own.or_else(|| self.profile.as_ref()?.plan.clone());
        }
        let now = SystemTime::now();
        let usage = self.usage(now);
        // The session is local and always readable: keep it live while the API is down.
        let session = self.current_session();
        let mut snapshot = match usage {
            Ok((windows, source)) => {
                let mut snap = ProviderSnapshot::new(windows);
                snap.source = Some(source);
                snap
            }
            Err(e) if e.is_transient() => self.fallback(e, session.is_some(), now)?,
            Err(e) => return Err(e),
        };
        snapshot.plan = self.plan.clone();
        if let Some(email) = self.profile.as_ref().and_then(|p| p.email.as_deref()) {
            snapshot.source = snapshot
                .source
                .map(|s| format!("{} · {s}", mask_email(email)));
        }
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
    use test_support::{home_with_creds, USAGE};

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
    fn poll_maps_401_to_auth_and_500_to_session_only() {
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
        let snap = p.poll().unwrap();
        assert!(snap.windows.is_empty());
        assert!(snap.session.is_some(), "local session still shown");
        assert!(snap.note.unwrap().contains("usage unavailable"));
        fs::remove_dir_all(home.path().join(".claude/projects")).unwrap();
        p.sources[0].backoff.reset();
        assert_eq!(p.poll(), Err(ProviderError::Network("HTTP 500".into())));
    }

    /// Makes the stored usage look one poll interval old, as if time had passed.
    fn age_usage(p: &mut Claude) {
        let interval = p.interval;
        p.last_usage.as_mut().unwrap().1 -= interval;
    }

    #[test]
    fn restart_reuses_fresh_cached_usage_without_calling() {
        let home = home_with_creds();
        let cache = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let ok = server.mock(|when, then| {
            when.method(GET).path("/api/oauth/usage");
            then.status(200).body(USAGE);
        });
        let account = Account::new("work", home.path().join(".claude"));
        let detect = || {
            Claude::detect(&Config::default(), &account)
                .unwrap()
                .with_api_base(server.base_url())
                .with_cache_dir(cache.path())
        };
        detect().poll().unwrap();
        let snap = detect().poll().unwrap();
        assert_eq!(snap.windows.len(), 2, "windows from disk");
        ok.assert_calls(1);
        let mut later = detect();
        age_usage(&mut later);
        later.poll().unwrap();
        ok.assert_calls(2);
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
        age_usage(&mut p);
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
        assert!(p.poll().is_ok(), "still backing off: no request");
        p.sources[0].backoff.reset();
        assert_eq!(p.poll(), Err(ProviderError::Auth));
    }

    #[test]
    fn rate_limit_backs_off_while_showing_cached_usage() {
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
        age_usage(&mut p);
        ok.delete();
        let limited = server.mock(|when, then| {
            when.method(GET).path("/api/oauth/usage");
            then.status(429).header("retry-after", "600");
        });
        let snap = p.poll().unwrap();
        assert_eq!(snap.windows.len(), 2, "cached windows kept");
        assert_eq!(snap.source.as_deref(), Some("CC · cached"));
        assert!(snap.note.unwrap().contains("rate limited"));
        let snap = p.poll().unwrap();
        assert!(snap.note.unwrap().contains("rate limited"));
        limited.assert_calls(1);
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
