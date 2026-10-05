//! Claude subscription logins of other coding agents (omo, omp), and merging them into
//! the cards of the same Claude account.

use super::creds::Creds;
use super::{Claude, Source};
use crate::accounts::Account;
use crate::config::Config;
use crate::model::Window;
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

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
pub(super) fn read_omp_account(db: &Path) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::claude::test_support::{home_with_creds, USAGE};
    use crate::providers::Provider;
    use httpmock::prelude::*;
    use std::fs;
    use std::time::UNIX_EPOCH;

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
}
