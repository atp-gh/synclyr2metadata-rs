//! FLAC metadata reader/writer.
//!
//! FLAC files start with the `fLaC` marker followed by a chain of metadata
//! blocks, each prefixed by a 4-byte header:
//!
//! ```text
//!  bit 0       : last-metadata-block flag
//!  bits 1-7    : block type (0=STREAMINFO, 4=VORBIS_COMMENT, ...)
//!  bits 8-31   : 24-bit big-endian payload length
//! ```
//!
//! We care about two block types:
//!
//! * `STREAMINFO` — carries `sample_rate` and `total_samples`, from which
//!   playback duration is derived.
//! * `VORBIS_COMMENT` — the standard tag container for FLAC. Tags are
//!   `KEY=VALUE` UTF-8 strings stored in a length-prefixed list (little-endian
//!   lengths, just like Ogg Vorbis comments). Lyrics live in a `LYRICS`
//!   field.
//!
//! Rewriting is straightforward because each block is self-describing:
//! we splice the new `VORBIS_COMMENT` block in place of the old one (or
//! insert one after `STREAMINFO` if none existed) and copy the audio
//! frames verbatim.

use std::path::Path;

use crate::metadata::{AudioFormat, LyricsOutcome, MetaError};
use crate::types::TrackMeta;

pub struct Flac;

const FLAC_MARKER: &[u8; 4] = b"fLaC";
const BLOCK_STREAMINFO: u8 = 0;
const BLOCK_VORBIS_COMMENT: u8 = 4;

/// One parsed metadata block: its type, whether it is the final block, and
/// the raw payload bytes (without the 4-byte header).
struct MetaBlock {
    block_type: u8,
    #[allow(dead_code)]
    is_last: bool,
    data: Vec<u8>,
}

/// Read the whole file and split it into `(header blocks, audio frames)`.
fn split_file(bytes: &[u8]) -> Result<(Vec<MetaBlock>, &[u8]), MetaError> {
    if bytes.len() < 4 || &bytes[..4] != FLAC_MARKER {
        return Err(MetaError(
            "not a FLAC file (missing 'fLaC' marker)".to_string(),
        ));
    }
    let mut pos = 4;
    let mut blocks = Vec::new();
    loop {
        if pos + 4 > bytes.len() {
            return Err(MetaError(
                "truncated FLAC metadata block header".to_string(),
            ));
        }
        let header = bytes[pos];
        let block_type = header & 0x7f;
        let is_last = (header & 0x80) != 0;
        let len = ((bytes[pos + 1] as usize) << 16)
            | ((bytes[pos + 2] as usize) << 8)
            | (bytes[pos + 3] as usize);
        pos += 4;
        if pos + len > bytes.len() {
            return Err(MetaError(
                "FLAC block payload exceeds file size".to_string(),
            ));
        }
        let data = bytes[pos..pos + len].to_vec();
        pos += len;
        blocks.push(MetaBlock {
            block_type,
            is_last,
            data,
        });
        if is_last {
            break;
        }
    }
    Ok((blocks, &bytes[pos..]))
}

/// Re-emit a list of metadata blocks as bytes, setting the `is_last` flag
/// on the final block (and clearing it on all others).
fn serialize_blocks(blocks: &[MetaBlock]) -> Vec<u8> {
    let mut out = Vec::new();
    let last_idx = blocks.len() - 1;
    for (i, b) in blocks.iter().enumerate() {
        let header = (if i == last_idx { 0x80 } else { 0x00 }) | (b.block_type & 0x7f);
        out.push(header);
        let len = b.data.len();
        out.push((len >> 16) as u8);
        out.push((len >> 8) as u8);
        out.push(len as u8);
        out.extend_from_slice(&b.data);
    }
    out
}

/// Parse a Vorbis comment block payload into `(vendor, comments)`.
/// Each comment is a raw `KEY=VALUE` byte string; case-normalisation is
/// done by the caller.
fn parse_vorbis_comment(data: &[u8]) -> Result<(String, Vec<Vec<u8>>), MetaError> {
    if data.len() < 4 {
        return Err(MetaError(
            "vorbis comment too short for vendor length".to_string(),
        ));
    }
    let vlen = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if 4 + vlen > data.len() {
        return Err(MetaError("vorbis vendor string overruns block".to_string()));
    }
    let vendor = String::from_utf8_lossy(&data[4..4 + vlen]).into_owned();
    let mut pos = 4 + vlen;
    if pos + 4 > data.len() {
        return Err(MetaError("vorbis comment missing count field".to_string()));
    }
    let count =
        u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
    pos += 4;
    let mut comments = Vec::with_capacity(count);
    for _ in 0..count {
        if pos + 4 > data.len() {
            return Err(MetaError("vorbis comment entry truncated".to_string()));
        }
        let clen =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if pos + clen > data.len() {
            return Err(MetaError("vorbis comment value overruns block".to_string()));
        }
        comments.push(data[pos..pos + clen].to_vec());
        pos += clen;
    }
    Ok((vendor, comments))
}

/// Serialise a vendor string + comment list back into a Vorbis comment
/// block payload.
fn build_vorbis_comment(vendor: &str, comments: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
        out.extend_from_slice(c);
    }
    out
}

