//! A minimal HTTPS/1.1 client built directly on `rustls`.
//!
//! Why hand-roll this instead of using `reqwest` / `ureq`?
//! Each of those crates pulls in a substantial dependency tree (hyper,
//! tokio, h2, http, ...). For a tool that only needs simple synchronous
//! GET requests against a single JSON API, a ~250-line client over
//! `rustls` keeps the dependency surface to exactly two crates:
//! `rustls` (TLS) and `webpki-roots` (CA bundle).
//!
//! Features:
//!   * TLS 1.2 / 1.3 with the Mozilla root store (no system CA files needed)
//!   * HTTP/1.1 GET with `Host`, `User-Agent`, `Accept-Encoding: identity`
//!   * `Content-Length` and `Transfer-Encoding: chunked` response bodies
//!   * Up to 5 redirect hops
//!   * Per-request timeout and 3 retries with exponential backoff

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::ClientConfig;
use rustls::{ClientConnection, RootCertStore, Stream};

/// Default per-request timeout (connect + read).
const TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum redirect hops before we give up.
const MAX_REDIRECTS: usize = 5;
/// How many times to retry a transiently-failed request.
const MAX_RETRIES: u32 = 3;
/// Base backoff (1s, 2s, 4s) — matches the C implementation.
const BASE_BACKOFF: Duration = Duration::from_secs(1);

const USER_AGENT: &str = "synclyr2metadata-rs (https://github.com/newtonsart/synclyr2metadata)";

/// An HTTP response: status code, header map and decoded body.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status_code: u16,
    /// Lowercased header name → value. Last value wins on duplicates.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Errors produced by the HTTP client.
#[derive(Debug, Clone)]
pub enum HttpError {
    /// URL was malformed or used an unsupported scheme.
    BadUrl(String),
    /// DNS resolution or TCP connection failure.
    Connect(String),
    /// TLS handshake / negotiation failure.
    Tls(String),
    /// The server closed early or sent malformed HTTP.
    Protocol(String),
    /// Reading the body exceeded the deadline.
    Timeout,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::BadUrl(m) => write!(f, "bad url: {m}"),
            HttpError::Connect(m) => write!(f, "connect: {m}"),
            HttpError::Tls(m) => write!(f, "tls: {m}"),
            HttpError::Protocol(m) => write!(f, "protocol: {m}"),
            HttpError::Timeout => write!(f, "timeout"),
        }
    }
}

impl std::error::Error for HttpError {}

/// A shareable HTTP client. Cheap to clone — the heavy state (TLS config
/// and root store) lives behind an `Arc`.
#[derive(Clone)]
pub struct HttpClient {
    tls: Arc<ClientConfig>,
}

