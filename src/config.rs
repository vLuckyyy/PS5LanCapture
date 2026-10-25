use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

use crate::dpapi;

pub const CONFIG_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    P720,
    P1080,
}

impl Resolution {
    pub fn size(self) -> (u32, u32) {
        match self {
            Resolution::P720 => (1280, 720),
            Resolution::P1080 => (1920, 1080),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Resolution::P720 => "1280×720 (720p)",
            Resolution::P1080 => "1920×1080 (1080p)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Upscaling {
    Off,
    Auto,
    Fsr,
}

impl Upscaling {
    pub fn label(self) -> &'static str {
        match self {
            Upscaling::Off => "Off",
            Upscaling::Auto => "RTX Video Super Resolution (FSR fallback)",
            Upscaling::Fsr => "AMD FSR 1",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    pub resolution: Resolution,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub hevc: bool,
    pub hardware_decoding: bool,
    pub upscaling: Upscaling,
    pub sharpness: f32,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            resolution: Resolution::P1080,
            fps: 60,
            bitrate_kbps: 15_000,
            hevc: true,
            hardware_decoding: true,
            upscaling: Upscaling::Auto,
            sharpness: 0.8,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub device_id: Option<String>,
    pub volume: f32,
    pub muted: bool,
    pub latency_ms: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            device_id: None,
            volume: 0.8,
            muted: false,
            latency_ms: 60,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerConfig {
    pub forward_input: bool,
    pub rumble: bool,
    pub adaptive_triggers: bool,
    pub lightbar: bool,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            forward_input: true,
            rumble: true,
            adaptive_triggers: true,
            lightbar: true,
        }
    }
}

impl ControllerConfig {
    pub fn settings(&self) -> crate::controller::ControllerSettings {
        crate::controller::ControllerSettings {
            enabled: self.forward_input,
            rumble: self.rumble,
            triggers: self.adaptive_triggers,
            lightbar: self.lightbar,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub show_stats: bool,
    pub auto_reconnect: bool,
    pub minimize_to_tray: bool,
    pub auto_hide_controls: bool,
    pub check_for_updates: bool,
    pub window_size: Option<(u32, u32)>,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            show_stats: false,
            auto_reconnect: true,
            minimize_to_tray: false,
            auto_hide_controls: true,
            check_for_updates: true,
            window_size: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerCheck {
    pub pad_controls_game: Option<bool>,
    pub monitor_shows_game: Option<bool>,
    pub app_shows_video: Option<bool>,
    pub pc_plays_audio: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsoleEntry {
    pub host_id: String,
    pub nickname: String,
    pub address: String,
    pub is_ps5: bool,
    pub target: i32,
    pub mac: String,
    pub protected_secret: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleSecret {
    pub regist_key: [u8; 16],
    pub rp_key: [u8; 16],
}

impl ConsoleSecret {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(33);
        out.push(1);
        out.extend_from_slice(&self.regist_key);
        out.extend_from_slice(&self.rp_key);
        out
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 33 || bytes[0] != 1 {
            return None;
        }
        let mut regist_key = [0u8; 16];
        let mut rp_key = [0u8; 16];
        regist_key.copy_from_slice(&bytes[1..17]);
        rp_key.copy_from_slice(&bytes[17..33]);
        Some(Self { regist_key, rp_key })
    }

    pub fn seal(&self) -> Result<String, String> {
        Ok(B64.encode(dpapi::protect(&self.to_bytes())?))
    }

    pub fn open(sealed: &str) -> Result<Self, String> {
        let cipher = B64
            .decode(sealed)
            .map_err(|_| "Corrupted registration data".to_string())?;
        let plain = dpapi::unprotect(&cipher)?;
        Self::from_bytes(&plain).ok_or_else(|| "Unknown registration data format".to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    pub wizard_completed: bool,
    pub controller_check: ControllerCheck,
    pub consoles: Vec<ConsoleEntry>,
    pub last_console: Option<String>,
    pub protected_account_id: Option<String>,
    pub audio: AudioSettings,
    pub video: VideoSettings,
    pub ui: UiSettings,
    pub controller: ControllerConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            wizard_completed: false,
            controller_check: ControllerCheck::default(),
            consoles: Vec::new(),
            last_console: None,
            protected_account_id: None,
            audio: AudioSettings::default(),
            video: VideoSettings::default(),
            ui: UiSettings::default(),
            controller: ControllerConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<AppConfig>(&text) {
            Ok(mut config) => {
                config.sanitize();
                config
            }
            Err(e) => {
                log::warn!("Cannot read config ({e}), using defaults");
                let _ = fs::rename(path, path.with_extension("json.broken"));
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).map_err(|e| e.to_string())?;
        fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    fn sanitize(&mut self) {
        self.version = CONFIG_VERSION;
        self.audio.volume = self.audio.volume.clamp(0.0, 1.0);
        self.audio.latency_ms = self.audio.latency_ms.clamp(20, 400);
        if self.video.fps != 30 {
            self.video.fps = 60;
        }
        self.video.bitrate_kbps = self.video.bitrate_kbps.clamp(2_000, 40_000);
        self.video.sharpness = self.video.sharpness.clamp(0.0, 1.0);
    }

    pub fn console(&self, host_id: &str) -> Option<&ConsoleEntry> {
        self.consoles.iter().find(|c| c.host_id == host_id)
    }

    pub fn upsert_console(&mut self, entry: ConsoleEntry) {
        match self
            .consoles
            .iter_mut()
            .find(|c| c.host_id == entry.host_id)
        {
            Some(existing) => *existing = entry,
            None => self.consoles.push(entry),
        }
    }

    pub fn account_id(&self) -> Option<[u8; 8]> {
        let sealed = self.protected_account_id.as_ref()?;
        let plain = dpapi::unprotect(&B64.decode(sealed).ok()?).ok()?;
        plain.try_into().ok()
    }

    pub fn set_account_id(&mut self, account_id: &[u8; 8]) -> Result<(), String> {
        self.protected_account_id = Some(B64.encode(dpapi::protect(account_id)?));
        Ok(())
    }
}

pub struct Paths {
    pub config_file: PathBuf,
    pub log_dir: PathBuf,
}

impl Paths {
    pub fn resolve() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let base = match exe_dir {
            Some(dir) if dir.join("portable.txt").exists() => dir.join("data"),
            _ => std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("PS5 LAN Capture"),
        };
        Self {
            config_file: base.join("config.json"),
            log_dir: base.join("logs"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pslc-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = temp_dir("missing");
        let config = AppConfig::load(&dir.join("config.json"));
        assert_eq!(config, AppConfig::default());
        assert!(!config.wizard_completed);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("config.json");
        let mut config = AppConfig {
            wizard_completed: true,
            ..Default::default()
        };
        config.audio.volume = 0.33;
        config.audio.device_id = Some("wasapi:{abc}".into());
        config.video.resolution = Resolution::P720;
        config.upsert_console(ConsoleEntry {
            host_id: "C0151B54314D".into(),
            nickname: "PS5-621".into(),
            address: "192.168.1.33".into(),
            is_ps5: true,
            target: 1_000_100,
            mac: "00:11:22:33:44:55".into(),
            protected_secret: ConsoleSecret {
                regist_key: [7; 16],
                rp_key: [9; 16],
            }
            .seal()
            .unwrap(),
        });
        config.save(&path).unwrap();
        let loaded = AppConfig::load(&path);
        assert_eq!(loaded, config);
        let secret = ConsoleSecret::open(&loaded.consoles[0].protected_secret).unwrap();
        assert_eq!(secret.regist_key, [7; 16]);
        assert_eq!(secret.rp_key, [9; 16]);
    }

    #[test]
    fn secrets_are_not_stored_in_plain_text() {
        let sealed = ConsoleSecret {
            regist_key: *b"abcdef0123456789",
            rp_key: [1; 16],
        }
        .seal()
        .unwrap();
        let raw = B64.decode(&sealed).unwrap();
        assert!(!raw.windows(16).any(|w| w == b"abcdef0123456789"));
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = temp_dir("corrupt");
        let path = dir.join("config.json");
        fs::write(&path, "{ not json").unwrap();
        let config = AppConfig::load(&path);
        assert_eq!(config, AppConfig::default());
        assert!(path.with_extension("json.broken").exists());
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let dir = temp_dir("clamp");
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{"audio":{"volume":7.0,"latency_ms":1},"video":{"fps":144,"bitrate_kbps":999999}}"#,
        )
        .unwrap();
        let config = AppConfig::load(&path);
        assert_eq!(config.audio.volume, 1.0);
        assert_eq!(config.audio.latency_ms, 20);
        assert_eq!(config.video.fps, 60);
        assert_eq!(config.video.bitrate_kbps, 40_000);
    }

    #[test]
    fn account_id_roundtrip() {
        let mut config = AppConfig::default();
        config.set_account_id(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(config.account_id(), Some([1, 2, 3, 4, 5, 6, 7, 8]));
    }
}
