//! MP3 metadata reader/writer via ID3v2 tags.
//!
//! MP3 files optionally begin with an ID3v2 tag (versions 2.3 and 2.4 are
//! supported; 2.2 is detected but rare). The tag is a sequence of frames,
//! each identified by a 4-character ID:
//!
//! * `TIT2` — title
//! * `TPE1` — artist
//! * `TALB` — album
//! * `TRCK` — track number (may be `"5"` or `"5/12"`)
//! * `USLT` — unsynchronised lyrics (the standard ID3 lyrics frame)
//! * `TXXX` — user-defined text; TagLib stores a `LYRICS` property here
//!   when the generic property interface is used, so we honour that on
//!   read for back-compat, but always write `USLT` going forward.
//!
//! Duration is estimated by parsing the first MPEG audio frame header and
//! looking for a `Xing`/`Info` VBR header. For CBR files this is exact;
//! for VBR files with a Xing header it is also exact; otherwise it falls
//! back to a bitrate estimate.

use std::path::Path;

use crate::metadata::{AudioFormat, LyricsOutcome, MetaError};
use crate::types::TrackMeta;

pub struct Mp3;

const ID3_MARKER: &[u8; 3] = b"ID3";

/// A single ID3v2 frame: 4-byte ID + raw payload (no header).
struct Frame {
    id: [u8; 4],
    data: Vec<u8>,
}

/// Parsed ID3v2 tag header info.
struct Id3Header {
    /// Major version (3 = v2.3, 4 = v2.4).
    version: u8,
    /// Whether the unsynchronisation scheme applies to the whole tag.
    #[allow(dead_code)]
    _flags: u8,
    /// Byte offset of the first byte after the ID3v2 tag.
    end: usize,
}

/// Find and parse an ID3v2 header at the start of `bytes`. Returns `None`
/// if the file does not begin with `ID3`.
fn parse_id3_header(bytes: &[u8]) -> Option<Id3Header> {
    if bytes.len() < 10 || &bytes[..3] != ID3_MARKER {
        return None;
    }
    let version = bytes[3];
    let flags = bytes[5];
    let size = crate::metadata::synchsafe_u32(&bytes[6..10]) as usize;
    let mut end = 10 + size;
    if end > bytes.len() {
        end = bytes.len();
    }
    Some(Id3Header {
        version,
        _flags: flags,
        end,
    })
}

/// Iterate the frames inside an ID3v2 tag body. Handles both v2.3
/// (regular 4-byte sizes) and v2.4 (synchsafe 4-byte sizes).
fn iter_frames(header: &Id3Header, body: &[u8]) -> Vec<Frame> {
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 10 <= body.len() {
        let id = [body[pos], body[pos + 1], body[pos + 2], body[pos + 3]];
        // Padding/termination: a zero byte in the ID means end of frames.
        if id[0] == 0 {
            break;
        }
        let _raw_size =
            u32::from_be_bytes([body[pos + 4], body[pos + 5], body[pos + 6], body[pos + 7]])
                as usize;
        let size = if header.version >= 4 {
            crate::metadata::synchsafe_u32(&body[pos + 4..pos + 8]) as usize
        } else {
            _raw_size
        };
        // Skip 2 bytes of flags.
        pos += 10;
        if pos + size > body.len() {
            break;
        }
        frames.push(Frame {
            id,
            data: body[pos..pos + size].to_vec(),
        });
        pos += size;
    }
    frames
}

/// Decode an ID3 text frame payload into a Rust `String`.
///
/// The first byte selects the encoding:
///   0 = ISO-8859-1, 1 = UTF-16 with BOM, 2 = UTF-16BE, 3 = UTF-8.
fn decode_text(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    let enc = data[0];
    let text = &data[1..];
    // Strip a single trailing NUL (frames sometimes include one).
    let text = strip_trailing_nul(text, enc);
    match enc {
        0 => {
            // ISO-8859-1 (Latin-1): each byte maps to the same code point.
            Some(text.iter().map(|&b| b as char).collect())
        }
        1 => {
            // UTF-16 with BOM.
            if text.len() < 2 {
                return None;
            }
            let (le, body) = match text {
                [0xFF, 0xFE, rest @ ..] => (true, rest),
                [0xFE, 0xFF, rest @ ..] => (false, rest),
                _ => (true, text), // assume LE if BOM missing
            };
            utf16_decode(body, le)
        }
        2 => {
            // UTF-16BE, no BOM.
            utf16_decode(text, false)
        }
        3 => String::from_utf8(text.to_vec()).ok(),
        _ => String::from_utf8_lossy(text).into_owned().into(),
    }
}

