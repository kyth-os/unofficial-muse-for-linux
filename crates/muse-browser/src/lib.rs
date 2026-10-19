//! Native-messaging-host protocol core for the browser tab bridge.
//!
//! This crate is `std`-only with zero dependencies. It speaks the
//! length-prefixed JSON framing the browser extension uses
//! (`u32` little-endian byte count, then UTF-8 JSON) and validates every
//! request against a strict schema before touching the [`TabBackend`].
//!
//! Scope (deliberate):
//!
//! * Tab metadata only: `tabs.list`, `tabs.open`, `tabs.close`.
//! * No tab-body reading, no cookies, no history, no bookmarks APIs.
//! * No network access, no credentials.
//! * Every action is relayed from an explicit user gesture in the extension
//!   popup (per-action consent). There is no remote-trigger or polling path.
//! * Form filling and per-action remote approval are explicitly OUT of scope.
//!
//! Error `detail` strings are `&'static str` constants only: user input is
//! never echoed back into replies.

use std::fmt;
use std::io;

// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

/// Fail-closed cap on a single native-messaging message body (1 MiB).
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Native-messaging host name both the extension and the host manifest use.
pub const NATIVE_HOST_NAME: &str = "org.unofficial_muse_for_linux";

/// Maximum accepted URL length (mirrors the desktop crate's OpenURI rules).
const MAX_URL_LEN: usize = 2048;

/// Cap on JSON nesting depth (fail-closed against stack exhaustion).
const MAX_DEPTH: usize = 64;

// ---------------------------------------------------------------------------
// Backend trait + types
// ---------------------------------------------------------------------------

/// One browser tab: metadata only. There is intentionally no body field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub id: u64,
    pub title: String,
    pub url: String,
}

/// Backend failure. `detail` is a static string so request bytes can never
/// leak back through an error reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendError {
    pub detail: &'static str,
}

impl BackendError {
    pub const fn new(detail: &'static str) -> Self {
        Self { detail }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail)
    }
}

impl std::error::Error for BackendError {}

