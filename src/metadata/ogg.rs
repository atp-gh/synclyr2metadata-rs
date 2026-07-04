//! OGG container reader/writer for Vorbis and Opus streams.
//!
//! Both codecs store tags in a "Vorbis comment" packet (vendor string +
//! `KEY=VALUE` list, identical layout to FLAC). The packet lives at a
//! fixed position in the stream:
//!
//! * Vorbis: packet 0 = identification, packet 1 = comment, packet 2 = setup
//! * Opus:   packet 0 = identification, packet 1 = comment
//!
//! Editing the comment packet changes its byte length, which forces us to
//! re-encode the OGG page structure: the header pages are rebuilt from
//! scratch and every subsequent page has its sequence number bumped and
//! its CRC32 recomputed. The audio packets themselves are copied verbatim.
//!
//! Duration comes from the last page's granule position:
//!
//! * Vorbis: `granule / sample_rate` (sample rate from the id header)
//! * Opus:   `granule / 48000` (Opus granules are always at 48 kHz)

use std::path::Path;

use crate::metadata::{AudioFormat, LyricsOutcome, MetaError};
use crate::types::TrackMeta;

pub struct Ogg;

const OGG_MAGIC: &[u8; 4] = b"OggS";
const MAX_SEGMENTS: usize = 255;

/// Precomputed CRC32 table for the OGG polynomial (0x04C11DB7, MSB-first).
static CRC_TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();

fn crc_table() -> &'static [u32; 256] {
    CRC_TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i << 24;
            for _ in 0..8 {
                if c & 0x8000_0000 != 0 {
                    c = (c << 1) ^ 0x04C1_1DB7;
                } else {
                    c <<= 1;
                }
            }
            t[i as usize] = c;
        }
        t
    })
}

/// Compute the OGG page CRC32 over `data` (the CRC field must be zeroed).
fn ogg_crc(data: &[u8]) -> u32 {
    let table = crc_table();
    let mut crc = 0u32;
    for &b in data {
        crc = (crc << 8) ^ table[(((crc >> 24) as u8) ^ b) as usize];
    }
    crc
}

/// One parsed OGG page. `raw` is the complete original page bytes (used to
/// clone audio pages with minimal mutation); the parsed fields drive the
/// re-encoder.
struct OggPage {
    #[allow(dead_code)]
    header_type: u8,
    granule: i64,
    serial: u32,
    #[allow(dead_code)]
    sequence: u32,
    /// Segment payloads in table order. A segment of exactly 255 bytes
    /// means "packet continues"; a shorter segment terminates a packet.
    segments: Vec<Vec<u8>>,
    #[allow(dead_code)]
    /// Byte length of the original page (for slicing `raw` if needed).
    raw_len: usize,
}

/// Parse every page in the stream. Returns the pages in file order.
fn parse_pages(bytes: &[u8]) -> Result<Vec<OggPage>, MetaError> {
    let mut pages = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        if pos + 27 > bytes.len() || &bytes[pos..pos + 4] != OGG_MAGIC {
            return Err(MetaError("invalid OGG page header".to_string()));
        }
        let header_type = bytes[pos + 5];
        let granule = i64::from_le_bytes([
            bytes[pos + 6],
            bytes[pos + 7],
            bytes[pos + 8],
            bytes[pos + 9],
            bytes[pos + 10],
            bytes[pos + 11],
            bytes[pos + 12],
            bytes[pos + 13],
        ]);
        let serial = u32::from_le_bytes([
            bytes[pos + 14],
            bytes[pos + 15],
            bytes[pos + 16],
            bytes[pos + 17],
        ]);
        let sequence = u32::from_le_bytes([
            bytes[pos + 18],
            bytes[pos + 19],
            bytes[pos + 20],
            bytes[pos + 21],
        ]);
        // bytes[pos+22..pos+26] = CRC (ignored on read).
        let n_seg = bytes[pos + 26] as usize;
        let seg_table_start = pos + 27;
        if seg_table_start + n_seg > bytes.len() {
            return Err(MetaError("OGG segment table truncated".to_string()));
        }
        let seg_lengths = &bytes[seg_table_start..seg_table_start + n_seg];
        let mut data_pos = seg_table_start + n_seg;
        let mut segments = Vec::with_capacity(n_seg);
        for &len in seg_lengths {
            if data_pos + len as usize > bytes.len() {
                return Err(MetaError("OGG segment body truncated".to_string()));
            }
            segments.push(bytes[data_pos..data_pos + len as usize].to_vec());
            data_pos += len as usize;
        }
        let raw_len = data_pos - pos;
        pages.push(OggPage {
            header_type,
            granule,
            serial,
            sequence,
            segments,
            raw_len,
        });
        pos = data_pos;
    }
    Ok(pages)
}

/// A reconstructed OGG packet plus the index of the page on which it
/// terminates (i.e. the page containing its final `<255` segment).
struct Packet {
    data: Vec<u8>,
    end_page: usize,
}

