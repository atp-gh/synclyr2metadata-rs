//! The lyrics sync engine.
//!
//! This is the heart of the tool and a direct port of `sync.c` from the
//! original C project. For each track it tries, in order:
//!
//! 1. **Local `.lrc` sidecar** — if a file `track.flac` is accompanied by
//!    `track.lrc`, embed its contents immediately and (optionally) delete it.
//! 2. **LRCLIB API** — query LRCLIB with the track's `(artist, title,
//!    album, duration)`, using the full fallback strategy implemented in
//!    [`crate::lrclib::lookup`].
//!
//! Workers run on `std::thread` and pull tracks from a shared atomic index,
//! matching the C version's pthread work-queue. A mutex guards the
//! aggregate counters, the optional log files, and the progress callback.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::fs_util;
use crate::http_client::HttpClient;
use crate::lrclib;
use crate::metadata::{self, LyricsOutcome};
use crate::types::{ProgressFn, SyncConfig, SyncResult, TrackMeta, TrackOutcome};

/// Per-track result returned by `process_track`. The caller maps each field
/// onto the right `SyncResult` counter.
struct TrackResult {
    outcome: TrackOutcome,
    /// `true` if the embedded lyrics were synced (not plain). Used to decide
    /// whether to log the track to the "plain lyrics" output file.
    is_plain: bool,
    /// `true` if the track should be logged to the "missing" output file.
    is_missing: bool,
}

/// Try to read and embed a local `.lrc` sidecar file. Returns `Some(result)`
/// if a sidecar was found (whether or not embedding succeeded); `None` if
/// no sidecar exists and the caller should fall back to the API.
fn try_local_lrc(track: &TrackMeta, config: &SyncConfig) -> Option<TrackResult> {
    let lrc_path = fs_util::lrc_sidecar(&track.filepath);
    let contents = std::fs::read_to_string(&lrc_path).ok()?;
    if contents.is_empty() {
        return None;
    }

    match metadata::sync_lyrics(&track.filepath, &contents, config.force) {
        Ok(LyricsOutcome::Written) => {
            if config.clean_lrc {
                let _ = std::fs::remove_file(&lrc_path);
            }
            Some(TrackResult {
                outcome: TrackOutcome::Synced,
                is_plain: false,
                is_missing: false,
            })
        }
        Ok(LyricsOutcome::Skipped) => Some(TrackResult {
            outcome: TrackOutcome::Skipped,
            is_plain: false,
            is_missing: false,
        }),
        Err(_) => Some(TrackResult {
            outcome: TrackOutcome::Error,
            is_plain: false,
            is_missing: false,
        }),
    }
}

/// Query LRCLIB for lyrics and embed them. Implements the full fallback
/// strategy via [`lrclib::lookup`].
fn try_api_lrc(client: &HttpClient, track: &TrackMeta, config: &SyncConfig) -> TrackResult {
    let found = lrclib::lookup(
        client,
        track.artist.as_deref().unwrap_or(""),
        track.title.as_deref().unwrap_or(""),
        track.album.as_deref(),
        if track.duration > 0 {
            Some(track.duration as f64)
        } else {
            None
        },
        config.prefer_synced,
    );

    let lrc = match found {
        Ok(Some(l)) => l,
        Ok(None) => {
            return TrackResult {
                outcome: TrackOutcome::NotFound,
                is_plain: false,
                is_missing: true,
            }
        }
        Err(e) => {
            eprintln!("error: LRCLIB lookup failed: {e}");
            return TrackResult {
                outcome: TrackOutcome::Error,
                is_plain: false,
                is_missing: false,
            };
        }
    };

    // Instrumental tracks count as a successful sync but write nothing.
    if lrc.instrumental {
        return TrackResult {
            outcome: TrackOutcome::Synced,
            is_plain: false,
            is_missing: false,
        };
    }

    // Prefer synced lyrics, fall back to plain.
    let (lyrics, is_synced) = if let Some(s) = lrc.synced_lyrics.as_deref() {
        if !s.is_empty() {
            (s.to_string(), true)
        } else if let Some(p) = lrc.plain_lyrics.as_deref() {
            (p.to_string(), false)
        } else {
            return TrackResult {
                outcome: TrackOutcome::NotFound,
                is_plain: false,
                is_missing: true,
            };
        }
    } else if let Some(p) = lrc.plain_lyrics.as_deref() {
        (p.to_string(), false)
    } else {
        return TrackResult {
            outcome: TrackOutcome::NotFound,
            is_plain: false,
            is_missing: true,
        };
    };

    match metadata::sync_lyrics(&track.filepath, &lyrics, config.force) {
        Ok(LyricsOutcome::Written) => TrackResult {
            outcome: if is_synced {
                TrackOutcome::Synced
            } else {
                TrackOutcome::Plain
            },
            is_plain: !is_synced,
            is_missing: false,
        },
        Ok(LyricsOutcome::Skipped) => TrackResult {
            outcome: TrackOutcome::Skipped,
            is_plain: false,
            is_missing: false,
        },
        Err(_) => TrackResult {
            outcome: TrackOutcome::Error,
            is_plain: false,
            is_missing: false,
        },
    }
}

