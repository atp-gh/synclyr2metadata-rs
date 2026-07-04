//! `synclyr2metadata` — embed LRCLIB lyrics into audio file metadata.
//!
//! This is a Rust rewrite of the original C project
//! (<https://github.com/newtonsart/synclyr2metadata>) with minimal
//! dependencies (only `rustls` + `webpki-roots`) and hand-written parsers
//! for FLAC, MP3/ID3v2, OGG (Vorbis/Opus) and MP4/M4A containers.
//!
//! See `--help` for usage. When invoked with no arguments and the
//! `lidarr_eventtype` environment variable set, runs as a Lidarr
//! Custom Script automatically.

mod cli;
mod fs_util;
mod http_client;
mod json;
mod lidarr;
mod logger;
mod lrclib;
mod metadata;
mod sync;
mod types;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cli::Mode;
use crate::http_client::HttpClient;
use crate::sync::sync_tracks;
use crate::types::{ProgressFn, SyncConfig, SyncResult};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let progname = argv
        .first()
        .map(|s| s.as_str())
        .unwrap_or("synclyr2metadata");

    // Auto-detect Lidarr: no CLI args + Lidarr env vars present.
    if argv.len() < 2 && lidarr::detected() {
        let self_path = PathBuf::from(progname);
        return ExitCode::from(lidarr::run(&self_path) as u8);
    }

    if argv.len() < 2 {
        cli::print_usage(progname);
        return ExitCode::from(1);
    }

    let args = match cli::parse(&argv[1..]) {
        Ok(a) => a,
        Err(ref e) if e == "__help__" => {
            cli::print_usage(progname);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n");
            cli::print_usage(progname);
            return ExitCode::from(1);
        }
    };

    // All CLI modes need an HTTP client.
    let client = match HttpClient::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to initialise HTTP client: {e}");
            return ExitCode::from(1);
        }
    };

    let exit = run_mode(&args.mode, &args.config, &client);
    ExitCode::from(exit as u8)
}

/// Dispatch to the selected CLI mode and return a process exit code.
fn run_mode(mode: &Mode, config: &SyncConfig, client: &HttpClient) -> i32 {
    let progress: ProgressFn = std::sync::Arc::new(|idx, total, title, status| {
        println!(
            "  [{:>2}/{}] {:<40} {}",
            idx + 1,
            total,
            truncate(title, 40),
            status
        );
    });

    match mode {
        Mode::Folder(dir) | Mode::Album(dir) => {
            let label = if matches!(mode, Mode::Folder(_)) {
                "folder"
            } else {
                "album"
            };
            cmd_single_dir(dir, config, client, Some(progress), label)
        }
        Mode::Artist(dir) => cmd_artist(dir, config, client, Some(progress)),
        Mode::Library(dir) => cmd_library(dir, config, client, Some(progress)),
    }
}

/// `--folder` / `--album`: scan one directory and sync every track in it.
fn cmd_single_dir(
    dir: &Path,
    config: &SyncConfig,
    client: &HttpClient,
    progress: Option<ProgressFn>,
    label: &str,
) -> i32 {
    let tracks = fs_util::scan_dir(dir);
    if tracks.is_empty() {
        println!("No audio files found in '{}'.", dir.display());
        return 0;
    }
    println!(
        "Syncing lyrics for {} track(s) in {} '{}' [{} threads]...\n",
        tracks.len(),
        label,
        dir.display(),
        config.num_threads
    );
    let result = sync_tracks(&tracks, config, client, progress);
    print_summary(&result);
    if result.has_errors() {
        1
    } else {
        0
    }
}

/// `--artist`: iterate album subdirectories under an artist directory.
fn cmd_artist(
    artist_path: &Path,
    config: &SyncConfig,
    client: &HttpClient,
    progress: Option<ProgressFn>,
) -> i32 {
    println!(
        "\u{2550}\u{2550}\u{2550} {} \u{2550}\u{2550}\u{2550}\n",
        fs_util::basename(artist_path)
    );

    let mut total = SyncResult::default();
    let mut album_count = 0u32;

    for sub in fs_util::subdirs(artist_path) {
        let tracks = fs_util::scan_dir(&sub);
        if tracks.is_empty() {
            continue;
        }
        album_count += 1;
        println!(
            "\u{25b6} {} ({} tracks)",
            fs_util::basename(&sub),
            tracks.len()
        );
        let r = sync_tracks(&tracks, config, client, progress.clone());
        total.add(&r);
        println!();
    }

    if album_count == 0 {
        println!("No albums found.");
        return 0;
    }
    print!("{album_count} album(s) processed");
    print_summary(&total);
    if total.has_errors() {
        1
    } else {
        0
    }
}

/// `--library`: walk `artist/album` directories and sync everything.
fn cmd_library(
    library_path: &Path,
    config: &SyncConfig,
    client: &HttpClient,
    progress: Option<ProgressFn>,
) -> i32 {
    println!("{}", "\u{2550}".repeat(60));
    println!("  synclyr2metadata \u{2014} Library Sync");
    println!("  Path:     {}", library_path.display());
    println!("  Threads:  {}", config.num_threads);
    println!("{}\n", "\u{2550}".repeat(60));

    let mut total = SyncResult::default();
    let mut artist_count = 0u32;
    let mut album_count = 0u32;

    for artist_dir in fs_util::subdirs(library_path) {
        let mut artist_albums = 0u32;
        for album_dir in fs_util::subdirs(&artist_dir) {
            let tracks = fs_util::scan_dir(&album_dir);
            if tracks.is_empty() {
                continue;
            }
            if artist_albums == 0 {
                println!(
                    "\u{2550}\u{2550}\u{2550} {}",
                    fs_util::basename(&artist_dir)
                );
            }
            artist_albums += 1;
            album_count += 1;
            println!(
                "  \u{25b6} {} ({} tracks)",
                fs_util::basename(&album_dir),
                tracks.len()
            );
            let r = sync_tracks(&tracks, config, client, progress.clone());
            total.add(&r);
        }
        if artist_albums > 0 {
            artist_count += 1;
            println!();
        }
    }

    println!("{}", "\u{2550}".repeat(60));
    println!("  Library Sync Complete");
    println!("  Artists:  {artist_count}");
    println!("  Albums:   {album_count}");
    print_summary(&total);
    if total.has_errors() {
        1
    } else {
        0
    }
}

/// Print the final tally, matching the C version's banner.
fn print_summary(r: &SyncResult) {
    println!();
    println!("{}", "\u{2500}".repeat(46));
    println!("  \u{2713} Synced:     {}", r.synced);
    if r.plain > 0 {
        println!("  \u{2713} Plain:      {}", r.plain);
    }
    println!("  \u{2298} Skipped:    {}", r.skipped);
    println!("  \u{2717} Not found:  {}", r.not_found);
    if r.errors > 0 {
        println!("  \u{2717} Errors:     {}", r.errors);
    }
    println!("{}", "\u{2500}".repeat(46));
}

/// Truncate `s` to at most `max` characters for tidy progress output.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
