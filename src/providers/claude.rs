//! Claude: OAuth usage endpoint + Claude Code session logs.

use super::jsonl::{self, FileInfo};
use super::usage_cache;
use super::{read_secret_file, Provider, ProviderError};
use crate::accounts::Account;
use crate::backoff::Backoff;
use crate::config::Config;
use crate::format::parse_rfc3339;
use crate::http;
use crate::model::{ProviderSnapshot, Session, Window};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
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

pub const API_BASE: &str = "https://api.anthropic.com";

const DEFAULT_CONTEXT: u64 = 200_000;
const LONG_CONTEXT: u64 = 1_000_000;
const SYNTHETIC_MODEL: &str = "<synthetic>";
/// Upper bound for one account lookup at startup.
const PROFILE_LIMIT: Duration = Duration::from_secs(5);

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

/// Where a Claude card gets its OAuth token.
enum Creds {
    /// Claude Code config folder (`.credentials.json` or the macOS keychain).
    ClaudeDir(PathBuf),
    /// omp's credential database (`[omp] db`, default `~/.omp/agent/agent.db`).
    Omp(PathBuf),
    /// omo's credential file (`[omo] auth`, default `~/.omo/agent/auth.json`).
    Omo(PathBuf),
}

impl Creds {
    /// Short name shown on the card: "CC" (Claude Code), "omo", "omp".
    fn label(&self) -> &'static str {
        match self {
            Self::ClaudeDir(_) => "CC",
            Self::Omo(_) => "omo",
            Self::Omp(_) => "omp",
        }
    }

    /// (access token, subscription type).
    fn token(&self) -> Option<(Zeroizing<String>, Option<String>)> {
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
    fn lookup(&self, api_base: &str) -> (Option<String>, Option<Profile>) {
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

#[derive(Deserialize)]
struct OmoAuth<'a> {
    #[serde(rename = "anthropic-subscription", borrow)]
    subscription: Option<OmoSubscription<'a>>,
}

#[derive(Deserialize)]
struct OmoSubscription<'a> {
    #[serde(borrow, default)]
    accounts: Vec<OmoAccount<'a>>,
}

#[derive(Deserialize)]
struct OmoAccount<'a> {
    #[serde(borrow)]
    access: Option<&'a str>,
    /// Unix milliseconds.
    expires: Option<u64>,
}

/// OAuth access token of omo's Claude subscription login: the first account in
/// `anthropic-subscription.accounts` whose token has not expired, else the first with a
/// token (the endpoint then rejects it and the card turns red). The top-level `access`
/// of that entry is not an OAuth token and is ignored. omo refreshes; this app never does.
pub fn parse_omo_auth(text: &str, now: SystemTime) -> Option<Zeroizing<String>> {
    let auth: OmoAuth = serde_json::from_str(text).ok()?;
    let now_ms = now.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_millis();
    let accounts: Vec<_> = auth
        .subscription?
        .accounts
        .into_iter()
        .filter(|a| a.access.is_some_and(|t| !t.trim().is_empty()))
        .collect();
    let live = accounts
        .iter()
        .find(|a| a.expires.is_some_and(|e| u128::from(e) > now_ms));
    let token = live.or(accounts.first())?.access?;
    Some(Zeroizing::new(token.trim().to_string()))
}

/// Newest enabled Anthropic OAuth login in omp's `auth_credentials` table.
const OMP_LOGIN: &str = "SELECT data FROM auth_credentials \
     WHERE provider = 'anthropic' AND credential_type = 'oauth' \
     AND disabled_cause IS NULL ORDER BY updated_at DESC, id DESC LIMIT 1";

