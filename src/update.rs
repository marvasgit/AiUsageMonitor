//! Checks GitHub for a newer release and replaces the running executable with it.
//!
//! Releases carry the bare executables and a `SHA256SUMS` file; a download is installed
//! only when its hash matches the one listed there.

use crate::http;
use crate::model::ProviderError;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The running version: the release version in CI builds, Cargo.toml's otherwise.
pub const CURRENT: &str = env!("AUM_VERSION");
pub const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/marvasgit/AiUsageMonitor/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/marvasgit/AiUsageMonitor/releases/latest";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const DOWNLOAD_LIMIT: u64 = 128 * 1024 * 1024;
const SUMS_LIMIT: u64 = 64 * 1024;
const SUMS_NAME: &str = "SHA256SUMS";

/// The release asset holding this platform's executable; `None` where none is published.
pub const ASSET: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("ai-usage-monitor-linux-x86_64")
} else if cfg!(all(windows, target_arch = "x86_64")) {
    Some("ai-usage-monitor-windows-x86_64.exe")
} else {
    None
};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum UpdateError {
    #[error("{0}")]
    Network(String),
    #[error("unexpected release data: {0}")]
    Parse(String),
    #[error("no update is published for this system")]
    Unsupported,
    #[error("{SUMS_NAME} has no entry for {0}")]
    NoChecksum(String),
    #[error("download does not match its checksum")]
    Checksum,
    #[error("cannot install: {0}")]
    Install(String),
}

impl From<ProviderError> for UpdateError {
    fn from(e: ProviderError) -> Self {
        Self::Network(e.to_string())
    }
}

/// `major.minor.patch`, optionally prefixed with `v`; pre-releases do not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().trim_start_matches('v').split('.');
        let mut next = || parts.next()?.parse().ok();
        let version = Self(next()?, next()?, next()?);
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: Version,
    /// The release's web page, for systems without a published executable.
    pub page: String,
    /// This platform's executable and the checksum file, when the release has both.
    pub binary: Option<(Asset, Asset)>,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// Parses GitHub's release JSON, picking the `asset` executable and `SHA256SUMS`.
pub fn parse_release(body: &str, asset: Option<&str>) -> Result<Release, UpdateError> {
    let api: ApiRelease =
        serde_json::from_str(body).map_err(|e| UpdateError::Parse(e.to_string()))?;
    let version = Version::parse(&api.tag_name)
        .ok_or_else(|| UpdateError::Parse(format!("tag {:?} is not a version", api.tag_name)))?;
    let find = |name: &str| {
        api.assets.iter().find(|a| a.name == name).map(|a| Asset {
            name: a.name.clone(),
            url: a.browser_download_url.clone(),
        })
    };
    let binary = asset.and_then(|name| Some((find(name)?, find(SUMS_NAME)?)));
    Ok(Release {
        version,
        page: api.html_url,
        binary,
    })
}

/// The latest release at `url` when it is newer than `current`.
pub fn check(url: &str, current: Version) -> Result<Option<Release>, UpdateError> {
    let resp = http::get(url, &[("Accept", "application/vnd.github+json")])?;
    // GitHub answers 404 while a repository has no release yet.
    if resp.status == 404 {
        return Ok(None);
    }
    resp.check()?;
    let release = parse_release(&resp.body, ASSET)?;
    Ok((release.version > current).then_some(release))
}

/// The hash `sums` (`sha256sum` output) lists for the file `name`.
pub fn checksum_for<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name).then_some(hash)
    })
}

/// Downloads the release's executable and checks it against `SHA256SUMS`.
pub fn download_verified(release: &Release) -> Result<Vec<u8>, UpdateError> {
    let (binary, sums) = release.binary.as_ref().ok_or(UpdateError::Unsupported)?;
    let sums = http::download(&sums.url, SUMS_LIMIT)?;
    let sums = String::from_utf8_lossy(&sums);
    let expected = checksum_for(&sums, &binary.name)
        .ok_or_else(|| UpdateError::NoChecksum(binary.name.clone()))?;
    let bytes = http::download(&binary.url, DOWNLOAD_LIMIT)?;
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(UpdateError::Checksum);
    }
    Ok(bytes)
}

/// Downloads, verifies and swaps in the release's executable; the running process keeps
/// the old code until it restarts.
pub fn install(release: &Release) -> Result<(), UpdateError> {
    let bytes = download_verified(release)?;
    let staged = std::env::temp_dir().join(format!(
        "ai-usage-monitor-update-{}{}",
        std::process::id(),
        std::env::consts::EXE_SUFFIX
    ));
    let install = |e: std::io::Error| UpdateError::Install(e.to_string());
    std::fs::write(&staged, bytes).map_err(install)?;
    let result = self_replace::self_replace(&staged).map_err(install);
    if let Err(e) = std::fs::remove_file(&staged) {
        log::warn!("cannot remove staged update {}: {e}", staged.display());
    }
    result
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Automatic checks are off and none was asked for yet.
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing(Release),
    /// Swapped in; takes effect on restart.
    Installed(Release),
    Failed(String),
}

enum Command {
    Check,
    Install,
}

