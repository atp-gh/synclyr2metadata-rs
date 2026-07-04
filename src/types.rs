//! Core data types shared across the sync pipeline.
//!
//! These mirror the C structs in the original project (`TrackMeta`,
//! `SyncConfig`, `SyncResult`) so the Rust rewrite preserves the same
//! conceptual model while leveraging Rust's ownership and `Option` types
//! instead of raw pointers and `NULL` checks.

use std::path::PathBuf;

/// Metadata extracted from a single audio file.
///
/// All string fields are optional because not every file is fully tagged;
/// the sync engine treats a missing `artist` or `title` as a hard skip
/// (the track is reported as "missing metadata").
#[derive(Debug, Clone)]
pub struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// 1-based track number from the file's tag, 0 when absent.
    pub track_number: u32,
    /// Playback duration in whole seconds.
    pub duration: u32,
    /// Absolute filesystem path of the audio file.
    pub filepath: PathBuf,
}

/// Aggregate counts produced by a sync run, shown in the final summary.
#[derive(Debug, Clone, Default)]
pub struct SyncResult {
    pub synced: u32,
    pub plain: u32,
    pub skipped: u32,
    pub not_found: u32,
    pub errors: u32,
}

impl SyncResult {
    /// Combine another run's counts into `self` (used by artist/library modes
    /// that iterate over many albums).
    pub fn add(&mut self, other: &SyncResult) {
        self.synced += other.synced;
        self.plain += other.plain;
        self.skipped += other.skipped;
        self.not_found += other.not_found;
        self.errors += other.errors;
    }

    /// `true` if any track hit a write/IO error during this run.
    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }
}

/// Configuration controlling a single `sync_tracks` invocation.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Overwrite lyrics that are already embedded in the file.
    pub force: bool,
    /// Delete the local `.lrc` sidecar after successfully embedding it.
    pub clean_lrc: bool,
    /// Give synced lyrics priority over plain matches during scoring.
    pub prefer_synced: bool,
    /// Number of parallel worker threads (clamped to 1..=16 by the CLI).
    pub num_threads: u32,
    /// Optional file to log tracks that only received plain lyrics.
    pub out_plain: Option<PathBuf>,
    /// Optional file to log tracks for which no lyrics were found.
    pub out_missing: Option<PathBuf>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            force: false,
            clean_lrc: false,
            prefer_synced: true,
            num_threads: 4,
            out_plain: None,
            out_missing: None,
        }
    }
}

/// Outcome of processing a single track, used both for the progress
/// callback and for incrementing the right `SyncResult` counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackOutcome {
    /// Lyrics were embedded successfully (synced, plain, or local `.lrc`).
    Synced,
    /// Only unsynchronised lyrics were available and were embedded.
    Plain,
    /// The file already had lyrics and `force` was not set.
    Skipped,
    /// LRCLIB had no matching track.
    NotFound,
    /// The file's tag was missing artist/title, or the write failed.
    Error,
}

impl TrackOutcome {
    /// Human-readable status line shown to the user, matching the
    /// glyphs used by the original C implementation.
    pub fn label(self) -> &'static str {
        match self {
            TrackOutcome::Synced => "\u{2713} synced",
            TrackOutcome::Plain => "\u{2713} plain",
            TrackOutcome::Skipped => "\u{2298} already has lyrics",
            TrackOutcome::NotFound => "\u{2717} not found",
            TrackOutcome::Error => "\u{2717} error",
        }
    }
}

/// Per-track progress callback.
///
/// `idx` is 0-based and `total` is the size of the track list. The callback
/// is invoked under the sync engine's result mutex, so it is safe to print
/// to shared streams without extra locking.
///
/// The callback is wrapped in an `Arc` so it can be cheaply cloned and
/// shared across album/library iterations that each drive their own
/// `sync_tracks` call.
pub type ProgressFn = std::sync::Arc<dyn Fn(usize, usize, &str, &str) + Send + Sync>;
