//! Audio metadata reading and lyrics embedding.
//!
//! The original C project delegates everything to TagLib. To keep the
//! Rust rewrite dependency-free for tag handling, we implement small,
//! focused parsers/writers for each container format by hand:
//!
//! | Extension  | Module   | Container / tag scheme              |
//! |------------|----------|-------------------------------------|
//! | `.flac`    | `flac`   | native FLAC + VORBIS_COMMENT        |
//! | `.mp3`     | `id3v2`  | ID3v2.3 / ID3v2.4 + MPEG frame scan |
//! | `.ogg`     | `ogg`    | OGG Vorbis (Vorbis comments)        |
//! | `.opus`    | `ogg`    | OGG Opus (Vorbis comments)          |
//! | `.m4a`/`.mp4`/`.aac` | `mp4` | MP4 box tree + `ilst` tags  |
//!
//! Each format implements the [`AudioFormat`] trait so the sync engine
//! can treat them uniformly. Dispatch happens by file extension in
//! [`read_metadata`] / [`sync_lyrics`].

use std::path::Path;

use crate::types::TrackMeta;

pub mod flac;
pub mod id3v2;
pub mod mp4;
pub mod ogg;

/// Outcome of a `sync_lyrics` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsOutcome {
    /// Lyrics were written to the file.
    Written,
    /// The file already contained lyrics and `force` was `false`.
    Skipped,
}

/// Errors that can occur while reading or writing tags.
#[derive(Debug)]
pub struct MetaError(pub String);

impl std::fmt::Display for MetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for MetaError {}

impl MetaError {
    #[allow(dead_code)]
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Per-format behaviour required by the sync engine.
///
/// Implementations read enough of the file to extract identity metadata
/// (title / artist / album / track / duration) and know how to embed a
/// `LYRICS` tag in the format-native way:
///
/// * FLAC / OGG: a `LYRICS` Vorbis comment field.
/// * MP3: a `USLT` (unsynchronised lyrics) frame, the ID3 standard.
/// * MP4: the `©lyr` atom, the iTunes-standard lyrics location.
pub trait AudioFormat {
    fn read_metadata(path: &Path) -> Result<TrackMeta, MetaError>;
    fn sync_lyrics(path: &Path, lyrics: &str, force: bool) -> Result<LyricsOutcome, MetaError>;
}

/// Extensions we know how to read tags from, in lower case.
fn audio_extension(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "flac" => "flac",
        "mp3" => "mp3",
        "ogg" => "ogg",
        "opus" => "opus",
        "m4a" | "mp4" | "m4b" | "m4p" | "aac" => "mp4",
        _ => return None,
    })
}

/// `true` if the file extension is one we support for tag IO.
pub fn is_audio_file(path: &Path) -> bool {
    audio_extension(path).is_some()
}

/// Dispatch metadata reading by extension.
pub fn read_metadata(path: &Path) -> Result<TrackMeta, MetaError> {
    match audio_extension(path) {
        Some("flac") => flac::Flac::read_metadata(path),
        Some("mp3") => id3v2::Mp3::read_metadata(path),
        Some("ogg") | Some("opus") => ogg::Ogg::read_metadata(path),
        Some("mp4") => mp4::Mp4::read_metadata(path),
        _ => Err(MetaError(format!(
            "unsupported audio format: {}",
            path.display()
        ))),
    }
}

/// Dispatch lyrics syncing by extension.
pub fn sync_lyrics(path: &Path, lyrics: &str, force: bool) -> Result<LyricsOutcome, MetaError> {
    match audio_extension(path) {
        Some("flac") => flac::Flac::sync_lyrics(path, lyrics, force),
        Some("mp3") => id3v2::Mp3::sync_lyrics(path, lyrics, force),
        Some("ogg") | Some("opus") => ogg::Ogg::sync_lyrics(path, lyrics, force),
        Some("mp4") => mp4::Mp4::sync_lyrics(path, lyrics, force),
        _ => Err(MetaError(format!(
            "unsupported audio format: {}",
            path.display()
        ))),
    }
}

// ---------- small shared byte helpers used by every format module ----------

/// Decode an ID3v2 "synchsafe" integer (7 bits per byte, 4 bytes for v2.3+).
pub(crate) fn synchsafe_u32(b: &[u8]) -> u32 {
    ((b[0] as u32 & 0x7f) << 21)
        | ((b[1] as u32 & 0x7f) << 14)
        | ((b[2] as u32 & 0x7f) << 7)
        | (b[3] as u32 & 0x7f)
}
