//! Builds the list of detected providers. Register new providers here.

use crate::accounts::{self, Account, Rules};
use crate::config::{AccountConfig, Config};
use crate::paths::Paths;
use crate::providers::claude::{self, Claude};
use crate::providers::copilot::Copilot;
use crate::providers::cursor::Cursor;
use crate::providers::minimax::MiniMax;
use crate::providers::openai::{self, OpenAi};
use crate::providers::Provider;
use std::path::Path;
use std::time::Duration;

/// Upper bound for scanning home for account folders (dead network mounts can block forever).
const SCAN_LIMIT: Duration = Duration::from_secs(3);

fn claude_valid(text: &str) -> bool {
    claude::parse_credentials(text).is_some()
}

fn codex_valid(text: &str) -> bool {
    openai::parse_auth(text).is_some()
}

const CLAUDE_RULES: Rules<'static> = Rules {
    cred_file: ".credentials.json",
    is_valid: claude_valid,
    strip: "claude",
};
const CODEX_RULES: Rules<'static> = Rules {
    cred_file: "auth.json",
    is_valid: codex_valid,
    strip: "codex",
};

/// Discovered accounts; on timeout only the CLI's default folder is used.
fn accounts_for(
    paths: &Paths,
    default_dir: &Path,
    rules: &'static Rules<'static>,
    extra: &[AccountConfig],
    hide: &[String],
) -> Vec<Account> {
    let (home, default) = (paths.home.clone(), default_dir.to_path_buf());
    let (extra, hide) = (extra.to_vec(), hide.to_vec());
    let scan_default = default.clone();
    let found = accounts::with_deadline(SCAN_LIMIT, move || {
        accounts::discover(&home, &scan_default, rules, &extra, &hide)
    });
    found.unwrap_or_else(|| {
        log::warn!(
            "account scan of {} timed out; using {} only",
            paths.home.display(),
            default.display()
        );
        vec![Account::new("", default)]
    })
}

pub fn all_providers(cfg: &Config, paths: &Paths) -> Vec<Box<dyn Provider>> {
    let mut found: Vec<Box<dyn Provider>> = Vec::new();
    if cfg.claude.enabled {
        let claude = &cfg.claude;
        for account in accounts_for(
            paths,
            &paths.claude_dir,
            &CLAUDE_RULES,
            &claude.accounts,
            &claude.hide,
        ) {
            if let Some(p) = Claude::detect(cfg, &account) {
                found.push(Box::new(p));
            }
        }
    }
    if cfg.openai.enabled {
        let openai = &cfg.openai;
        for account in accounts_for(
            paths,
            &paths.codex_home,
            &CODEX_RULES,
            &openai.accounts,
            &openai.hide,
        ) {
            if let Some(p) = OpenAi::detect(cfg, &account) {
                found.push(Box::new(p));
            }
        }
    }
    if let Some(p) = Copilot::detect(cfg, paths) {
        found.push(Box::new(p));
    }
    if let Some(p) = Cursor::detect(cfg, paths) {
        found.push(Box::new(p));
    }
    if let Some(p) = MiniMax::detect(cfg, paths) {
        found.push(Box::new(p));
    }
    let rank = |id: &str| {
        let kind = id.split(':').next().unwrap_or(id);
        cfg.order
            .iter()
            .position(|o| o == kind)
            .unwrap_or(usize::MAX)
    };
    found.sort_by_key(|p| rank(p.id()));
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn full_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        fs::create_dir_all(h.join(".claude")).unwrap();
        fs::write(
            h.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"a"}}"#,
        )
        .unwrap();
        fs::create_dir_all(h.join(".codex")).unwrap();
        fs::write(
            h.join(".codex/auth.json"),
            r#"{"tokens":{"access_token":"b"}}"#,
        )
        .unwrap();
        fs::create_dir_all(h.join(".mmx")).unwrap();
        fs::write(h.join(".mmx/config.json"), r#"{"api_key":"c"}"#).unwrap();
        home
    }

    fn cfg() -> Config {
        let mut cfg = Config::default();
        cfg.minimax.api_key_env = "AUM_TEST_UNSET_MINIMAX_KEY".into();
        // Copilot reads the real env/keychain; covered by its own tests.
        cfg.copilot.enabled = false;
        cfg
    }

    #[test]
    fn cursor_login_is_registered_after_openai() {
        let home = full_home();
        let h = home.path();
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            r#"{"sub":"auth0|u","exp":4000000000}"#,
        );
        let dir = h
            .join(".config")
            .join(if cfg!(windows) { "Cursor" } else { "cursor" });
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("auth.json"),
            format!(r#"{{"accessToken":"h.{payload}.s"}}"#),
        )
        .unwrap();
        assert_eq!(
            ids(&all_providers(&cfg(), &Paths::for_home(h))),
            vec!["claude", "openai", "cursor", "minimax"]
        );
    }

    fn ids(list: &[Box<dyn Provider>]) -> Vec<&str> {
        list.iter().map(|p| p.id()).collect()
    }

    #[test]
    fn detects_all_in_default_order() {
        let home = full_home();
        assert_eq!(
            ids(&all_providers(&cfg(), &Paths::for_home(home.path()))),
            vec!["claude", "openai", "minimax"]
        );
    }

    #[test]
    fn respects_configured_order_and_unknown_ids() {
        let home = full_home();
        let mut c = cfg();
        c.order = vec!["minimax".into(), "nonsense".into(), "claude".into()];
        assert_eq!(
            ids(&all_providers(&c, &Paths::for_home(home.path()))),
            vec!["minimax", "claude", "openai"]
        );
    }

    #[test]
    fn one_provider_per_account_grouped_by_kind() {
        let home = full_home();
        let h = home.path();
        fs::create_dir_all(h.join(".claude-personal")).unwrap();
        fs::write(
            h.join(".claude-personal/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"p"}}"#,
        )
        .unwrap();
        fs::create_dir_all(h.join(".codex-work")).unwrap();
        fs::write(
            h.join(".codex-work/auth.json"),
            r#"{"tokens":{"access_token":"w"}}"#,
        )
        .unwrap();
        let found = all_providers(&cfg(), &Paths::for_home(h));
        assert_eq!(
            ids(&found),
            vec![
                "claude",
                "claude:personal",
                "openai",
                "openai:work",
                "minimax"
            ]
        );
        let mut c = cfg();
        c.order = vec!["openai".into(), "claude".into()];
        c.claude.hide = vec!["default".into()];
        let found = all_providers(&c, &Paths::for_home(h));
        assert_eq!(
            ids(&found),
            vec!["openai", "openai:work", "claude:personal", "minimax"]
        );
    }

    #[test]
    fn skips_disabled_and_undetected() {
        let home = full_home();
        fs::remove_file(home.path().join(".codex/auth.json")).unwrap();
        let mut c = cfg();
        c.claude.enabled = false;
        assert_eq!(
            ids(&all_providers(&c, &Paths::for_home(home.path()))),
            vec!["minimax"]
        );
    }
}
