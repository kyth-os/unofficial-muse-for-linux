//! Offline integration tests for muse-calendar.
//!
//! A scripted fake CalDAV server speaks real HTTP over a 127.0.0.1 TCP
//! socket. No internet, no real credentials. The client under test is wired
//! to [`muse_calendar::LoopbackTransport`], which does real socket I/O
//! against the fake; the production TLS transport is never used here.

use muse_calendar::{CalError, CalendarAccount, CalendarClient, CalendarDraft, LoopbackTransport};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Fake server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct LoggedRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct Scripted {
    method: &'static str,
    path: &'static str,
    status: u16,
    body: Vec<u8>,
    /// If true, close the connection without any response (PUT-uncertain path).
    hangup: bool,
    /// Assert this header is present with this value; else reply 400.
    require_header: Option<(&'static str, &'static str)>,
}

impl Scripted {
    fn text(method: &'static str, path: &'static str, status: u16, body: &str) -> Self {
        Self {
            method,
            path,
            status,
            body: body.as_bytes().to_vec(),
            hangup: false,
            require_header: None,
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        412 => "Precondition Failed",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn read_request(stream: &mut TcpStream) -> Option<LoggedRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut head = String::new();
    reader.read_line(&mut head).ok()?;
    if head.trim().is_empty() {
        return None;
    }
    let mut parts = head.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    let mut content_len = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end().to_string();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_lowercase();
            let v = v.trim().to_string();
            if k == "content-length" {
                content_len = v.parse().unwrap_or(0);
            }
            headers.insert(k, v);
        }
    }
    let mut body = vec![0u8; content_len.min(8 * 1024 * 1024)];
    if !body.is_empty() {
        reader.read_exact(&mut body).ok()?;
    }
    Some(LoggedRequest {
        method,
        path,
        headers,
        body,
    })
}

