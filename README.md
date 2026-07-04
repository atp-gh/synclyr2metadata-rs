# synclyr2metadata-rs

A from-scratch Rust rewrite of [synclyr2metadata](https://github.com/newtonsart/synclyr2metadata) — a command-line tool that reads local `.lrc` files or downloads synchronized lyrics from [LRCLIB](https://lrclib.net) and embeds them permanently into your audio files' metadata.

This rewrite keeps the dependency surface as small as possible: **only two direct crates** (`rustls` for TLS, `webpki-roots` for the CA bundle). The HTTP/1.1 client, JSON parser, and tag readers/writers for **FLAC, MP3/ID3v2, OGG (Vorbis + Opus), and MP4/M4A** are all implemented by hand — no `reqwest`, no `serde`, no `lofty`, no TagLib.

---

## What does it do?

1. Scans audio files and reads their metadata (Artist, Title, Album, Duration).
2. Looks for a local `.lrc` sidecar file. If found, embeds it directly.
3. Otherwise searches LRCLIB for synced lyrics, falling back to plain lyrics.
4. Embeds the lyrics into the file's native lyrics tag — no separate `.lrc` files needed.

Supported formats and the tag each one writes:

| Format | Lyrics location |
|--------|----------------|
| `.flac` | `LYRICS` Vorbis comment |
| `.mp3` | `USLT` (unsynchronised lyrics) ID3v2 frame |
| `.ogg` | `LYRICS` Vorbis comment |
| `.opus` | `LYRICS` Vorbis comment |
| `.m4a` / `.mp4` / `.m4b` / `.aac` | `©lyr` atom in the `ilst` box |

---

## Build

Requirements: Rust 1.74+ (stable).

```bash
cargo build --release
# → target/release/synclyr2metadata (~1.8 MB, statically linked)
```

No system libraries are required — `ring` (the TLS crypto backend) ships its own assembly, and the Mozilla root store is embedded at compile time by `webpki-roots`.

---

## CLI usage

```bash
# Sync a flat folder of audio files
./synclyr2metadata --folder "/path/to/downloaded_tracks"

# Sync a single album
./synclyr2metadata --album "/path/to/Artist/Album (2024)"

# Sync all albums from an artist
./synclyr2metadata --artist "/path/to/Artist" --threads 8

# Sync your entire library (Artist/Album structure)
./synclyr2metadata --library "/path/to/music" --threads 4

# Log tracks that only got plain lyrics and tracks with missing lyrics
./synclyr2metadata --library "/path/to/music" --out-plain ./plain.txt --out-missing ./missing.txt

# Embed local .lrc sidecar files and delete them afterward
./synclyr2metadata --album "/path/to/album" --clean-lrc

# Overwrite lyrics that are already embedded
./synclyr2metadata --folder "/path/to/folder" --force
```

### Options

| Option | Description |
|---|---|
| `--folder PATH` | Sync audio files directly in one folder |
| `--album PATH` | Sync a single album directory |
| `--artist PATH` | Sync all albums under an artist directory |
| `--library PATH` | Sync an entire library (artist/album structure) |
| `--out-plain FILE` | Write paths of tracks that fell back to plain lyrics |
| `--out-missing FILE` | Write paths of tracks not found on LRCLIB |
| `--force` | Overwrite existing embedded lyrics |
| `--clean-lrc` | Delete the local `.lrc` file after embedding it |
| `--threads N` | Parallel download threads (default: 4, max: 16) |
| `--help` | Show help |

---

## Lidarr integration

When invoked with no CLI arguments and the `lidarr_eventtype` environment variable set, the binary automatically runs as a Lidarr Custom Script. Configure it in Lidarr under **Settings → Connect → + → Custom Script** with:

- **On Release Import**: ✓
- **On Upgrade**: ✓
- **Path**: `/config/scripts/synclyr2metadata`

Logs are written next to the binary:

- `synclyr2metadata.log` — execution log (auto-rotates at 100 KB)
- `synclyr2metadata_plain.log` — tracks that only got plain lyrics
- `synclyr2metadata_missing.log` — tracks not found on LRCLIB

---

## Example output

```
═══ MF DOOM ═══

▶ MM.FOOD (2004) (26 tracks)
  [ 1/26] Beef Rapp                                ✓ synced
  [ 2/26] Hoe Cakes                                ✓ plain
  [ 3/26] Potholderz                               ⊘ already has lyrics
  [ 4/26] Unreleased Track                         ✗ not found

──────────────────────────────────────────────
  ✓ Synced:     22
  ✓ Plain:      1
  ⊘ Skipped:    1
  ✗ Not found:  2
──────────────────────────────────────────────
```

---

## Architecture

```
src/
├── main.rs           CLI entry point + mode dispatch (folder/album/artist/library)
├── cli.rs            Hand-written argument parser
├── types.rs          TrackMeta, SyncConfig, SyncResult, TrackOutcome, ProgressFn
├── sync.rs           Parallel sync engine (std::thread work-queue)
├── fs_util.rs        Directory scanning + .lrc sidecar resolution
├── http_client.rs    rustls HTTPS/1.1 client (GET, redirects, retries, URL-encode)
├── json.rs           Minimal JSON parser (objects, arrays, strings, numbers, bools)
├── lrclib.rs         LRCLIB API client with exact→fuzzy→relaxed fallback + scoring
├── logger.rs         Timestamped logger with 100 KB rotation (Lidarr mode)
├── lidarr.rs         Lidarr Custom Script env-var integration
└── metadata/
    ├── mod.rs        AudioFormat trait + extension dispatch
    ├── flac.rs       FLAC: STREAMINFO + VORBIS_COMMENT read/write
    ├── id3v2.rs      MP3: ID3v2.3/2.4 frames + MPEG duration estimation
    ├── ogg.rs        OGG Vorbis/Opus: page re-encoding with CRC32 for tag edits
    └── mp4.rs        MP4/M4A: box tree + stco/co64 offset patching
```

### Key design decisions

- **No async runtime.** The sync engine uses `std::thread` with a shared atomic work index, matching the original pthread design. Each worker owns its TLS state via `Arc<HttpClient>`.
- **One HTTP connection per request.** rustls session tickets provide TLS resumption across connections, so the overhead is minimal while keeping the code simple. Retries use exponential backoff (1s, 2s, 4s).
- **`meta` FullBox handling.** The MP4 `meta` box carries a 4-byte version/flags prefix before its children; the parser strips it on read and re-inserts zeros on write.
- **stco/co64 patching.** When editing `moov` shifts a preceding `mdat`, every chunk offset in every `stco`/`co64` table is shifted by the delta so the file stays playable.
- **OGG re-encoding.** Editing a Vorbis/Opus comment packet changes its length, so the header pages are rebuilt from scratch and every subsequent page gets a new sequence number and recomputed OGG CRC32.

---

## Dependencies

| Crate | Why |
|---|---|
| `rustls` | Pure-Rust TLS 1.2/1.3 (no OpenSSL) |
| `webpki-roots` | Mozilla CA bundle embedded at compile time |

Everything else — HTTP/1.1, JSON, ID3v2, FLAC, OGG, MP4, URL encoding, OGG CRC32, MPEG frame parsing — is implemented in this crate (~4,300 lines of Rust).

---

## License

[GPLv3](LICENSE)

## Acknowledgements
[synclyr2metadata](https://github.com/newtonsart/synclyr2metadata)
[LRCLIB](https://lrclib.net)
