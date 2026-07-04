//! MP4 / M4A metadata reader/writer.
//!
//! An MP4 file is a tree of "boxes" (also called "atoms"), each prefixed by
//! a 4-byte big-endian size and a 4-byte type. Tags live inside the `ilst`
//! box, which is usually at `moov / udta / ilst`:
//!
//! ```text
//! moov
//! ├─ mvhd            (timescale + duration → playback length)
//! ├─ trak / mdia / mdhd (per-track duration, alternate source)
//! └─ udta
//!    └─ ilst
//!       ├─ ©nam      (title, as a `data` sub-box with UTF-8 text)
//!       ├─ ©ART      (artist)
//!       ├─ ©alb      (album)
//!       ├─ trkn      (track number: 8-byte binary payload)
//!       └─ ©lyr      (lyrics — what we embed)
//! ```
//!
//! Editing `ilst` changes the size of `ilst` → `udta` → `moov`. When `moov`
//! sits *before* `mdat` in the file (less common but legal), growing `moov`
//! shifts `mdat`'s byte offset, which invalidates every chunk offset in
//! every `stco`/`co64` table. We detect that case and patch the offsets by
//! the size delta so the file stays playable.

use std::path::Path;

use crate::metadata::{AudioFormat, LyricsOutcome, MetaError};
use crate::types::TrackMeta;

pub struct Mp4;

/// Known container boxes whose payload is itself a list of boxes.
///
/// Inside `ilst`, every tag atom (`©nam`, `©ART`, `©alb`, `©lyr`, `trkn`,
/// `----`, ...) is also a container holding a `data` sub-box. iTunes tag
/// atom names either start with the `©` byte (`0xA9`) or are one of the
/// short all-lowercase names (`trkn`, `disk`, `cpil`, ...), so we treat
/// those as containers too.
fn is_container(box_type: &[u8; 4]) -> bool {
    matches!(
        box_type,
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"udta" | b"ilst" | b"edts" | b"meta"
    ) || box_type[0] == 0xA9 // ©-prefixed iTunes tag atoms
        || matches!(box_type, b"----" | b"trkn" | b"disk" | b"cpil" | b"tmpo" | b"pgap")
}

/// A parseable, mutable box tree. Leaves carry their raw payload; container
/// nodes carry their parsed children. Working on a tree (rather than raw
/// bytes) lets us resize any box and recompute parent sizes automatically
/// during serialisation.
#[derive(Debug, Clone)]
enum Tree {
    Leaf {
        box_type: [u8; 4],
        payload: Vec<u8>,
    },
    Container {
        box_type: [u8; 4],
        children: Vec<Tree>,
    },
}

impl Tree {
    fn box_type(&self) -> &[u8; 4] {
        match self {
            Tree::Leaf { box_type, .. } => box_type,
            Tree::Container { box_type, .. } => box_type,
        }
    }

    /// Parse a flat byte range into a list of sibling boxes (top-level or
    /// the children of a container).
    fn from_bytes(bytes: &[u8]) -> Result<Vec<Tree>, MetaError> {
        let mut trees = Vec::new();
        let mut pos = 0;
        while pos < bytes.len() {
            let (node, consumed) = parse_box(bytes, pos)?;
            pos += consumed;
            trees.push(Tree::from_node(node));
            if consumed == 0 {
                break;
            }
        }
        Ok(trees)
    }

    /// Convert a flat [`Node`] into a [`Tree`], recursing into containers.
    /// The `meta` box is a "FullBox" container: its payload starts with a
    /// 4-byte version/flags field that must be skipped before parsing the
    /// child boxes. We drop the field on parse and re-insert zeros on
    /// serialise, which is correct for every standard iTunes-style file.
    fn from_node(node: Node) -> Tree {
        if is_container(&node.box_type) {
            let payload = if node.box_type == *b"meta" && node.payload.len() >= 4 {
                &node.payload[4..]
            } else {
                &node.payload[..]
            };
            let children = Tree::from_bytes(payload).unwrap_or_default();
            Tree::Container {
                box_type: node.box_type,
                children,
            }
        } else {
            Tree::Leaf {
                box_type: node.box_type,
                payload: node.payload,
            }
        }
    }