/// Runs checks and installs on a background thread and publishes its [`Status`].
pub struct Updater {
    status: Arc<Mutex<Status>>,
    commands: mpsc::Sender<Command>,
    /// Resolved at start: once replaced, the running file's path may no longer resolve.
    exe: Option<PathBuf>,
}

impl Updater {
    /// Checks `url` now and every few hours when `auto_check` is set, otherwise only on
    /// [`Updater::check_now`]. `on_change` runs after every status change.
    pub fn spawn(url: String, auto_check: bool, on_change: impl Fn() + Send + 'static) -> Self {
        let initial = if auto_check {
            Status::Checking
        } else {
            Status::Idle
        };
        let status = Arc::new(Mutex::new(initial));
        let (commands, rx) = mpsc::channel();
        if auto_check {
            commands.send(Command::Check).ok();
        }
        let shared = status.clone();
        let spawned = std::thread::Builder::new()
            .name("update".into())
            .spawn(move || {
                let set = |s: Status| {
                    *shared.lock().unwrap_or_else(|p| p.into_inner()) = s;
                    on_change();
                };
                let get = || shared.lock().unwrap_or_else(|p| p.into_inner()).clone();
                loop {
                    let command = if auto_check {
                        rx.recv_timeout(CHECK_EVERY)
                    } else {
                        rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
                    };
                    match (command, get()) {
                        (Err(RecvTimeoutError::Disconnected), _) => break,
                        // Nothing more to find until the installed version runs.
                        (_, Status::Installing(_) | Status::Installed(_)) => {}
                        (Ok(Command::Install), Status::Available(release)) => {
                            set(Status::Installing(release.clone()));
                            set(match install(&release) {
                                Ok(()) => Status::Installed(release),
                                Err(e) => {
                                    log::warn!("update to {} failed: {e}", release.version);
                                    Status::Failed(e.to_string())
                                }
                            });
                        }
                        (Ok(Command::Install), _) => {}
                        (Ok(Command::Check) | Err(RecvTimeoutError::Timeout), _) => {
                            set(Status::Checking);
                            set(check_status(&url));
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            log::warn!("cannot start update thread: {e}");
            *status.lock().unwrap_or_else(|p| p.into_inner()) = Status::Failed(e.to_string());
        }
        Self {
            status,
            commands,
            exe: std::env::current_exe().ok(),
        }
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn check_now(&self) {
        self.commands.send(Command::Check).ok();
    }

    /// Installs the available release; ignored unless the status is `Available`.
    pub fn install(&self) {
        self.commands.send(Command::Install).ok();
    }

    /// Starts the (updated) executable; the caller then closes this instance.
    pub fn restart(&self) -> std::io::Result<()> {
        let exe = self
            .exe
            .as_ref()
            .ok_or_else(|| std::io::Error::other("executable path unknown"))?;
        std::process::Command::new(exe).spawn().map(drop)
    }
}

fn check_status(url: &str) -> Status {
    let Some(current) = Version::parse(CURRENT) else {
        return Status::Failed(format!("running version {CURRENT:?} is not a version"));
    };
    match check(url, current) {
        Ok(Some(release)) => Status::Available(release),
        Ok(None) => Status::UpToDate,
        Err(e) => {
            log::warn!("update check failed: {e}");
            Status::Failed(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    const OTHER_HASH: &str = "c9fa4f3a6ec5a9ae1b4a2ac2a7b2bff2ef2ae2f3b6e0d0b7f6aa2ad8a5a1b3b5";

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn release_json(tag: &str, base: &str) -> String {
        serde_json::json!({
            "tag_name": tag,
            "html_url": format!("{base}/page"),
            "assets": [
                {"name": "ai-usage-monitor-linux-x86_64", "browser_download_url": format!("{base}/bin")},
                {"name": "ai-usage-monitor-windows-x86_64.exe", "browser_download_url": format!("{base}/exe")},
                {"name": "SHA256SUMS", "browser_download_url": format!("{base}/sums")}
            ]
        })
        .to_string()
    }

    fn release_with(server: &MockServer, sums: &str) -> Release {
        server.mock(|when, then| {
            when.method(GET).path("/bin");
            then.status(200).body("new binary");
        });
        let sums = sums.to_string();
        server.mock(|when, then| {
            when.method(GET).path("/sums");
            then.status(200).body(sums);
        });
        parse_release(
            &release_json("v9.0.0", &server.base_url()),
            Some("ai-usage-monitor-linux-x86_64"),
        )
        .unwrap()
    }

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(Version::parse("v2.10.0"), Some(Version(2, 10, 0)));
        assert_eq!(Version::parse(" 2.1.3 "), Some(Version(2, 1, 3)));
        assert!(Version::parse("2.10.0") > Version::parse("2.9.9"));
        assert_eq!(Version::parse("2.1"), None);
        assert_eq!(Version::parse("2.1.0.1"), None);
        assert_eq!(Version::parse("2.1.0-rc1"), None);
        assert!(Version::parse(CURRENT).is_some(), "{CURRENT}");
    }

    #[test]
    fn release_picks_platform_asset_and_checksums() {
        let r = parse_release(
            &release_json("v2.2.0", "http://h"),
            Some("ai-usage-monitor-linux-x86_64"),
        )
        .unwrap();
        assert_eq!(r.version, Version(2, 2, 0));
        assert_eq!(r.page, "http://h/page");
        let (bin, sums) = r.binary.unwrap();
        assert_eq!(
            (bin.url.as_str(), sums.url.as_str()),
            ("http://h/bin", "http://h/sums")
        );

        let none = parse_release(&release_json("v2.2.0", "http://h"), None).unwrap();
        assert_eq!(none.binary, None);
        let older = r#"{"tag_name":"v2.0.3","html_url":"p","assets":[{"name":"x.zip","browser_download_url":"u"}]}"#;
        assert_eq!(
            parse_release(older, Some("ai-usage-monitor-linux-x86_64"))
                .unwrap()
                .binary,
            None
        );
        assert!(matches!(
            parse_release(r#"{"tag_name":"nightly","html_url":"p"}"#, None),
            Err(UpdateError::Parse(_))
        ));
    }

    #[test]
    fn check_reports_only_newer_releases() {
        let server = MockServer::start();
        let body = release_json("v2.2.0", &server.base_url());
        server.mock(|when, then| {
            when.method(GET)
                .path("/latest")
                .header("accept", "application/vnd.github+json");
            then.status(200).body(body);
        });
        let url = server.url("/latest");
        let found = check(&url, Version(2, 1, 0)).unwrap().unwrap();
        assert_eq!(found.version, Version(2, 2, 0));
        assert_eq!(check(&url, Version(2, 2, 0)).unwrap(), None);
        assert_eq!(check(&url, Version(3, 0, 0)).unwrap(), None);
    }

    #[test]
    fn check_treats_missing_release_as_up_to_date_and_errors_as_errors() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/none");
            then.status(404);
        });
        server.mock(|when, then| {
            when.method(GET).path("/down");
            then.status(500);
        });
        assert_eq!(check(&server.url("/none"), Version(1, 0, 0)).unwrap(), None);
        assert!(matches!(
            check(&server.url("/down"), Version(1, 0, 0)),
            Err(UpdateError::Network(_))
        ));
    }

    #[test]
    fn checksum_lookup_reads_sha256sum_output() {
        let sums = format!(
            "{OTHER_HASH}  ai-usage-monitor-linux-x86_64\nabc *ai-usage-monitor-windows-x86_64.exe\n"
        );
        assert_eq!(
            checksum_for(&sums, "ai-usage-monitor-linux-x86_64"),
            Some(OTHER_HASH)
        );
        assert_eq!(
            checksum_for(&sums, "ai-usage-monitor-windows-x86_64.exe"),
            Some("abc")
        );
        assert_eq!(checksum_for(&sums, "ai-usage-monitor-linux"), None);
    }

    #[test]
    fn download_returns_bytes_only_when_checksum_matches() {
        let server = MockServer::start();
        let good = format!(
            "{}  ai-usage-monitor-linux-x86_64\n",
            sha256_hex(b"new binary")
        );
        let release = release_with(&server, &good);
        assert_eq!(download_verified(&release).unwrap(), b"new binary");

        let server = MockServer::start();
        let bad = format!("{OTHER_HASH}  ai-usage-monitor-linux-x86_64\n");
        assert_eq!(
            download_verified(&release_with(&server, &bad)),
            Err(UpdateError::Checksum)
        );

        let server = MockServer::start();
        let missing = format!("{OTHER_HASH}  something-else\n");
        assert!(matches!(
            download_verified(&release_with(&server, &missing)),
            Err(UpdateError::NoChecksum(_))
        ));

        let mut unsupported = release;
        unsupported.binary = None;
        assert_eq!(
            download_verified(&unsupported),
            Err(UpdateError::Unsupported)
        );
    }

    #[test]
    fn updater_checks_on_start_and_on_request() {
        let server = MockServer::start();
        let mut latest = server.mock(|when, then| {
            when.method(GET).path("/latest");
            then.status(200).body(release_json("v0.0.1", "http://h"));
        });
        let (changed, changes) = mpsc::channel();
        let wait_for = |want: &dyn Fn(&Status) -> bool, updater: &Updater| loop {
            changes
                .recv_timeout(Duration::from_secs(10))
                .expect("no status change");
            if want(&updater.status()) {
                break;
            }
        };
        let updater = Updater::spawn(server.url("/latest"), true, move || {
            changed.send(()).ok();
        });
        wait_for(&|s| *s == Status::UpToDate, &updater);

        latest.delete();
        server.mock(|when, then| {
            when.method(GET).path("/latest");
            then.status(200).body(release_json("v999.0.0", "http://h"));
        });
        updater.check_now();
        wait_for(
            &|s| matches!(s, Status::Available(r) if r.version == Version(999, 0, 0)),
            &updater,
        );
    }

    #[test]
    fn updater_without_auto_check_stays_idle() {
        let updater = Updater::spawn("http://127.0.0.1:9/".into(), false, || {});
        assert_eq!(updater.status(), Status::Idle);
    }
}
