//! Minimal blocking HTTPS GET with timeout and body cap.

use crate::model::ProviderError;
use std::time::{Duration, SystemTime};

pub const TIMEOUT: Duration = Duration::from_secs(15);
pub const BODY_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    /// `Retry-After`, from either its seconds or its HTTP-date form.
    pub retry_after: Option<Duration>,
}

impl HttpResponse {
    /// Like [`check_status`], but a 429 carries the server's `Retry-After`.
    pub fn check(&self) -> Result<(), ProviderError> {
        match self.status {
            429 => Err(ProviderError::RateLimited(self.retry_after)),
            status => check_status(status),
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .user_agent(concat!("ai-usage-monitor/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// GET `url`. Non-2xx statuses are returned; only transport failures are errors.
/// Error text comes from ureq and never contains header values.
pub fn get(url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, ProviderError> {
    let mut req = agent().get(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let mut resp = req
        .call()
        .map_err(|e| ProviderError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_retry_after(v, SystemTime::now()));
    let body = resp
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|e| ProviderError::Network(e.to_string()))?;
    Ok(HttpResponse {
        status,
        body,
        retry_after,
    })
}

/// `Retry-After` is either delay seconds or an HTTP date; a date in the past means now.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at: SystemTime = chrono::DateTime::parse_from_rfc2822(value).ok()?.into();
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

pub fn check_status(status: u16) -> Result<(), ProviderError> {
    match status {
        200..=299 => Ok(()),
        401 | 403 => Err(ProviderError::Auth),
        429 => Err(ProviderError::RateLimited(None)),
        other => Err(ProviderError::Network(format!("HTTP {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[test]
    fn get_returns_status_and_body_and_sends_headers() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/u")
                .header("authorization", "Bearer t");
            then.status(200).body("{\"ok\":true}");
        });
        let resp = get(&server.url("/u"), &[("Authorization", "Bearer t")]).unwrap();
        m.assert();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "{\"ok\":true}");
    }

    #[test]
    fn non_2xx_is_returned_not_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/u");
            then.status(401).body("nope");
        });
        assert_eq!(get(&server.url("/u"), &[]).unwrap().status, 401);
    }

    #[test]
    fn oversized_body_is_network_error() {
        let server = MockServer::start();
        let big = "x".repeat(BODY_LIMIT as usize + 10);
        server.mock(|when, then| {
            when.method(GET).path("/big");
            then.status(200).body(big);
        });
        assert!(matches!(
            get(&server.url("/big"), &[]),
            Err(ProviderError::Network(_))
        ));
    }

    #[test]
    fn connection_refused_is_network_error() {
        assert!(matches!(
            get("http://127.0.0.1:9/", &[]),
            Err(ProviderError::Network(_))
        ));
    }

    #[test]
    fn check_status_classifies() {
        assert_eq!(check_status(200), Ok(()));
        assert_eq!(check_status(204), Ok(()));
        assert_eq!(check_status(401), Err(ProviderError::Auth));
        assert_eq!(check_status(403), Err(ProviderError::Auth));
        assert_eq!(
            check_status(500),
            Err(ProviderError::Network("HTTP 500".into()))
        );
        assert_eq!(check_status(429), Err(ProviderError::RateLimited(None)));
    }

    #[test]
    fn rate_limit_carries_retry_after() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/limited");
            then.status(429).header("retry-after", "300");
        });
        let resp = get(&server.url("/limited"), &[]).unwrap();
        assert_eq!(
            resp.check(),
            Err(ProviderError::RateLimited(Some(Duration::from_secs(300))))
        );
        server.mock(|when, then| {
            when.method(GET).path("/limited-junk");
            then.status(429).header("retry-after", "soon");
        });
        let resp = get(&server.url("/limited-junk"), &[]).unwrap();
        assert_eq!(resp.check(), Err(ProviderError::RateLimited(None)));
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let now: SystemTime = chrono::DateTime::parse_from_rfc2822("Wed, 21 Oct 2026 07:28:00 GMT")
            .unwrap()
            .into();
        assert_eq!(
            parse_retry_after(" 90 ", now),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:30:00 GMT", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }
}
