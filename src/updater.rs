use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, w};

use crate::http;

pub const REPOSITORY_URL: &str = "https://github.com/vLuckyyy/PS5LanCapture";
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/vLuckyyy/PS5LanCapture/releases/latest";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const INSTALLER_PREFIX: &str = "PS5-LAN-Capture-Setup-";
const CHECKSUMS_NAME: &str = "SHA256SUMS.txt";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let core = text.trim().trim_start_matches(['v', 'V']);
        let core = core.split(['-', '+']).next()?;
        let mut parts = core.split('.').map(|p| p.parse::<u32>());
        let major = parts.next()?.ok()?;
        let minor = parts.next().unwrap_or(Ok(0)).ok()?;
        let patch = parts.next().unwrap_or(Ok(0)).ok()?;
        Some(Self(major, minor, patch))
    }

    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("valid package version")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: Version,
    pub notes: String,
    pub page_url: String,
    pub installer: Option<Asset>,
    pub checksums: Option<Asset>,
}

pub fn parse_release(json: &serde_json::Value) -> Option<Release> {
    if json["draft"].as_bool() == Some(true) || json["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let version = Version::parse(json["tag_name"].as_str()?)?;
    let assets: Vec<Asset> = json["assets"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a["name"].as_str()?.to_string(),
                        url: a["browser_download_url"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let find =
        |predicate: &dyn Fn(&str) -> bool| assets.iter().find(|a| predicate(&a.name)).cloned();
    Some(Release {
        version,
        notes: json["body"].as_str().unwrap_or_default().trim().to_string(),
        page_url: json["html_url"]
            .as_str()
            .unwrap_or(REPOSITORY_URL)
            .to_string(),
        installer: find(&|n| n.starts_with(INSTALLER_PREFIX) && n.ends_with(".exe")),
        checksums: find(&|n| n == CHECKSUMS_NAME),
    })
}

fn fetch_latest() -> Result<Option<Release>, String> {
    let headers = "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28";
    let (status, body) = http::fetch("GET", LATEST_RELEASE_API, headers, &[], 2 << 20)?;
    match status {
        200 => {}
        404 => return Ok(None),
        code => return Err(format!("GitHub returned HTTP {code}")),
    }
    let json: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| "Invalid GitHub response".to_string())?;
    Ok(parse_release(&json).filter(|r| r.version > Version::current()))
}

pub fn expected_checksum(sums: &str, file_name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == file_name && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_file(path: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn download_installer(
    release: &Release,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let asset = release
        .installer
        .as_ref()
        .ok_or("This release has no installer")?;
    let dir = std::env::temp_dir().join("PS5LanCapture-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join(&asset.name);
    http::download(
        &asset.url,
        &target,
        &|done, total| {
            progress.done.store(done, Ordering::Relaxed);
            progress.total.store(total.unwrap_or(0), Ordering::Relaxed);
        },
        cancel,
    )?;
    if let Some(sums) = &release.checksums {
        let (status, body) = http::fetch("GET", &sums.url, "", &[], 1 << 16)?;
        if status != 200 {
            return Err(format!("Cannot download checksums (HTTP {status})"));
        }
        let expected = expected_checksum(&String::from_utf8_lossy(&body), &asset.name)
            .ok_or("The installer is missing from the checksum list")?;
        if sha256_file(&target)? != expected {
            let _ = std::fs::remove_file(&target);
            return Err("The downloaded installer is damaged (checksum mismatch)".into());
        }
    }
    Ok(target)
}

/// True when the app was installed by the setup (so it can update itself in place).
pub fn is_installed() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("unins000.exe").exists()))
        .unwrap_or(false)
}

/// Starts the downloaded setup in silent update mode. The caller must quit right after.
pub fn launch_installer(path: &std::path::Path) -> Result<(), String> {
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(path.as_os_str()),
            w!("/SILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /UPDATE=1"),
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err("Cannot start the installer".into())
    }
}

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
}

#[derive(Clone, Debug)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading(Release),
    Ready(Release, PathBuf),
    Failed(String),
}

enum Message {
    Checked(Result<Option<Release>, String>),
    Downloaded(Result<PathBuf, String>),
}

pub struct Updater {
    pub state: UpdateState,
    pub progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    tx: Sender<Message>,
    rx: Receiver<Message>,
    last_check: Option<Instant>,
    manual_check: bool,
    waker: Arc<dyn Fn() + Send + Sync>,
}

impl Updater {
    pub fn new(waker: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            state: UpdateState::Idle,
            progress: Arc::new(Progress::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            last_check: None,
            manual_check: false,
            waker,
        }
    }

    pub fn available(&self) -> Option<&Release> {
        match &self.state {
            UpdateState::Available(r) | UpdateState::Downloading(r) | UpdateState::Ready(r, _) => {
                Some(r)
            }
            _ => None,
        }
    }

