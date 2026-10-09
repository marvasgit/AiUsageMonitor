//! Background polling: one thread per provider writing into shared state.

use crate::alerts::{Alert, Tracker};
use crate::model::{ProviderError, ProviderSnapshot, ProviderState};
use crate::providers::Provider;
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

pub type SharedState = Arc<RwLock<Vec<ProviderState>>>;

pub const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Transient failures back off exponentially (at least 2 × interval, at most 15 min),
/// and never retry sooner than a rate limit's `Retry-After`.
pub fn next_delay(
    result: &Result<ProviderSnapshot, ProviderError>,
    prev: Duration,
    interval: Duration,
) -> Duration {
    match result {
        Err(e) => error_delay(e, prev, interval),
        Ok(_) => interval,
    }
}

pub fn error_delay(err: &ProviderError, prev: Duration, interval: Duration) -> Duration {
    if !err.is_transient() {
        return interval;
    }
    let backoff = (prev * 2).max(interval * 2).min(MAX_BACKOFF).max(interval);
    match err {
        ProviderError::RateLimited(Some(after)) => backoff.max((*after).min(MAX_BACKOFF)),
        _ => backoff,
    }
}

/// Polls once, stores the result at `index`, returns the delay before the next poll and
/// any near-limit alerts the new numbers raise.
pub fn poll_once(
    provider: &mut dyn Provider,
    index: usize,
    state: &SharedState,
    prev: Duration,
    tracker: &mut Tracker,
) -> (Duration, Vec<Alert>) {
    let result = provider.poll();
    let alerts = match &result {
        Ok(snap) => tracker.check(provider.display_name(), snap, SystemTime::now()),
        Err(e) => {
            log::warn!("{}: {e}", provider.id());
            Vec::new()
        }
    };
    let delay = next_delay(&result, prev, provider.poll_interval());
    let mut guard = state
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(slot) = guard.get_mut(index) {
        slot.apply(result);
        slot.next_poll = Some(SystemTime::now() + delay);
    }
    (delay, alerts)
}

pub fn spawn(
    mut provider: Box<dyn Provider>,
    index: usize,
    state: SharedState,
    mut tracker: Tracker,
    on_update: impl Fn() + Send + 'static,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("poll-{}", provider.id()))
        .spawn(move || {
            let mut delay = provider.poll_interval();
            loop {
                let alerts;
                (delay, alerts) = poll_once(provider.as_mut(), index, &state, delay, &mut tracker);
                on_update();
                alerts.iter().for_each(crate::notify::send);
                std::thread::sleep(delay);
            }
        })
        .expect("failed to spawn poller thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProviderStatus, Window};
    use std::collections::VecDeque;

    const I: Duration = Duration::from_secs(60);

    #[test]
    fn delay_rules() {
        let ok: Result<ProviderSnapshot, ProviderError> = Ok(ProviderSnapshot::new(vec![]));
        let net = Err(ProviderError::Network("x".into()));
        let parse = Err(ProviderError::Parse("x".into()));
        assert_eq!(next_delay(&ok, Duration::from_secs(480), I), I);
        assert_eq!(next_delay(&net, I, I), Duration::from_secs(120));
        assert_eq!(
            next_delay(&parse, Duration::from_secs(120), I),
            Duration::from_secs(240)
        );
        assert_eq!(next_delay(&net, Duration::from_secs(600), I), MAX_BACKOFF);
        assert_eq!(
            next_delay(&net, Duration::from_secs(1), I),
            Duration::from_secs(120)
        );
        assert_eq!(
            next_delay(&Err(ProviderError::Auth), Duration::from_secs(600), I),
            I
        );
        assert_eq!(next_delay(&Err(ProviderError::NoCredentials), I, I), I);
        let limited = |s| Err(ProviderError::RateLimited(s));
        assert_eq!(next_delay(&limited(None), I, I), Duration::from_secs(120));
        assert_eq!(
            next_delay(&limited(Some(Duration::from_secs(500))), I, I),
            Duration::from_secs(500)
        );
        assert_eq!(
            next_delay(&limited(Some(Duration::from_secs(86_400))), I, I),
            MAX_BACKOFF
        );
    }

    struct Scripted(VecDeque<Result<ProviderSnapshot, ProviderError>>);

    impl Provider for Scripted {
        fn id(&self) -> &str {
            "fake"
        }
        fn display_name(&self) -> &str {
            "Fake"
        }
        fn poll_interval(&self) -> Duration {
            I
        }
        fn poll(&mut self) -> Result<ProviderSnapshot, ProviderError> {
            self.0
                .pop_front()
                .unwrap_or(Err(ProviderError::Parse("script exhausted".into())))
        }
    }

    fn one_state() -> SharedState {
        Arc::new(RwLock::new(vec![ProviderState::new("fake", "Fake")]))
    }

    #[test]
    fn poll_once_updates_state_and_returns_delay() {
        let state = one_state();
        let mut p = Scripted(VecDeque::from(vec![
            Ok(ProviderSnapshot::new(vec![Window::new("5h", 1.0, None)])),
            Err(ProviderError::Network("down".into())),
        ]));
        let mut t = Tracker::disabled();
        assert_eq!(poll_once(&mut p, 0, &state, I, &mut t).0, I);
        assert_eq!(state.read().unwrap()[0].status, ProviderStatus::Ok);
        assert_eq!(
            poll_once(&mut p, 0, &state, I, &mut t).0,
            Duration::from_secs(120)
        );
        let st = state.read().unwrap()[0].clone();
        assert!(matches!(
            st.status,
            ProviderStatus::Error(ProviderError::Network(_))
        ));
        assert!(st.last_ok.is_some());
    }

    #[test]
    fn poll_once_ignores_out_of_range_index() {
        let state = one_state();
        let mut p = Scripted(VecDeque::from(vec![Ok(ProviderSnapshot::new(vec![]))]));
        assert_eq!(
            poll_once(&mut p, 7, &state, I, &mut Tracker::disabled()).0,
            I
        );
        assert_eq!(state.read().unwrap()[0].status, ProviderStatus::Pending);
    }

    #[test]
    fn spawn_runs_first_poll_and_notifies() {
        let state = one_state();
        let (tx, rx) = std::sync::mpsc::channel();
        let p = Scripted(VecDeque::from(vec![Ok(ProviderSnapshot::new(vec![]))]));
        let _handle = spawn(
            Box::new(p),
            0,
            state.clone(),
            Tracker::disabled(),
            move || {
                let _ = tx.send(());
            },
        );
        rx.recv_timeout(Duration::from_secs(5))
            .expect("no update notification");
        assert_eq!(state.read().unwrap()[0].status, ProviderStatus::Ok);
    }
}