/// Reassemble packets from page segments, tracking which page each one
/// ends on. Packets may span page boundaries via 255-byte continuation
/// segments.
fn reconstruct_packets(pages: &[OggPage]) -> Vec<Packet> {
    let mut packets = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    for (page_idx, page) in pages.iter().enumerate() {
        for seg in &page.segments {
            let buf = current.get_or_insert_with(Vec::new);
            buf.extend_from_slice(seg);
            if seg.len() < 255 {
                packets.push(Packet {
                    data: current.take().unwrap(),
                    end_page: page_idx,
                });
            }
        }
    }
    packets
}

/// Build the segment-table lacing for a single packet.
fn packet_segments(packet: &[u8]) -> Vec<u8> {
    let mut segs = Vec::new();
    let mut i = 0;
    while i + 255 <= packet.len() {
        segs.push(255);
        i += 255;
    }
    segs.push((packet.len() - i) as u8);
    segs
}

/// Serialise a single OGG page containing exactly one complete packet.
fn build_page(
    header_type: u8,
    granule: i64,
    serial: u32,
    sequence: u32,
    packet: &[u8],
) -> Result<Vec<u8>, MetaError> {
    let segs = packet_segments(packet);
    if segs.len() > MAX_SEGMENTS {
        // Packet too big for one page — would need multi-page spanning,
        // which doesn't happen for realistic comment headers.
        return Err(MetaError(
            "OGG header packet too large for a single page".to_string(),
        ));
    }
    let n_seg = segs.len() as u8;
    let mut page = Vec::with_capacity(27 + segs.len() + packet.len());
    page.extend_from_slice(OGG_MAGIC);
    page.push(0); // stream structure version
    page.push(header_type);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&sequence.to_le_bytes());
    page.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
    page.push(n_seg);
    page.extend_from_slice(&segs);
    page.extend_from_slice(packet);
    // Compute and insert the CRC.
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    Ok(page)
}

/// Rewrite the CRC of an already-serialised page (used when we mutate only
/// the sequence number of an audio page).
fn recompute_crc(page: &mut [u8]) {
    page[22..26].copy_from_slice(&[0, 0, 0, 0]);
    let crc = ogg_crc(page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
}

// ─── Vorbis comment helpers (shared with FLAC) ─────────────────────────────

fn parse_vorbis_comment(data: &[u8]) -> Result<(String, Vec<Vec<u8>>), MetaError> {
    if data.len() < 4 {
        return Err(MetaError("vorbis comment too short".to_string()));
    }
    let vlen = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if 4 + vlen > data.len() {
        return Err(MetaError("vorbis vendor overruns block".to_string()));
    }
    let vendor = String::from_utf8_lossy(&data[4..4 + vlen]).into_owned();
    let mut pos = 4 + vlen;
    if pos + 4 > data.len() {
        return Err(MetaError("vorbis comment missing count".to_string()));
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
            return Err(MetaError("vorbis comment value overruns".to_string()));
        }
        comments.push(data[pos..pos + clen].to_vec());
        pos += clen;
    }
    Ok((vendor, comments))
}

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

fn find_comment<'a>(comments: &'a [Vec<u8>], key: &str) -> Option<&'a [u8]> {
    for c in comments {
        if let Some(idx) = c.iter().position(|&b| b == b'=') {
            if c[..idx].eq_ignore_ascii_case(key.as_bytes()) {
                return Some(&c[idx + 1..]);
            }
        }
    }
    None
}

// ─── Stream type detection ─────────────────────────────────────────────────

#[derive(PartialEq)]
enum Codec {
    Vorbis,
    Opus,
}

/// Detect the codec from the first packet's magic bytes.
fn detect_codec(first_packet: &[u8]) -> Result<Codec, MetaError> {
    if first_packet.starts_with(b"\x01vorbis") {
        Ok(Codec::Vorbis)
    } else if first_packet.starts_with(b"OpusHead") {
        Ok(Codec::Opus)
    } else {
        Err(MetaError(
            "unknown OGG codec (not Vorbis or Opus)".to_string(),
        ))
    }
}

/// Number of header packets before audio data begins.
fn header_packet_count(codec: &Codec) -> usize {
    match codec {
        Codec::Vorbis => 3,
        Codec::Opus => 2,
    }
}

/// Extract the comment packet body (without the codec magic prefix)
/// from a raw header packet.
fn comment_body<'a>(packet: &'a [u8], codec: &Codec) -> &'a [u8] {
    match codec {
        Codec::Vorbis => &packet[7..], // skip 0x01 + "vorbis"
        Codec::Opus => &packet[8..],   // skip "OpusTags"
    }
}

/// Reassemble a comment packet from a new comment body, re-attaching the
/// codec's magic prefix.
fn rebuild_comment_packet(codec: &Codec, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match codec {
        Codec::Vorbis => {
            out.push(0x03);
            out.extend_from_slice(b"vorbis");
        }
        Codec::Opus => {
            out.extend_from_slice(b"OpusTags");
        }
    }
    out.extend_from_slice(body);
    out
}