/// Process a single track end-to-end. This runs on a worker thread.
fn process_track(client: &HttpClient, track: &TrackMeta, config: &SyncConfig) -> TrackResult {
    // A track without artist or title cannot be looked up.
    if track.artist.is_none() || track.title.is_none() {
        return TrackResult {
            outcome: TrackOutcome::NotFound,
            is_plain: false,
            is_missing: true,
        };
    }

    if let Some(r) = try_local_lrc(track, config) {
        return r;
    }
    try_api_lrc(client, track, config)
}

/// Shared, thread-safe state accumulated by the workers.
struct Shared {
    result: Mutex<SyncResult>,
    plain_file: Mutex<Option<std::fs::File>>,
    missing_file: Mutex<Option<std::fs::File>>,
    progress: Mutex<Option<ProgressFn>>,
}

/// Run the sync engine over `tracks`. Returns the aggregate `SyncResult`.
///
/// `progress` is called once per track (under a mutex) with
/// `(idx, total, title, status_label)`.
pub fn sync_tracks(
    tracks: &[TrackMeta],
    config: &SyncConfig,
    client: &HttpClient,
    progress: Option<ProgressFn>,
) -> SyncResult {
    if tracks.is_empty() {
        return SyncResult::default();
    }

    let total = tracks.len();
    let _ = total;
    let next_index = Arc::new(AtomicUsize::new(0));
    let tracks_arc = Arc::new(tracks.to_vec());
    let config_arc = Arc::new(config.clone());
    let client_arc = Arc::new(client.clone());

    // Open optional output logs in append mode (created if missing).
    let plain_file = open_log(config.out_plain.as_deref());
    let missing_file = open_log(config.out_missing.as_deref());

    let shared = Arc::new(Shared {
        result: Mutex::new(SyncResult::default()),
        plain_file: Mutex::new(plain_file),
        missing_file: Mutex::new(missing_file),
        progress: Mutex::new(progress),
    });

    // Spawn workers (capped by the track count to avoid idle threads).
    let num_workers = config.num_threads.min(tracks.len() as u32).max(1) as usize;
    let mut handles = Vec::with_capacity(num_workers);
    for _ in 0..num_workers {
        let next_index = Arc::clone(&next_index);
        let tracks = Arc::clone(&tracks_arc);
        let config = Arc::clone(&config_arc);
        let client = Arc::clone(&client_arc);
        let shared = Arc::clone(&shared);

        handles.push(thread::spawn(move || {
            worker(&next_index, &tracks, &config, &client, &shared);
        }));
    }
    for h in handles {
        let _ = h.join();
    }

    let result = shared.result.lock().unwrap();
    result.clone()
}

/// Worker loop: pull the next track index, process it, update shared state.
fn worker(
    next_index: &AtomicUsize,
    tracks: &[TrackMeta],
    config: &SyncConfig,
    client: &HttpClient,
    shared: &Shared,
) {
    loop {
        let idx = next_index.fetch_add(1, Ordering::SeqCst);
        if idx >= tracks.len() {
            break;
        }
        let track = &tracks[idx];
        let result = process_track(client, track, config);

        // Update shared state under the result mutex so the progress
        // callback sees a consistent view.
        let mut result_guard = shared.result.lock().unwrap();
        match result.outcome {
            TrackOutcome::Synced => result_guard.synced += 1,
            TrackOutcome::Plain => result_guard.plain += 1,
            TrackOutcome::Skipped => result_guard.skipped += 1,
            TrackOutcome::NotFound => result_guard.not_found += 1,
            TrackOutcome::Error => result_guard.errors += 1,
        }

        let total = tracks.len();
        if result.is_plain {
            if let Some(f) = shared.plain_file.lock().unwrap().as_mut() {
                let _ = writeln!(f, "{}", track.filepath.display());
                let _ = f.flush();
            }
        }
        if result.is_missing {
            if let Some(f) = shared.missing_file.lock().unwrap().as_mut() {
                let _ = writeln!(f, "{}", track.filepath.display());
                let _ = f.flush();
            }
        }

        if let Some(cb) = shared.progress.lock().unwrap().as_ref() {
            let title = track
                .title
                .clone()
                .unwrap_or_else(|| "(unknown)".to_string());
            cb(idx, total, &title, result.outcome.label());
        }
        drop(result_guard);
    }
}

/// Open `path` for appending, creating it if it doesn't exist.
fn open_log(path: Option<&Path>) -> Option<std::fs::File> {
    let path = path?;
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("warning: could not open log '{}': {}", path.display(), e);
            None
        }
    }
}
