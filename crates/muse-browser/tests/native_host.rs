//! Native-host framing + schema tests with a fake [`TabBackend`].
//!
//! Every assertion is on exact reply bytes: 4-byte little-endian length
//! prefix followed by UTF-8 JSON.

use muse_browser::{frame, handle_message, run_loop, BackendError, TabBackend, TabInfo};
use std::cell::{Cell, RefCell};
use std::io::Cursor;

// ---------------------------------------------------------------------------
// Fake backend
// ---------------------------------------------------------------------------

struct Fake {
    tabs: RefCell<Vec<TabInfo>>,
    next_id: Cell<u64>,
    fail: Cell<bool>,
    closed: RefCell<Vec<u64>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            tabs: RefCell::new(vec![
                TabInfo {
                    id: 1,
                    title: "Example".to_string(),
                    url: "https://example.com/".to_string(),
                },
                TabInfo {
                    id: 2,
                    title: "Quote \" \\ test".to_string(),
                    url: "https://example.org/a?b=c".to_string(),
                },
            ]),
            next_id: Cell::new(3),
            fail: Cell::new(false),
            closed: RefCell::new(Vec::new()),
        }
    }

    fn failing() -> Self {
        let f = Self::new();
        f.fail.set(true);
        f
    }
}

impl TabBackend for Fake {
    fn list_tabs(&self) -> Result<Vec<TabInfo>, BackendError> {
        if self.fail.get() {
            return Err(BackendError::new("list unavailable"));
        }
        Ok(self.tabs.borrow().clone())
    }

    fn open_tab(&self, url: &str) -> Result<TabInfo, BackendError> {
        if self.fail.get() {
            return Err(BackendError::new("open unavailable"));
        }
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let tab = TabInfo {
            id,
            title: "New tab".to_string(),
            url: url.to_string(),
        };
        self.tabs.borrow_mut().push(tab.clone());
        Ok(tab)
    }

