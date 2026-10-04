//! Per-endpoint backoff for providers that keep showing fallback data while an API is failing.
//!
//! Such polls still succeed, so the poller's own backoff never kicks in; this one does.

use crate::model::ProviderError;
use crate::poller;
use std::time::{Duration, Instant};

struct Hold {
    until: Instant,
    delay: Duration,
    error: ProviderError,
}

pub struct Backoff {
    interval: Duration,
    hold: Option<Hold>,
}

impl Backoff {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            hold: None,
        }
    }

    /// Runs `call` unless holding off after a transient failure, in which case that
    /// failure is returned again without calling.
    pub fn call<T>(
        &mut self,
        call: impl FnOnce() -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        if let Some(hold) = &self.hold {
            if Instant::now() < hold.until {
                return Err(hold.error.clone());
            }
        }
        let result = call();
        self.hold = match &result {
            Err(e) if e.is_transient() => {
                let prev = self.hold.as_ref().map_or(Duration::ZERO, |h| h.delay);
                let delay = poller::error_delay(e, prev, self.interval);
                Some(Hold {
                    until: Instant::now() + delay,
                    delay,
                    error: e.clone(),
                })
            }
            _ => None,
        };
        result
    }

    pub fn reset(&mut self) {
        self.hold = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const I: Duration = Duration::from_secs(60);

    #[test]
    fn transient_error_holds_off_further_calls() {
        let calls = Cell::new(0);
        let mut b = Backoff::new(I);
        let fail = || {
            calls.set(calls.get() + 1);
            Err::<(), _>(ProviderError::RateLimited(None))
        };
        assert_eq!(b.call(fail), Err(ProviderError::RateLimited(None)));
        assert_eq!(b.call(fail), Err(ProviderError::RateLimited(None)));
        assert_eq!(calls.get(), 1);
        b.reset();
        assert_eq!(b.call(|| Ok(7)), Ok(7));
    }

    #[test]
    fn auth_errors_and_successes_do_not_hold() {
        let calls = Cell::new(0);
        let mut b = Backoff::new(I);
        let auth = || {
            calls.set(calls.get() + 1);
            Err::<(), _>(ProviderError::Auth)
        };
        let _ = b.call(auth);
        let _ = b.call(auth);
        assert_eq!(calls.get(), 2);
        assert_eq!(b.call(|| Ok(1)), Ok(1));
        assert_eq!(b.call(|| Ok(2)), Ok(2));
    }
}
