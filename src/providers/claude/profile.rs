//! The Claude account behind a token (`/api/oauth/profile`), and how it is shown.

use crate::http;
use serde::Deserialize;
use std::time::Duration;
use zeroize::Zeroizing;

const PROFILE_LIMIT: Duration = Duration::from_secs(5);

/// Claude account behind an OAuth token (`/api/oauth/profile`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Profile {
    pub(super) uuid: String,
    pub(super) email: Option<String>,
    /// "max" or "pro".
    pub(super) plan: Option<String>,
}

/// Asks the profile endpoint, bounded so a hung call cannot block startup or a poll.
pub(super) fn fetch_profile(api_base: &str, token: Zeroizing<String>) -> Option<Profile> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_email_keeps_only_starts_and_top_level_domain() {
        assert_eq!(mask_email("office@example.bg"), "of…@ex….bg");
        assert_eq!(mask_email("ab@x.io"), "a…@x….io");
        assert_eq!(mask_email("Ünïcode@mail.exämple.co.uk"), "Ün…@ma….uk");
        assert_eq!(mask_email("root@localhost"), "ro…@lo…");
        assert_eq!(mask_email("not-an-email"), "…");
    }
}
