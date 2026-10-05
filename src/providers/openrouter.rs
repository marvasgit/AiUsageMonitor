//! OpenRouter: credits spent by the API key today, this week and this month
//! (`GET /api/v1/key`). Periods are calendar periods in UTC, as OpenRouter counts them.

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

fn dollars(amount: f64) -> String {
    format!("${amount:.2}")
}

fn midnight(date: NaiveDate) -> Option<SystemTime> {
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().into())
}

/// Next UTC start of the day, the ISO week (Monday) and the month after `now`.
fn period_ends(now: SystemTime) -> [Option<SystemTime>; 3] {
    let today = DateTime::<Utc>::from(now).date_naive();
    let monday = today.week(chrono::Weekday::Mon).first_day();
    let first = today.with_day(1);
    [
        today.checked_add_days(Days::new(1)),
        monday.checked_add_days(Days::new(7)),
        first.and_then(|d| d.checked_add_months(Months::new(1))),
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
    snap.plan = key.is_free_tier.then(|| "free tier".to_string());
    snap.note = match (key.limit, key.limit_remaining) {
        (Some(l), Some(left)) => Some(format!("{} left of {}", dollars(left), dollars(l))),
        _ => None,
    };
    Ok(snap)
}

pub struct OpenRouter {
    api_key_env: String,
    env: EnvFn,
    api_base: String,
}

impl OpenRouter {
    /// Present when the configured environment variable holds a key.
    pub fn detect(cfg: &Config, env: EnvFn) -> Option<Self> {
        if !cfg.openrouter.enabled {
            return None;
        }
        let provider = Self {
            api_key_env: cfg.openrouter.api_key_env.clone(),
            env,
            api_base: API_BASE.to_string(),
        };
        provider.key()?;
        Some(provider)
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    fn key(&self) -> Option<Zeroizing<String>> {
        let value = Zeroizing::new((self.env)(&self.api_key_env)?);
        let key = value.trim();
        (!key.is_empty()).then(|| Zeroizing::new(key.to_string()))
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
        let key = self.key().ok_or(ProviderError::NoCredentials)?;
        let bearer = Zeroizing::new(format!("Bearer {}", key.as_str()));
        let resp = http::get(
            &format!("{}/api/v1/key", self.api_base),
            &[
                ("Authorization", bearer.as_str()),
                ("Accept", "application/json"),
            ],
        )?;
        resp.check()?;
        parse_key(&resp.body, SystemTime::now())
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

    fn env_with_key(name: &str) -> Option<String> {
        (name == "OPENROUTER_API_KEY").then(|| " or-key \n".to_string())
    }

    fn env_blank(_: &str) -> Option<String> {
        Some("  ".to_string())
    }

    #[test]
    fn detect_needs_key_and_enabled() {
        assert!(OpenRouter::detect(&Config::default(), env_with_key).is_some());
        assert!(OpenRouter::detect(&Config::default(), env_blank).is_none());
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
}
