//! App configuration (`config.toml`); every key is optional.

use serde::Deserialize;
use std::path::Path;

const MAX_CONFIG_BYTES: u64 = 256 * 1024;

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
            ],
            claude: ClaudeConfig::default(),
            openai: OpenAiConfig::default(),
            copilot: CopilotConfig::default(),
            cursor: CursorConfig::default(),
            minimax: MiniMaxConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub enabled: bool,
    pub context_limit: Option<u64>,
    pub hide: Vec<String>,
    pub accounts: Vec<AccountConfig>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            context_limit: None,
            hide: Vec::new(),
            accounts: Vec::new(),
        }
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
    fn empty_text_gives_defaults() {
        let (cfg, ignored) = parse("").unwrap();
        assert_eq!(cfg, Config::default());
        assert!(ignored.is_empty());
        assert_eq!(
            cfg.order,
            vec!["claude", "openai", "copilot", "cursor", "minimax"]
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
