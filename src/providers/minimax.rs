//! MiniMax Coding Plan: `coding_plan/remains` endpoint.

use super::{read_secret_file, Provider, ProviderError};
use crate::config::Config;
use crate::format::{from_unix_millis, window_label_from_minutes};
use crate::http;
use crate::model::{ProviderSnapshot, Window};
use crate::paths::Paths;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;
use zeroize::Zeroizing;

const POLL_INTERVAL: Duration = Duration::from_secs(120);
const GLOBAL_BASE: &str = "https://api.minimax.io";
const CN_BASE: &str = "https://api.minimaxi.com";
const REMAINS_PATH: &str = "/v1/api/openplatform/coding_plan/remains";
const PLAN_NAME: &str = "Coding Plan";
const MS_PER_MINUTE: i64 = 60_000;

pub fn base_url(region: &str) -> &'static str {
    match region {
        "cn" => CN_BASE,
        "global" => GLOBAL_BASE,
        other => {
            log::warn!("minimax: unknown region {other:?}; using global");
            GLOBAL_BASE
        }
    }
}

#[derive(Deserialize)]
struct MmxConfig<'a> {
    #[serde(borrow)]
    api_key: Option<&'a str>,
    region: Option<String>,
}

pub fn resolve_key(
    env_value: Option<String>,
    mmx_config: Option<&str>,
) -> Option<Zeroizing<String>> {
    let env_value = Zeroizing::new(env_value.unwrap_or_default());
    if !env_value.trim().is_empty() {
        return Some(Zeroizing::new(env_value.trim().to_string()));
    }
    let cfg: MmxConfig = serde_json::from_str(mmx_config?).ok()?;
    let key = cfg.api_key.map(str::trim).filter(|k| !k.is_empty())?;
    Some(Zeroizing::new(key.to_string()))
}

pub fn resolve_region(cfg_region: Option<&str>, mmx_config: Option<&str>) -> String {
    if let Some(region) = cfg_region {
        return region.to_string();
    }
    mmx_config
        .and_then(|text| serde_json::from_str::<MmxConfig>(text).ok())
        .and_then(|c| c.region)
        .unwrap_or_else(|| "global".to_string())
}

#[derive(Deserialize)]
struct RemainsResponse {
    #[serde(default)]
    model_remains: Vec<ModelRemains>,
    base_resp: Option<BaseResp>,
}

#[derive(Deserialize)]
struct BaseResp {
    status_code: i64,
    #[serde(default)]
    status_msg: String,
}

#[derive(Deserialize)]
struct ModelRemains {
    model_name: String,
    start_time: Option<i64>,
    end_time: Option<i64>,
    weekly_end_time: Option<i64>,
    current_interval_remaining_percent: Option<f64>,
    current_weekly_remaining_percent: Option<f64>,
}

impl ModelRemains {
    fn windows(&self, prefix: &str) -> Vec<Window> {
        let mut out = Vec::new();
        if let (Some(pct), Some(start), Some(end)) = (
            self.current_interval_remaining_percent,
            self.start_time,
            self.end_time,
        ) {
            let minutes = ((end - start).max(0) / MS_PER_MINUTE) as u64;
            let label = format!("{prefix}{}", window_label_from_minutes(minutes));
            out.push(Window::new(label, 100.0 - pct, from_unix_millis(end)));
        }
        if let Some(pct) = self.current_weekly_remaining_percent {
            let reset = self.weekly_end_time.and_then(from_unix_millis);
            out.push(Window::new(format!("{prefix}week"), 100.0 - pct, reset));
        }
        out
    }
}

pub fn parse_remains(body: &str, models: &[String]) -> Result<Vec<Window>, ProviderError> {
    let resp: RemainsResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    if let Some(base) = &resp.base_resp {
        if base.status_code != 0 {
            return Err(ProviderError::Parse(format!(
                "{}: {}",
                base.status_code, base.status_msg
            )));
        }
    }
    let windows: Vec<Window> = models
        .iter()
        .filter_map(|name| resp.model_remains.iter().find(|m| &m.model_name == name))
        .flat_map(|m| {
            let prefix = if models.len() > 1 {
                format!("{} ", m.model_name)
            } else {
                String::new()
            };
            m.windows(&prefix)
        })
        .collect();
    if windows.is_empty() {
        return Err(ProviderError::Parse(format!(
            "none of models {models:?} found in plan"
        )));
    }
    Ok(windows)
}

pub struct MiniMax {
    mmx_config: PathBuf,
    api_key_env: String,
    api_base: String,
    models: Vec<String>,
}

impl MiniMax {
    pub fn detect(cfg: &Config, paths: &Paths) -> Option<Self> {
        if !cfg.minimax.enabled {
            return None;
        }
        let mmx_config = paths.mmx_dir.join("config.json");
        let mmx_text = read_secret_file(&mmx_config);
        let mmx = mmx_text.as_deref().map(String::as_str);
        resolve_key(std::env::var(&cfg.minimax.api_key_env).ok(), mmx)?;
        let region = resolve_region(cfg.minimax.region.as_deref(), mmx);
        Some(Self {
            mmx_config,
            api_key_env: cfg.minimax.api_key_env.clone(),
            api_base: base_url(&region).to_string(),
            models: cfg.minimax.models.clone(),
        })
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }
}

impl Provider for MiniMax {
    fn id(&self) -> &str {
        "minimax"
    }