impl HttpClient {
    /// Build a client using the Mozilla root certificate bundle that
    /// `webpki-roots` embeds at compile time.
    pub fn new() -> Result<Self, HttpError> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(HttpClient {
            tls: Arc::new(config),
        })
    }

    /// Perform a GET request, following redirects and retrying transient
    /// errors. Returns the final response.
    pub fn get(&self, url: &str) -> Result<HttpResponse, HttpError> {
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let resp = self.get_once(&current)?;
            if (300..400).contains(&resp.status_code) {
                if let Some(loc) = resp.header("location") {
                    let next = resolve_relative(&current, loc)?;
                    current = next;
                    continue;
                }
                return Err(HttpError::Protocol(format!(
                    "redirect {} without Location",
                    resp.status_code
                )));
            }
            return Ok(resp);
        }
        Err(HttpError::Protocol("too many redirects".into()))
    }

    /// Single attempt, no redirect handling.
    fn get_once(&self, url: &str) -> Result<HttpResponse, HttpError> {
        let ParsedUrl { host, port, path } = parse_url(url)?;

        let mut last_err: Option<HttpError> = None;
        for attempt in 0..=MAX_RETRIES {
            match self.do_request(&host, port, &path) {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    last_err = Some(e.clone());
                    if attempt < MAX_RETRIES && is_retryable(&e) {
                        let delay = BASE_BACKOFF * (1 << attempt);
                        eprintln!(
                            "warning: {}, retrying in {}s ({}/{})",
                            e,
                            delay.as_secs(),
                            attempt + 1,
                            MAX_RETRIES
                        );
                        std::thread::sleep(delay);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| HttpError::Protocol("no attempt made".into())))
    }

    /// Open a TCP+TLS connection and send a single GET request.
    fn do_request(&self, host: &str, port: u16, path: &str) -> Result<HttpResponse, HttpError> {
        // DNS resolution + TCP connect.
        let addr = format!("{host}:{port}");
        let socket_addrs = addr
            .to_socket_addrs()
            .map_err(|e| HttpError::Connect(format!("dns: {e}")))?;
        let mut tcp = None;
        let mut last_err = String::new();
        for sa in socket_addrs {
            match TcpStream::connect_timeout(&sa, TIMEOUT) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        let mut tcp = tcp.ok_or(HttpError::Connect(last_err))?;
        tcp.set_read_timeout(Some(TIMEOUT))
            .map_err(|e| HttpError::Connect(e.to_string()))?;
        tcp.set_write_timeout(Some(TIMEOUT))
            .map_err(|e| HttpError::Connect(e.to_string()))?;

        // TLS handshake.
        let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|e| HttpError::Tls(format!("invalid server name: {e}")))?;
        let mut conn = ClientConnection::new(self.tls.clone(), server_name)
            .map_err(|e| HttpError::Tls(e.to_string()))?;
        let mut tls = Stream::new(&mut conn, &mut tcp);

        // Build and send the request.
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host_header}\r\n\
             User-Agent: {ua}\r\n\
             Accept: application/json\r\n\
             Accept-Encoding: identity\r\n\
             Connection: close\r\n\
             \r\n",
            host_header = host_with_port(host, port),
            ua = USER_AGENT,
        );
        tls.write_all(request.as_bytes())
            .map_err(|e| HttpError::Protocol(format!("write: {e}")))?;
        tls.flush()
            .map_err(|e| HttpError::Protocol(format!("flush: {e}")))?;

        // Read the entire response into memory (LRCLIB responses are small).
        let mut raw = Vec::with_capacity(8192);
        let mut buf = [0u8; 4096];
        loop {
            match tls.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    return Err(HttpError::Timeout);
                }
                Err(e) => return Err(HttpError::Protocol(format!("read: {e}"))),
            }
        }

        parse_response(&raw)
    }
}

/// Decide whether a failure is worth retrying (transient network/TLS issues).
fn is_retryable(e: &HttpError) -> bool {
    matches!(
        e,
        HttpError::Connect(_) | HttpError::Tls(_) | HttpError::Timeout
    )
}

fn host_with_port(host: &str, port: u16) -> String {
    // HTTP default port 80 and HTTPS default port 443 should not be sent in
    // the Host header; other ports must be included.
    if port == 443 || port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

/// Parsed components of a URL. Only what we need for HTTP/HTTPS GETs.
struct ParsedUrl {
    host: String,
    port: u16,
    path: String,
}

/// Parse an `http(s)://host[:port]/path?query` URL.
fn parse_url(url: &str) -> Result<ParsedUrl, HttpError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| HttpError::BadUrl("missing scheme".into()))?;
    let default_port = match scheme {
        "https" => 443,
        "http" => 80,
        other => return Err(HttpError::BadUrl(format!("unsupported scheme '{other}'"))),
    };

    // Split authority from path. The path starts at the first '/'.
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let (host, port) = match authority.rfind(':') {
        Some(i) if !authority[i..].starts_with("]:") => {
            let (h, p) = authority.split_at(i);
            let p = &p[1..]; // strip ':'
            let p: u16 = p
                .parse()
                .map_err(|_| HttpError::BadUrl(format!("invalid port '{p}'")))?;
            (h.to_string(), p)
        }
        _ => (authority.to_string(), default_port),
    };

    if host.is_empty() {
        return Err(HttpError::BadUrl("empty host".into()));
    }

    Ok(ParsedUrl {
        host,
        port,
        path: path.to_string(),
    })
}

