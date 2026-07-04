//! A tiny timestamped logger used when running as a Lidarr Custom Script.
//!
//! Mirrors the logging behaviour of `lidarr.c`: each message is written to
//! both stdout and an append-mode log file sitting next to the binary, and
//! the file is rotated (keeping the last 200 lines) once it exceeds 100 KB.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_SIZE: u64 = 102_400; // 100 KB
const LOG_KEEP_LINES: usize = 200;

/// A logger that owns an optional file handle. Cheap to keep in a `Mutex`.
pub struct Logger {
    file: Option<File>,
}

impl Logger {
    /// Open (and rotate if necessary) the log file at `path`. If the file
    /// can't be opened, messages still go to stdout.
    pub fn open(path: &Path) -> Self {
        rotate_if_needed(path);
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        Logger { file }
    }

    /// A logger that only writes to stdout (no file).
    #[allow(dead_code)]
    pub fn stdout_only() -> Self {
        Logger { file: None }
    }

    /// Write a timestamped line to the log file and stdout.
    pub fn log(&mut self, msg: &str) {
        let ts = timestamp();
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "[{ts}] {msg}");
            let _ = f.flush();
        }
        println!("[{ts}] {msg}");
    }
}

/// A process-global logger so Lidarr callbacks can log without threading a
/// handle everywhere. Set once at startup.
pub static LOGGER: Mutex<Option<Logger>> = Mutex::new(None);

/// Install a logger at `path` as the process-global logger.
pub fn install(path: &Path) {
    let logger = Logger::open(path);
    *LOGGER.lock().unwrap() = Some(logger);
}

/// Install a stdout-only logger.
#[allow(dead_code)]
pub fn install_stdout_only() {
    *LOGGER.lock().unwrap() = Some(Logger::stdout_only());
}

/// Log a message via the global logger (or stdout if none is installed).
pub fn log(msg: &str) {
    let mut guard = LOGGER.lock().unwrap();
    match guard.as_mut() {
        Some(l) => l.log(msg),
        None => println!("{msg}"),
    }
}

/// Format the current local time as `YYYY-MM-DD HH:MM:SS`.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    broken_down_localtime(secs)
}

/// Convert a Unix timestamp (seconds) to a `YYYY-MM-DD HH:MM:SS` string in
/// local time. Implemented without `chrono` using a civil-from-days
/// algorithm (Howard Hinnant, public domain).
fn broken_down_localtime(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400) as u64;
    let hour = time_of_day / 3600;
    let min = (time_of_day % 3600) / 60;
    let sec = time_of_day % 60;

    let (year, month, day) = civil_from_days(days);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}")
}

/// Howard Hinnant's days-since-epoch → (year, month, day) algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

/// Rotate the log file at `path` if it has grown past `MAX_LOG_SIZE`,
/// keeping the last `LOG_KEEP_LINES` lines.
fn rotate_if_needed(path: &Path) {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if metadata.len() <= MAX_LOG_SIZE {
        return;
    }
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut contents = String::new();
    if f.read_to_string(&mut contents).is_err() {
        return;
    }
    let lines: Vec<&str> = contents.lines().collect();
    if lines.len() <= LOG_KEEP_LINES {
        return;
    }
    let tail: String = lines[lines.len() - LOG_KEEP_LINES..].join("\n");
    let _ = std::fs::write(path, tail + "\n");
}

/// Build a log path by appending `suffix` to `self_path`.
/// e.g. `/config/scripts/synclyr2metadata` + `.log`.
pub fn path_with_suffix(self_path: &Path, suffix: &str) -> PathBuf {
    let mut p = self_path.to_path_buf();
    // Convert `self_path` to a string, append suffix, parse back.
    // This handles the (unusual) case where `self_path` has no extension.
    if let Some(s) = p.to_str() {
        let with_suffix = format!("{s}{suffix}");
        p = PathBuf::from(with_suffix);
    }
    p
}