/// Remove a single trailing NUL terminator appropriate for the encoding.
fn strip_trailing_nul(text: &[u8], enc: u8) -> &[u8] {
    if text.is_empty() {
        return text;
    }
    if enc == 1 || enc == 2 {
        if text.len() >= 2 && text[text.len() - 2] == 0 && text[text.len() - 1] == 0 {
            return &text[..text.len() - 2];
        }
    } else if text[text.len() - 1] == 0 {
        return &text[..text.len() - 1];
    }
    text
}

/// Decode a UTF-16 byte sequence (little- or big-endian) into a `String`.
fn utf16_decode(bytes: &[u8], le: bool) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let u = if le {
            u16::from_le_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], chunk[1]])
        };
        units.push(u);
    }
    String::from_utf16(&units).ok()
}

/// Encode a Rust string as an ID3 text frame payload using UTF-8 (encoding 3).
#[allow(dead_code)]
fn encode_text_utf8(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + s.len());
    out.push(3); // UTF-8
    out.extend_from_slice(s.as_bytes());
    out
}

/// Find the first frame with the given 4-byte ID and decode its text.
fn find_text(frames: &[Frame], id: &[u8; 4]) -> Option<String> {
    for f in frames {
        if &f.id == id {
            if let Some(s) = decode_text(&f.data) {
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// `true` if any frame contains embedded lyrics — either a `USLT` frame or
/// a `TXXX` frame whose description is `LYRICS` (TagLib's back-compat form).
fn has_lyrics(frames: &[Frame]) -> bool {
    for f in frames {
        if &f.id == b"USLT" {
            return true;
        }
        if &f.id == b"TXXX" {
            // TXXX payload: encoding(1) + description(null-term) + value.
            if f.data.is_empty() {
                continue;
            }
            let enc = f.data[0];
            let rest = &f.data[1..];
            let term = find_string_terminator(rest, enc);
            if let Some(desc) = &rest.get(..term) {
                if desc.eq_ignore_ascii_case(b"LYRICS") {
                    return true;
                }
            }
        }
    }
    false
}

/// Find the byte length of a NUL-terminated ID3 string, honouring the
/// encoding (UTF-16 uses a double-NUL terminator).
fn find_string_terminator(data: &[u8], enc: u8) -> usize {
    if enc == 1 || enc == 2 {
        let mut i = 0;
        while i + 1 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 {
                return i;
            }
            i += 2;
        }
        data.len()
    } else {
        data.iter().position(|&b| b == 0).unwrap_or(data.len())
    }
}

/// Build a `USLT` frame containing `lyrics`. Uses UTF-8 encoding, language
/// `eng`, and an empty content descriptor.
fn build_uslt(lyrics: &str) -> Frame {
    let mut data = Vec::with_capacity(8 + lyrics.len());
    data.push(3); // UTF-8
    data.extend_from_slice(b"eng"); // language
    data.push(0); // empty content descriptor (NUL terminator)
    data.extend_from_slice(lyrics.as_bytes());
    Frame { id: *b"USLT", data }
}

/// Serialise frames back into an ID3v2 tag body (v2.4, synchsafe sizes).
fn serialize_tag(frames: &[Frame]) -> Vec<u8> {
    let mut body = Vec::new();
    for f in frames {
        body.extend_from_slice(&f.id);
        // v2.4 synchsafe size.
        let s = f.data.len() as u32;
        body.push((s >> 21) as u8 & 0x7f);
        body.push((s >> 14) as u8 & 0x7f);
        body.push((s >> 7) as u8 & 0x7f);
        body.push(s as u8 & 0x7f);
        body.extend_from_slice(&[0, 0]); // flags
        body.extend_from_slice(&f.data);
    }
    let mut tag = Vec::with_capacity(10 + body.len());
    tag.extend_from_slice(b"ID3");
    tag.push(4); // version 2.4
    tag.push(0); // revision
    tag.push(0); // flags
    let size = body.len() as u32;
    tag.push((size >> 21) as u8 & 0x7f);
    tag.push((size >> 14) as u8 & 0x7f);
    tag.push((size >> 7) as u8 & 0x7f);
    tag.push(size as u8 & 0x7f);
    tag.extend_from_slice(&body);
    tag
}

// ─── MPEG frame parsing for duration estimation ────────────────────────────

/// Bitrate tables (kbps) indexed by bitrate index 0..15.
/// Row 0 = MPEG 1 Layer I, 1 = Layer II, 2 = Layer III,
/// row 3 = MPEG 2/2.5 Layer I, 4 = Layer II/III.
const BITRATES: [[u32; 16]; 5] = [
    [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
    ],
    [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
    ],
    [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ],
];

/// Sample rates (Hz) indexed by sample-rate index 0..3.
/// Row 0 = MPEG 1, 1 = MPEG 2, 2 = MPEG 2.5.
const SAMPLE_RATES: [[u32; 4]; 3] = [
    [44100, 48000, 32000, 0],
    [22050, 24000, 16000, 0],
    [11025, 12000, 8000, 0],
];

/// Samples per frame per (version, layer) combination.
fn samples_per_frame(version_mpeg1: bool, layer: u8) -> u32 {
    match layer {
        3 => 384,  // Layer I
        2 => 1152, // Layer II
        1 => {
            // Layer III
            if version_mpeg1 {
                1152
            } else {
                576
            }
        }
        _ => 0,
    }
}

/// Side-info size (bytes) for the first frame, used to locate the Xing
/// header. MPEG 1 = 32 (mono 17); MPEG 2/2.5 = 17 (mono 9).
fn sideinfo_size(version_mpeg1: bool, mono: bool) -> usize {
    match (version_mpeg1, mono) {
        (true, false) => 32,
        (true, true) => 17,
        (false, false) => 17,
        (false, true) => 9,
    }
}

/// Parsed first-MPEG-frame info used for duration estimation.
struct MpegInfo {
    bitrate: u32,     // kbps
    sample_rate: u32, // Hz
    samples_per_frame: u32,
    #[allow(dead_code)]
    frame_length: usize, // bytes
    version_mpeg1: bool,
    vbr_frames: Option<u32>, // Some(n) if a Xing/Info header was found
}

/// Decode a 4-byte MPEG frame header. Returns `None` if the header is
/// invalid (reserved bits, bad index).
fn parse_mpeg_header(h: &[u8]) -> Option<MpegInfo> {
    if h.len() < 4 {
        return None;
    }
    if h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None; // sync word
    }
    let version_bits = (h[1] >> 3) & 0x03;
    let layer_bits = (h[1] >> 1) & 0x03;
    let bitrate_index = (h[2] >> 4) & 0x0F;
    let sr_index = (h[2] >> 2) & 0x03;
    let padding = (h[2] >> 1) & 0x01;
    let channel_mode = (h[3] >> 6) & 0x03;

    // version_bits: 00 = 2.5, 01 = reserved, 10 = 2, 11 = 1
    let version_mpeg1 = version_bits == 0b11;
    let sr_row = match version_bits {
        0b11 => 0, // MPEG 1
        0b10 => 1, // MPEG 2
        0b00 => 2, // MPEG 2.5
        _ => return None,
    };
    // layer_bits: 01 = Layer III, 10 = Layer II, 11 = Layer I
    let layer = match layer_bits {
        0b01 => 1, // Layer III
        0b10 => 2, // Layer II
        0b11 => 3, // Layer I
        _ => return None,
    };
    if bitrate_index == 0 || bitrate_index == 15 {
        return None;
    }
    let br_row = match (version_mpeg1, layer) {
        (true, 3) => 0,
        (true, 2) => 1,
        (true, 1) => 2,
        (false, 3) => 3,
        (false, 2 | 1) => 4,
        _ => return None,
    };
    let bitrate = BITRATES[br_row][bitrate_index as usize] * 1000;
    let sample_rate = SAMPLE_RATES[sr_row][sr_index as usize];
    if sample_rate == 0 || bitrate == 0 {
        return None;
    }
    let spf = samples_per_frame(version_mpeg1, layer);
    // Frame length: Layer I uses 4-byte slots, others use 1-byte slots.
    let frame_length = if layer == 3 {
        (12 * bitrate as usize / sample_rate as usize + padding as usize) * 4
    } else {
        144 * bitrate as usize / sample_rate as usize + padding as usize
    };
    let mono = channel_mode == 0b11;
    let _ = sideinfo_size(version_mpeg1, mono); // computed later by caller
    Some(MpegInfo {
        bitrate,
        sample_rate,
        samples_per_frame: spf,
        frame_length,
        version_mpeg1,
        vbr_frames: None,
    })
}

/// Scan the file for the first valid MPEG frame after `start` and use it
/// (plus any Xing/Info VBR header) to estimate duration in seconds.
fn estimate_duration(bytes: &[u8], start: usize) -> u32 {
    let mut pos = start;
    // Limit the search window so a corrupt file doesn't make us scan forever.
    let scan_limit = (start + 1_000_000).min(bytes.len());
    while pos + 4 < scan_limit {
        // Look for an MPEG sync word (11 set bits).
        if bytes[pos] == 0xFF && (bytes[pos + 1] & 0xE0) == 0xE0 {
            if let Some(mut info) = parse_mpeg_header(&bytes[pos..pos + 4]) {
                // Look for a Xing/Info header in the first frame.
                let channel_mode = (bytes[pos + 3] >> 6) & 0x03;
                let mono = channel_mode == 0b11;
                let si = sideinfo_size(info.version_mpeg1, mono);
                let xing_off = pos + 4 + si;
                if xing_off + 8 <= bytes.len() {
                    let magic = &bytes[xing_off..xing_off + 4];
                    if magic == b"Xing" || magic == b"Info" {
                        let flags = u32::from_be_bytes([
                            bytes[xing_off + 4],
                            bytes[xing_off + 5],
                            bytes[xing_off + 6],
                            bytes[xing_off + 7],
                        ]);
                        if (flags & 0x01) != 0 {
                            // Frame-count field is present.
                            let frames = u32::from_be_bytes([
                                bytes[xing_off + 8],
                                bytes[xing_off + 9],
                                bytes[xing_off + 10],
                                bytes[xing_off + 11],
                            ]);
                            info.vbr_frames = Some(frames);
                        }
                    }
                }
                if let Some(frames) = info.vbr_frames {
                    if info.sample_rate > 0 {
                        let total_samples = frames as u64 * info.samples_per_frame as u64;
                        return (total_samples / info.sample_rate as u64) as u32;
                    }
                }
                // CBR / no-Xing fallback: bitrate estimate over audio size.
                let audio_bytes = bytes.len().saturating_sub(pos);
                if info.bitrate > 0 {
                    return ((audio_bytes as u64 * 8) / info.bitrate as u64) as u32;
                }
                return 0;
            }
        }
        pos += 1;
    }
    0
}

impl AudioFormat for Mp3 {
    fn read_metadata(path: &Path) -> Result<TrackMeta, MetaError> {
        let bytes = std::fs::read(path).map_err(|e| MetaError(e.to_string()))?;

        let mut track = TrackMeta {
            title: None,
            artist: None,
            album: None,
            track_number: 0,
            duration: 0,
            filepath: path.to_path_buf(),
        };

        let audio_start;
        if let Some(header) = parse_id3_header(&bytes) {
            let body = &bytes[10..header.end];
            let frames = iter_frames(&header, body);
            track.title = find_text(&frames, b"TIT2");
            track.artist = find_text(&frames, b"TPE1");
            track.album = find_text(&frames, b"TALB");
            if let Some(t) = find_text(&frames, b"TRCK") {
                track.track_number = t
                    .split('/')
                    .next()
                    .unwrap_or("0")
                    .trim()
                    .parse()
                    .unwrap_or(0);
            }
            audio_start = header.end;
        } else {
            audio_start = 0;
        }

        track.duration = estimate_duration(&bytes, audio_start);
        Ok(track)
    }

    fn sync_lyrics(path: &Path, lyrics: &str, force: bool) -> Result<LyricsOutcome, MetaError> {
        let bytes = std::fs::read(path).map_err(|e| MetaError(e.to_string()))?;

        // Split into (id3 tag, audio tail).
        let (mut frames, audio_start, had_id3) = if let Some(header) = parse_id3_header(&bytes) {
            let body = &bytes[10..header.end];
            (iter_frames(&header, body), header.end, true)
        } else {
            (Vec::new(), 0, false)
        };

        if !force && has_lyrics(&frames) {
            return Ok(LyricsOutcome::Skipped);
        }

        // Remove any existing lyrics frames so we don't leave duplicates.
        frames.retain(|f| {
            if &f.id == b"USLT" {
                return false;
            }
            if &f.id == b"TXXX" && !f.data.is_empty() {
                let enc = f.data[0];
                let rest = &f.data[1..];
                let term = find_string_terminator(rest, enc);
                if let Some(desc) = rest.get(..term) {
                    if desc.eq_ignore_ascii_case(b"LYRICS") {
                        return false;
                    }
                }
            }
            true
        });

        frames.push(build_uslt(lyrics));

        let new_tag = serialize_tag(&frames);
        let mut out = Vec::with_capacity(new_tag.len() + bytes.len() - audio_start);
        out.extend_from_slice(&new_tag);
        out.extend_from_slice(&bytes[audio_start..]);
        std::fs::write(path, out).map_err(|e| MetaError(e.to_string()))?;
        let _ = had_id3;
        Ok(LyricsOutcome::Written)
    }
}