/// Resolve a possibly-relative redirect `Location` against the request URL.
fn resolve_relative(base: &str, location: &str) -> Result<String, HttpError> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    if location.starts_with("//") {
        let scheme = base
            .split("://")
            .next()
            .ok_or_else(|| HttpError::BadUrl("bad base url".into()))?;
        return Ok(format!("{scheme}:{location}"));
    }
    let base_parsed = parse_url(base)?;
    let authority = host_with_port(&base_parsed.host, base_parsed.port);
    if location.starts_with('/') {
        Ok(format!("https://{authority}{location}"))
    } else {
        // Relative to the base path's directory.
        let dir = base_parsed
            .path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        if dir.is_empty() {
            Ok(format!("https://{authority}/{location}"))
        } else {
            Ok(format!("https://{authority}{dir}/{location}"))
        }
    }
}

/// Parse a raw HTTP/1.1 response buffer into status + headers + body.
fn parse_response(raw: &[u8]) -> Result<HttpResponse, HttpError> {
    // Split headers from body at the first blank line (\r\n\r\n).
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| HttpError::Protocol("no header terminator".into()))?;
    let header_bytes = &raw[..split];
    let body_bytes = &raw[split + 4..];

    let header_text = std::str::from_utf8(header_bytes)
        .map_err(|_| HttpError::Protocol("non-utf8 headers".into()))?;

    // First line: "HTTP/1.1 200 OK".
    let status_line = header_text
        .lines()
        .next()
        .ok_or_else(|| HttpError::Protocol("empty header section".into()))?;
    let mut parts = status_line.split_whitespace();
    parts.next(); // HTTP version
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| HttpError::Protocol("bad status line".into()))?;

    // Collect headers (lowercased names) and pick out framing info.
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in header_text.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
            if k == "content-length" {
                content_length = v.parse::<usize>().ok();
            }
            headers.push((k, v));
        }
    }

    let body = if chunked {
        decode_chunked(body_bytes)?
    } else if let Some(len) = content_length {
        let len = len.min(body_bytes.len());
        String::from_utf8_lossy(&body_bytes[..len]).into_owned()
    } else {
        // No framing info: take everything up to EOF (Connection: close).
        String::from_utf8_lossy(body_bytes).into_owned()
    };

    Ok(HttpResponse {
        status_code: status,
        headers,
        body,
    })
}

/// Decode an HTTP/1.1 chunked transfer-encoding body.
fn decode_chunked(data: &[u8]) -> Result<String, HttpError> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        // Read the chunk size line (hex digits terminated by \r\n).
        let line_end = data[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| HttpError::Protocol("chunk size line truncated".into()))?;
        let size_str = std::str::from_utf8(&data[pos..pos + line_end])
            .map_err(|_| HttpError::Protocol("non-utf8 chunk size".into()))?;
        // Chunk extensions (";ext") may follow the size — ignore them.
        let size_hex = size_str.split(';').next().unwrap_or(size_str).trim();
        let chunk_size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| HttpError::Protocol(format!("bad chunk size '{size_hex}'")))?;
        pos += line_end + 2;
        if chunk_size == 0 {
            break;
        }
        if pos + chunk_size > data.len() {
            return Err(HttpError::Protocol("chunk body truncated".into()));
        }
        out.extend_from_slice(&data[pos..pos + chunk_size]);
        pos += chunk_size;
        // Skip the trailing CRLF after the chunk data.
        if pos + 2 <= data.len() && &data[pos..pos + 2] == b"\r\n" {
            pos += 2;
        }
    }
    String::from_utf8(out).map_err(|_| HttpError::Protocol("non-utf8 chunk body".into()))
}

/// Percent-encode a string for use in a URL query parameter.
///
/// Encodes everything except unreserved characters (`A-Z a-z 0-9 - _ . ~`)
/// as `%HH`. Spaces become `%20` (not `+`), matching the behaviour of
/// libcurl's `curl_easy_escape` used by the original C implementation.
pub fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}
