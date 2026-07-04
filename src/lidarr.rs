//! Lidarr Custom Script integration.
//!
//! When Lidarr invokes the binary as a Custom Script, it passes event data
//! through environment variables instead of command-line arguments:
//!
//! | Variable                  | Meaning                                   |
//! |---------------------------|-------------------------------------------|
//! | `lidarr_eventtype`        | `Test`, `AlbumDownload`, `Grab`, ...      |
//! | `lidarr_addedtrackpaths`  | pipe-separated list of imported files     |
//! | `lidarr_artist_path`      | root directory of the artist              |
//! | `lidarr_album_title`      | title of the imported album               |
//!
//! On `AlbumDownload` we locate the album directory (from the imported
//! track paths, falling back to a title-based directory search, then to
//! syncing the whole artist folder), embed lyrics for every track, and
//! log the result next to the binary.

use std::path::{Path, PathBuf};

use crate::fs_util;
use crate::http_client::HttpClient;
use crate::logger;
use crate::sync;
use crate::types::{SyncConfig, TrackOutcome};

const LIDARR_THREADS: u32 = 4;

/// `true` if the `lidarr_eventtype` environment variable is set, i.e. the
/// process was launched by Lidarr as a Custom Script.
pub fn detected() -> bool {
    std::env::var_os("lidarr_eventtype").is_some()
}

/// Run the Lidarr handler. `self_path` is `argv[0]`, used to place the log
/// files next to the binary. Returns the process exit code.
pub fn run(self_path: &Path) -> i32 {
    // Set up logging beside the binary.
    let main_log = logger::path_with_suffix(self_path, ".log");
    logger::install(&main_log);
    let plain_log = logger::path_with_suffix(self_path, "_plain.log");
    let missing_log = logger::path_with_suffix(self_path, "_missing.log");

    let event = match std::env::var("lidarr_eventtype") {
        Ok(v) => v,
        Err(_) => {
            logger::log("ERROR: lidarr_eventtype not set");
            return 1;
        }
    };

    if event == "Test" {
        logger::log("Test OK");
        return 0;
    }
    if event != "AlbumDownload" {
        logger::log(&format!("Ignoring event: {event}"));
        return 0;
    }

    let artist_path = std::env::var_os("lidarr_artist_path").map(PathBuf::from);

    // Strategy 1: derive the album directory from the imported track paths.
    let mut album_dir = album_dir_from_tracks();

    // Strategy 2: match the album title under the artist directory.
    if album_dir.is_none() {
        if let Some(ap) = &artist_path {
            if let Ok(title) = std::env::var("lidarr_album_title") {
                album_dir = album_dir_from_title(ap, &title);
            }
        }
    }

    // Set up HTTP and run the sync.
    let client = match HttpClient::new() {
        Ok(c) => c,
        Err(e) => {
            logger::log(&format!("ERROR: failed to initialise HTTP client: {e}"));
            return 1;
        }
    };

    let config = SyncConfig {
        force: false,
        clean_lrc: false, // safe default for Lidarr
        prefer_synced: true,
        num_threads: LIDARR_THREADS,
        out_plain: Some(plain_log.clone()),
        out_missing: Some(missing_log.clone()),
    };

    let exit_code;
    if let Some(dir) = &album_dir {
        logger::log(&format!("Album: {}", dir.display()));
        sync_dir(dir, &client, &config);
        exit_code = 0;
    } else if let Some(ap) = &artist_path {
        logger::log(&format!(
            "Album dir not found, syncing artist: {}",
            ap.display()
        ));
        for sub in fs_util::subdirs(ap) {
            sync_dir(&sub, &client, &config);
        }
        exit_code = 0;
    } else {
        logger::log("ERROR: could not determine album directory");
        exit_code = 1;
    }

    exit_code
}

/// Extract the album directory from `lidarr_addedtrackpaths`. The variable
/// contains pipe-separated full file paths; we take the first path and
/// strip the filename to get its parent directory.
fn album_dir_from_tracks() -> Option<PathBuf> {
    let paths = std::env::var("lidarr_addedtrackpaths").ok()?;
    let first = paths.split('|').next()?;
    let p = Path::new(first);
    p.parent().map(|p| p.to_path_buf())
}

/// Search `artist_path` for a subdirectory whose name contains `title`.
fn album_dir_from_title(artist_path: &Path, title: &str) -> Option<PathBuf> {
    for sub in fs_util::subdirs(artist_path) {
        if let Some(name) = sub.file_name().and_then(|n| n.to_str()) {
            if name.contains(title) {
                return Some(sub);
            }
        }
    }
    None
}

/// Scan and sync a single directory, logging a per-track line and a summary.
fn sync_dir(dir: &Path, client: &HttpClient, config: &SyncConfig) {
    let tracks = crate::fs_util::scan_dir(dir);
    if tracks.is_empty() {
        logger::log(&format!("No audio files found in '{}'", dir.display()));
        return;
    }
    logger::log(&format!(
        "Syncing {} track(s) in '{}'",
        tracks.len(),
        dir.display()
    ));

    let progress: crate::types::ProgressFn = std::sync::Arc::new(|idx, total, title, status| {
        logger::log(&format!(
            "  [{:>2}/{}] {:<40} {}",
            idx + 1,
            total,
            truncate(title, 40),
            status
        ));
    });

    let result = sync::sync_tracks(&tracks, config, client, Some(progress));
    logger::log(&format!(
        "Done: {} synced, {} plain, {} skipped, {} not found",
        result.synced, result.plain, result.skipped, result.not_found
    ));
    let _ = TrackOutcome::Synced; // keep the import meaningful even if unused
}

/// Truncate `s` to at most `max` characters, appending nothing (the log
/// format pads with spaces).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
