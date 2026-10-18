//! Minimal, dependency-free logger for the native messaging host.
//!
//! Firefox launches the host with inherited stdio pipes. stdout is reserved for
//! the native-messaging protocol, so diagnostics are appended to
//! `%LOCALAPPDATA%\RDM\logs\native.log`. Errors are additionally mirrored to
//! stderr, which Firefox forwards to the browser console — that is usually how
//! users discover a native host that refuses to start.
//!
//! Never log secret material here: callers redact URLs (`url_util::redact_url`)
//! and log only cookie *counts/lengths*, never cookie values.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Rotate the log once it grows beyond this size (keeps one previous file).
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

pub struct Logger {
    sink: Option<Mutex<File>>,
    path: PathBuf,
}

impl Logger {
    /// Open (or create) `<state_dir>/logs/native.log`.
    ///
    /// Logging must never take the host down: if the directory or file cannot
    /// be opened the logger degrades to stderr-only for warnings/errors.
    pub fn new(state_dir: &Path) -> Logger {
        let dir = state_dir.join("logs");
        let path = dir.join("native.log");
        let sink = open_log_file(&dir, &path).ok().map(Mutex::new);
        Logger { sink, path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn info(&self, message: &str) {
        self.log(Level::Info, message);
    }

    pub fn warn(&self, message: &str) {
        self.log(Level::Warn, message);
    }

    pub fn error(&self, message: &str) {
        self.log(Level::Error, message);
    }

    pub fn log(&self, level: Level, message: &str) {
        let line = format!(
            "{} [{}] [pid={}] {}\n",
            format_utc(SystemTime::now()),
            level.as_str(),
            std::process::id(),
            message
        );
        if let Some(sink) = &self.sink {
            if let Ok(mut file) = sink.lock() {
                // Ignore write errors: a full disk must not abort a handoff.
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
        if level != Level::Info {
            // Firefox forwards our stderr to the browser console.
            eprint!("{}", line);
        }
    }
}

fn open_log_file(dir: &Path, path: &Path) -> std::io::Result<File> {
    fs::create_dir_all(dir)?;
    if fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
        let rotated = dir.join("native.log.1");
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(path, &rotated);
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// `2000-01-01T00:00:00.000Z` style UTC timestamp without external crates.
pub fn format_utc(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let days = (secs / 86_400) as i64;
    let second_of_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60
    )
}

/// Howard Hinnant's `civil_from_days` algorithm (days since 1970-01-01).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn formats_epoch() {
        assert_eq!(format_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn formats_y2k() {
        assert_eq!(format_utc(at(946_684_800)), "2000-01-01T00:00:00.000Z");
    }

    #[test]
    fn formats_leap_day() {
        assert_eq!(format_utc(at(1_709_164_800)), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn formats_milliseconds_and_time_of_day() {
        let t = at(1_724_243_045) + Duration::from_millis(123);
        assert_eq!(format_utc(t), "2024-08-21T12:24:05.123Z");
    }

    #[test]
    fn logger_creates_file_and_appends() {
        let dir = std::env::temp_dir().join(format!(
            "rdm-native-log-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let logger = Logger::new(&dir);
        logger.info("hello");
        logger.warn("careful");
        let text = std::fs::read_to_string(logger.path()).unwrap();
        assert!(text.contains("[INFO] [pid="));
        assert!(text.contains("hello"));
        assert!(text.contains("careful"));
        assert_eq!(text.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