    fn display_name(&self) -> &str {
        "MiniMax"
    }

    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn poll(&mut self) -> Result<ProviderSnapshot, ProviderError> {
        let mmx_text = read_secret_file(&self.mmx_config);
        let key = resolve_key(
            std::env::var(&self.api_key_env).ok(),
            mmx_text.as_deref().map(String::as_str),
        )
        .ok_or(ProviderError::NoCredentials)?;
        let bearer = Zeroizing::new(format!("Bearer {}", key.as_str()));
        let resp = http::get(
            &format!("{}{REMAINS_PATH}", self.api_base),
            &[
                ("Authorization", bearer.as_str()),
                ("Accept", "application/json"),
            ],
        )?;
        resp.check()?;
        let mut snap = ProviderSnapshot::new(parse_remains(&resp.body, &self.models)?);
        snap.plan = Some(PLAN_NAME.to_string());
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    const REMAINS: &str = include_str!("../../tests/fixtures/minimax/remains.json");

    fn at_ms(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    #[test]
    fn base_urls_by_region() {
        assert_eq!(base_url("global"), "https://api.minimax.io");
        assert_eq!(base_url("cn"), "https://api.minimaxi.com");
        assert_eq!(base_url("mars"), "https://api.minimax.io");
    }

    #[test]
    fn resolve_key_prefers_env_then_mmx_file() {
        let mmx = r#"{"region":"global","api_key":"file-key"}"#;
        assert_eq!(
            resolve_key(Some("env-key".into()), Some(mmx))
                .unwrap()
                .as_str(),
            "env-key"
        );
        assert_eq!(resolve_key(None, Some(mmx)).unwrap().as_str(), "file-key");
        assert!(resolve_key(None, None).is_none());
    }

    #[test]
    fn resolve_key_ignores_blank_values() {
        assert!(resolve_key(Some("".into()), Some(r#"{"api_key":"  "}"#)).is_none());
        assert!(resolve_key(None, Some("not json")).is_none());
    }

    #[test]
    fn resolve_region_order() {
        assert_eq!(
            resolve_region(Some("cn"), Some(r#"{"region":"global"}"#)),
            "cn"
        );
        assert_eq!(resolve_region(None, Some(r#"{"region":"cn"}"#)), "cn");
        assert_eq!(resolve_region(None, None), "global");
    }

    #[test]
    fn parse_remains_general_only_by_default() {
        let w = parse_remains(REMAINS, &["general".into()]).unwrap();
        assert_eq!(
            w,
            vec![
                Window::new("5h", 8.0, Some(at_ms(1_791_039_600_000))),
                Window::new("week", 7.0, Some(at_ms(1_791_158_400_000))),
            ]
        );
    }

    #[test]
    fn parse_remains_prefixes_labels_for_multiple_models() {
        let w = parse_remains(REMAINS, &["general".into(), "video".into()]).unwrap();
        let labels: Vec<&str> = w.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["general 5h", "general week", "video 1d", "video week"]
        );
    }

    #[test]
    fn parse_remains_errors() {
        let bad =
            r#"{"model_remains":[],"base_resp":{"status_code":1004,"status_msg":"login fail"}}"#;
        assert_eq!(
            parse_remains(bad, &["general".into()]),
            Err(ProviderError::Parse("1004: login fail".into()))
        );
        assert!(matches!(
            parse_remains(REMAINS, &["music".into()]),
            Err(ProviderError::Parse(_))
        ));
        assert!(matches!(
            parse_remains("{", &["general".into()]),
            Err(ProviderError::Parse(_))
        ));
    }

    fn home_with_mmx() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".mmx")).unwrap();
        fs::write(
            home.path().join(".mmx/config.json"),
            r#"{"region":"global","api_key":"test-key"}"#,
        )
        .unwrap();
        home
    }

    fn cfg_without_env() -> Config {
        let mut cfg = Config::default();
        cfg.minimax.api_key_env = "AUM_TEST_UNSET_MINIMAX_KEY".into();
        cfg
    }

    #[test]
    fn detect_needs_key_and_enabled() {
        let home = home_with_mmx();
        let paths = Paths::for_home(home.path());
        assert!(MiniMax::detect(&cfg_without_env(), &paths).is_some());
        let mut cfg = cfg_without_env();
        cfg.minimax.enabled = false;
        assert!(MiniMax::detect(&cfg, &paths).is_none());
        let empty = tempfile::tempdir().unwrap();
        assert!(MiniMax::detect(&cfg_without_env(), &Paths::for_home(empty.path())).is_none());
    }

    #[test]
    fn poll_calls_remains_with_bearer_key() {
        let home = home_with_mmx();
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/api/openplatform/coding_plan/remains")
                .header("authorization", "Bearer test-key");
            then.status(200).body(REMAINS);
        });
        let mut p = MiniMax::detect(&cfg_without_env(), &Paths::for_home(home.path()))
            .unwrap()
            .with_api_base(server.base_url());
        let snap = p.poll().unwrap();
        m.assert();
        assert_eq!(snap.plan.as_deref(), Some("Coding Plan"));
        assert_eq!(snap.windows.len(), 2);
    }

    #[test]
    fn poll_maps_401_to_auth() {
        let home = home_with_mmx();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/api/openplatform/coding_plan/remains");
            then.status(401);
        });
        let mut p = MiniMax::detect(&cfg_without_env(), &Paths::for_home(home.path()))
            .unwrap()
            .with_api_base(server.base_url());
        assert_eq!(p.poll(), Err(ProviderError::Auth));
    }
}