/// Opens omp's database read-only. No `immutable`: omp writes it in WAL mode while it runs.
fn open_omp(db: &Path) -> Option<rusqlite::Connection> {
    use rusqlite::{Connection, OpenFlags};
    if !db.is_file() {
        return None;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(db, flags).ok()
}

/// Access token of the newest enabled Anthropic OAuth login in omp's database.
/// omp refreshes the token; this app never does.
pub fn read_omp_token(db: &Path) -> Option<Zeroizing<String>> {
    let token: String = open_omp(db)?
        .query_row(
            &format!("SELECT json_extract(data, '$.access') FROM ({OMP_LOGIN})"),
            [],
            |row| row.get(0),
        )
        .ok()?;
    let token = Zeroizing::new(token);
    (!token.trim().is_empty()).then(|| Zeroizing::new(token.trim().to_string()))
}

/// Account uuid omp saved with its newest Anthropic login.
fn read_omp_account(db: &Path) -> Option<String> {
    open_omp(db)?
        .query_row(
            &format!("SELECT json_extract(data, '$.accountId') FROM ({OMP_LOGIN})"),
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .filter(|id| !id.is_empty())
}

/// The usage omp last fetched for that login (`usage_history`), and when. Labels follow
/// omp's limit ids without the `anthropic:` prefix: "5h", "7d", "7d fable".
pub fn read_omp_usage(db: &Path) -> Option<(Vec<Window>, SystemTime)> {
    let from_ms = |ms: i64| SystemTime::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64);
    let conn = open_omp(db)?;
    let sql = format!(
        "WITH login AS (SELECT 'account:' || json_extract(data, '$.accountId') AS key \
                        FROM ({OMP_LOGIN})), \
              rows AS (SELECT u.* FROM usage_history u, login \
                       WHERE u.provider = 'anthropic' AND instr(u.account_key, login.key) > 0) \
         SELECT limit_id, used_fraction, resets_at, recorded_at FROM rows \
         WHERE recorded_at = (SELECT max(recorded_at) FROM rows) \
           AND used_fraction IS NOT NULL ORDER BY limit_id"
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let at = from_ms(rows.first()?.3);
    let windows = rows
        .into_iter()
        .map(|(id, fraction, resets, _)| {
            let label = id
                .strip_prefix("anthropic:")
                .unwrap_or(&id)
                .replace(':', " ");
            Window::new(label, fraction * 100.0, resets.map(from_ms))
        })
        .collect();
    Some((windows, at))
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

/// A Claude subscription login of another coding agent (omo, omp).
pub struct AgentLogin {
    name: &'static str,
    creds: Creds,
}

/// omo's and omp's Claude logins, in that order, when enabled and holding a token.
pub fn agent_logins(cfg: &Config, omo_auth: &Path, omp_db: &Path) -> Vec<AgentLogin> {
    if !cfg.claude.enabled {
        return Vec::new();
    }
    [
        (cfg.omo.enabled, "omo", Creds::Omo(omo_auth.to_path_buf())),
        (cfg.omp.enabled, "omp", Creds::Omp(omp_db.to_path_buf())),
    ]
    .into_iter()
    .filter(|(enabled, _, creds)| *enabled && creds.token().is_some())
    .map(|(_, name, creds)| AgentLogin { name, creds })
    .collect()
}

/// Adds each agent login as an extra token source to the card of the same Claude
/// account (asked from the profile endpoint, or omp's saved account id). A login whose
/// account matches no card, or cannot be told, gets its own card: "Claude" when there is
/// no Claude Code card, else "Claude · omo" / "Claude · omp".
pub fn merge_agent_logins(
    cfg: &Config,
    mut cards: Vec<Claude>,
    logins: Vec<AgentLogin>,
    api_base: &str,
) -> Vec<Claude> {
    if logins.is_empty() {
        return cards;
    }
    let mut accounts = Vec::new();
    for card in &mut cards {
        let (id, profile) = card.sources[0].creds.lookup(api_base);
        card.profile = profile;
        card.profile_tried = true;
        accounts.push(id);
    }
    for login in logins {
        let (account, profile) = login.creds.lookup(api_base);
        let same = account
            .as_ref()
            .and_then(|a| accounts.iter().position(|c| c.as_ref() == Some(a)));
        if let Some(i) = same {
            let card = &mut cards[i];
            card.sources.push(Source::new(login.creds, card.interval));
            continue;
        }
        let (id, name) = if cards.iter().any(|c| c.id == "claude") {
            Account::new(login.name, "").identity("claude", "Claude")
        } else {
            ("claude".to_string(), "Claude".to_string())
        };
        let mut card = Claude::new(cfg, id, name, login.creds, None);
        card.api_base = api_base.to_string();
        card.profile = profile;
        card.profile_tried = true;
        cards.push(card);
        accounts.push(account);
    }
    cards
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

/// Claude account behind an OAuth token (`/api/oauth/profile`).
#[derive(Debug, Clone, PartialEq)]
struct Profile {
    uuid: String,
    email: Option<String>,
    /// "max" or "pro".
    plan: Option<String>,
}

/// Asks the profile endpoint, bounded so a hung call cannot block startup or a poll.
fn fetch_profile(api_base: &str, token: Zeroizing<String>) -> Option<Profile> {
    #[derive(Deserialize)]
    struct Body {
        account: BodyAccount,
    }
    #[derive(Deserialize)]
    struct BodyAccount {
        uuid: String,
        email: Option<String>,
        #[serde(default)]
        has_claude_max: bool,
        #[serde(default)]
        has_claude_pro: bool,
    }
    let url = format!("{api_base}/api/oauth/profile");
    let call = move || {
        let bearer = Zeroizing::new(format!("Bearer {}", token.as_str()));
        let resp = http::get(
            &url,
            &[
                ("Authorization", bearer.as_str()),
                ("anthropic-beta", "oauth-2025-04-20"),
                ("Accept", "application/json"),
            ],
        )
        .ok()?;
        resp.check().ok()?;
        let a = serde_json::from_str::<Body>(&resp.body).ok()?.account;
        let plan = match (a.has_claude_max, a.has_claude_pro) {
            (true, _) => Some("max".to_string()),
            (false, true) => Some("pro".to_string()),
            _ => None,
        };
        Some(Profile {
            uuid: a.uuid,
            email: a.email,
            plan,
        })
    };
    crate::accounts::with_deadline(PROFILE_LIMIT, call).flatten()
}

/// Masks an email for display, keeping only enough to tell accounts apart:
/// "office@example.com" → "of…@ex….com". The domain keeps its start and top-level part.
pub fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "…".to_string();
    };
    let head = |part: &str| -> String {
        let keep = if part.chars().count() > 2 { 2 } else { 1 };
        part.chars().take(keep).collect()
    };
    let domain = match domain.rsplit_once('.') {
        Some((name, tld)) => format!("{}….{tld}", head(name)),
        None => format!("{}…", head(domain)),
    };
    format!("{}…@{domain}", head(local))
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
    fn mask_email_keeps_only_starts_and_top_level_domain() {
        assert_eq!(mask_email("office@example.bg"), "of…@ex….bg");
        assert_eq!(mask_email("ab@x.io"), "a…@x….io");
        assert_eq!(mask_email("Ünïcode@mail.exämple.co.uk"), "Ün…@ma….uk");
        assert_eq!(mask_email("root@localhost"), "ro…@lo…");
        assert_eq!(mask_email("not-an-email"), "…");
    }

    #[test]
    fn omp_card_shows_omp_usage_when_rate_limited() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("agent.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE auth_credentials (id INTEGER PRIMARY KEY, provider TEXT,
               credential_type TEXT, data TEXT, disabled_cause TEXT, updated_at INTEGER);
             INSERT INTO auth_credentials VALUES
               (1, 'anthropic', 'oauth', '{\"access\":\"t\",\"accountId\":\"me\"}', NULL, 1);
             CREATE TABLE usage_history (recorded_at INTEGER, provider TEXT,
               account_key TEXT, limit_id TEXT, used_fraction REAL, resets_at INTEGER);
             INSERT INTO usage_history VALUES
               (1000, 'anthropic', 'oauth|account:me|email:x', 'anthropic:5h', 0.90, NULL),
               (2000, 'anthropic', 'oauth|account:me|email:x', 'anthropic:5h', 0.03, 4102444800000),
               (2000, 'anthropic', 'oauth|account:me|email:x', 'anthropic:7d:fable', 0.58, NULL),
               (3000, 'anthropic', 'oauth|account:other|email:y', 'anthropic:5h', 0.99, NULL);",
        )
        .unwrap();
        let server = MockServer::start();
        let limited = server.mock(|when, then| {
            when.method(GET).path("/api/oauth/usage");
            then.status(429);
        });
        server.mock(|when, then| {
            when.method(GET).path("/api/oauth/profile");
            then.status(200).body(
                r#"{"account":{"uuid":"me","email":"office@example.com","has_claude_max":true}}"#,
            );
        });
        let logins = agent_logins(&Config::default(), &dir.path().join("none"), &db);
        let mut cards =
            merge_agent_logins(&Config::default(), Vec::new(), logins, &server.base_url());
        assert_eq!(cards.len(), 1);
        let p = &mut cards[0];
        assert_eq!(
            p.id(),
            "claude",
            "no Claude Code card: the omp login is the Claude card"
        );
        let snap = p.poll().unwrap();
        limited.assert_calls(1);
        let shown: Vec<(&str, f64)> = snap
            .windows
            .iter()
            .map(|w| (w.label.as_str(), w.used_pct.round()))
            .collect();
        assert_eq!(shown, vec![("5h", 3.0), ("7d fable", 58.0)]);
        assert_eq!(snap.source.as_deref(), Some("of…@ex….com · omp saved"));
        assert_eq!(
            snap.plan.as_deref(),
            Some("max"),
            "no Claude Code: plan from the profile"
        );
        assert!(snap.windows[0].resets_at.is_some());
        assert!(snap.note.unwrap().contains("rate limited"));
        drop(conn);
    }

    #[test]
    fn omo_token_prefers_unexpired_account_and_ignores_top_level_access() {
        let now = UNIX_EPOCH + Duration::from_millis(2_000);
        let auth = |accounts: &str| {
            format!(
                r#"{{"anthropic-subscription":{{"type":"oauth","access":"claude-sdk",
                "expires":4102444800000,"accounts":[{accounts}]}}}}"#
            )
        };
        let both =
            auth(r#"{"access":"expired","expires":1000},{"access":" live ","expires":3000}"#);
        assert_eq!(parse_omo_auth(&both, now).unwrap().as_str(), "live");
        // All expired: the first token is still used, so the endpoint can reject it (red card).
        let stale = auth(r#"{"access":"","expires":3000},{"access":"old","expires":1000}"#);
        assert_eq!(parse_omo_auth(&stale, now).unwrap().as_str(), "old");
        assert!(parse_omo_auth(&auth(""), now).is_none());
        assert!(parse_omo_auth(r#"{"openai":{}}"#, now).is_none());

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        fs::write(&file, auth(r#"{"access":"tok","expires":99999999999999}"#)).unwrap();
        let none = dir.path().join("none");
        assert_eq!(agent_logins(&Config::default(), &file, &none).len(), 1);
        let mut off = Config::default();
        off.omo.enabled = false;
        assert!(agent_logins(&off, &file, &none).is_empty());
    }

    #[test]
    fn omp_token_is_newest_enabled_anthropic_oauth() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("agent.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE auth_credentials (id INTEGER PRIMARY KEY, provider TEXT,
               credential_type TEXT, data TEXT, disabled_cause TEXT, updated_at INTEGER);
             INSERT INTO auth_credentials VALUES
               (1, 'anthropic', 'oauth', '{\"access\":\"old\"}', NULL, 10),
               (2, 'anthropic', 'oauth', '{\"access\":\"new\"}', NULL, 20),
               (3, 'anthropic', 'oauth', '{\"access\":\"disabled\"}', 'revoked', 30),
               (4, 'anthropic', 'api_key', '{\"access\":\"key\"}', NULL, 40),
               (5, 'openai-codex', 'oauth', '{\"access\":\"codex\"}', NULL, 50);",
        )
        .unwrap();
        // Connection stays open: uncheckpointed WAL rows must still be visible.
        assert_eq!(read_omp_token(&db).unwrap().as_str(), "new");
        let none = dir.path().join("none");
        assert_eq!(agent_logins(&Config::default(), &none, &db).len(), 1);
        let mut off = Config::default();
        off.omp.enabled = false;
        assert!(agent_logins(&off, &none, &db).is_empty());
        drop(conn);
        assert!(read_omp_token(&dir.path().join("missing.db")).is_none());
    }

    #[test]
    fn agent_logins_join_the_card_of_their_account_and_take_over_on_429() {
        let home = home_with_creds();
        let omo = home.path().join("omo.json");
        fs::write(
            &omo,
            r#"{"anthropic-subscription":{"accounts":[{"access":"omo-tok","expires":99999999999999}]}}"#,
        )
        .unwrap();
        let omp = home.path().join("agent.db");
        rusqlite::Connection::open(&omp)
            .unwrap()
            .execute_batch(
                "CREATE TABLE auth_credentials (id INTEGER PRIMARY KEY, provider TEXT,
                   credential_type TEXT, data TEXT, disabled_cause TEXT, updated_at INTEGER);
                 INSERT INTO auth_credentials VALUES
                   (1, 'anthropic', 'oauth', '{\"access\":\"omp-tok\",\"accountId\":\"other\"}', NULL, 1);",
            )
            .unwrap();
        let server = MockServer::start();
        for token in ["test-access-token", "omo-tok"] {
            server.mock(|when, then| {
                when.method(GET)
                    .path("/api/oauth/profile")
                    .header("authorization", format!("Bearer {token}"));
                then.status(200).body(
                    r#"{"account":{"uuid":"me","email":"user@example.com","has_claude_pro":true}}"#,
                );
            });
        }
        let limited = server.mock(|when, then| {
            when.method(GET)
                .path("/api/oauth/usage")
                .header("authorization", "Bearer test-access-token");
            then.status(429);
        });
        let ok = server.mock(|when, then| {
            when.method(GET)
                .path("/api/oauth/usage")
                .header("authorization", "Bearer omo-tok");
            then.status(200).body(USAGE);
        });
        let cfg = Config::default();
        let claude_code = Claude::detect(&cfg, &Account::new("", home.path().join(".claude")))
            .unwrap()
            .with_api_base(server.base_url());
        let logins = agent_logins(&cfg, &omo, &omp);
        let mut cards = merge_agent_logins(&cfg, vec![claude_code], logins, &server.base_url());
        let ids: Vec<&str> = cards.iter().map(|c| c.id()).collect();
        assert_eq!(
            ids,
            vec!["claude", "claude:omp"],
            "omo joined, omp is another account"
        );
        assert_eq!(cards[0].sources.len(), 2);

        let snap = cards[0].poll().unwrap();
        assert_eq!(snap.windows.len(), 2, "omo's token answered");
        assert_eq!(snap.source.as_deref(), Some("us…@ex….com · omo · live"));
        assert_eq!(snap.note, None);
        assert_eq!(
            snap.plan.as_deref(),
            Some("max"),
            "plan still from Claude Code"
        );
        limited.assert_calls(1);
        ok.assert_calls(1);
    }

    #[test]
    fn logins_whose_account_cannot_be_told_keep_their_own_cards() {
        // Profile endpoint down (e.g. offline at startup): nothing may be merged by guess.
        let home = home_with_creds();
        let omo = home.path().join("omo.json");
        fs::write(
            &omo,
            r#"{"anthropic-subscription":{"accounts":[{"access":"omo-tok","expires":99999999999999}]}}"#,
        )
        .unwrap();
        let omp = home.path().join("agent.db");
        rusqlite::Connection::open(&omp)
            .unwrap()
            .execute_batch(
                "CREATE TABLE auth_credentials (id INTEGER PRIMARY KEY, provider TEXT,
                   credential_type TEXT, data TEXT, disabled_cause TEXT, updated_at INTEGER);
                 INSERT INTO auth_credentials VALUES
                   (1, 'anthropic', 'oauth', '{\"access\":\"omp-tok\",\"accountId\":\"me\"}', NULL, 1);",
            )
            .unwrap();
        let server = MockServer::start();
        let profile = server.mock(|when, then| {
            when.method(GET).path("/api/oauth/profile");
            then.status(503);
        });
        let cfg = Config::default();
        let claude_code = Claude::detect(&cfg, &Account::new("", home.path().join(".claude")))
            .unwrap()
            .with_api_base(server.base_url());
        let logins = agent_logins(&cfg, &omo, &omp);
        let cards = merge_agent_logins(&cfg, vec![claude_code], logins, &server.base_url());
        let ids: Vec<&str> = cards.iter().map(|c| c.id()).collect();
        // omp's saved account id is known, but Claude Code's account is not: no match.
        assert_eq!(ids, vec!["claude", "claude:omo", "claude:omp"]);
        assert!(cards.iter().all(|c| c.sources.len() == 1));
        profile.assert_calls(3);
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