/// Implemented by the real browser side; faked in tests.
pub trait TabBackend {
    fn list_tabs(&self) -> Result<Vec<TabInfo>, BackendError>;
    fn open_tab(&self, url: &str) -> Result<TabInfo, BackendError>;
    fn close_tab(&self, id: u64) -> Result<(), BackendError>;
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Handle one raw message `body` (bytes AFTER the 4-byte length prefix) and
/// return the full framed reply: 4-byte little-endian length + UTF-8 JSON.
///
/// Pure: all I/O happens in [`run_loop`]; tests call this with a fake
/// backend and assert exact reply bytes.
pub fn handle_message(body: &[u8], backend: &dyn TabBackend) -> Vec<u8> {
    frame(dispatch(body, backend).as_bytes())
}

/// Run the native-messaging loop: read framed requests from `reader`, write
/// framed replies to `writer`, until clean EOF or a fail-closed stop.
///
/// * A declared length above [`MAX_MESSAGE_BYTES`] gets a single `too_large`
///   reply and the loop stops (fail-closed: the body is NOT drained, so a
///   hostile 4 GiB length prefix cannot force unbounded reads).
/// * A truncated header or body (EOF mid-message) stops the loop silently.
pub fn run_loop<R: io::Read, W: io::Write>(
    reader: &mut R,
    writer: &mut W,
    backend: &dyn TabBackend,
) -> io::Result<()> {
    loop {
        let mut len_buf = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            match reader.read(&mut len_buf[got..])? {
                0 => return Ok(()), // clean EOF, or truncated header: stop
                n => got += n,
            }
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_MESSAGE_BYTES {
            let reply = frame(error_json(None, "too_large", "message too large").as_bytes());
            writer.write_all(&reply)?;
            writer.flush()?;
            return Ok(());
        }
        let mut body = vec![0u8; len];
        match reader.read_exact(&mut body) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let reply = handle_message(&body, backend);
        writer.write_all(&reply)?;
        writer.flush()?;
    }
}

/// Prefix `payload` with its 4-byte little-endian length.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

// ---------------------------------------------------------------------------
// Dispatch + validation
// ---------------------------------------------------------------------------

fn dispatch(body: &[u8], backend: &dyn TabBackend) -> String {
    if body.len() > MAX_MESSAGE_BYTES {
        return error_json(None, "too_large", "message too large");
    }
    let text = match std::str::from_utf8(body) {
        Ok(t) => t,
        Err(_) => return error_json(None, "invalid_request", "message is not valid utf-8"),
    };
    let value = match parse_json(text) {
        Ok(v) => v,
        Err(_) => return error_json(None, "invalid_request", "message is not valid json"),
    };
    let req = match value {
        Json::Obj(fields) => fields,
        _ => {
            return error_json(
                None,
                "invalid_request",
                "request must be an object with id, op and params",
            )
        }
    };

    // Strict top-level schema: exactly {id, op, params}, nothing else.
    if req.len() != 3
        || !req.iter().any(|(k, _)| k == "id")
        || !req.iter().any(|(k, _)| k == "op")
        || !req.iter().any(|(k, _)| k == "params")
    {
        // Distinguish "wrong shape" from "extra field" only by message;
        // both are invalid_request.
        let known = ["id", "op", "params"];
        if req.iter().any(|(k, _)| !known.contains(&k.as_str())) {
            // `id` may itself be unusable here; try to salvage it.
            let id = req
                .iter()
                .find(|(k, _)| k == "id")
                .and_then(|(_, v)| v.as_u64());
            return error_json(id, "invalid_request", "unknown request field");
        }
        return error_json(
            None,
            "invalid_request",
            "request must be an object with id, op and params",
        );
    }
    let get = |k: &str| req.iter().find(|(key, _)| key == k).map(|(_, v)| v);

    let id = match get("id").and_then(Json::as_u64) {
        Some(n) => n,
        None => return error_json(None, "invalid_request", "id must be an unsigned integer"),
    };
    let op = match get("op") {
        Some(Json::Str(s)) => s.as_str(),
        _ => {
            return error_json(
                Some(id),
                "invalid_request",
                "op must be one of tabs.list, tabs.open, tabs.close",
            )
        }
    };
    let params = match get("params") {
        Some(Json::Obj(fields)) => fields,
        _ => return error_json(Some(id), "invalid_request", "params must be an object"),
    };

    match op {
        "tabs.list" => {
            if !params.is_empty() {
                return error_json(Some(id), "invalid_request", "tabs.list takes no params");
            }
            match backend.list_tabs() {
                Ok(tabs) => {
                    let mut r = String::from("{\"tabs\":[");
                    for (i, t) in tabs.iter().enumerate() {
                        if i > 0 {
                            r.push(',');
                        }
                        push_tab(&mut r, t);
                    }
                    r.push_str("]}");
                    ok_json(id, &r)
                }
                Err(e) => error_json(Some(id), "backend_error", e.detail),
            }
        }
        "tabs.open" => {
            if params.len() != 1 || params.iter().any(|(k, _)| k != "url") {
                if params.iter().any(|(k, _)| k != "url") {
                    return error_json(Some(id), "invalid_request", "unknown params field");
                }
                return error_json(Some(id), "invalid_request", "url is missing");
            }
            let url = match &params[0].1 {
                Json::Str(s) => s.as_str(),
                _ => return error_json(Some(id), "invalid_request", "url must be a string"),
            };
            if validate_url(url).is_err() {
                return error_json(Some(id), "invalid_request", "url rejected");
            }
            match backend.open_tab(url) {
                Ok(tab) => {
                    let mut r = String::from("{\"tab\":");
                    push_tab(&mut r, &tab);
                    r.push('}');
                    ok_json(id, &r)
                }
                Err(e) => error_json(Some(id), "backend_error", e.detail),
            }
        }
        "tabs.close" => {
            if params.len() != 1 || params.iter().any(|(k, _)| k != "id") {
                if params.iter().any(|(k, _)| k != "id") {
                    return error_json(Some(id), "invalid_request", "unknown params field");
                }
                return error_json(Some(id), "invalid_request", "tab id is missing");
            }
            let tab_id = match params[0].1.as_u64() {
                Some(n) => n,
                None => {
                    return error_json(
                        Some(id),
                        "invalid_request",
                        "tab id must be an unsigned integer",
                    )
                }
            };
            match backend.close_tab(tab_id) {
                Ok(()) => ok_json(id, "{}"),
                Err(e) => error_json(Some(id), "backend_error", e.detail),
            }
        }
        _ => error_json(Some(id), "unsupported_op", "unsupported op"),
    }
}

/// URL rules reimplemented small from the desktop crate (no cross-crate dep):
/// non-empty, <= 2048 bytes, no whitespace/control bytes, `http(s)://`
/// scheme (case-insensitive), non-empty authority with no userinfo.
fn validate_url(url: &str) -> Result<(), ()> {
    if url.is_empty() || url.len() > MAX_URL_LEN {
        return Err(());
    }
    if url.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(());
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(());
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Err(());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON emission
// ---------------------------------------------------------------------------

fn ok_json(id: u64, result: &str) -> String {
    format!("{{\"id\":{id},\"ok\":true,\"result\":{result}}}")
}

fn error_json(id: Option<u64>, code: &str, detail: &str) -> String {
    let id_str = match id {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    };
    format!("{{\"id\":{id_str},\"ok\":false,\"error\":{{\"code\":\"{code}\",\"detail\":\"{detail}\"}}}}")
}

fn push_tab(out: &mut String, tab: &TabInfo) {
    out.push_str("{\"id\":");
    out.push_str(&tab.id.to_string());
    out.push_str(",\"title\":\"");
    push_escaped(out, &tab.title);
    out.push_str("\",\"url\":\"");
    push_escaped(out, &tab.url);
    out.push_str("\"}");
}

fn push_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal strict JSON parser (std only; rejects duplicate object keys)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
// `Bool`/`Arr` payloads are accepted by the parser but never valid in a
// request; they are read only via rejection, hence dead-code-allowed.
#[allow(dead_code)]
enum Json {
    Null,
    Bool(bool),
    Num(JsonNum),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

#[derive(Debug, Clone, Copy)]
struct JsonNum {
    negative: bool,
    int: u64,
    overflow: bool,
    has_frac_or_exp: bool,
}

impl Json {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) if !n.negative && !n.has_frac_or_exp && !n.overflow => Some(n.int),
            _ => None,
        }
    }
}

