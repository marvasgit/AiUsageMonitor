//! Near-limit alerts: one notification when a window first crosses each threshold.

use crate::format::fmt_countdown;
use crate::model::ProviderSnapshot;
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
    seen: HashMap<String, Seen>,
    primed: bool,
}

impl Tracker {
    /// `thresholds` in percent; out-of-range values are dropped. Empty disables alerts.
    pub fn new(thresholds: &[u8]) -> Self {
        let mut thresholds: Vec<u8> = thresholds
            .iter()
            .copied()
            .filter(|t| (1..=100).contains(t))
            .collect();
        thresholds.sort_unstable();
        thresholds.dedup();
        Self {
            thresholds,
            seen: HashMap::new(),
            primed: false,
        }
    }

    pub fn disabled() -> Self {
        Self::new(&[])
    }

    /// Highest threshold `pct` has reached.
    fn reached(&self, pct: f64) -> Option<u8> {
        self.thresholds
            .iter()
            .rev()
            .copied()
            .find(|t| pct >= f64::from(*t))
    }

    /// Alerts for windows of `snap` that crossed a threshold not yet alerted since their
    /// last reset. `name` is the card's display name.
    pub fn check(&mut self, name: &str, snap: &ProviderSnapshot, now: SystemTime) -> Vec<Alert> {
        if self.thresholds.is_empty() {
            return Vec::new();
        }
        let priming = !self.primed;
        self.primed = true;
        let mut alerts = Vec::new();
        for window in &snap.windows {
            let reached = self.reached(window.used_pct);
            let prev = self.seen.get(&window.label);
            let reset = prev.is_some_and(|p| p.resets_at != window.resets_at);
            let before = prev.filter(|_| !reset).and_then(|p| p.alerted);
            // Below every threshold re-arms; otherwise never step down within a window.
            let alerted = match (reached, before) {
                (None, _) => None,
                (Some(r), Some(b)) => Some(r.max(b)),
                (Some(r), None) => Some(r),
            };
            if !priming && reached.is_some() && reached > before {
                alerts.push(alert(
                    name,
                    &window.label,
                    window.used_pct,
                    window.resets_at,
                    now,
                ));
            }
            self.seen.insert(
                window.label.clone(),
                Seen {
                    alerted,
                    resets_at: window.resets_at,
                },
            );
        }
        alerts
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
