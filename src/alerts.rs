//! Near-limit alerts: one notification when a window, or the session's context, first
//! crosses each threshold.

use crate::format::{fmt_countdown, fmt_tokens};
use crate::model::{ProviderSnapshot, Session};
use std::collections::HashMap;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub title: String,
    pub body: String,
}

/// What was last seen for one window: the highest threshold already alerted (if any)
/// and the reset time, whose change means the window started over.
struct Seen {
    alerted: Option<u8>,
    resets_at: Option<SystemTime>,
}

/// Per-provider alert state. The first snapshot only primes it, so restarting the app
/// does not repeat alerts for usage that was already high.
pub struct Tracker {
    thresholds: Vec<u8>,
    context: Vec<u8>,
    seen: HashMap<String, Seen>,
    primed: bool,
}

/// Key of the session's context in `seen`; window labels never start with NUL.
const CONTEXT_KEY: &str = "\0ctx";

/// Percent thresholds, sorted and deduplicated; out-of-range values are dropped.
fn clean(thresholds: &[u8]) -> Vec<u8> {
    let mut thresholds: Vec<u8> = thresholds
        .iter()
        .copied()
        .filter(|t| (1..=100).contains(t))
        .collect();
    thresholds.sort_unstable();
    thresholds.dedup();
    thresholds
}

/// Highest of the sorted `thresholds` that `pct` has reached.
fn reached(thresholds: &[u8], pct: f64) -> Option<u8> {
    thresholds
        .iter()
        .rev()
        .copied()
        .find(|t| pct >= f64::from(*t))
}

impl Tracker {
    /// Window `thresholds` in percent; out-of-range values are dropped. Empty disables
    /// window alerts.
    pub fn new(thresholds: &[u8]) -> Self {
        Self {
            thresholds: clean(thresholds),
            context: Vec::new(),
            seen: HashMap::new(),
            primed: false,
        }
    }

    /// Also alerts when the session's context window crosses one of `thresholds`.
    pub fn with_context(mut self, thresholds: &[u8]) -> Self {
        self.context = clean(thresholds);
        self
    }

    pub fn disabled() -> Self {
        Self::new(&[])
    }

    /// Records `pct` for `key`; returns true when it crossed a threshold not yet alerted
    /// since the last reset (a changed `resets_at`, or dropping below every threshold).
    fn crossed(
        &mut self,
        key: &str,
        thresholds: &[u8],
        pct: f64,
        resets_at: Option<SystemTime>,
    ) -> bool {
        let reached = reached(thresholds, pct);
        let prev = self.seen.get(key);
        let reset = prev.is_some_and(|p| p.resets_at != resets_at);
        let before = prev.filter(|_| !reset).and_then(|p| p.alerted);
        // Below every threshold re-arms; otherwise never step down within a window.
        let alerted = match (reached, before) {
            (None, _) => None,
            (Some(r), Some(b)) => Some(r.max(b)),
            (Some(r), None) => Some(r),
        };
        self.seen
            .insert(key.to_string(), Seen { alerted, resets_at });
        reached.is_some() && reached > before
    }

    /// Alerts for windows of `snap`, and its session's context, that crossed a threshold
    /// not yet alerted since their last reset. `name` is the card's display name.
    pub fn check(&mut self, name: &str, snap: &ProviderSnapshot, now: SystemTime) -> Vec<Alert> {
        if self.thresholds.is_empty() && self.context.is_empty() {
            return Vec::new();
        }
        let priming = !self.primed;
        self.primed = true;
        let mut alerts = Vec::new();
        let thresholds = std::mem::take(&mut self.thresholds);
        if !thresholds.is_empty() {
            for window in &snap.windows {
                let pct = window.used_pct;
                if self.crossed(&window.label, &thresholds, pct, window.resets_at) && !priming {
                    alerts.push(alert(name, &window.label, pct, window.resets_at, now));
                }
            }
        }
        self.thresholds = thresholds;
        let context = std::mem::take(&mut self.context);
        if let Some(session) = snap.session.as_ref().filter(|_| !context.is_empty()) {
            if self.crossed(CONTEXT_KEY, &context, session.context_pct(), None) && !priming {
                alerts.push(context_alert(name, session));
            }
        }
        self.context = context;
        alerts
    }
}

fn context_alert(name: &str, s: &Session) -> Alert {
    Alert {
        title: format!("{name}: context at {:.0}%", s.context_pct()),
        body: format!(
            "{} / {} tokens in the current {} session ({})",
            fmt_tokens(s.context_tokens),
            fmt_tokens(s.context_limit),
            s.source,
            s.model
        ),
    }
}

