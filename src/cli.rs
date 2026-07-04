//! Command-line argument parsing.
//!
//! Hand-rolled to keep the dependency count at zero. Supports the same flags
//! as the original C implementation:

use std::path::PathBuf;

use crate::types::SyncConfig;

/// The sync mode chosen on the command line.
#[derive(Debug, Clone)]
pub enum Mode {
    /// `--folder PATH` — sync audio files directly in one directory.
    Folder(PathBuf),
    /// `--album PATH` — sync a single album directory.
    Album(PathBuf),
    /// `--artist PATH` — sync every album under an artist directory.
    Artist(PathBuf),
    /// `--library PATH` — sync an entire artist/album library.
    Library(PathBuf),
}

/// Parsed command-line arguments.
#[derive(Debug, Clone)]
pub struct Args {
    pub mode: Mode,
    pub config: SyncConfig,
}

/// Default thread count when `--threads` is not given.
const DEFAULT_THREADS: u32 = 4;
/// Maximum allowed parallel workers.
const MAX_THREADS: u32 = 16;

/// Print the usage banner to `stderr`.
pub fn print_usage(progname: &str) {
    eprintln!(
        "Usage:\n  \
         {p} --folder  \"/path/to/folder\"  [--force] [--threads N]\n  \
         {p} --album   \"/path/to/album\"   [--force] [--threads N]\n  \
         {p} --artist  \"/path/to/artist\"  [--force] [--threads N]\n  \
         {p} --library \"/path/to/music\"   [--force] [--threads N]\n\n\
         Options:\n  \
         --folder       Sync lyrics for audio files directly in one folder\n  \
         --album        Sync lyrics for a single album directory\n  \
         --artist       Sync lyrics for all albums of an artist\n  \
         --library      Sync lyrics for an entire library (artist/album)\n  \
         --force        Overwrite existing lyrics\n  \
         --clean-lrc    Delete local .lrc file after embedding it\n  \
         --threads      Number of parallel threads (default: 4, max: 16)\n  \
         --out-plain    File to log tracks that got plain lyrics\n  \
         --out-missing  File to log tracks with no lyrics found\n  \
         --help         Show this help message",
        p = progname
    );
}

/// Parse `argv`. Returns `Err(message)` on a malformed command line; the
/// caller is responsible for printing usage and choosing an exit code.
pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut mode: Option<Mode> = None;
    let mut force = false;
    let mut clean_lrc = false;
    let mut threads = DEFAULT_THREADS;
    let mut out_plain: Option<PathBuf> = None;
    let mut out_missing: Option<PathBuf> = None;

    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--help" | "-h" => return Err("__help__".to_string()),
            "--force" => force = true,
            "--clean-lrc" => clean_lrc = true,
            "--threads" => {
                i += 1;
                let v = argv
                    .get(i)
                    .ok_or_else(|| "missing value for --threads".to_string())?;
                threads = v
                    .parse()
                    .map_err(|_| format!("invalid --threads value '{v}'"))?;
                threads = threads.clamp(1, MAX_THREADS);
            }
            "--folder" | "--album" | "--artist" | "--library" => {
                i += 1;
                let v = argv
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))
                    .map(PathBuf::from)?;
                let new_mode = match arg {
                    "--folder" => Mode::Folder(v),
                    "--album" => Mode::Album(v),
                    "--artist" => Mode::Artist(v),
                    "--library" => Mode::Library(v),
                    _ => unreachable!(),
                };
                if mode.is_some() {
                    return Err("multiple mode flags given (only one of --folder/--album/--artist/--library)".to_string());
                }
                mode = Some(new_mode);
            }
            "--out-plain" => {
                i += 1;
                out_plain =
                    Some(PathBuf::from(argv.get(i).ok_or_else(|| {
                        "missing value for --out-plain".to_string()
                    })?));
            }
            "--out-missing" => {
                i += 1;
                out_missing =
                    Some(PathBuf::from(argv.get(i).ok_or_else(|| {
                        "missing value for --out-missing".to_string()
                    })?));
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
        i += 1;
    }

    let mode = mode.ok_or_else(|| {
        "no mode given (use one of --folder/--album/--artist/--library)".to_string()
    })?;

    Ok(Args {
        mode,
        config: SyncConfig {
            force,
            clean_lrc,
            prefer_synced: true,
            num_threads: threads,
            out_plain,
            out_missing,
        },
    })
}
