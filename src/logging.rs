use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};
use parking_lot::Mutex;

const RECENT_CAPACITY: usize = 300;
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

pub struct Logger {
    file: Mutex<Option<File>>,
    recent: Mutex<VecDeque<String>>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub fn init(log_dir: &Path) {
    let _ = fs::create_dir_all(log_dir);
    let path = log_dir.join("ps5-lan-capture.log");
    if fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = fs::rename(&path, log_dir.join("ps5-lan-capture.old.log"));
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let logger = LOGGER.get_or_init(|| Logger {
        file: Mutex::new(file),
        recent: Mutex::new(VecDeque::with_capacity(RECENT_CAPACITY)),
    });
    let _ = log::set_logger(logger);
    log::set_max_level(if cfg!(debug_assertions) {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    });
}

pub fn recent_lines() -> Vec<String> {
    LOGGER
        .get()
        .map(|l| l.recent.lock().iter().cloned().collect())
        .unwrap_or_default()
}

fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{:03}Z", now.subsec_millis())
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let line = format!(
            "{} [{level}] {}: {}",
            timestamp(),
            record.target(),
            record.args()
        );
        if let Some(file) = self.file.lock().as_mut() {
            let _ = writeln!(file, "{line}");
        }
        if cfg!(debug_assertions) {
            eprintln!("{line}");
        }
        let mut recent = self.recent.lock();
        if recent.len() == RECENT_CAPACITY {
            recent.pop_front();
        }
        recent.push_back(line);
    }

    fn flush(&self) {
        if let Some(file) = self.file.lock().as_mut() {
            let _ = file.flush();
        }
    }
}