    fn busy(&self) -> bool {
        matches!(
            self.state,
            UpdateState::Checking | UpdateState::Downloading(_)
        )
    }

    pub fn check_now(&mut self) {
        if self.busy() {
            return;
        }
        self.manual_check = true;
        self.spawn_check();
    }

    fn spawn_check(&mut self) {
        self.last_check = Some(Instant::now());
        self.state = UpdateState::Checking;
        let tx = self.tx.clone();
        let waker = self.waker.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Message::Checked(fetch_latest()));
            waker();
        });
    }

    pub fn start_download(&mut self) {
        let UpdateState::Available(release) = self.state.clone() else {
            return;
        };
        self.cancel.store(false, Ordering::Relaxed);
        self.progress.done.store(0, Ordering::Relaxed);
        self.progress.total.store(0, Ordering::Relaxed);
        self.state = UpdateState::Downloading(release.clone());
        let (tx, waker, progress, cancel) = (
            self.tx.clone(),
            self.waker.clone(),
            self.progress.clone(),
            self.cancel.clone(),
        );
        std::thread::spawn(move || {
            let result = download_installer(&release, &progress, &cancel);
            let _ = tx.send(Message::Downloaded(result));
            waker();
        });
    }

    pub fn cancel_download(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Returns the version of a newly found update so the UI can announce it once.
    pub fn tick(&mut self, automatic: bool) -> Option<Version> {
        let mut announced = None;
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Message::Checked(Ok(Some(release))) => {
                    log::info!("Update available: {}", release.version);
                    announced = Some(release.version);
                    self.state = UpdateState::Available(release);
                }
                Message::Checked(Ok(None)) => self.state = UpdateState::UpToDate,
                Message::Checked(Err(e)) => {
                    log::warn!("Update check failed: {e}");
                    self.state = if self.manual_check {
                        UpdateState::Failed(e)
                    } else {
                        UpdateState::Idle
                    };
                }
                Message::Downloaded(result) => {
                    let UpdateState::Downloading(release) = self.state.clone() else {
                        continue;
                    };
                    self.state = match result {
                        Ok(path) => UpdateState::Ready(release, path),
                        Err(e) if self.cancel.load(Ordering::Relaxed) => {
                            log::info!("Update download cancelled: {e}");
                            UpdateState::Available(release)
                        }
                        Err(e) => {
                            log::warn!("Update download failed: {e}");
                            UpdateState::Failed(e)
                        }
                    };
                }
            }
        }
        let due = self.last_check.is_none_or(|t| t.elapsed() > CHECK_INTERVAL);
        let idle = matches!(self.state, UpdateState::Idle | UpdateState::UpToDate);
        if automatic && due && idle {
            self.manual_check = false;
            self.spawn_check();
        }
        announced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("1.10"), Some(Version(1, 10, 0)));
        assert_eq!(Version::parse("v2.0.0-beta.1"), Some(Version(2, 0, 0)));
        assert_eq!(Version::parse("latest"), None);
        assert!(Version(1, 10, 0) > Version(1, 9, 9));
    }

    #[test]
    fn parses_github_release() {
        let json = serde_json::json!({
            "tag_name": "v9.1.0",
            "html_url": "https://github.com/vLuckyyy/PS5LanCapture/releases/tag/v9.1.0",
            "body": "Fixes",
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": "PS5-LAN-Capture-9.1.0-portable.zip", "browser_download_url": "https://x/zip"},
                {"name": "PS5-LAN-Capture-Setup-9.1.0.exe", "browser_download_url": "https://x/setup"},
                {"name": "SHA256SUMS.txt", "browser_download_url": "https://x/sums"}
            ]
        });
        let release = parse_release(&json).unwrap();
        assert_eq!(release.version, Version(9, 1, 0));
        assert_eq!(release.installer.unwrap().url, "https://x/setup");
        assert_eq!(release.checksums.unwrap().url, "https://x/sums");
        assert_eq!(release.notes, "Fixes");
    }

    #[test]
    fn ignores_prereleases() {
        let json = serde_json::json!({"tag_name": "v9.0.0", "prerelease": true, "assets": []});
        assert!(parse_release(&json).is_none());
    }

    #[test]
    fn finds_checksum_line() {
        let hash = "a".repeat(64);
        let sums = format!(
            "{hash}  PS5-LAN-Capture-Setup-1.0.1.exe\n{}  other.zip\n",
            "b".repeat(64)
        );
        assert_eq!(
            expected_checksum(&sums, "PS5-LAN-Capture-Setup-1.0.1.exe"),
            Some(hash)
        );
        assert_eq!(expected_checksum(&sums, "missing.exe"), None);
    }

    #[test]
    #[ignore = "contacts GitHub"]
    fn live_latest_release_request() {
        let result = fetch_latest();
        println!("{result:?}");
        assert!(result.is_ok());
    }
}
