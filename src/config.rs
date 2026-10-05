//! App configuration (`config.toml`); every key is optional.

use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const CLAUDE_DEFAULT_POLL_SECS: u64 = 120;
const CLAUDE_MIN_POLL_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub always_on_top: bool,
    pub order: Vec<String>,
    pub claude: ClaudeConfig,
    pub openai: OpenAiConfig,
    pub copilot: CopilotConfig,
    pub cursor: CursorConfig,
    pub minimax: MiniMaxConfig,
    pub openrouter: OpenRouterConfig,
    pub omp: OmpConfig,
    pub omo: OmoConfig,
    pub notifications: NotificationsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            always_on_top: false,
            order: vec![
                "claude".into(),
                "openai".into(),
                "copilot".into(),
                "cursor".into(),
                "minimax".into(),
                "openrouter".into(),
            ],
            claude: ClaudeConfig::default(),
            openai: OpenAiConfig::default(),
            copilot: CopilotConfig::default(),
            cursor: CursorConfig::default(),
            minimax: MiniMaxConfig::default(),
            openrouter: OpenRouterConfig::default(),
            omp: OmpConfig::default(),
            omo: OmoConfig::default(),
            notifications: NotificationsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub enabled: bool,
    pub context_limit: Option<u64>,
    /// Seconds between usage-endpoint calls; the token is shared with Claude Code.
    pub poll_seconds: u64,
    pub hide: Vec<String>,
    pub accounts: Vec<AccountConfig>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            context_limit: None,
            poll_seconds: CLAUDE_DEFAULT_POLL_SECS,
            hide: Vec::new(),
            accounts: Vec::new(),
        }
    }
}

impl ClaudeConfig {
    /// `poll_seconds`, but never below the minimum.
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_seconds.max(CLAUDE_MIN_POLL_SECS))
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct OpenAiConfig {
    pub enabled: bool,
    pub live_poll: bool,
    pub hide: Vec<String>,
    pub accounts: Vec<AccountConfig>,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            live_poll: true,
            hide: Vec::new(),
            accounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MiniMaxConfig {
    pub enabled: bool,
    pub api_key_env: String,
    pub region: Option<String>,
    pub models: Vec<String>,
}

impl Default for MiniMaxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key_env: "MINIMAX_API_KEY".into(),
            region: None,
            models: vec!["general".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct OpenRouterConfig {
    pub enabled: bool,
    /// Environment variable that holds the OpenRouter API key.
    pub api_key_env: String,
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key_env: "OPENROUTER_API_KEY".into(),
        }
    }
}

/// omp's Claude subscription login, an extra token source for the Claude card of its account.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct OmpConfig {
    pub enabled: bool,
    /// omp's database; `~` is the home folder.
    pub db: String,
}

impl Default for OmpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            db: "~/.omp/agent/agent.db".into(),
        }
    }
}

/// omo's Claude subscription login, an extra token source for the Claude card of its account.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct OmoConfig {
    pub enabled: bool,
    /// omo's credential file; `~` is the home folder.
    pub auth: String,
}

impl Default for OmoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auth: "~/.omo/agent/auth.json".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct CopilotConfig {
    pub enabled: bool,
}

impl Default for CopilotConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct CursorConfig {
    pub enabled: bool,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Desktop notifications when a rate-limit window crosses one of `thresholds` (percent).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    pub enabled: bool,
    pub thresholds: Vec<u8>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            thresholds: vec![80, 95],
        }
    }
}

impl NotificationsConfig {
    pub fn tracker(&self) -> crate::alerts::Tracker {
        if self.enabled {
            crate::alerts::Tracker::new(&self.thresholds)
        } else {
            crate::alerts::Tracker::disabled()
        }
    }
}

/// One extra CLI config folder (an account) listed in `config.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    pub dir: String,
}

/// Parses config text; returns the config and the sorted paths of unknown keys.
pub fn parse(text: &str) -> Result<(Config, Vec<String>), String> {
    let mut ignored = Vec::new();
    let de = toml::Deserializer::parse(text).map_err(|e| e.to_string())?;
    let cfg: Config = serde_ignored::deserialize(de, |path| ignored.push(path.to_string()))
        .map_err(|e| e.to_string())?;
    ignored.sort();
    Ok((cfg, ignored))
}