/// Case-insensitive lookup of `KEY=VALUE` in a comment list. Returns the
/// value part if found.
fn find_comment<'a>(comments: &'a [Vec<u8>], key: &str) -> Option<&'a [u8]> {
    for c in comments {
        if let Some(idx) = find_eq(c) {
            let k = &c[..idx];
            if k.eq_ignore_ascii_case(key.as_bytes()) {
                return Some(&c[idx + 1..]);
            }
        }
    }
    None
}

/// Find the byte index of the first `=` in a comment, if any.
fn find_eq(c: &[u8]) -> Option<usize> {
    c.iter().position(|&b| b == b'=')
}

/// Extract `sample_rate` and `total_samples` from a STREAMINFO payload.
fn parse_streaminfo(data: &[u8]) -> Result<(u32, u64), MetaError> {
    if data.len() < 18 {
        return Err(MetaError("STREAMINFO payload too short".to_string()));
    }
    // sample_rate: 20 bits starting at byte 10.
    let sample_rate =
        ((data[10] as u32) << 12) | ((data[11] as u32) << 4) | ((data[12] as u32) >> 4);
    // total_samples: 36 bits — top 4 bits in low nibble of byte 13,
    // bottom 32 bits in bytes 14-17 (big-endian).
    let hi = (data[13] as u64) & 0x0F;
    let lo = u32::from_be_bytes([data[14], data[15], data[16], data[17]]) as u64;
    let total_samples = (hi << 32) | lo;
    Ok((sample_rate, total_samples))
}

impl AudioFormat for Flac {
    fn read_metadata(path: &Path) -> Result<TrackMeta, MetaError> {
        let bytes = std::fs::read(path).map_err(|e| MetaError(e.to_string()))?;
        let (blocks, _audio) = split_file(&bytes)?;

        let mut track = TrackMeta {
            title: None,
            artist: None,
            album: None,
            track_number: 0,
            duration: 0,
            filepath: path.to_path_buf(),
        };

        for b in &blocks {
            match b.block_type {
                BLOCK_STREAMINFO => {
                    if let Ok((sr, total)) = parse_streaminfo(&b.data) {
                        if sr > 0 {
                            track.duration = (total / sr as u64) as u32;
                        }
                    }
                }
                BLOCK_VORBIS_COMMENT => {
                    if let Ok((_vendor, comments)) = parse_vorbis_comment(&b.data) {
                        track.title = find_comment(&comments, "TITLE")
                            .and_then(|v| String::from_utf8(v.to_vec()).ok())
                            .filter(|s| !s.is_empty());
                        track.artist = find_comment(&comments, "ARTIST")
                            .and_then(|v| String::from_utf8(v.to_vec()).ok())
                            .filter(|s| !s.is_empty());
                        track.album = find_comment(&comments, "ALBUM")
                            .and_then(|v| String::from_utf8(v.to_vec()).ok())
                            .filter(|s| !s.is_empty());
                        if let Some(v) = find_comment(&comments, "TRACKNUMBER") {
                            if let Ok(s) = std::str::from_utf8(v) {
                                track.track_number = s
                                    .split('/')
                                    .next()
                                    .unwrap_or("0")
                                    .trim()
                                    .parse()
                                    .unwrap_or(0);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(track)
    }

    fn sync_lyrics(path: &Path, lyrics: &str, force: bool) -> Result<LyricsOutcome, MetaError> {
        let bytes = std::fs::read(path).map_err(|e| MetaError(e.to_string()))?;
        let (mut blocks, audio) = split_file(&bytes)?;

        // Locate the existing VORBIS_COMMENT block, if any.
        let vc_index = blocks
            .iter()
            .position(|b| b.block_type == BLOCK_VORBIS_COMMENT);

        // Read existing comments to honour `force=false`.
        if !force {
            if let Some(idx) = vc_index {
                if let Ok((_vendor, comments)) = parse_vorbis_comment(&blocks[idx].data) {
                    if find_comment(&comments, "LYRICS").is_some() {
                        return Ok(LyricsOutcome::Skipped);
                    }
                }
            }
        }

        // Build the new comment list: copy existing comments (except any old
        // LYRICS field) and append the new LYRICS entry.
        let new_lyrics_entry = format!("LYRICS={lyrics}");
        let (vendor, mut comments) = match vc_index {
            Some(idx) => parse_vorbis_comment(&blocks[idx].data)
                .unwrap_or_else(|_| (String::new(), Vec::new())),
            None => (String::new(), Vec::new()),
        };
        comments.retain(|c| {
            find_eq(c)
                .map(|i| !c[..i].eq_ignore_ascii_case(b"LYRICS"))
                .unwrap_or(true)
        });
        comments.push(new_lyrics_entry.into_bytes());
        let new_payload = build_vorbis_comment(&vendor, &comments);

        let new_block = MetaBlock {
            block_type: BLOCK_VORBIS_COMMENT,
            is_last: false,
            data: new_payload,
        };
        match vc_index {
            Some(idx) => blocks[idx] = new_block,
            None => {
                // Insert a new VORBIS_COMMENT right after STREAMINFO.
                let insert_at = blocks
                    .iter()
                    .position(|b| b.block_type == BLOCK_STREAMINFO)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                blocks.insert(insert_at, new_block);
            }
        }

        let mut out = Vec::with_capacity(4 + bytes.len() + lyrics.len() + 8);
        out.extend_from_slice(FLAC_MARKER);
        out.extend(serialize_blocks(&blocks));
        out.extend_from_slice(audio);
        std::fs::write(path, out).map_err(|e| MetaError(e.to_string()))?;
        Ok(LyricsOutcome::Written)
    }
}