impl AudioFormat for Ogg {
    fn read_metadata(path: &Path) -> Result<TrackMeta, MetaError> {
        let bytes = std::fs::read(path).map_err(|e| MetaError(e.to_string()))?;
        let pages = parse_pages(&bytes)?;
        if pages.is_empty() {
            return Err(MetaError("empty OGG file".to_string()));
        }
        let packets = reconstruct_packets(&pages);
        if packets.is_empty() {
            return Err(MetaError("OGG stream has no packets".to_string()));
        }
        let codec = detect_codec(&packets[0].data)?;

        let mut track = TrackMeta {
            title: None,
            artist: None,
            album: None,
            track_number: 0,
            duration: 0,
            filepath: path.to_path_buf(),
        };

        // Sample rate from the identification header (Vorbis only — Opus
        // uses a fixed 48 kHz granule clock regardless of the input rate).
        let sample_rate = if codec == Codec::Vorbis {
            let id = &packets[0].data;
            if id.len() >= 16 {
                u32::from_le_bytes([id[12], id[13], id[14], id[15]])
            } else {
                0
            }
        } else {
            48_000
        };

        // Tags from the comment packet.
        if packets.len() >= 2 {
            let body = comment_body(&packets[1].data, &codec);
            if let Ok((_vendor, comments)) = parse_vorbis_comment(body) {
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

        // Duration from the last page's granule position.
        if let Some(last) = pages.last() {
            if last.granule > 0 && sample_rate > 0 {
                track.duration = (last.granule as u64 / sample_rate as u64) as u32;
            }
        }

        Ok(track)
    }

    fn sync_lyrics(path: &Path, lyrics: &str, force: bool) -> Result<LyricsOutcome, MetaError> {
        let bytes = std::fs::read(path).map_err(|e| MetaError(e.to_string()))?;
        let pages = parse_pages(&bytes)?;
        if pages.is_empty() {
            return Err(MetaError("empty OGG file".to_string()));
        }
        let serial = pages[0].serial;
        let mut packets = reconstruct_packets(&pages);
        if packets.len() < 2 {
            return Err(MetaError("OGG stream missing header packets".to_string()));
        }
        let codec = detect_codec(&packets[0].data)?;
        let num_headers = header_packet_count(&codec);

        // Honour `force=false`: skip if a LYRICS field already exists.
        let body = comment_body(&packets[1].data, &codec);
        let (vendor, mut comments) = parse_vorbis_comment(body)?;
        if !force && find_comment(&comments, "LYRICS").is_some() {
            return Ok(LyricsOutcome::Skipped);
        }

        // Replace the LYRICS comment.
        comments.retain(|c| {
            c.iter()
                .position(|&b| b == b'=')
                .map(|i| !c[..i].eq_ignore_ascii_case(b"LYRICS"))
                .unwrap_or(true)
        });
        comments.push(format!("LYRICS={lyrics}").into_bytes());
        let new_body = build_vorbis_comment(&vendor, &comments);
        packets[1].data = rebuild_comment_packet(&codec, &new_body);

        // Re-encode the header pages (one packet per page, granule 0).
        let mut out = Vec::new();
        for (i, pkt) in packets.iter().enumerate().take(num_headers) {
            let header_type = if i == 0 { 0x02 } else { 0x00 }; // BOS on first
            let page = build_page(header_type, 0, serial, i as u32, &pkt.data)?;
            out.extend_from_slice(&page);
        }

        // Copy every page after the last header page, bumping the sequence
        // number by the delta between old and new header page counts and
        // recomputing the CRC.
        let header_end_page = packets[num_headers - 1].end_page;
        let new_header_pages = num_headers;
        let old_header_pages = header_end_page + 1;
        let seq_delta = new_header_pages as i64 - old_header_pages as i64;

        // Walk the original bytes page by page for the audio section.
        let mut pos = 0;
        let mut page_idx = 0;
        // Skip past the original header pages in the byte stream.
        for _ in 0..old_header_pages {
            // Re-derive each page's length from the segment table.
            let n_seg = bytes[pos + 26] as usize;
            let seg_len: usize = bytes[pos + 27..pos + 27 + n_seg]
                .iter()
                .map(|&b| b as usize)
                .sum();
            pos += 27 + n_seg + seg_len;
            page_idx += 1;
        }
        // Now `pos` points at the first audio page. Clone-and-patch each.
        while pos < bytes.len() {
            let n_seg = bytes[pos + 26] as usize;
            let seg_len: usize = bytes[pos + 27..pos + 27 + n_seg]
                .iter()
                .map(|&b| b as usize)
                .sum();
            let page_len = 27 + n_seg + seg_len;
            let mut page = bytes[pos..pos + page_len].to_vec();
            let new_seq = (page_idx as i64 + seq_delta) as u32;
            page[18..22].copy_from_slice(&new_seq.to_le_bytes());
            recompute_crc(&mut page);
            out.extend_from_slice(&page);
            pos += page_len;
            page_idx += 1;
        }

        std::fs::write(path, out).map_err(|e| MetaError(e.to_string()))?;
        Ok(LyricsOutcome::Written)
    }
}