/// Loads the config file; any problem is logged and defaults are used.
pub fn load_or_default(path: &Path) -> Config {
    let Ok(meta) = std::fs::metadata(path) else {
        return Config::default();
    };
    if meta.len() > MAX_CONFIG_BYTES {
        log::warn!("config file larger than {MAX_CONFIG_BYTES} bytes; using defaults");
        return Config::default();
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            log::warn!("cannot read config: {e}; using defaults");
            return Config::default();
        }
    };
    match parse(&text) {
        Ok((cfg, ignored)) => {
            for key in ignored {
                log::warn!("unknown config key ignored: {key}");
            }
            cfg
        }
        Err(e) => {
            log::warn!("invalid config: {e}; using defaults");
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_poll_interval_defaults_and_minimum() {
        let mut c = ClaudeConfig::default();
        assert_eq!(c.poll_interval(), Duration::from_secs(120));
        c.poll_seconds = 5;
        assert_eq!(c.poll_interval(), Duration::from_secs(60));
        c.poll_seconds = 300;
        assert_eq!(c.poll_interval(), Duration::from_secs(300));
    }

    #[test]
    fn empty_text_gives_defaults() {
        let (cfg, ignored) = parse("").unwrap();
        assert_eq!(cfg, Config::default());
        assert!(ignored.is_empty());
        assert_eq!(
            cfg.order,
            vec![
                "claude",
                "openai",
                "copilot",
                "cursor",
                "minimax",
                "openrouter"
            ]
        );
        assert!(cfg.copilot.enabled && cfg.cursor.enabled);
        assert!(cfg.claude.enabled && cfg.openai.enabled && cfg.minimax.enabled);
        assert!(cfg.openai.live_poll);
        assert_eq!(cfg.minimax.api_key_env, "MINIMAX_API_KEY");
        assert_eq!(cfg.minimax.models, vec!["general"]);
        assert_eq!(cfg.claude.context_limit, None);
        assert!(!cfg.always_on_top);
    }

    #[test]
    fn partial_sections_keep_other_defaults() {
        let (cfg, _) = parse("[openai]\nlive_poll = false\n[minimax]\nregion = \"cn\"\n").unwrap();
        assert!(!cfg.openai.live_poll);
        assert!(cfg.openai.enabled);
        assert_eq!(cfg.minimax.region.as_deref(), Some("cn"));
        assert_eq!(cfg.minimax.models, vec!["general"]);
    }

    #[test]
    fn unknown_keys_are_reported_not_fatal() {
        let (cfg, ignored) = parse("colour = \"red\"\n[claude]\nenabld = false\n").unwrap();
        assert!(cfg.claude.enabled);
        assert_eq!(
            ignored,
            vec!["claude.enabld".to_string(), "colour".to_string()]
        );
    }

    #[test]
    fn account_lists_and_hide_parse() {
        let text = r#"
[claude]
hide = ["default"]
[[claude.accounts]]
name = "work"
dir = "~/w"
[[openai.accounts]]
name = "o"
dir = "/x"
"#;
        let (cfg, ignored) = parse(text).unwrap();
        assert!(ignored.is_empty(), "{ignored:?}");
        assert_eq!(cfg.claude.hide, vec!["default"]);
        assert_eq!(
            cfg.claude.accounts,
            vec![AccountConfig {
                name: "work".into(),
                dir: "~/w".into()
            }]
        );
        assert_eq!(
            cfg.openai.accounts,
            vec![AccountConfig {
                name: "o".into(),
                dir: "/x".into()
            }]
        );
        assert!(cfg.openai.hide.is_empty());
    }

    #[test]
    fn account_without_dir_is_invalid() {
        assert!(parse("[[claude.accounts]]\nname = \"x\"\n").is_err());
    }

    #[test]
    fn copilot_and_cursor_can_be_disabled() {
        let (cfg, ignored) =
            parse("[copilot]\nenabled = false\n[cursor]\nenabled = false\n").unwrap();
        assert!(ignored.is_empty());
        assert!(!cfg.copilot.enabled && !cfg.cursor.enabled);
    }

    #[test]
    fn malformed_text_is_an_error() {
        assert!(parse("[claude\nenabled = ").is_err());
        assert!(parse("always_on_top = \"yes\"").is_err());
    }

    #[test]
    fn example_config_parses_without_unknown_keys() {
        let (cfg, ignored) = parse(include_str!("../config.example.toml")).unwrap();
        assert!(ignored.is_empty(), "unknown keys: {ignored:?}");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn load_or_default_handles_missing_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_or_default(&dir.path().join("none.toml")),
            Config::default()
        );
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[[[").unwrap();
        assert_eq!(load_or_default(&bad), Config::default());
        let big = dir.path().join("big.toml");
        std::fs::write(
            &big,
            format!(
                "# {}\nalways_on_top = true\n",
                "x".repeat(MAX_CONFIG_BYTES as usize)
            ),
        )
        .unwrap();
        assert_eq!(load_or_default(&big), Config::default());
        let good = dir.path().join("good.toml");
        std::fs::write(&good, "always_on_top = true").unwrap();
        assert!(load_or_default(&good).always_on_top);
    }
}
