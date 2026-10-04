//! Minimal blocking HTTPS GET with timeout and body cap.

use crate::model::ProviderError;
use std::time::Duration;

pub const TIMEOUT: Duration = Duration::from_secs(15);
pub const BODY_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
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
    let body = resp
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|e| ProviderError::Network(e.to_string()))?;
    Ok(HttpResponse { status, body })
}

pub fn check_status(status: u16) -> Result<(), ProviderError> {
    match status {
        200..=299 => Ok(()),
        401 | 403 => Err(ProviderError::Auth),
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
        assert_eq!(
            check_status(429),
            Err(ProviderError::Network("HTTP 429".into()))
        );
    }
}
