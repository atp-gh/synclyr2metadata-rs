//! A tiny dependency-free JSON parser.
//!
//! The LRCLIB API returns flat JSON objects and arrays of objects with
//! string / number / boolean fields. Rather than pull in `serde` + `serde_json`
//! (which would each drag in a dozen transitive crates), we implement just
//! enough JSON to parse those responses: objects, arrays, strings (with the
//! full set of escape sequences), numbers (f64), booleans and null.
//!
//! The parser preserves object key insertion order, which keeps diagnostics
//! and debugging pleasant.

use std::collections::HashMap;

/// A parsed JSON value.
#[derive(Debug, Clone)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    /// Object stored as an ordered list of `(key, value)` pairs.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Look up a field in an object by key. Returns `None` for non-objects
    /// or missing keys.
    pub fn get(&self, key: &str) -> Option<&Json> {
        if let Json::Object(entries) = self {
            for (k, v) in entries {
                if k == key {
                    return Some(v);
                }
            }
        }
        None
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Json::String(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        if let Json::Number(n) = self {
            Some(*n)
        } else {
            None
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if let Json::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        if let Json::Array(a) = self {
            Some(a)
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
}

/// Parse error carrying a byte offset into the original input for context.
#[derive(Debug, Clone)]
pub struct JsonError {
    pub offset: usize,
    pub message: String,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "json error at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for JsonError {}

/// Parse a JSON document. Returns the root value or an error.
pub fn parse(input: &str) -> Result<Json, JsonError> {
    let bytes = input.as_bytes();
    let mut p = Parser { bytes, pos: 0 };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != bytes.len() {
        return Err(p.err("trailing data after JSON value"));
    }
    Ok(v)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> JsonError {
        JsonError {
            offset: self.pos,
            message: msg.to_string(),
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Result<Json, JsonError> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Json::String(self.parse_string()?)),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err(self.err("expected JSON value")),
        }
    }

    fn parse_object(&mut self) -> Result<Json, JsonError> {
        // Consume the opening brace.
        self.pos += 1;
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(entries));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected string key in object"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after object key"));
            }
            self.pos += 1;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    continue;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
        Ok(Json::Object(entries))
    }

    fn parse_array(&mut self) -> Result<Json, JsonError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    continue;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
        Ok(Json::Array(items))
    }

    /// Parse a JSON string, resolving every standard escape sequence and
    /// UTF-16 surrogate pair into proper Rust `String` (UTF-8).
    fn parse_string(&mut self) -> Result<String, JsonError> {
        // Consume the opening quote.
        self.pos += 1;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.parse_hex4()?;
                            // Combine a high/low surrogate pair into a single scalar.
                            if (0xD800..=0xDBFF).contains(&cp) {
                                if self.peek() == Some(b'\\') {
                                    self.pos += 1;
                                    if self.peek() == Some(b'u') {
                                        self.pos += 1;
                                        let lo = self.parse_hex4()?;
                                        if (0xDC00..=0xDFFF).contains(&lo) {
                                            let scalar =
                                                0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                            if let Some(ch) = char::from_u32(scalar) {
                                                out.push(ch);
                                                continue;
                                            }
                                        }
                                    }
                                    // Malformed pair: fall back to the replacement char.
                                    out.push('\u{FFFD}');
                                } else {
                                    out.push('\u{FFFD}');
                                }
                            } else if let Some(ch) = char::from_u32(cp) {
                                out.push(ch);
                            } else {
                                out.push('\u{FFFD}');
                            }
                        }
                        _ => return Err(self.err("invalid escape character")),
                    }
                }
                _ => {
                    // Copy a UTF-8 byte sequence verbatim. We scan until the
                    // next special byte so multi-byte characters pass through.
                    let start = self.pos;
                    while let Some(b) = self.peek() {
                        if b == b'"' || b == b'\\' {
                            break;
                        }
                        self.pos += 1;
                    }
                    if let Ok(s) = std::str::from_utf8(&self.bytes[start..self.pos]) {
                        out.push_str(s);
                    } else {
                        out.push('\u{FFFD}');
                    }
                }
            }
        }
        Err(self.err("unterminated string"))
    }

    /// Read exactly 4 hex digits following a `\u` escape.
    fn parse_hex4(&mut self) -> Result<u32, JsonError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let b = self
                .peek()
                .ok_or_else(|| self.err("incomplete \\u escape"))?;
            let d = match b {
                b'0'..=b'9' => (b - b'0') as u32,
                b'a'..=b'f' => (b - b'a' + 10) as u32,
                b'A'..=b'F' => (b - b'A' + 10) as u32,
                _ => return Err(self.err("invalid hex digit in \\u escape")),
            };
            value = (value << 4) | d;
            self.pos += 1;
        }
        Ok(value)
    }

    fn parse_bool(&mut self) -> Result<Json, JsonError> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(Json::Bool(true))
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(Json::Bool(false))
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn parse_null(&mut self) -> Result<Json, JsonError> {
        if self.bytes[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(Json::Null)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn parse_number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("non-utf8 number"))?;
        text.parse::<f64>()
            .map(Json::Number)
            .map_err(|_| self.err("invalid number"))
    }
}

/// Convenience helper: build a small lookup map from an object's keys.
/// Useful when a caller wants O(1) access rather than the preserved order.
#[allow(dead_code)]
pub fn object_map(obj: &Json) -> Option<HashMap<&str, &Json>> {
    if let Json::Object(entries) = obj {
        Some(entries.iter().map(|(k, v)| (k.as_str(), v)).collect())
    } else {
        None
    }
}