    fn close_tab(&self, id: u64) -> Result<(), BackendError> {
        if self.fail.get() {
            return Err(BackendError::new("close unavailable"));
        }
        self.closed.borrow_mut().push(id);
        let mut tabs = self.tabs.borrow_mut();
        if let Some(pos) = tabs.iter().position(|t| t.id == id) {
            tabs.remove(pos);
            Ok(())
        } else {
            Err(BackendError::new("no such tab"))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split a framed reply into (declared_len, payload text).
fn split(reply: &[u8]) -> (u32, String) {
    assert!(reply.len() >= 4, "reply shorter than length prefix");
    let len = u32::from_le_bytes(reply[0..4].try_into().unwrap());
    assert_eq!(
        len as usize,
        reply.len() - 4,
        "length prefix must match payload bytes"
    );
    let payload = std::str::from_utf8(&reply[4..]).expect("payload must be utf-8");
    (len, payload.to_string())
}

fn req(id: u64, op: &str, params: &str) -> Vec<u8> {
    format!("{{\"id\":{id},\"op\":\"{op}\",\"params\":{params}}}").into_bytes()
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[test]
fn frame_prefix_is_exact_le_length() {
    let payload = b"{\"a\":1}";
    let framed = frame(payload);
    assert_eq!(&framed[0..4], &(payload.len() as u32).to_le_bytes());
    assert_eq!(&framed[4..], payload);
}

#[test]
fn tabs_list_reply_is_exact_bytes() {
    let reply = handle_message(&req(7, "tabs.list", "{}"), &Fake::new());
    let (_, payload) = split(&reply);
    assert_eq!(
        payload,
        "{\"id\":7,\"ok\":true,\"result\":{\"tabs\":[\
         {\"id\":1,\"title\":\"Example\",\"url\":\"https://example.com/\"},\
         {\"id\":2,\"title\":\"Quote \\\" \\\\ test\",\"url\":\"https://example.org/a?b=c\"}\
         ]}}"
    );
}

#[test]
fn tabs_open_reply_returns_new_tab() {
    let backend = Fake::new();
    let reply = handle_message(
        &req(3, "tabs.open", "{\"url\":\"https://example.com/new\"}"),
        &backend,
    );
    let (_, payload) = split(&reply);
    assert_eq!(
        payload,
        "{\"id\":3,\"ok\":true,\"result\":{\"tab\":\
         {\"id\":3,\"title\":\"New tab\",\"url\":\"https://example.com/new\"}}}"
    );
}

#[test]
fn tabs_close_reply_is_empty_result() {
    let backend = Fake::new();
    let reply = handle_message(&req(9, "tabs.close", "{\"id\":2}"), &backend);
    let (_, payload) = split(&reply);
    assert_eq!(payload, "{\"id\":9,\"ok\":true,\"result\":{}}");
    assert_eq!(*backend.closed.borrow(), vec![2]);
}

// ---------------------------------------------------------------------------
// Cap rejection (fail-closed)
// ---------------------------------------------------------------------------

#[test]
fn oversize_body_is_too_large() {
    let mut body = b"{\"id\":1,\"op\":\"tabs.list\",\"params\":{}}".to_vec();
    body.resize(muse_browser::MAX_MESSAGE_BYTES + 1, b' ');
    let reply = handle_message(&body, &Fake::new());
    let (_, payload) = split(&reply);
    assert_eq!(
        payload,
        "{\"id\":null,\"ok\":false,\"error\":{\"code\":\"too_large\",\"detail\":\"message too large\"}}"
    );
}

#[test]
fn run_loop_stops_fail_closed_on_oversize_declared_length() {
    // Declared 2 MiB but no body follows: one too_large reply, then stop.
    // (The body is deliberately NOT drained: unbounded reads are the DoS.)
    let mut input = Vec::new();
    input.extend_from_slice(&((muse_browser::MAX_MESSAGE_BYTES + 1) as u32).to_le_bytes());
    let mut reader = Cursor::new(input);
    let mut writer: Vec<u8> = Vec::new();
    run_loop(&mut reader, &mut writer, &Fake::new()).unwrap();
    let (_, payload) = split(&writer);
    assert!(payload.contains("\"code\":\"too_large\""));
}

// ---------------------------------------------------------------------------
// Malformed input
// ---------------------------------------------------------------------------

#[test]
fn malformed_json_is_invalid_request() {
    for body in [
        b"{\"id\":1,".as_slice(),
        b"not json".as_slice(),
        b"[1,2]".as_slice(),
        b"{\"id\":1,\"op\":\"tabs.list\",\"params\":{}}trailing".as_slice(),
        b"{\"id\":01,\"op\":\"tabs.list\",\"params\":{}}".as_slice(), // leading zero
    ] {
        let (_, payload) = split(&handle_message(body, &Fake::new()));
        let expected = if body == b"[1,2]" {
            // Parses, but is not a request object.
            "\"request must be an object with id, op and params\""
        } else {
            "\"message is not valid json\""
        };
        assert!(
            payload.contains("\"code\":\"invalid_request\"") && payload.contains(expected),
            "body {body:?} -> {payload}"
        );
    }
}

#[test]
fn invalid_utf8_is_invalid_request() {
    let body = b"{\"id\":\xff}".to_vec();
    let (_, payload) = split(&handle_message(&body, &Fake::new()));
    assert!(payload.contains("\"code\":\"invalid_request\""));
    assert!(payload.contains("not valid utf-8"));
}

#[test]
fn missing_keys_are_invalid_request() {
    let (_, payload) = split(&handle_message(
        b"{\"id\":1,\"op\":\"tabs.list\"}",
        &Fake::new(),
    ));
    assert!(payload.contains("\"code\":\"invalid_request\""));
    assert!(payload.contains("id, op and params"));
}

// ---------------------------------------------------------------------------
// Unknown ops and fields
// ---------------------------------------------------------------------------

#[test]
fn unknown_op_is_unsupported_op_with_request_id() {
    let (_, payload) = split(&handle_message(
        &req(4, "tabs.readBody", "{}"),
        &Fake::new(),
    ));
    assert_eq!(
        payload,
        "{\"id\":4,\"ok\":false,\"error\":{\"code\":\"unsupported_op\",\"detail\":\"unsupported op\"}}"
    );
}

#[test]
fn unknown_top_level_field_rejected() {
    let body = b"{\"id\":5,\"op\":\"tabs.list\",\"params\":{},\"extra\":1}";
    let (_, payload) = split(&handle_message(body, &Fake::new()));
    assert!(payload.contains("\"code\":\"invalid_request\""));
    assert!(payload.contains("\"id\":5"));
    assert!(payload.contains("unknown request field"));
}

#[test]
fn unknown_params_field_rejected() {
    for (op, params) in [
        ("tabs.list", "{\"verbose\":true}"),
        (
            "tabs.open",
            "{\"url\":\"https://example.com/\",\"extra\":1}",
        ),
        ("tabs.close", "{\"id\":1,\"extra\":1}"),
    ] {
        let (_, payload) = split(&handle_message(&req(6, op, params), &Fake::new()));
        assert!(
            payload.contains("\"code\":\"invalid_request\""),
            "{op} {params} -> {payload}"
        );
    }
}

#[test]
fn duplicate_keys_rejected() {
    let body = b"{\"id\":1,\"id\":2,\"op\":\"tabs.list\",\"params\":{}}";
    let (_, payload) = split(&handle_message(body, &Fake::new()));
    assert!(payload.contains("\"code\":\"invalid_request\""));
}

// ---------------------------------------------------------------------------
// ID validation
// ---------------------------------------------------------------------------

#[test]
fn non_u64_ids_rejected() {
    for id_json in [
        "\"7\"",
        "7.0",
        "-1",
        "18446744073709551616", // u64::MAX + 1
        "null",
        "true",
    ] {
        let body = format!("{{\"id\":{id_json},\"op\":\"tabs.list\",\"params\":{{}}}}");
        let (_, payload) = split(&handle_message(body.as_bytes(), &Fake::new()));
        assert!(
            payload.contains("\"code\":\"invalid_request\"") && payload.contains("\"id\":null"),
            "{id_json} -> {payload}"
        );
    }
}

#[test]
fn tabs_close_requires_u64_tab_id() {
    for params in ["{\"id\":\"2\"}", "{\"id\":2.5}", "{\"id\":-2}", "{}"] {
        let (_, payload) = split(&handle_message(&req(8, "tabs.close", params), &Fake::new()));
        assert!(
            payload.contains("\"code\":\"invalid_request\"") && payload.contains("\"id\":8"),
            "{params} -> {payload}"
        );
    }
}

// ---------------------------------------------------------------------------
// URL validation (mirrors the desktop crate's OpenURI rules)
// ---------------------------------------------------------------------------

#[test]
fn url_validation_rejects_dangerous_schemes() {
    for url in [
        "javascript:alert(1)",
        "JaVaScRiPt:alert(1)",
        "file:///etc/passwd",
        "data:text/html,<h1>x</h1>",
        "chrome://settings",
        "about:blank",
        "http:example.com", // missing ://
        "example.com",
        "",
        "https://",
        "https://user@example.com/",
        "https://exa mple.com/",
        "https://example.com/\u{7f}",
    ] {
        let params = format!("{{\"url\":\"{url}\"}}");
        let (_, payload) = split(&handle_message(&req(1, "tabs.open", &params), &Fake::new()));
        assert!(
            payload.contains("\"code\":\"invalid_request\"")
                && payload.contains("\"detail\":\"url rejected\""),
            "{url:?} -> {payload}"
        );
        // Static-only detail: the URL must never be echoed back.
        assert!(!payload.contains("alert"), "{url:?} echoed in {payload}");
    }
}

#[test]
fn url_validation_accepts_http_and_https() {
    let backend = Fake::new();
    for url in [
        "http://example.com/",
        "https://example.com/a?b=c#d",
        "HTTPS://example.com/",
        "http://localhost:8080/x",
    ] {
        let params = format!("{{\"url\":\"{url}\"}}");
        let (_, payload) = split(&handle_message(&req(1, "tabs.open", &params), &backend));
        assert!(payload.contains("\"ok\":true"), "{url} -> {payload}");
    }
}

#[test]
fn overlong_url_rejected() {
    let long = format!("https://example.com/{}", "a".repeat(2048));
    assert!(long.len() > 2048);
    let params = format!("{{\"url\":\"{long}\"}}");
    let (_, payload) = split(&handle_message(&req(1, "tabs.open", &params), &Fake::new()));
    assert!(payload.contains("\"detail\":\"url rejected\""));
}

#[test]
fn non_string_url_rejected() {
    let (_, payload) = split(&handle_message(
        &req(1, "tabs.open", "{\"url\":42}"),
        &Fake::new(),
    ));
    assert!(payload.contains("\"code\":\"invalid_request\""));
}

// ---------------------------------------------------------------------------
// Backend error mapping
// ---------------------------------------------------------------------------

#[test]
fn backend_errors_map_to_backend_error_code() {
    let backend = Fake::failing();
    for (op, params) in [
        ("tabs.list", "{}"),
        ("tabs.open", "{\"url\":\"https://example.com/\"}"),
        ("tabs.close", "{\"id\":1}"),
    ] {
        let (_, payload) = split(&handle_message(&req(2, op, params), &backend));
        assert!(
            payload.contains("\"code\":\"backend_error\"") && payload.contains("\"id\":2"),
            "{op} -> {payload}"
        );
    }
}

#[test]
fn close_of_missing_tab_is_backend_error() {
    let (_, payload) = split(&handle_message(
        &req(2, "tabs.close", "{\"id\":999}"),
        &Fake::new(),
    ));
    assert_eq!(
        payload,
        "{\"id\":2,\"ok\":false,\"error\":{\"code\":\"backend_error\",\"detail\":\"no such tab\"}}"
    );
}

// ---------------------------------------------------------------------------
// run_loop end-to-end
// ---------------------------------------------------------------------------

#[test]
fn run_loop_serves_two_messages_then_eof() {
    let mut input = Vec::new();
    for body in [
        req(1, "tabs.list", "{}"),
        req(2, "tabs.close", "{\"id\":1}"),
    ] {
        input.extend_from_slice(&(body.len() as u32).to_le_bytes());
        input.extend_from_slice(&body);
    }
    let mut reader = Cursor::new(input);
    let mut writer: Vec<u8> = Vec::new();
    run_loop(&mut reader, &mut writer, &Fake::new()).unwrap();

    // Walk the two framed replies.
    let mut pos = 0;
    let mut payloads = Vec::new();
    while pos < writer.len() {
        let len = u32::from_le_bytes(writer[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        payloads.push(
            std::str::from_utf8(&writer[pos..pos + len])
                .unwrap()
                .to_string(),
        );
        pos += len;
    }
    assert_eq!(payloads.len(), 2);
    assert!(payloads[0].contains("\"id\":1") && payloads[0].contains("\"tabs\""));
    assert_eq!(payloads[1], "{\"id\":2,\"ok\":true,\"result\":{}}");
}

#[test]
fn run_loop_stops_on_truncated_body() {
    let body = req(1, "tabs.list", "{}");
    let mut input = Vec::new();
    input.extend_from_slice(&(body.len() as u32).to_le_bytes());
    input.extend_from_slice(&body[..body.len() / 2]); // truncated
    let mut reader = Cursor::new(input);
    let mut writer: Vec<u8> = Vec::new();
    run_loop(&mut reader, &mut writer, &Fake::new()).unwrap();
    assert!(writer.is_empty(), "truncated message must yield no reply");
}