struct Fake {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<LoggedRequest>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Fake {
    fn spawn(scripts: Vec<Scripted>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        listener.set_nonblocking(false).expect("blocking listener");
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let log_c = Arc::clone(&log);
        let down_c = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            while !down_c.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                };
                let req = match read_request(&mut stream) {
                    Some(r) => r,
                    None => continue,
                };
                log_c.lock().unwrap().push(req.clone());
                let hit = scripts.iter().find(|s| {
                    s.method == req.method
                        && (s.path == req.path
                            || (s.path.ends_with('*')
                                && req.path.starts_with(&s.path[..s.path.len() - 1])))
                });
                match hit {
                    None => {
                        let r = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                        let _ = stream.write_all(r.as_bytes());
                    }
                    Some(s) => {
                        if s.hangup {
                            drop(stream);
                            continue;
                        }
                        if let Some((hk, hv)) = s.require_header {
                            let got = req
                                .headers
                                .get(&hk.to_lowercase())
                                .map(|v| v.as_str())
                                .unwrap_or("");
                            if got != hv {
                                let r = "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                                let _ = stream.write_all(r.as_bytes());
                                continue;
                            }
                        }
                        let head = format!(
                            "HTTP/1.1 {} {}\r\ncontent-type: application/xml\r\ncontent-length: {}\r\netag: \"fake-1\"\r\nconnection: close\r\n\r\n",
                            s.status,
                            reason(s.status),
                            s.body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&s.body);
                    }
                }
            }
        });
        Self {
            addr,
            log,
            shutdown,
            handle: Some(handle),
        }
    }

    fn logged(&self) -> Vec<LoggedRequest> {
        self.log.lock().unwrap().clone()
    }

    fn puts(&self) -> Vec<LoggedRequest> {
        self.logged()
            .into_iter()
            .filter(|r| r.method == "PUT")
            .collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock a pending accept().
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn account(fake: &Fake) -> CalendarAccount {
    CalendarAccount {
        host: "127.0.0.1".to_string(),
        port: fake.addr.port(),
        username: "user".to_string(),
        base_path: "/dav/".to_string(),
    }
}

fn client() -> CalendarClient {
    CalendarClient::with_transport(Arc::new(LoopbackTransport::new()))
}

const PRINCIPAL_XML: &str = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/</D:href>
<D:propstat><D:prop><D:current-user-principal><D:href>/dav/principal/</D:href>
</D:current-user-principal></D:prop></D:propstat></D:response></D:multistatus>"#;

const HOMESET_XML: &str = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
<D:response><D:href>/dav/principal/</D:href><D:propstat><D:prop>
<C:calendar-home-set><D:href>/dav/calendars/user/</D:href></C:calendar-home-set>
</D:prop></D:propstat></D:response></D:multistatus>"#;

const LIST_XML: &str = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
<D:response><D:href>/dav/calendars/user/</D:href>
<D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop>
</D:propstat></D:response>
<D:response><D:href>/dav/calendars/user/work/</D:href>
<D:propstat><D:prop><D:displayname>Work</D:displayname>
<D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop>
</D:propstat></D:response>
<D:response><D:href>/dav/calendars/user/home/</D:href>
<D:propstat><D:prop><D:displayname>Home</D:displayname>
<D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop>
</D:propstat></D:response>
</D:multistatus>"#;

fn discovery_scripts() -> Vec<Scripted> {
    vec![
        Scripted::text("PROPFIND", "/dav/", 207, PRINCIPAL_XML),
        Scripted::text("PROPFIND", "/dav/principal/", 207, HOMESET_XML),
        Scripted::text("PROPFIND", "/dav/calendars/user/", 207, LIST_XML),
    ]
}

fn event_ics(uid: &str, summary: &str, dtstart: &str, dtend: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\nDTSTART:{dtstart}\r\nDTEND:{dtend}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn report_xml(items: &[(&str, String)]) -> String {
    let mut s = String::from(
        r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">"#,
    );
    for (href, ics) in items {
        // Minimal XML escaping for the embedded iCalendar payload.
        let esc = ics
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        s.push_str(&format!(
            "<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
             <C:calendar-data>{esc}</C:calendar-data></D:prop></D:propstat></D:response>"
        ));
    }
    s.push_str("</D:multistatus>");
    s
}

fn draft(cal: &str) -> CalendarDraft {
    CalendarDraft {
        calendar_path: cal.to_string(),
        title: "Planning".to_string(),
        // 2026-01-15T10:00:00Z .. 11:00:00Z
        start_utc: 1768471200,
        end_utc: 1768474800,
        description: "quarterly planning".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[test]
fn discovery_returns_calendars() {
    let fake = Fake::spawn(discovery_scripts());
    let out = client().calendars(&account(&fake), "s3cret").unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].path, "/dav/calendars/user/work/");
    assert_eq!(out[0].name, "Work");
    assert_eq!(out[1].path, "/dav/calendars/user/home/");
    assert_eq!(out[1].name, "Home");
}

#[test]
fn discovery_auth_failure_carries_no_credentials() {
    let fake = Fake::spawn(vec![Scripted::text(
        "PROPFIND",
        "/dav/",
        401,
        "unauthorized",
    )]);
    let err = client()
        .calendars(&account(&fake), "s3cret-pw")
        .unwrap_err();
    assert!(matches!(err, CalError::Auth), "got {err:?}");
    let dbg = format!("{err:?}");
    let disp = format!("{err}");
    assert!(!dbg.contains("s3cret-pw"), "password leaked in {dbg}");
    assert!(!disp.contains("s3cret-pw"), "password leaked in {disp}");
}

#[test]
fn discovery_missing_homeset_fails_closed() {
    let no_home = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/principal/</D:href>
<D:propstat><D:prop></D:prop></D:propstat></D:response></D:multistatus>"#;
    let fake = Fake::spawn(vec![
        Scripted::text("PROPFIND", "/dav/", 207, PRINCIPAL_XML),
        Scripted::text("PROPFIND", "/dav/principal/", 207, no_home),
    ]);
    let err = client().calendars(&account(&fake), "pw").unwrap_err();
    assert!(matches!(err, CalError::Discovery(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Events: bounds, filtering, malformed input
// ---------------------------------------------------------------------------

#[test]
fn events_rejects_range_over_31_days_without_network() {
    let fake = Fake::spawn(vec![]);
    let c = client();
    let err = c
        .events(
            &account(&fake),
            "pw",
            "/dav/calendars/user/work/",
            1767225600,
            1767225600 + 32 * 86400,
        )
        .unwrap_err();
    assert!(matches!(err, CalError::InvalidRange), "got {err:?}");
    assert!(fake.logged().is_empty(), "no request must be sent");
}

#[test]
fn events_rejects_inverted_range_without_network() {
    let fake = Fake::spawn(vec![]);
    let err = client()
        .events(
            &account(&fake),
            "pw",
            "/dav/calendars/user/work/",
            1768474800,
            1768471200,
        )
        .unwrap_err();
    assert!(matches!(err, CalError::InvalidRange), "got {err:?}");
    assert!(fake.logged().is_empty());
}

#[test]
fn events_parses_vevent_and_ignores_vtodo_vjournal() {
    let mixed = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
        BEGIN:VEVENT\r\nUID:e1\r\nSUMMARY:Standup\r\nDTSTART:20260115T100000Z\r\nDTEND:20260115T101500Z\r\nEND:VEVENT\r\n\
        BEGIN:VTODO\r\nUID:t1\r\nSUMMARY:Buy milk\r\nDUE:20260115T120000Z\r\nEND:VTODO\r\n\
        BEGIN:VJOURNAL\r\nUID:j1\r\nSUMMARY:Notes\r\nEND:VJOURNAL\r\nEND:VCALENDAR\r\n";
    let body = report_xml(&[("/dav/calendars/user/work/e1.ics", mixed.to_string())]);
    let fake = Fake::spawn(vec![Scripted::text(
        "REPORT",
        "/dav/calendars/user/work/",
        207,
        &body,
    )]);
    let out = client()
        .events(
            &account(&fake),
            "pw",
            "/dav/calendars/user/work/",
            1767225600,
            1767225600 + 86400 * 30,
        )
        .unwrap();
    assert_eq!(out.len(), 1, "only the VEVENT survives");
    assert_eq!(out[0].uid, "e1");
    assert_eq!(out[0].title, "Standup");
    assert_eq!(out[0].start_utc, 1768471200);
    assert_eq!(out[0].end_utc, 1768472100);
    assert!(!out[0].all_day);
}

#[test]
fn events_caps_results_at_200() {
    let mut items: Vec<(String, String)> = Vec::new();
    for i in 0..250u32 {
        items.push((
            format!("/dav/calendars/user/work/e{i}.ics"),
            event_ics(
                &format!("u{i}"),
                "Bulk",
                "20260115T100000Z",
                "20260115T101500Z",
            ),
        ));
    }
    // report_xml borrows; build owned first.
    let owned: Vec<(String, String)> = items;
    let refs: Vec<(&str, String)> = owned.iter().map(|(h, b)| (h.as_str(), b.clone())).collect();
    let body = report_xml(&refs);
    let fake = Fake::spawn(vec![Scripted::text(
        "REPORT",
        "/dav/calendars/user/work/",
        207,
        &body,
    )]);
    let out = client()
        .events(
            &account(&fake),
            "pw",
            "/dav/calendars/user/work/",
            1767225600,
            1767225600 + 86400,
        )
        .unwrap();
    assert_eq!(out.len(), 200);
}

#[test]
fn events_malformed_ical_fails_closed() {
    let bad = "BEGIN:VCALENDAR\r\nTHIS IS NOT ICAL AT ALL\r\nBEGIN:VEVENT\r\nUID:\r\nSUMMARY broken\x01control\r\n";
    let body = report_xml(&[("/dav/calendars/user/work/bad.ics", bad.to_string())]);
    let fake = Fake::spawn(vec![Scripted::text(
        "REPORT",
        "/dav/calendars/user/work/",
        207,
        &body,
    )]);
    let err = client()
        .events(
            &account(&fake),
            "pw",
            "/dav/calendars/user/work/",
            1767225600,
            1767225600 + 86400,
        )
        .unwrap_err();
    assert!(matches!(err, CalError::Malformed(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// event_detail: size bound, plain text
// ---------------------------------------------------------------------------

#[test]
fn event_detail_returns_plain_fields() {
    let ics = event_ics("d9", "Dentist", "20260115T100000Z", "20260115T110000Z");
    let fake = Fake::spawn(vec![Scripted {
        method: "GET",
        path: "/dav/calendars/user/work/d9.ics",
        status: 200,
        body: ics.into_bytes(),
        hangup: false,
        require_header: None,
    }]);
    let d = client()
        .event_detail(
            &account(&fake),
            "pw",
            "/dav/calendars/user/work/",
            "/dav/calendars/user/work/d9.ics",
        )
        .unwrap();
    assert_eq!(d.uid, "d9");
    assert_eq!(d.title, "Dentist");
    assert_eq!(d.start_utc, 1768471200);
}

#[test]
fn event_detail_rejects_oversize_body() {
    let big = vec![b'x'; 70 * 1024];
    let fake = Fake::spawn(vec![Scripted {
        method: "GET",
        path: "/dav/calendars/user/work/big.ics",
        status: 200,
        body: big,
        hangup: false,
        require_header: None,
    }]);
    let err = client()
        .event_detail(
            &account(&fake),
            "pw",
            "/dav/calendars/user/work/",
            "/dav/calendars/user/work/big.ics",
        )
        .unwrap_err();
    assert!(matches!(err, CalError::TooLarge), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Draft validation
// ---------------------------------------------------------------------------

#[test]
fn draft_validation_rejects_bad_inputs() {
    let c = client();
    // end <= start
    let mut d = draft("/dav/calendars/user/work/");
    d.end_utc = d.start_utc;
    assert!(c.queue_create(d).is_err());
    // duration > 24h
    let mut d = draft("/dav/calendars/user/work/");
    d.end_utc = d.start_utc + 86401;
    assert!(c.queue_create(d).is_err());
    // oversize title
    let mut d = draft("/dav/calendars/user/work/");
    d.title = "t".repeat(4097);
    assert!(c.queue_create(d).is_err());
    // empty calendar path
    let mut d = draft("/dav/calendars/user/work/");
    d.calendar_path = String::new();
    assert!(c.queue_create(d).is_err());
}

// ---------------------------------------------------------------------------
// Approval gate: PUT only on approve, If-None-Match, replay, deny, conflict
// ---------------------------------------------------------------------------

fn put_server(status: u16) -> Vec<Scripted> {
    vec![Scripted {
        method: "PUT",
        path: "/dav/calendars/user/work/*",
        status,
        body: b"".to_vec(),
        hangup: false,
        require_header: Some(("if-none-match", "*")),
    }]
}

#[test]
fn approve_puts_once_with_if_none_match() {
    let fake = Fake::spawn(put_server(201));
    let c = client();
    let pending = c.queue_create(draft("/dav/calendars/user/work/")).unwrap();
    assert!(fake.logged().is_empty(), "queueing must not touch network");
    let receipt = c.approve_create(&account(&fake), "pw", &pending).unwrap();
    let puts = fake.puts();
    assert_eq!(puts.len(), 1);
    assert_eq!(
        puts[0].headers.get("if-none-match").map(|s| s.as_str()),
        Some("*")
    );
    assert!(puts[0].path.starts_with("/dav/calendars/user/work/"));
    assert!(puts[0].path.ends_with(".ics"));
    let sent = String::from_utf8_lossy(&puts[0].body);
    assert!(sent.contains("BEGIN:VEVENT"), "PUT carries iCalendar");
    assert!(
        sent.contains("SUMMARY:Planning"),
        "PUT carries staged title"
    );
    assert!(
        sent.contains("DTSTART:20260115T100000Z"),
        "PUT carries staged start"
    );
    assert!(receipt.href.starts_with("/dav/calendars/user/work/"));
}

#[test]
fn double_approve_replay_fails_without_second_put() {
    let fake = Fake::spawn(put_server(201));
    let c = client();
    let pending = c.queue_create(draft("/dav/calendars/user/work/")).unwrap();
    c.approve_create(&account(&fake), "pw", &pending).unwrap();
    let err = c
        .approve_create(&account(&fake), "pw", &pending)
        .unwrap_err();
    assert!(
        matches!(err, CalError::UnknownCreate),
        "replay must fail, got {err:?}"
    );
    assert_eq!(fake.puts().len(), 1, "no second PUT on replay");
}

#[test]
fn deny_sends_nothing_and_consumes_the_draft() {
    let fake = Fake::spawn(put_server(201));
    let c = client();
    let pending = c.queue_create(draft("/dav/calendars/user/work/")).unwrap();
    assert!(c.deny_create(&pending));
    assert!(fake.logged().is_empty(), "deny must not touch network");
    let err = c
        .approve_create(&account(&fake), "pw", &pending)
        .unwrap_err();
    assert!(matches!(err, CalError::UnknownCreate), "got {err:?}");
    assert!(fake.logged().is_empty());
}

#[test]
fn conflict_412_fails_closed_with_single_put() {
    let fake = Fake::spawn(put_server(412));
    let c = client();
    let pending = c.queue_create(draft("/dav/calendars/user/work/")).unwrap();
    let err = c
        .approve_create(&account(&fake), "pw", &pending)
        .unwrap_err();
    assert!(matches!(err, CalError::Conflict), "got {err:?}");
    assert_eq!(fake.puts().len(), 1, "never retry, never overwrite");
    // Consumed: no second attempt possible.
    assert!(matches!(
        c.approve_create(&account(&fake), "pw", &pending)
            .unwrap_err(),
        CalError::UnknownCreate
    ));
    assert_eq!(fake.puts().len(), 1);
}

#[test]
fn uncertain_put_outcome_is_surfaced_not_retried() {
    let fake = Fake::spawn(vec![Scripted {
        method: "PUT",
        path: "/dav/calendars/user/work/*",
        status: 201,
        body: vec![],
        hangup: true, // server reads the PUT then hangs up with no response
        require_header: None,
    }]);
    let c = client();
    let pending = c.queue_create(draft("/dav/calendars/user/work/")).unwrap();
    let err = c
        .approve_create(&account(&fake), "pw", &pending)
        .unwrap_err();
    assert!(matches!(err, CalError::Uncertain), "got {err:?}");
    assert_eq!(fake.puts().len(), 1, "uncertain outcomes are never retried");
}

// ---------------------------------------------------------------------------
// Review regressions: href confinement, field validation, epoch extremes
// ---------------------------------------------------------------------------

#[test]
fn event_detail_rejects_sibling_prefix_and_empty_scope_without_network() {
    // Regression: raw starts_with let /a match /ab/x and skipped the check
    // for an empty calendar path.
    let fake = Fake::spawn(vec![]);
    let c = client();
    for (cal, href) in [
        ("/dav/calendars/user/a", "/dav/calendars/user/ab/x.ics"),
        (
            "/dav/calendars/user/a",
            "/dav/calendars/user/attacker/evil.ics",
        ),
        ("", "/dav/calendars/user/work/d9.ics"),
        (
            "/dav/calendars/user/work/",
            "/dav/calendars/user/work/../x.ics",
        ),
    ] {
        let err = c
            .event_detail(&account(&fake), "pw", cal, href)
            .unwrap_err();
        assert!(
            matches!(err, CalError::InvalidArgument(_)),
            "cal={cal:?} href={href:?} got {err:?}"
        );
    }
    assert!(fake.logged().is_empty(), "no request must be sent");
}

#[test]
fn control_characters_and_extreme_epochs_rejected_without_network() {
    // Regression: control/CRLF characters flowed into the raw request head
    // and plain i64 subtraction panicked on extreme epochs.
    let fake = Fake::spawn(vec![]);
    let c = client();
    let mut evil = account(&fake);
    evil.username = "u\r\nX-injected: yes".to_string();
    let err = c
        .events(&evil, "pw", "/dav/calendars/user/work/", 1, 2)
        .unwrap_err();
    assert!(matches!(err, CalError::InvalidArgument(_)), "got {err:?}");
    let mut evil = account(&fake);
    evil.host = "127.0.0.1/evil".to_string();
    assert!(matches!(
        c.events(&evil, "pw", "/dav/calendars/user/work/", 1, 2)
            .unwrap_err(),
        CalError::InvalidArgument(_)
    ));
    assert!(matches!(
        c.events(
            &account(&fake),
            "pw",
            "/dav/calendars/user/work/",
            i64::MIN,
            i64::MAX
        )
        .unwrap_err(),
        CalError::InvalidRange
    ));
    let mut d = draft("/dav/calendars/user/work/");
    d.calendar_path = "/dav/\r\nX: 1".to_string();
    assert!(c.queue_create(d).is_err());
    assert!(fake.logged().is_empty(), "no request must be sent");
}
