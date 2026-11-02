//! OpenRouter: credits spent today, this week and this month. With a management key the
//! numbers cover the whole account (`GET /api/v1/credits` and `/api/v1/activity`),
//! otherwise only the API key (`GET /api/v1/key`). Periods are calendar periods in UTC,
//! as OpenRouter counts them.

use super::{EnvFn, Provider, ProviderError};
use crate::config::Config;
use crate::http;
use crate::model::{ProviderSnapshot, Window};
use chrono::{DateTime, Datelike, Days, Months, NaiveDate, Utc};
use serde::Deserialize;
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

pub const API_BASE: &str = "https://openrouter.ai";
const POLL_INTERVAL: Duration = Duration::from_secs(300);
/// Card marker: the numbers cover one API key.
pub const KEY_ICON: &str = "🗝";
/// Card marker: the numbers cover the whole account (management key).
pub const ACCOUNT_ICON: &str = "⛃";

#[derive(Deserialize)]
struct KeyResponse {
    data: KeyData,
}

#[derive(Deserialize)]
struct KeyData {
    usage: f64,
    usage_daily: f64,
    usage_weekly: f64,
    usage_monthly: f64,
    limit: Option<f64>,
    limit_remaining: Option<f64>,
    /// "daily", "weekly", "monthly", or null when the limit never resets.
    limit_reset: Option<String>,
    #[serde(default)]
    is_free_tier: bool,
}

#[derive(Deserialize)]
struct CreditsResponse {
    data: CreditsData,
}

#[derive(Deserialize)]
struct CreditsData {
    total_credits: f64,
    total_usage: f64,
}

/// Account spend per day, model and endpoint for the last 30 completed UTC days.
#[derive(Deserialize)]
struct ActivityResponse {
    data: Vec<ActivityItem>,
}

#[derive(Deserialize)]
struct ActivityItem {
    date: String,
    usage: f64,
}

fn parse_error(e: impl ToString) -> ProviderError {
    ProviderError::Parse(e.to_string())
}

fn dollars(amount: f64) -> String {
    format!("${amount:.2}")
}

fn midnight(date: NaiveDate) -> Option<SystemTime> {
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().into())
}

/// The Monday of `today`'s ISO week and the first day of its month.
fn period_starts(today: NaiveDate) -> (NaiveDate, NaiveDate) {
    (
        today.week(chrono::Weekday::Mon).first_day(),
        today - Days::new(today.day0().into()),
    )
}

/// Next UTC start of the day, the ISO week (Monday) and the month after `now`.
fn period_ends(now: SystemTime) -> [Option<SystemTime>; 3] {
    let today = DateTime::<Utc>::from(now).date_naive();
    let (monday, first) = period_starts(today);
    [
        today.checked_add_days(Days::new(1)),
        monday.checked_add_days(Days::new(7)),
        first.checked_add_months(Months::new(1)),
    ]
    .map(|d| d.and_then(midnight))
}

/// Maps the key response to "day", "week" and "month" windows. The bar of the period the
/// key's credit limit resets on shows the share of that limit; a limit that never resets
/// gets its own "total" window.
pub fn parse_key(body: &str, now: SystemTime) -> Result<ProviderSnapshot, ProviderError> {
    let key = serde_json::from_str::<KeyResponse>(body)
        .map_err(|e| ProviderError::Parse(e.to_string()))?
        .data;
    let limit = key.limit.filter(|l| *l > 0.0);
    let share = |used: f64, period: Option<&str>| {
        limit
            .filter(|_| key.limit_reset.as_deref() == period)
            .map(|l| used / l * 100.0)
    };
    let [day_end, week_end, month_end] = period_ends(now);
    let mut windows = vec![
        Window::amount(
            "day",
            dollars(key.usage_daily),
            share(key.usage_daily, Some("daily")),
            day_end,
        ),
        Window::amount(
            "week",
            dollars(key.usage_weekly),
            share(key.usage_weekly, Some("weekly")),
            week_end,
        ),
        Window::amount(
            "month",
            dollars(key.usage_monthly),
            share(key.usage_monthly, Some("monthly")),
            month_end,
        ),
    ];
    if let (Some(l), None) = (limit, key.limit_reset.as_deref()) {
        windows.push(Window::amount(
            "total",
            dollars(key.usage),
            Some(key.usage / l * 100.0),
            None,
        ));
    }
    let mut snap = ProviderSnapshot::new(windows);
    snap.source = Some(KEY_ICON.to_string());
    snap.plan = key.is_free_tier.then(|| "free tier".to_string());
    snap.note = match (key.limit, key.limit_remaining) {
        (Some(l), Some(left)) => Some(format!("{} left of {}", dollars(left), dollars(l))),
        _ => None,
    };
    Ok(snap)
}

