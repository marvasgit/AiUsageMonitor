//! Pure formatting and time-conversion helpers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

const WARN_PCT: f64 = 60.0;
const CRIT_PCT: f64 = 85.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Crit,
}

pub fn pct_level(pct: f64) -> Level {
    if pct >= CRIT_PCT {
        Level::Crit
    } else if pct >= WARN_PCT {
        Level::Warn
    } else {
        Level::Ok
    }
}

pub fn fmt_tokens(n: u64) -> String {
    match n {
        1_000_000_000.. => format!("{:.2}B", n as f64 / 1e9),
        1_000_000.. => format!("{:.2}M", n as f64 / 1e6),
        1_000.. => format!("{:.1}K", n as f64 / 1e3),
        _ => n.to_string(),
    }
}

pub fn window_label_from_minutes(minutes: u64) -> String {
    match minutes {
        10_080 => "week".to_string(),
        m if m > 0 && m % 1_440 == 0 => format!("{}d", m / 1_440),
        m if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

pub fn fmt_countdown(resets_at: SystemTime, now: SystemTime) -> String {
    let secs = match resets_at.duration_since(now) {
        Ok(d) if d.as_secs() > 0 => d.as_secs(),
        _ => return "now".to_string(),
    };
    let (days, hours, mins, s) = (
        secs / 86_400,
        secs % 86_400 / 3_600,
        secs % 3_600 / 60,
        secs % 60,
    );
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m {s}s")
    } else if mins > 0 {
        format!("{mins}m {s}s")
    } else {
        format!("{s}s")
    }
}

pub fn fmt_absolute(resets_at: SystemTime, now: SystemTime) -> String {
    let at: chrono::DateTime<chrono::Local> = resets_at.into();
    let today: chrono::DateTime<chrono::Local> = now.into();
    if at.date_naive() == today.date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%a %H:%M").to_string()
    }
}

pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(SystemTime::from)
}

pub fn from_unix_secs(secs: i64) -> Option<SystemTime> {
    (secs > 0).then(|| UNIX_EPOCH + Duration::from_secs(secs as u64))
}

pub fn from_unix_millis(millis: i64) -> Option<SystemTime> {
    (millis > 0).then(|| UNIX_EPOCH + Duration::from_millis(millis as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn tokens_match_legacy_format() {
        assert_eq!(fmt_tokens(180), "180");
        assert_eq!(fmt_tokens(31_800), "31.8K");
        assert_eq!(fmt_tokens(200_000), "200.0K");
        assert_eq!(fmt_tokens(4_970_000), "4.97M");
        assert_eq!(fmt_tokens(2_500_000_000), "2.50B");
    }

    #[test]
    fn labels_from_minutes() {
        assert_eq!(window_label_from_minutes(300), "5h");
        assert_eq!(window_label_from_minutes(10_080), "week");
        assert_eq!(window_label_from_minutes(1_440), "1d");
        assert_eq!(window_label_from_minutes(4_320), "3d");
        assert_eq!(window_label_from_minutes(120), "2h");
        assert_eq!(window_label_from_minutes(45), "45m");
        assert_eq!(window_label_from_minutes(0), "0m");
    }

    #[test]
    fn countdown_formats() {
        let now = at(1_000_000);
        assert_eq!(
            fmt_countdown(at(1_000_000 + 4 * 3600 + 12 * 60 + 58), now),
            "4h 12m 58s"
        );
        assert_eq!(
            fmt_countdown(at(1_000_000 + 3 * 86_400 + 2 * 3600 + 2 * 60 + 9), now),
            "3d 2h 2m"
        );
        assert_eq!(fmt_countdown(at(1_000_000 + 12 * 60 + 5), now), "12m 5s");
        assert_eq!(fmt_countdown(at(1_000_000 + 7), now), "7s");
        assert_eq!(fmt_countdown(at(999_000), now), "now");
    }

    #[test]
    fn absolute_shows_weekday_only_on_other_days() {
        let now = SystemTime::now();
        let later = now + Duration::from_secs(3 * 86_400);
        let same_day = fmt_absolute(now, now);
        assert_eq!(same_day.len(), 5, "expected HH:MM, got {same_day}");
        let other_day = fmt_absolute(later, now);
        assert_eq!(other_day.len(), 9, "expected 'Ddd HH:MM', got {other_day}");
    }

    #[test]
    fn levels_follow_thresholds() {
        assert_eq!(pct_level(0.0), Level::Ok);
        assert_eq!(pct_level(59.9), Level::Ok);
        assert_eq!(pct_level(60.0), Level::Warn);
        assert_eq!(pct_level(84.9), Level::Warn);
        assert_eq!(pct_level(85.0), Level::Crit);
    }

    #[test]
    fn parses_rfc3339_with_offset_and_fraction() {
        let t = parse_rfc3339("2026-10-03T15:00:00.034674+00:00").unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_791_039_600
        );
        let z = parse_rfc3339("2026-10-03T17:00:00+02:00").unwrap();
        assert_eq!(
            z.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_791_039_600
        );
        assert!(parse_rfc3339("not a date").is_none());
    }

    #[test]
    fn unix_conversions() {
        assert_eq!(from_unix_secs(1_791_044_262), Some(at(1_791_044_262)));
        assert_eq!(from_unix_millis(1_791_039_600_000), Some(at(1_791_039_600)));
        assert_eq!(from_unix_secs(-5), None);
        assert_eq!(from_unix_secs(0), None);
    }
}
