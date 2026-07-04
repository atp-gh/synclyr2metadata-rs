//! Client for the [LRCLIB](https://lrclib.net) lyrics API.
//!
//! This is a direct port of `lrclib.c` from the original C project,
//! preserving its two-stage lookup strategy and fuzzy candidate scoring:
//!
//! 1. **Exact `/api/get`** — fast path, returns the single best match for
//!    the given `(artist, track, album, duration)`.
//! 2. **Fuzzy `/api/search`** — when the exact endpoint returns nothing
//!    useful (no synced lyrics and the track isn't marked instrumental),
//!    we search and score every candidate by artist/title/album/duration
//!    similarity and pick the best one above a quality threshold.
//! 3. **Relaxed `/api/get`** — last resort: drop the album and duration
//!    constraints and ask for any track matching `(artist, track)`.
//!
//! All scoring constants mirror the C implementation so behaviour matches.

use crate::http_client::{url_encode, HttpClient, HttpError};
use crate::json::{self, Json};

const LRCLIB_BASE_URL: &str = "https://lrclib.net/api";

/// A single lyrics record from LRCLIB.
#[derive(Debug, Clone, Default)]
pub struct LrclibTrack {
    pub artist_name: Option<String>,
    pub track_name: Option<String>,
    pub album_name: Option<String>,
    pub duration: f64,
    pub synced_lyrics: Option<String>,
    pub plain_lyrics: Option<String>,
    pub instrumental: bool,
}

impl LrclibTrack {
    /// `true` if this record carries any lyrics at all (synced, plain, or
    /// the instrumental flag).
    fn has_any_lyrics(&self) -> bool {
        self.instrumental
            || self
                .synced_lyrics
                .as_deref()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
            || self
                .plain_lyrics
                .as_deref()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
    }

    fn has_synced_or_instrumental(&self) -> bool {
        self.instrumental
            || self
                .synced_lyrics
                .as_deref()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
    }
}

/// Parse a JSON object into an `LrclibTrack`.
fn parse_track(obj: &Json) -> Option<LrclibTrack> {
    let get_str = |key: &str| obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
    let duration = obj.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let instrumental = obj
        .get("instrumental")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Some(LrclibTrack {
        artist_name: get_str("artistName"),
        track_name: get_str("trackName"),
        album_name: get_str("albumName"),
        duration,
        synced_lyrics: get_str("syncedLyrics"),
        plain_lyrics: get_str("plainLyrics"),
        instrumental,
    })
}

/// Perform an authenticated GET and parse the body as JSON.
/// A 404 is reported as `Ok(None)` rather than an error, since "no match"
/// is a normal outcome for the `/api/get` endpoint.
fn api_get(client: &HttpClient, url: &str) -> Result<Option<Json>, HttpError> {
    let resp = client.get(url)?;
    if resp.status_code == 404 {
        return Ok(None);
    }
    if resp.status_code != 200 {
        eprintln!("error: LRCLIB API returned HTTP {}", resp.status_code);
        return Ok(None);
    }
    match json::parse(&resp.body) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            eprintln!("error: failed to parse API response as JSON: {e}");
            Ok(None)
        }
    }
}

/// Build a `/api/get` URL with optional album and duration filters.
fn build_get_url(artist: &str, track: &str, album: Option<&str>, duration: Option<f64>) -> String {
    let mut url = format!(
        "{base}/get?artist_name={a}&track_name={t}",
        base = LRCLIB_BASE_URL,
        a = url_encode(artist),
        t = url_encode(track),
    );
    if let Some(alb) = album {
        url.push_str(&format!("&album_name={}", url_encode(alb)));
    }
    if let Some(d) = duration {
        if d > 0.0 {
            url.push_str(&format!("&duration={:.0}", d));
        }
    }
    url
}

/// Build a `/api/search?q=...` URL (free-text query).
fn build_search_q_url(query: &str) -> String {
    format!(
        "{base}/search?q={q}",
        base = LRCLIB_BASE_URL,
        q = url_encode(query)
    )
}

/// Build a `/api/search?artist_name=...&track_name=...` URL.
fn build_search_at_url(artist: &str, track: &str) -> String {
    format!(
        "{base}/search?artist_name={a}&track_name={t}",
        base = LRCLIB_BASE_URL,
        a = url_encode(artist),
        t = url_encode(track),
    )
}

/// Public equivalent of `lrclib_get`: fetch the single best exact match.
/// `album` and `duration` may be `None`/`None` to omit them.
pub fn lrclib_get(
    client: &HttpClient,
    artist: &str,
    track: &str,
    album: Option<&str>,
    duration: Option<f64>,
) -> Result<Option<LrclibTrack>, HttpError> {
    let url = build_get_url(artist, track, album, duration);
    let json = match api_get(client, &url)? {
        Some(j) => j,
        None => return Ok(None),
    };
    Ok(parse_track(&json))
}

/// Public equivalent of `lrclib_search_best`: run `/api/search` (first by
/// artist+track, then by free-text query) and return the highest-scoring
/// candidate above the quality threshold.
pub fn lrclib_search_best(
    client: &HttpClient,
    artist: &str,
    track: &str,
    album: Option<&str>,
    duration: Option<f64>,
    prefer_synced: bool,
) -> Result<Option<LrclibTrack>, HttpError> {
    // First attempt: search by artist + track name.
    let url = build_search_at_url(artist, track);
    if let Some(j) = api_get(client, &url)? {
        if let Some(best) = pick_best(&j, artist, track, album, duration, prefer_synced) {
            return Ok(Some(best));
        }
    }

    // Second attempt: free-text "artist track" query.
    let query = format!("{artist} {track}");
    let url = build_search_q_url(&query);
    if let Some(j) = api_get(client, &url)? {
        if let Some(best) = pick_best(&j, artist, track, album, duration, prefer_synced) {
            return Ok(Some(best));
        }
    }
    Ok(None)
}

