//! Minimal append-only file logger (`~/Library/Logs/Remounty.log`).
//!
//! Logging must never interfere with the app: every failure here is swallowed
//! and messages are mirrored to stderr so they show up when run from a terminal.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn init() {
    let Some(dir) = crate::paths::logs_dir() else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("Remounty.log");
    // Keep one previous log around instead of growing forever.
    if let Ok(meta) = fs::metadata(&path)
        && meta.len() > MAX_LOG_BYTES
    {
        let _ = fs::rename(&path, dir.join("Remounty.old.log"));
    }
    if let Ok(mut guard) = LOG_PATH.lock() {
        *guard = Some(path);
    }
}

pub fn log_path() -> Option<PathBuf> {
    LOG_PATH.lock().ok().and_then(|guard| guard.clone())
}

pub fn write(level: &str, message: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{}] {level:5} {message}\n", format_utc(secs));
    eprint!("{line}");
    let Ok(guard) = LOG_PATH.lock() else {
        return;
    };
    if let Some(path) = guard.as_ref()
        && let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Formats a Unix timestamp as `YYYY-MM-DD HH:MM:SS` (UTC) without extra crates.
fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days algorithm by Howard Hinnant.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}Z")
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logging::write("INFO", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logging::write("WARN", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::logging::write("ERROR", &format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::format_utc;

    #[test]
    fn formats_known_timestamps() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00Z");
        assert_eq!(format_utc(1_790_000_000), "2026-09-21 14:13:20Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00Z");
    }
}
