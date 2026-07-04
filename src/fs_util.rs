//! Filesystem helpers: directory scanning and path utilities.
//!
//! These keep `std::fs` calls out of the higher-level modules and provide
//! the same "scan a directory for audio files, sorted by track number"
//! behaviour as `metadata_scan_dir` in the original C project.

use std::path::{Path, PathBuf};

use crate::metadata;
use crate::types::TrackMeta;

/// Return `true` if `path` is a directory (not a symlink to one).
#[allow(dead_code)]
pub fn is_directory(path: &Path) -> bool {
    path.is_dir()
}

/// Derive the `.lrc` sidecar path for an audio file: same basename, `.lrc`
/// extension. E.g. `01-track.flac` → `01-track.lrc`.
pub fn lrc_sidecar(audio_path: &Path) -> PathBuf {
    let mut p = audio_path.to_path_buf();
    p.set_extension("lrc");
    p
}

/// Scan `dir` for audio files (by extension), read each one's metadata,
/// sort by track number, and return the list. Non-audio and unreadable
/// files are silently skipped, matching the C behaviour.
pub fn scan_dir(dir: &Path) -> Vec<TrackMeta> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: could not open directory '{}': {}", dir.display(), e);
            return Vec::new();
        }
    };

    let mut tracks: Vec<TrackMeta> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Skip hidden files and directories.
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                continue;
            }
        }
        if !path.is_file() {
            continue;
        }
        if !metadata::is_audio_file(&path) {
            continue;
        }
        match metadata::read_metadata(&path) {
            Ok(meta) => tracks.push(meta),
            Err(e) => {
                eprintln!("warning: could not read '{}': {}", path.display(), e);
            }
        }
    }

    // Sort by track number (0 sorts last, preserving relative order via
    // a stable sort).
    tracks.sort_by_key(|t| t.track_number);
    tracks
}

/// List immediate subdirectories of `dir`, skipping hidden entries.
pub fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: could not open directory '{}': {}", dir.display(), e);
            return Vec::new();
        }
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                continue;
            }
        }
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs
}

/// Return the last path component (the "filename") as a string, or `?` if
/// it can't be represented as UTF-8.
pub fn basename(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string()
}