    /// Serialise this node (and its children) back into size-prefixed bytes.
    /// `meta` boxes get a 4-byte `0x00000000` version/flags prefix re-added.
    fn serialize(&self) -> Vec<u8> {
        let mut payload: Vec<u8> = match self {
            Tree::Leaf { payload, .. } => payload.clone(),
            Tree::Container { children, .. } => {
                children.iter().flat_map(|c| c.serialize()).collect()
            }
        };
        if self.box_type() == b"meta" {
            // Re-insert the 4-byte version/flags that `from_node` stripped.
            let mut with_header = vec![0, 0, 0, 0];
            with_header.append(&mut payload);
            payload = with_header;
        }
        let total = 8 + payload.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(self.box_type());
        out.extend_from_slice(&payload);
        out
    }

    /// Immutable recursive search for the first descendant matching `target`.
    fn find(&self, target: &[u8; 4]) -> Option<&Tree> {
        if self.box_type() == target {
            return Some(self);
        }
        if let Tree::Container { children, .. } = self {
            for c in children {
                if let Some(found) = c.find(target) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Mutable recursive search for the first descendant matching `target`.
    fn find_mut(&mut self, target: &[u8; 4]) -> Option<&mut Tree> {
        if self.box_type() == target {
            return Some(self);
        }
        if let Tree::Container { children, .. } = self {
            for c in children.iter_mut() {
                if let Some(found) = c.find_mut(target) {
                    return Some(found);
                }
            }
        }
        None
    }
}

/// A flat box: 4-byte type + raw payload (no header).
struct Node {
    box_type: [u8; 4],
    payload: Vec<u8>,
}

/// Parse one box starting at `pos`. Returns the node and bytes consumed.
/// Supports `size == 0` (extends to EOF) and `size == 1` (64-bit largesize).
fn parse_box(bytes: &[u8], pos: usize) -> Result<(Node, usize), MetaError> {
    if pos + 8 > bytes.len() {
        return Err(MetaError("truncated MP4 box header".to_string()));
    }
    let size =
        u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
    let box_type = [
        bytes[pos + 4],
        bytes[pos + 5],
        bytes[pos + 6],
        bytes[pos + 7],
    ];
    let (total_size, header_size) = if size == 0 {
        (bytes.len() - pos, 8usize)
    } else if size == 1 {
        if pos + 16 > bytes.len() {
            return Err(MetaError("truncated MP4 largesize".to_string()));
        }
        let large = u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize;
        (large, 16usize)
    } else {
        (size, 8usize)
    };
    if pos + total_size > bytes.len() {
        return Err(MetaError("MP4 box extends past EOF".to_string()));
    }
    let payload = bytes[pos + header_size..pos + total_size].to_vec();
    Ok((Node { box_type, payload }, total_size))
}

// ─── Tag extraction helpers ────────────────────────────────────────────────

/// Find the `data` sub-box inside an `ilst` tag atom and return its payload.
fn data_box_payload(tag_node: &Tree) -> Option<&Vec<u8>> {
    let data_box = tag_node.find(b"data")?;
    if let Tree::Leaf { payload, .. } = data_box {
        Some(payload)
    } else {
        None
    }
}

/// Extract UTF-8 text from an `ilst` tag atom. The `data` sub-box payload
/// is `[4-byte flags][4-byte reserved][text]`.
fn read_text_tag(tag_node: &Tree) -> Option<String> {
    let payload = data_box_payload(tag_node)?;
    if payload.len() < 8 {
        return None;
    }
    String::from_utf8(payload[8..].to_vec())
        .ok()
        .filter(|s| !s.is_empty())
}

/// Parse the `trkn` tag: 4 flags + 4 reserved + track(u16 BE) + total(u16 BE) + 4 reserved.
fn read_trkn(tag_node: &Tree) -> Option<u32> {
    let payload = data_box_payload(tag_node)?;
    if payload.len() < 12 {
        return None;
    }
    Some(u16::from_be_bytes([payload[8], payload[9]]) as u32)
}

/// Build an `ilst` text tag atom containing a UTF-8 `data` sub-box.
fn build_text_tag(box_type: &[u8; 4], text: &str) -> Tree {
    let mut data_payload = Vec::with_capacity(8 + text.len());
    data_payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // flags: UTF-8
    data_payload.extend_from_slice(&[0, 0, 0, 0]); // reserved
    data_payload.extend_from_slice(text.as_bytes());
    Tree::Container {
        box_type: *box_type,
        children: vec![Tree::Leaf {
            box_type: *b"data",
            payload: data_payload,
        }],
    }
}

// ─── mvhd / duration ───────────────────────────────────────────────────────

/// Parse an `mvhd` payload for `(timescale, duration)`. Supports version 0
/// (32-bit times) and version 1 (64-bit times).
fn parse_mvhd(payload: &[u8]) -> Option<(u32, u64)> {
    if payload.len() < 4 {
        return None;
    }
    let version = payload[0];
    match version {
        0 if payload.len() >= 24 => {
            let timescale =
                u32::from_be_bytes([payload[12], payload[13], payload[14], payload[15]]);
            let duration =
                u32::from_be_bytes([payload[16], payload[17], payload[18], payload[19]]) as u64;
            Some((timescale, duration))
        }
        1 if payload.len() >= 36 => {
            let timescale =
                u32::from_be_bytes([payload[20], payload[21], payload[22], payload[23]]);
            let duration = u64::from_be_bytes([
                payload[24],
                payload[25],
                payload[26],
                payload[27],
                payload[28],
                payload[29],
                payload[30],
                payload[31],
            ]);
            Some((timescale, duration))
        }
        _ => None,
    }
}

// ─── stco / co64 offset patching ───────────────────────────────────────────

/// Walk every `stco`/`co64` table under `node` and add `delta` to each
/// chunk offset. Used when a growing `moov` shifts a preceding `mdat`.
fn patch_chunk_offsets(node: &mut Tree, delta: i64) {
    if delta == 0 {
        return;
    }
    if let Tree::Leaf { box_type, payload } = node {
        match &*box_type {
            b"stco" if payload.len() >= 8 => {
                let count =
                    u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
                for i in 0..count {
                    let off = 8 + i * 4;
                    if off + 4 > payload.len() {
                        break;
                    }
                    let v = u32::from_be_bytes([
                        payload[off],
                        payload[off + 1],
                        payload[off + 2],
                        payload[off + 3],
                    ]) as i64;
                    let nv = (v + delta).max(0) as u32;
                    payload[off..off + 4].copy_from_slice(&nv.to_be_bytes());
                }
            }
            b"co64" if payload.len() >= 8 => {
                let count =
                    u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
                for i in 0..count {
                    let off = 8 + i * 8;
                    if off + 8 > payload.len() {
                        break;
                    }
                    let v = u64::from_be_bytes([
                        payload[off],
                        payload[off + 1],
                        payload[off + 2],
                        payload[off + 3],
                        payload[off + 4],
                        payload[off + 5],
                        payload[off + 6],
                        payload[off + 7],
                    ]) as i64;
                    let nv = (v + delta).max(0) as u64;
                    payload[off..off + 8].copy_from_slice(&nv.to_be_bytes());
                }
            }
            _ => {}
        }
        return;
    }
    if let Tree::Container { children, .. } = node {
        for c in children {
            patch_chunk_offsets(c, delta);
        }
    }
}

// ─── AudioFormat impl ──────────────────────────────────────────────────────

impl AudioFormat for Mp4 {
    fn read_metadata(path: &Path) -> Result<TrackMeta, MetaError> {
        let bytes = std::fs::read(path).map_err(|e| MetaError(e.to_string()))?;
        let top = Tree::from_bytes(&bytes)?;
        let moov = top
            .iter()
            .find(|t| t.box_type() == b"moov")
            .ok_or_else(|| MetaError("MP4 missing moov box".to_string()))?;

        let mut track = TrackMeta {
            title: None,
            artist: None,
            album: None,
            track_number: 0,
            duration: 0,
            filepath: path.to_path_buf(),
        };

        if let Some(Tree::Leaf { payload, .. }) = moov.find(b"mvhd") {
            if let Some((ts, dur)) = parse_mvhd(payload) {
                if ts > 0 {
                    track.duration = (dur / ts as u64) as u32;
                }
            }
        }

        if let Some(Tree::Container { children, .. }) = moov.find(b"ilst") {
            for tag in children {
                match tag.box_type() {
                    b"\xa9nam" => track.title = read_text_tag(tag),
                    b"\xa9ART" => track.artist = read_text_tag(tag),
                    b"\xa9alb" => track.album = read_text_tag(tag),
                    b"trkn" => track.track_number = read_trkn(tag).unwrap_or(0),
                    _ => {}
                }
            }
        }

        Ok(track)
    }

    fn sync_lyrics(path: &Path, lyrics: &str, force: bool) -> Result<LyricsOutcome, MetaError> {
        let bytes = std::fs::read(path).map_err(|e| MetaError(e.to_string()))?;
        let mut top = Tree::from_bytes(&bytes)?;

        let moov_idx = top
            .iter()
            .position(|t| t.box_type() == b"moov")
            .ok_or_else(|| MetaError("MP4 missing moov box".to_string()))?;

        // Snapshot the old moov byte size and whether `mdat` follows `moov`.
        let old_moov_size = top[moov_idx].serialize().len();
        let mdat_after_moov = top
            .iter()
            .enumerate()
            .any(|(i, t)| i > moov_idx && t.box_type() == b"mdat");

        // Ensure `udta` and `ilst` exist under `moov`. Each check is done
        // with an immutable borrow so we can mutate on the next line.
        let has_udta = top[moov_idx].find(b"udta").is_some();
        if !has_udta {
            if let Tree::Container { children, .. } = &mut top[moov_idx] {
                children.push(Tree::Container {
                    box_type: *b"udta",
                    children: vec![Tree::Container {
                        box_type: *b"ilst",
                        children: vec![],
                    }],
                });
            }
        }
        let has_ilst = top[moov_idx].find(b"ilst").is_some();
        if !has_ilst {
            let udta = top[moov_idx].find_mut(b"udta").unwrap();
            if let Tree::Container { children, .. } = udta {
                children.push(Tree::Container {
                    box_type: *b"ilst",
                    children: vec![],
                });
            }
        }

        // Modify the ilst: honour `force`, drop any old ©lyr, add the new one.
        {
            let udta = top[moov_idx].find_mut(b"udta").unwrap();
            let ilst = udta.find_mut(b"ilst").unwrap();
            if let Tree::Container { children, .. } = ilst {
                let has_lyrics = children
                    .iter()
                    .any(|t| t.box_type() == b"\xa9lyr" && read_text_tag(t).is_some());
                if !force && has_lyrics {
                    return Ok(LyricsOutcome::Skipped);
                }
                children.retain(|t| t.box_type() != b"\xa9lyr");
                children.push(build_text_tag(b"\xa9lyr", lyrics));
            }
        }

        // Compute the new moov size. The stco/co64 patch (if any) does not
        // change the size, so this delta is final.
        let new_moov_size = top[moov_idx].serialize().len();
        let delta = new_moov_size as i64 - old_moov_size as i64;

        // If moov sits before mdat and grew/shrunk, shift every chunk offset.
        if !mdat_after_moov && delta != 0 {
            patch_chunk_offsets(&mut top[moov_idx], delta);
        }

        // Emit the whole file: every top-level box in order.
        let mut out = Vec::with_capacity(bytes.len() + delta.max(0) as usize);
        for t in &top {
            out.extend_from_slice(&t.serialize());
        }
        std::fs::write(path, out).map_err(|e| MetaError(e.to_string()))?;
        Ok(LyricsOutcome::Written)
    }
}
