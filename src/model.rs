//! Provider-neutral data model shared by providers, poller and UI.

use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProviderError {
    #[error("no credentials found")]
    NoCredentials,
    #[error("authentication rejected")]
    Auth,
    #[error("network error: {0}")]
    Network(String),
    #[error("unexpected response: {0}")]
    Parse(String),
    /// HTTP 429, with the server's `Retry-After` when it sent one.
    #[error("rate limited")]
    RateLimited(Option<Duration>),
}

impl ProviderError {
    /// Failures worth retrying later; the last good data stays on screen meanwhile.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Network(_) | Self::Parse(_) | Self::RateLimited(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub label: String,
    pub used_pct: f64,
    pub resets_at: Option<SystemTime>,
    /// Shown instead of the percentage, e.g. "$4.20".
    pub amount: Option<String>,
    /// False when there is no limit to measure `used_pct` against.
    pub bar: bool,
}

impl Window {
    pub fn new(label: impl Into<String>, used_pct: f64, resets_at: Option<SystemTime>) -> Self {
        let used_pct = if used_pct.is_finite() {
            used_pct.clamp(0.0, 100.0)
        } else {
            0.0
        };
        Self {
            label: label.into(),
            used_pct,
            resets_at,
            amount: None,
            bar: true,
        }
    }

    /// A money window: `amount` is shown instead of the percentage; the bar is drawn
    /// only when there is a limit to measure against (`used_pct`).
    pub fn amount(
        label: impl Into<String>,
        amount: String,
        used_pct: Option<f64>,
        resets_at: Option<SystemTime>,
    ) -> Self {
        Self {
            amount: Some(amount),
            bar: used_pct.is_some(),
            ..Self::new(label, used_pct.unwrap_or(0.0), resets_at)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub model: String,
    pub effort: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_create: u64,
    pub cache_read: u64,
    pub requests: u64,
    pub context_tokens: u64,
    pub context_limit: u64,
}

impl Session {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_create + self.cache_read
    }

    pub fn context_pct(&self) -> f64 {
        if self.context_limit == 0 {
            return 0.0;
        }
        self.context_tokens as f64 / self.context_limit as f64 * 100.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSnapshot {
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    pub session: Option<Session>,
    pub note: Option<String>,
    /// Where the numbers came from, e.g. "omo · live" or "omp saved".
    pub source: Option<String>,
    pub updated_at: SystemTime,
}

impl ProviderSnapshot {
    pub fn new(windows: Vec<Window>) -> Self {
        Self {
            plan: None,
            windows,
            session: None,
            note: None,
            source: None,
            updated_at: SystemTime::now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderStatus {
    Pending,
    Ok,
    Error(ProviderError),
}

#[derive(Debug, Clone)]
pub struct ProviderState {
    pub id: String,
    pub name: String,
    pub status: ProviderStatus,
    pub last_ok: Option<ProviderSnapshot>,
    /// When the poller will check this provider next; `None` before the first poll.
    pub next_poll: Option<SystemTime>,
}

impl ProviderState {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status: ProviderStatus::Pending,
            last_ok: None,
            next_poll: None,
        }
    }

    /// Records a poll result; the last good snapshot survives later errors.
    pub fn apply(&mut self, result: Result<ProviderSnapshot, ProviderError>) {
        match result {
            Ok(snapshot) => {
                self.status = ProviderStatus::Ok;
                self.last_ok = Some(snapshot);
            }
            Err(err) => self.status = ProviderStatus::Error(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample_session() -> Session {
        Session {
            model: "m".into(),
            effort: None,
            input: 0,
            output: 0,
            cache_create: 0,
            cache_read: 0,
            requests: 0,
            context_tokens: 0,
            context_limit: 200_000,
        }
    }

    #[test]
    fn window_new_clamps_and_sanitizes() {
        assert_eq!(Window::new("5h", 120.0, None).used_pct, 100.0);
        assert_eq!(Window::new("5h", -3.0, None).used_pct, 0.0);
        assert_eq!(Window::new("5h", f64::NAN, None).used_pct, 0.0);
        assert_eq!(Window::new("5h", 42.5, None).used_pct, 42.5);
    }

    #[test]
    fn session_totals_and_context_pct() {
        let s = Session {
            input: 180,
            output: 31_800,
            cache_create: 1_000,
            cache_read: 3_970_000,
            requests: 90,
            context_tokens: 72_200,
            ..sample_session()
        };
        assert_eq!(s.total(), 180 + 31_800 + 1_000 + 3_970_000);
        assert!((s.context_pct() - 36.1).abs() < 1e-9);
    }

    #[test]
    fn session_context_pct_with_zero_limit_is_zero() {
        let s = Session {
            context_limit: 0,
            context_tokens: 5,
            ..sample_session()
        };
        assert_eq!(s.context_pct(), 0.0);
    }

    #[test]
    fn state_keeps_last_ok_across_errors() {
        let mut st = ProviderState::new("claude", "Claude");
        assert_eq!(st.status, ProviderStatus::Pending);
        let snap = ProviderSnapshot::new(vec![Window::new("5h", 10.0, None)]);
        st.apply(Ok(snap.clone()));
        assert_eq!(st.status, ProviderStatus::Ok);
        st.apply(Err(ProviderError::Network("boom".into())));
        assert_eq!(
            st.status,
            ProviderStatus::Error(ProviderError::Network("boom".into()))
        );
        assert_eq!(st.last_ok.as_ref().unwrap().windows, snap.windows);
    }

    #[test]
    fn state_accepts_owned_identity() {
        let st = ProviderState::new(String::from("claude:work"), format!("Claude · {}", "work"));
        assert_eq!(st.id, "claude:work");
        assert_eq!(st.name, "Claude · work");
    }

    #[test]
    fn snapshot_new_sets_updated_at_now() {
        let before = SystemTime::now() - Duration::from_secs(1);
        let snap = ProviderSnapshot::new(vec![]);
        assert!(snap.updated_at >= before);
        assert!(snap.plan.is_none() && snap.session.is_none() && snap.note.is_none());
    }
}