fn alert(
    name: &str,
    label: &str,
    pct: f64,
    resets_at: Option<SystemTime>,
    now: SystemTime,
) -> Alert {
    let reset = resets_at
        .map(|t| format!(" · resets in {}", fmt_countdown(t, now)))
        .unwrap_or_default();
    Alert {
        title: format!("{name}: {label} window at {pct:.0}%"),
        body: format!("{pct:.0}% of the {label} limit used{reset}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Window;
    use std::time::Duration;

    const NOW: SystemTime = SystemTime::UNIX_EPOCH;

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn snap(pct: f64, resets: Option<SystemTime>) -> ProviderSnapshot {
        ProviderSnapshot::new(vec![Window::new("5h", pct, resets)])
    }

    fn titles(t: &mut Tracker, pct: f64, resets: Option<SystemTime>) -> Vec<String> {
        t.check("Claude", &snap(pct, resets), NOW)
            .into_iter()
            .map(|a| a.title)
            .collect()
    }

    #[test]
    fn alerts_once_per_threshold_crossing() {
        let mut t = Tracker::new(&[95, 80]);
        assert!(titles(&mut t, 50.0, at(100)).is_empty(), "priming");
        assert_eq!(
            titles(&mut t, 81.0, at(100)),
            vec!["Claude: 5h window at 81%"]
        );
        assert!(titles(&mut t, 85.0, at(100)).is_empty(), "already alerted");
        assert_eq!(titles(&mut t, 97.0, at(100)).len(), 1, "next threshold");
        assert!(titles(&mut t, 99.0, at(100)).is_empty());
    }

    #[test]
    fn first_snapshot_primes_without_alerting() {
        let mut t = Tracker::new(&[80]);
        assert!(titles(&mut t, 90.0, at(100)).is_empty());
        assert!(titles(&mut t, 92.0, at(100)).is_empty());
    }

    #[test]
    fn window_reset_or_drop_rearms() {
        let mut t = Tracker::new(&[80]);
        titles(&mut t, 10.0, at(100));
        assert_eq!(titles(&mut t, 85.0, at(100)).len(), 1);
        assert_eq!(titles(&mut t, 85.0, at(200)).len(), 1, "new window");
        titles(&mut t, 5.0, at(200));
        assert_eq!(
            titles(&mut t, 81.0, at(200)).len(),
            1,
            "dropped below, re-armed"
        );
    }

    #[test]
    fn disabled_and_invalid_thresholds() {
        let mut off = Tracker::disabled();
        titles(&mut off, 0.0, None);
        assert!(titles(&mut off, 100.0, None).is_empty());
        let t = Tracker::new(&[0, 101, 90, 90, 70]);
        assert_eq!(t.thresholds, vec![70, 90]);
    }

    fn with_context(tokens: u64) -> ProviderSnapshot {
        let mut s = snap(10.0, None);
        s.session = Some(Session {
            source: "omo",
            model: "claude-opus-5-5".into(),
            effort: None,
            input: 0,
            output: 0,
            cache_create: 0,
            cache_read: 0,
            requests: 1,
            context_tokens: tokens,
            context_limit: 200_000,
        });
        s
    }

    #[test]
    fn context_alerts_once_per_threshold_and_rearms_after_drop() {
        let mut t = Tracker::new(&[80]).with_context(&[70, 90]);
        let mut titles = |tokens| -> Vec<String> {
            t.check("Claude", &with_context(tokens), NOW)
                .into_iter()
                .map(|a| a.title)
                .collect()
        };
        assert!(titles(150_000).is_empty(), "priming");
        assert!(titles(150_000).is_empty(), "already over 70 when primed");
        assert_eq!(titles(182_000), vec!["Claude: context at 91%"]);
        assert!(titles(190_000).is_empty());
        assert!(titles(20_000).is_empty(), "compacted: re-armed");
        assert_eq!(titles(141_000), vec!["Claude: context at 70%"]);
    }

    #[test]
    fn context_alert_body_and_window_alerts_are_independent() {
        let mut ctx_only = Tracker::disabled().with_context(&[50]);
        ctx_only.check("Claude", &with_context(0), NOW);
        let mut high = with_context(120_000);
        high.windows[0].used_pct = 99.0;
        let a = ctx_only.check("Claude", &high, NOW);
        assert_eq!(a.len(), 1, "no window alert without window thresholds");
        assert_eq!(
            a[0].body,
            "120.0K / 200.0K tokens in the current omo session (claude-opus-5-5)"
        );
        let mut windows_only = Tracker::new(&[80]);
        windows_only.check("Claude", &with_context(0), NOW);
        assert!(windows_only
            .check("Claude", &with_context(199_000), NOW)
            .is_empty());
    }

    #[test]
    fn body_mentions_reset_countdown() {
        let mut t = Tracker::new(&[80]);
        t.check("Claude", &snap(0.0, None), NOW);
        let a = t.check("Claude", &snap(90.0, at(3_600)), NOW);
        assert!(a[0]
            .body
            .starts_with("90% of the 5h limit used · resets in"));
    }
}