/// Normalise a string for fuzzy comparison: lowercase ASCII alphanumeric only.
/// Mirrors `normalized_copy` in the C code.
fn normalized_copy(src: &str) -> String {
    src.bytes()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|b| b.to_ascii_lowercase() as char)
        .collect()
}

/// Score how well `expected` matches `actual` after normalisation.
/// Returns `exact_score` on equality, `contains_score` on substring either
/// way, and 0 otherwise.
fn normalized_match_score(expected: &str, actual: &str, exact: i32, contains: i32) -> i32 {
    let a = normalized_copy(expected);
    let b = normalized_copy(actual);
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    if a == b {
        exact
    } else if a.contains(&b) || b.contains(&a) {
        contains
    } else {
        0
    }
}

/// Score a candidate against the query. The constants below are identical
/// to `candidate_score` in `lrclib.c`.
fn candidate_score(
    candidate: &LrclibTrack,
    artist: &str,
    track: &str,
    album: Option<&str>,
    duration: Option<f64>,
    prefer_synced: bool,
) -> i32 {
    let track_score =
        normalized_match_score(track, candidate.track_name.as_deref().unwrap_or(""), 60, 25);
    let artist_score = normalized_match_score(
        artist,
        candidate.artist_name.as_deref().unwrap_or(""),
        40,
        15,
    );

    // If either the artist or the track failed to normalise-match at all,
    // this candidate is not a real match.
    if track_score == 0 || artist_score == 0 {
        return 0;
    }

    let mut score = track_score + artist_score;

    if let (Some(album), Some(cand_album)) = (album, candidate.album_name.as_deref()) {
        score += normalized_match_score(album, cand_album, 15, 5);
    }

    if let Some(d) = duration {
        let cd = candidate.duration;
        if d > 0.0 && cd > 0.0 {
            let diff = (d - cd).abs();
            if diff <= 2.0 {
                score += 15;
            } else if diff <= 5.0 {
                score += 8;
            }
        }
    }

    if candidate.instrumental {
        score += 20;
    } else if candidate
        .synced_lyrics
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        score += if prefer_synced { 30 } else { 15 };
    } else if candidate
        .plain_lyrics
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        score += 5;
    }

    score
}

/// Walk a search-result array and pick the highest-scoring candidate with
/// a score of at least 70. Matches `pick_best_from_search_json` in C.
fn pick_best(
    json: &Json,
    artist: &str,
    track: &str,
    album: Option<&str>,
    duration: Option<f64>,
    prefer_synced: bool,
) -> Option<LrclibTrack> {
    let arr = json.as_array()?;
    let mut best: Option<LrclibTrack> = None;
    let mut best_score = 0i32;
    for item in arr {
        let candidate = parse_track(item)?;
        let score = candidate_score(&candidate, artist, track, album, duration, prefer_synced);
        if score >= 70 && score > best_score {
            best = Some(candidate);
            best_score = score;
        }
    }
    best
}

/// Composite lookup that mirrors the full strategy used by the sync engine
/// in the original `try_api_lrc`. This keeps the heuristics in one place.
///
/// Returns the best track LRCLIB can offer, or `None` if nothing useful
/// was found. `relaxed` indicates the final relaxed lookup was used.
pub fn lookup(
    client: &HttpClient,
    artist: &str,
    track: &str,
    album: Option<&str>,
    duration: Option<f64>,
    prefer_synced: bool,
) -> Result<Option<LrclibTrack>, HttpError> {
    // 1) Exact `/api/get`.
    let mut current = lrclib_get(client, artist, track, album, duration)?;

    // 2) If the exact result is unsatisfying and we prefer synced lyrics,
    //    try the fuzzy search and adopt its candidate when it carries
    //    synced/instrumental lyrics or when the exact match had nothing.
    if prefer_synced
        && current
            .as_ref()
            .map(|c| !c.has_synced_or_instrumental())
            .unwrap_or(true)
    {
        if let Some(candidate) =
            lrclib_search_best(client, artist, track, album, duration, prefer_synced)?
        {
            let adopt = candidate.has_synced_or_instrumental()
                || current
                    .as_ref()
                    .map(|c| {
                        c.plain_lyrics
                            .as_deref()
                            .map(|s| s.is_empty())
                            .unwrap_or(true)
                    })
                    .unwrap_or(true);
            if adopt {
                current = Some(candidate);
            }
        }
    }

    // 3) Final fallback: relax album + duration.
    if current
        .as_ref()
        .map(|c| !c.has_synced_or_instrumental())
        .unwrap_or(true)
    {
        if let Some(relaxed) = lrclib_get(client, artist, track, None, None)? {
            current = Some(relaxed);
        }
    }

    // 4) If we still have nothing usable, return None.
    if current
        .as_ref()
        .map(|c| c.has_any_lyrics())
        .unwrap_or(false)
    {
        Ok(current)
    } else {
        Ok(None)
    }
}