fn parse_json(text: &str) -> Result<Json, ()> {
    let bytes = text.as_bytes();
    let mut p = Parser { bytes, pos: 0 };
    p.skip_ws();
    let v = p.parse_value(0)?;
    p.skip_ws();
    if p.pos != bytes.len() {
        return Err(());
    }
    Ok(v)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Json, ()> {
        if depth > MAX_DEPTH {
            return Err(());
        }
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => Ok(Json::Str(self.parse_string()?)),
            Some(b't') => self.parse_literal("true", Json::Bool(true)),
            Some(b'f') => self.parse_literal("false", Json::Bool(false)),
            Some(b'n') => self.parse_literal("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err(()),
        }
    }

    fn parse_literal(&mut self, word: &str, value: Json) -> Result<Json, ()> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(())
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Json, ()> {
        self.pos += 1; // {
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(());
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(());
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.parse_value(depth + 1)?;
            if fields.iter().any(|(k, _): &(String, Json)| *k == key) {
                return Err(()); // duplicate key: reject
            }
            fields.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(fields));
                }
                _ => return Err(()),
            }
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Json, ()> {
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(()),
            }
        }
    }

    fn parse_number(&mut self) -> Result<Json, ()> {
        let mut negative = false;
        if self.peek() == Some(b'-') {
            negative = true;
            self.pos += 1;
        }
        let int_start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == int_start {
            return Err(());
        }
        // Leading zeros ("01") are invalid JSON.
        if self.pos - int_start > 1 && self.bytes[int_start] == b'0' {
            return Err(());
        }
        let int_end = self.pos;
        let mut has_frac_or_exp = false;
        if self.peek() == Some(b'.') {
            has_frac_or_exp = true;
            self.pos += 1;
            let frac_start = self.pos;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == frac_start {
                return Err(());
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            has_frac_or_exp = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exp_start = self.pos;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == exp_start {
                return Err(());
            }
        }
        // Integer digits are ASCII by construction, so this cannot fail.
        let int_text = std::str::from_utf8(&self.bytes[int_start..int_end]).map_err(|_| ())?;
        let mut int: u64 = 0;
        let mut overflow = false;
        for b in int_text.bytes() {
            let d = (b - b'0') as u64;
            match int.checked_mul(10).and_then(|v| v.checked_add(d)) {
                Some(v) => int = v,
                None => {
                    overflow = true;
                    break;
                }
            }
        }
        Ok(Json::Num(JsonNum {
            negative,
            int,
            overflow,
            has_frac_or_exp,
        }))
    }

    fn parse_string(&mut self) -> Result<String, ()> {
        self.pos += 1; // opening "
        let mut out = String::new();
        loop {
            let b = self.peek().ok_or(())?;
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    match self.peek().ok_or(())? {
                        b'"' => {
                            out.push('"');
                            self.pos += 1;
                        }
                        b'\\' => {
                            out.push('\\');
                            self.pos += 1;
                        }
                        b'/' => {
                            out.push('/');
                            self.pos += 1;
                        }
                        b'b' => {
                            out.push('\u{08}');
                            self.pos += 1;
                        }
                        b'f' => {
                            out.push('\u{0C}');
                            self.pos += 1;
                        }
                        b'n' => {
                            out.push('\n');
                            self.pos += 1;
                        }
                        b'r' => {
                            out.push('\r');
                            self.pos += 1;
                        }
                        b't' => {
                            out.push('\t');
                            self.pos += 1;
                        }
                        b'u' => {
                            self.pos += 1;
                            let hi = self.parse_hex4()?;
                            if (0xD800..0xDC00).contains(&hi) {
                                // Surrogate pair required.
                                if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u".as_slice())
                                {
                                    return Err(());
                                }
                                self.pos += 2;
                                let lo = self.parse_hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(());
                                }
                                let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                out.push(char::from_u32(cp).ok_or(())?);
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err(()); // lone low surrogate
                            } else {
                                out.push(char::from_u32(hi).ok_or(())?);
                            }
                        }
                        _ => return Err(()),
                    }
                }
                0x00..=0x1F => return Err(()), // unescaped control: invalid
                _ => {
                    // Consume one UTF-8 code point.
                    let rest = std::str::from_utf8(&self.bytes[self.pos..]).map_err(|_| ())?;
                    let c = rest.chars().next().ok_or(())?;
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u32, ()> {
        if self.pos + 4 > self.bytes.len() {
            return Err(());
        }
        let mut v: u32 = 0;
        for i in 0..4 {
            let d = match self.bytes[self.pos + i] {
                b'0'..=b'9' => (self.bytes[self.pos + i] - b'0') as u32,
                b'a'..=b'f' => (self.bytes[self.pos + i] - b'a' + 10) as u32,
                b'A'..=b'F' => (self.bytes[self.pos + i] - b'A' + 10) as u32,
                _ => return Err(()),
            };
            v = v * 16 + d;
        }
        self.pos += 4;
        Ok(v)
    }
}
