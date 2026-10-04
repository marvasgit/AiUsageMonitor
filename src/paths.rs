//! Filesystem locations used by the app and by the CLIs it reads.

use std::path::{Path, PathBuf};

const APP_DIR: &str = "ai-usage-monitor";

#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub claude_dir: PathBuf,
    pub codex_home: PathBuf,
    pub mmx_dir: PathBuf,
    /// Per-user config root: XDG config dir on Linux, `%APPDATA%` on Windows.
    pub config_home: PathBuf,
    /// `%LOCALAPPDATA%` on Windows; same as `config_home` elsewhere.
    pub local_config_home: PathBuf,
}

impl Paths {
    /// Real locations, honouring `CLAUDE_CONFIG_DIR` and `CODEX_HOME`.
    pub fn from_env() -> Option<Self> {
        let home = dirs::home_dir()?;
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| home.join(".config"))
            .join(APP_DIR);
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
        let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from);
        let mut paths = Self::with_overrides(home, config_dir, claude, codex);
        paths.config_home = dirs::config_dir().unwrap_or_else(|| paths.home.join(".config"));
        paths.local_config_home =
            dirs::config_local_dir().unwrap_or_else(|| paths.config_home.clone());
        Some(paths)
    }

    /// Default layout under an arbitrary home directory (used by tests).
    pub fn for_home(home: &Path) -> Self {
        Self::with_overrides(
            home.to_path_buf(),
            home.join(".config").join(APP_DIR),
            None,
            None,
        )
    }

    pub fn with_overrides(
        home: PathBuf,
        config_dir: PathBuf,
        claude_dir: Option<PathBuf>,
        codex_home: Option<PathBuf>,
    ) -> Self {
        Self {
            claude_dir: claude_dir.unwrap_or_else(|| home.join(".claude")),
            codex_home: codex_home.unwrap_or_else(|| home.join(".codex")),
            mmx_dir: home.join(".mmx"),
            config_home: home.join(".config"),
            local_config_home: home.join(".config"),
            config_dir,
            home,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_home_uses_default_layout() {
        let p = Paths::for_home(Path::new("/h"));
        assert_eq!(p.claude_dir, PathBuf::from("/h/.claude"));
        assert_eq!(p.codex_home, PathBuf::from("/h/.codex"));
        assert_eq!(p.mmx_dir, PathBuf::from("/h/.mmx"));
        assert_eq!(p.config_home, PathBuf::from("/h/.config"));
        assert_eq!(p.local_config_home, PathBuf::from("/h/.config"));
        assert_eq!(
            p.config_file(),
            PathBuf::from("/h/.config/ai-usage-monitor/config.toml")
        );
    }

    #[test]
    fn overrides_replace_cli_dirs() {
        let p = Paths::with_overrides(
            PathBuf::from("/h"),
            PathBuf::from("/cfg"),
            Some(PathBuf::from("/c")),
            Some(PathBuf::from("/x")),
        );
        assert_eq!(p.claude_dir, PathBuf::from("/c"));
        assert_eq!(p.codex_home, PathBuf::from("/x"));
        assert_eq!(p.config_dir, PathBuf::from("/cfg"));
    }
}
