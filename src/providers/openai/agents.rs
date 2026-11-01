//! Read-only Codex subscription credentials maintained by coding agents.

use super::{parse_auth, read_auth, read_secret_file};
use base64::Engine;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::SystemTime;
use zeroize::Zeroizing;

pub(super) type Login = (Zeroizing<String>, Option<String>);

#[derive(Clone)]
pub(super) enum Credentials {
    Codex(PathBuf),
    Omo(PathBuf),
    Omp(PathBuf),
}

#[derive(Deserialize)]
struct AgentAuth<'a> {
    #[serde(borrow)]
    access: Option<&'a str>,
    #[serde(rename = "accountId")]
    account_id: Option<String>,
    expires: Option<u64>,
}

impl AgentAuth<'_> {
    fn login(self) -> Option<Login> {
        let token = self.access?.trim();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_millis();
        if token.is_empty() || self.expires.is_some_and(|expiry| u128::from(expiry) <= now) {
            return None;
        }
        let account = self.account_id.filter(|id| !id.is_empty()).or_else(|| {
            #[derive(Deserialize)]
            struct Claims {
                #[serde(rename = "https://api.openai.com/auth")]
                auth: AccountClaim,
            }
            #[derive(Deserialize)]
            struct AccountClaim {
                chatgpt_account_id: Option<String>,
            }
            let bytes = Zeroizing::new(
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(token.split('.').nth(1)?.trim_end_matches('='))
                    .ok()?,
            );
            serde_json::from_slice::<Claims>(&bytes)
                .ok()?
                .auth
                .chatgpt_account_id
                .filter(|id| !id.is_empty())
        });
        Some((Zeroizing::new(token.to_owned()), account))
    }
}

impl Credentials {
    pub(super) fn logins(&self) -> Vec<Login> {
        match self {
            Self::Codex(dir) => read_auth(dir)
                .and_then(|text| parse_auth(&text))
                .into_iter()
                .collect(),
            Self::Omo(path) => {
                #[derive(Deserialize)]
                struct Auth<'a> {
                    #[serde(rename = "chatgpt-subscription", borrow)]
                    subscription: Option<Subscription<'a>>,
                }
                #[derive(Deserialize)]
                struct Subscription<'a> {
                    #[serde(borrow, default)]
                    accounts: Vec<AgentAuth<'a>>,
                }
                let Some(text) = read_secret_file(path) else {
                    return Vec::new();
                };
                let Ok(auth) = serde_json::from_str::<Auth>(&text) else {
                    return Vec::new();
                };
                auth.subscription
                    .into_iter()
                    .flat_map(|s| s.accounts)
                    .filter_map(AgentAuth::login)
                    .collect()
            }
            Self::Omp(path) => {
                use rusqlite::{Connection, OpenFlags};
                let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
                let Ok(conn) = Connection::open_with_flags(path, flags) else {
                    return Vec::new();
                };
                let Ok(mut stmt) = conn.prepare(
                    "SELECT data FROM auth_credentials \
                     WHERE provider = 'openai-codex' AND credential_type = 'oauth' \
                     AND disabled_cause IS NULL ORDER BY updated_at DESC, id DESC",
                ) else {
                    return Vec::new();
                };
                let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) else {
                    return Vec::new();
                };
                rows.filter_map(|row| {
                    let text = Zeroizing::new(row.ok()?);
                    serde_json::from_str::<AgentAuth>(&text).ok()?.login()
                })
                .collect()
            }
        }
    }
}

pub(super) struct Source {
    pub(super) credentials: Credentials,
    pub(super) account: Option<String>,
}

impl Source {
    pub(super) fn login(&self) -> Option<Login> {
        self.credentials
            .logins()
            .into_iter()
            .find(|(_, account)| *account == self.account)
    }
}