/// Maps the account's credits and activity to "week", "month" and "credits" windows.
/// OpenRouter reports activity only for completed UTC days, so the week and month rows
/// run to the end of yesterday and there is no "day" row. The "credits" bar is the share
/// of all purchased credits that has been used.
pub fn parse_account(
    credits: &str,
    activity: &str,
    now: SystemTime,
) -> Result<ProviderSnapshot, ProviderError> {
    let credits = serde_json::from_str::<CreditsResponse>(credits)
        .map_err(parse_error)?
        .data;
    let days = serde_json::from_str::<ActivityResponse>(activity)
        .map_err(parse_error)?
        .data
        .into_iter()
        .map(|item| {
            // Documented as "YYYY-MM-DD", but sent as "YYYY-MM-DD 00:00:00".
            let (date, _) =
                NaiveDate::parse_and_remainder(&item.date, "%Y-%m-%d").map_err(parse_error)?;
            Ok((date, item.usage))
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let today = DateTime::<Utc>::from(now).date_naive();
    let (monday, first) = period_starts(today);
    let spent_since = |start: NaiveDate| -> f64 {
        days.iter()
            .filter(|(date, _)| (start..today).contains(date))
            // fold from 0.0: an empty f64 `sum()` is -0.0, shown as "$-0.00".
            .fold(0.0, |total, (_, usage)| total + usage)
    };
    let [_, week_end, month_end] = period_ends(now);
    let share =
        (credits.total_credits > 0.0).then(|| credits.total_usage / credits.total_credits * 100.0);
    let mut snap = ProviderSnapshot::new(vec![
        Window::amount("week", dollars(spent_since(monday)), None, week_end),
        Window::amount("month", dollars(spent_since(first)), None, month_end),
        Window::amount("credits", dollars(credits.total_usage), share, None),
    ]);
    snap.source = Some(ACCOUNT_ICON.to_string());
    snap.note = Some(format!(
        "{} left of {} · week and month to yesterday (UTC)",
        dollars(credits.total_credits - credits.total_usage),
        dollars(credits.total_credits)
    ));
    Ok(snap)
}

pub struct OpenRouter {
    api_key_env: String,
    management_key_env: String,
    env: EnvFn,
    api_base: String,
}

impl OpenRouter {
    /// Present when the configured environment variables hold an API or management key.
    pub fn detect(cfg: &Config, env: EnvFn) -> Option<Self> {
        if !cfg.openrouter.enabled {
            return None;
        }
        let provider = Self {
            api_key_env: cfg.openrouter.api_key_env.clone(),
            management_key_env: cfg.openrouter.management_key_env.clone(),
            env,
            api_base: API_BASE.to_string(),
        };
        (provider.key(&provider.api_key_env).is_some()
            || provider.key(&provider.management_key_env).is_some())
        .then_some(provider)
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    fn key(&self, env_name: &str) -> Option<Zeroizing<String>> {
        let value = Zeroizing::new((self.env)(env_name)?);
        let key = value.trim();
        (!key.is_empty()).then(|| Zeroizing::new(key.to_string()))
    }

    fn fetch(&self, path: &str, key: &str) -> Result<String, ProviderError> {
        let bearer = Zeroizing::new(format!("Bearer {key}"));
        let resp = http::get(
            &format!("{}{path}", self.api_base),
            &[
                ("Authorization", bearer.as_str()),
                ("Accept", "application/json"),
            ],
        )?;
        resp.check()?;
        Ok(resp.body)
    }
}

impl Provider for OpenRouter {
    fn id(&self) -> &str {
        "openrouter"
    }

    fn display_name(&self) -> &str {
        "OpenRouter"
    }

    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn poll(&mut self) -> Result<ProviderSnapshot, ProviderError> {
        if let Some(management) = self.key(&self.management_key_env) {
            let credits = self.fetch("/api/v1/credits", &management)?;
            let activity = self.fetch("/api/v1/activity", &management)?;
            return parse_account(&credits, &activity, SystemTime::now());
        }
        let key = self
            .key(&self.api_key_env)
            .ok_or(ProviderError::NoCredentials)?;
        parse_key(&self.fetch("/api/v1/key", &key)?, SystemTime::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn at(rfc3339: &str) -> SystemTime {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().into()
    }

    fn body(limit: &str, reset: &str) -> String {
        format!(
            r#"{{"data":{{"usage":40.5,"usage_daily":1.25,"usage_weekly":7.5,
            "usage_monthly":20,"limit":{limit},"limit_remaining":{remaining},
            "limit_reset":{reset},"is_free_tier":false}}}}"#,
            remaining = if limit == "null" { "null" } else { "80" }
        )
    }

    fn shown(snap: &ProviderSnapshot) -> Vec<(&str, &str, Option<f64>)> {
        snap.windows
            .iter()
            .map(|w| {
                (
                    w.label.as_str(),
                    w.amount.as_deref().unwrap(),
                    w.bar.then_some(w.used_pct),
                )
            })
            .collect()
    }

    #[test]
    fn unlimited_key_shows_amounts_without_bars_and_calendar_resets() {
        // Sunday 2026-10-04 23:30 UTC: day ends at midnight, week on Monday, month on Nov 1.
        let snap = parse_key(&body("null", "null"), at("2026-10-04T23:30:00Z")).unwrap();
        assert_eq!(
            shown(&snap),
            vec![
                ("day", "$1.25", None),
                ("week", "$7.50", None),
                ("month", "$20.00", None)
            ]
        );
        let resets: Vec<_> = snap.windows.iter().map(|w| w.resets_at).collect();
        assert_eq!(
            resets,
            vec![
                Some(at("2026-10-05T00:00:00Z")),
                Some(at("2026-10-05T00:00:00Z")),
                Some(at("2026-11-01T00:00:00Z")),
            ]
        );
        assert_eq!(snap.note, None);
    }

    #[test]
    fn limit_bar_sits_on_the_period_it_resets_on() {
        let now = at("2026-12-31T12:00:00Z");
        let monthly = parse_key(&body("100", "\"monthly\""), now).unwrap();
        assert_eq!(
            shown(&monthly),
            vec![
                ("day", "$1.25", None),
                ("week", "$7.50", None),
                ("month", "$20.00", Some(20.0))
            ]
        );
        assert_eq!(
            monthly.windows[2].resets_at,
            Some(at("2027-01-01T00:00:00Z"))
        );
        assert_eq!(monthly.note.as_deref(), Some("$80.00 left of $100.00"));

        let never = parse_key(&body("100", "null"), now).unwrap();
        assert_eq!(shown(&never)[3], ("total", "$40.50", Some(40.5)));
    }

    #[test]
    fn rejects_unexpected_body() {
        assert!(matches!(
            parse_key("<html>", SystemTime::now()),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn account_sums_completed_days_of_this_week_and_month() {
        // Wednesday 2026-11-04: the week started Monday 11-02, the month on Sunday 11-01.
        let credits = r#"{"data":{"total_credits":50,"total_usage":40}}"#;
        let activity = r#"{"data":[
            {"date":"2026-10-31","usage":5,"model":"a"},
            {"date":"2026-11-01","usage":2,"model":"a"},
            {"date":"2026-11-02 00:00:00","usage":1.5,"model":"a"},
            {"date":"2026-11-03 00:00:00","usage":0.25,"model":"a"},
            {"date":"2026-11-03 00:00:00","usage":0.5,"model":"b"},
            {"date":"2026-11-04","usage":9,"model":"a"}]}"#;
        let snap = parse_account(credits, activity, at("2026-11-04T10:00:00Z")).unwrap();
        assert_eq!(
            shown(&snap),
            vec![
                ("week", "$2.25", None),
                ("month", "$4.25", None),
                ("credits", "$40.00", Some(80.0))
            ]
        );
        let resets: Vec<_> = snap.windows.iter().map(|w| w.resets_at).collect();
        assert_eq!(
            resets,
            vec![
                Some(at("2026-11-09T00:00:00Z")),
                Some(at("2026-12-01T00:00:00Z")),
                None
            ]
        );
        assert_eq!(snap.source.as_deref(), Some(ACCOUNT_ICON));
        assert!(snap.note.unwrap().starts_with("$10.00 left of $50.00"));
    }

    #[test]
    fn account_rejects_bad_dates_and_bodies() {
        let credits = r#"{"data":{"total_credits":0,"total_usage":0}}"#;
        let now = SystemTime::now();
        let bad_date = r#"{"data":[{"date":"yesterday","usage":1}]}"#;
        assert!(matches!(
            parse_account(credits, bad_date, now),
            Err(ProviderError::Parse(_))
        ));
        assert!(matches!(
            parse_account("<html>", r#"{"data":[]}"#, now),
            Err(ProviderError::Parse(_))
        ));
        // No purchased credits: amounts only, no bar.
        let empty = parse_account(credits, r#"{"data":[]}"#, now).unwrap();
        assert_eq!(
            shown(&empty),
            vec![
                ("week", "$0.00", None),
                ("month", "$0.00", None),
                ("credits", "$0.00", None)
            ]
        );
    }

    fn env_with_key(name: &str) -> Option<String> {
        (name == "OPENROUTER_API_KEY").then(|| " or-key \n".to_string())
    }

    fn env_with_both(name: &str) -> Option<String> {
        match name {
            "OPENROUTER_API_KEY" => Some("or-key".to_string()),
            "OPENROUTER_MANAGEMENT_KEY" => Some("mgmt-key".to_string()),
            _ => None,
        }
    }

    fn env_management_only(name: &str) -> Option<String> {
        (name == "OPENROUTER_MANAGEMENT_KEY").then(|| "mgmt-key".to_string())
    }

    fn env_blank(_: &str) -> Option<String> {
        Some("  ".to_string())
    }

    #[test]
    fn detect_needs_key_and_enabled() {
        assert!(OpenRouter::detect(&Config::default(), env_with_key).is_some());
        assert!(OpenRouter::detect(&Config::default(), env_blank).is_none());
        assert!(OpenRouter::detect(&Config::default(), env_management_only).is_some());
        let mut cfg = Config::default();
        cfg.openrouter.enabled = false;
        assert!(OpenRouter::detect(&cfg, env_with_key).is_none());
    }

    #[test]
    fn poll_sends_trimmed_key_and_maps_401() {
        let server = MockServer::start();
        let mut ok = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/key")
                .header("authorization", "Bearer or-key");
            then.status(200).body(body("null", "null"));
        });
        let mut p = OpenRouter::detect(&Config::default(), env_with_key)
            .unwrap()
            .with_api_base(server.base_url());
        assert_eq!(p.poll().unwrap().windows.len(), 3);
        ok.assert();
        ok.delete();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/key");
            then.status(401);
        });
        assert_eq!(p.poll().unwrap_err(), ProviderError::Auth);
    }

    #[test]
    fn management_key_polls_the_account_not_the_key() {
        let server = MockServer::start();
        let key = server.mock(|when, then| {
            when.method(GET).path("/api/v1/key");
            then.status(200).body(body("null", "null"));
        });
        let credits = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/credits")
                .header("authorization", "Bearer mgmt-key");
            then.status(200)
                .body(r#"{"data":{"total_credits":10,"total_usage":4}}"#);
        });
        let activity = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/activity")
                .header("authorization", "Bearer mgmt-key");
            then.status(200).body(r#"{"data":[]}"#);
        });
        let mut p = OpenRouter::detect(&Config::default(), env_with_both)
            .unwrap()
            .with_api_base(server.base_url());
        let snap = p.poll().unwrap();
        assert_eq!(snap.source.as_deref(), Some(ACCOUNT_ICON));
        assert_eq!(snap.windows[2].used_pct, 40.0);
        credits.assert();
        activity.assert();
        key.assert_calls(0);
    }
}
