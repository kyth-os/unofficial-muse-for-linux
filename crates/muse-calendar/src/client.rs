//! [`CalendarClient`]: CalDAV read + approval-gated create.

use crate::ical::{self, ParseError};
use crate::transport::{HttpResponse, HttpTransport, ReqwestTransport, TransportError};
use crate::xml;
use std::collections::HashMap;
use std::fmt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Bounds (frozen by contract)
// ---------------------------------------------------------------------------

/// Maximum time range per [`CalendarClient::events`] call: 31 days.
pub const MAX_RANGE_SECS: i64 = 31 * 86400;
/// Maximum events returned per [`CalendarClient::events`] call.
pub const MAX_RESULTS: usize = 200;
/// Maximum bytes accepted for [`CalendarClient::event_detail`].
pub const MAX_EVENT_BYTES: usize = 64 * 1024;
/// Maximum bytes per discovery PROPFIND response.
const MAX_DISCOVERY_BYTES: usize = 256 * 1024;
/// Maximum bytes per REPORT response.
const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;
/// Maximum bytes per draft text field.
const MAX_FIELD_BYTES: usize = 4 * 1024;
/// Maximum single event duration accepted at draft time: 24 h.
const MAX_DURATION_SECS: i64 = 24 * 3600;

// ---------------------------------------------------------------------------
// Public types (frozen names)
// ---------------------------------------------------------------------------

/// Connection parameters. The password is **never** part of this struct: it
/// is passed per call as `&str` and never stored.
pub struct CalendarAccount {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub base_path: String,
}

// Hand-written so a future secret field cannot leak via a derived Debug.
impl fmt::Debug for CalendarAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CalendarAccount")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("base_path", &self.base_path)
            .finish()
    }
}

impl Clone for CalendarAccount {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            base_path: self.base_path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarInfo {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSummary {
    pub id: String,
    pub uid: String,
    pub title: String,
    pub start_utc: i64,
    pub end_utc: i64,
    pub all_day: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventDetail {
    pub href: String,
    pub uid: String,
    pub title: String,
    pub start_utc: i64,
    pub end_utc: i64,
    pub all_day: bool,
    /// Plain-text DESCRIPTION only; iCalendar TEXT has no markup.
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarDraft {
    pub calendar_path: String,
    pub title: String,
    pub start_utc: i64,
    pub end_utc: i64,
    pub description: String,
}

/// Handle to a locally staged (not yet sent) event creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCreate {
    pub id: u64,
    /// Integrity tag binding this handle to the staged bytes.
    tag: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateReceipt {
    pub href: String,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalError {
    Auth,
    NotFound,
    Conflict,
    /// The PUT was issued but the outcome could not be confirmed.
    /// Never auto-retried; the server-side state is unknown.
    Uncertain,
    TamperedDraft,
    UnknownCreate,
    InvalidRange,
    InvalidDraft(String),
    InvalidArgument(String),
    Malformed(String),
    TooLarge,
    UnsupportedTimezone(String),
    Discovery(String),
    TlsRequired,
    Transport(String),
    Http(u16),
}

impl fmt::Display for CalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // No credentials, hosts, or passwords are ever rendered here.
            Self::Auth => write!(f, "authentication failed"),
            Self::NotFound => write!(f, "calendar object not found"),
            Self::Conflict => write!(
                f,
                "event already exists (precondition failed); not overwritten"
            ),
            Self::Uncertain => write!(
                f,
                "create outcome uncertain: request sent but unconfirmed; not retried"
            ),
            Self::TamperedDraft => {
                write!(f, "pending creation failed integrity check; fails closed")
            }
            Self::UnknownCreate => write!(f, "unknown or already-consumed creation id"),
            Self::InvalidRange => write!(
                f,
                "invalid time range: must be ordered and span at most 31 days"
            ),
            Self::InvalidDraft(m) => write!(f, "invalid draft: {m}"),
            Self::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            Self::Malformed(m) => write!(f, "malformed server data: {m}"),
            Self::TooLarge => write!(f, "server response exceeds the size bound"),
            Self::UnsupportedTimezone(z) => {
                write!(f, "unsupported timezone '{z}': refusing to guess")
            }
            Self::Discovery(m) => write!(f, "calendar discovery failed: {m}"),
            Self::TlsRequired => write!(f, "refusing non-TLS connection"),
            Self::Transport(m) => write!(f, "transport failure: {m}"),
            Self::Http(s) => write!(f, "unexpected HTTP status {s}"),
        }
    }
}

impl std::error::Error for CalError {}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StoredPending {
    tag: u64,
    hash: u64,
    draft: CalendarDraft,
}

/// CalDAV connector. Sync; safe to share across threads.
pub struct CalendarClient {
    transport: Arc<dyn HttpTransport>,
    /// URL scheme. Always `https` for production (`new`); the test-only
    /// [`CalendarClient::with_transport`] constructor uses `http` so a
    /// plaintext loopback fake can be exercised over real sockets.
    scheme: &'static str,
    pending: Mutex<HashMap<u64, StoredPending>>,
    next_id: AtomicU64,
}

impl CalendarClient {
    /// Production client: TLS-only transport, system trust store.
    pub fn new() -> Self {
        Self {
            transport: Arc::new(ReqwestTransport::default()),
            scheme: "https",
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Test hook: inject a transport (e.g. [`crate::LoopbackTransport`]).
    /// Uses plaintext `http` URLs and must only target 127.0.0.1/::1.
    #[doc(hidden)]
    pub fn with_transport(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            scheme: "http",
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn url(&self, account: &CalendarAccount, path: &str) -> String {
        format!(
            "{}://{}:{}{}",
            self.scheme, account.host, account.port, path
        )
    }

    fn auth_value(username: &str, password: &str) -> String {
        format!("Basic {}", base64(&format!("{username}:{password}")))
    }

    fn send(
        &self,
        method: &str,
        url: &str,
        account: &CalendarAccount,
        password: &str,
        extra: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, CalError> {
        let auth = Self::auth_value(&account.username, password);
        let mut headers: Vec<(&str, &str)> = Vec::with_capacity(extra.len() + 1);
        headers.push(("authorization", &auth));
        for (k, v) in extra {
            headers.push((k, v));
        }
        self.transport
            .request(method, url, &headers, body)
            .map_err(|e| match e {
                TransportError::Uncertain(m) => CalError::Transport(format!("uncertain: {m}")),
                TransportError::Failed(m) => CalError::Transport(m),
            })
    }

    // -- discovery --------------------------------------------------------

    /// Discover calendars via principal + calendar-home-set, bounded.
    pub fn calendars(
        &self,
        account: &CalendarAccount,
        password: &str,
    ) -> Result<Vec<CalendarInfo>, CalError> {
        validate_account(account)?;
        let base = join_base(&account.base_path)?;
        let principal_body = br#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:current-user-principal/></D:prop></D:propfind>"#;
        let r = self.send(
            "PROPFIND",
            &self.url(account, &base),
            account,
            password,
            &[("depth", "0"), ("content-type", "application/xml")],
            Some(principal_body),
        )?;
        let principal = self.check_discovery(r, "current-user-principal")?;

        let home_body = br#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><C:calendar-home-set/></D:prop></D:propfind>"#;
        let r = self.send(
            "PROPFIND",
            &self.url(account, &principal),
            account,
            password,
            &[("depth", "0"), ("content-type", "application/xml")],
            Some(home_body),
        )?;
        let home = self.check_discovery(r, "calendar-home-set")?;

        let list_body = br#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:displayname/><D:resourcetype/></D:prop></D:propfind>"#;
        let r = self.send(
            "PROPFIND",
            &self.url(account, &home),
            account,
            password,
            &[("depth", "1"), ("content-type", "application/xml")],
            Some(list_body),
        )?;
        let body = self.check_status(r, MAX_DISCOVERY_BYTES)?;
        let entries = xml::responses(&body).map_err(|e| CalError::Discovery(e.to_string()))?;
        Ok(entries
            .into_iter()
            .filter(|e| e.is_calendar && e.href != home)
            .map(|e| CalendarInfo {
                name: if e.displayname.is_empty() {
                    e.href.clone()
                } else {
                    e.displayname
                },
                path: e.href,
            })
            .collect())
    }

    fn check_discovery(&self, r: HttpResponse, outer: &str) -> Result<String, CalError> {
        let body = self.check_status(r, MAX_DISCOVERY_BYTES)?;
        xml::inner_href(&body, outer)
            .map_err(|e| CalError::Discovery(format!("{outer}: {e}")))
            .and_then(|href| {
                if href.starts_with('/') {
                    Ok(href)
                } else {
                    Err(CalError::Discovery(format!(
                        "{outer} href is not an absolute path"
                    )))
                }
            })
    }

    fn check_status(&self, r: HttpResponse, cap: usize) -> Result<String, CalError> {
        match r.status {
            200 | 201 | 204 | 207 => {}
            401 | 403 => return Err(CalError::Auth),
            404 => return Err(CalError::NotFound),
            412 => return Err(CalError::Conflict),
            s => return Err(CalError::Http(s)),
        }
        if r.body.len() > cap {
            return Err(CalError::TooLarge);
        }
        String::from_utf8(r.body).map_err(|_| CalError::Malformed("response is not UTF-8".into()))
    }

    // -- events -----------------------------------------------------------

    /// List VEVENTs in `[from_utc, to_utc]` (each an epoch). Range must be
    /// ordered and span at most 31 days; at most 200 events are returned.
    pub fn events(
        &self,
        account: &CalendarAccount,
        password: &str,
        calendar_path: &str,
        from_utc: i64,
        to_utc: i64,
    ) -> Result<Vec<EventSummary>, CalError> {
        validate_account(account)?;
        if !clean_path(calendar_path) {
            return Err(CalError::InvalidArgument(
                "calendar path must be absolute".into(),
            ));
        }
        if to_utc <= from_utc || to_utc.saturating_sub(from_utc) > MAX_RANGE_SECS {
            return Err(CalError::InvalidRange);
        }
        let report = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
<D:prop><D:getetag/><C:calendar-data/></D:prop>
<C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
<C:time-range start="{}" end="{}"/></C:comp-filter></C:comp-filter></C:filter>
</C:calendar-query>"#,
            ical::format_utc(from_utc),
            ical::format_utc(to_utc)
        );
        let r = self.send(
            "REPORT",
            &self.url(account, calendar_path),
            account,
            password,
            &[("depth", "1"), ("content-type", "application/xml")],
            Some(report.as_bytes()),
        )?;
        let body = self.check_status(r, MAX_REPORT_BYTES)?;
        let entries = xml::responses(&body).map_err(|e| CalError::Malformed(e.to_string()))?;
        let mut out = Vec::new();
        for e in entries {
            let Some(data) = e.calendar_data else {
                continue;
            };
            let vevents = ical::parse_vevents(&data).map_err(ical_err)?;
            for v in vevents {
                if out.len() >= MAX_RESULTS {
                    break;
                }
                out.push(EventSummary {
                    id: e.href.clone(),
                    uid: v.uid,
                    title: v.summary,
                    start_utc: v.start_utc,
                    end_utc: v.end_utc,
                    all_day: v.all_day,
                });
            }
            if out.len() >= MAX_RESULTS {
                break;
            }
        }
        Ok(out)
    }

    // -- event_detail -----------------------------------------------------

    /// Fetch one event by href. Bounded to 64 KiB; plain-text fields only.
    pub fn event_detail(
        &self,
        account: &CalendarAccount,
        password: &str,
        calendar_path: &str,
        event_href: &str,
    ) -> Result<EventDetail, CalError> {
        validate_account(account)?;
        if !href_in_calendar(calendar_path, event_href) {
            return Err(CalError::InvalidArgument(
                "event href is outside the given calendar".into(),
            ));
        }
        let r = self.send(
            "GET",
            &self.url(account, event_href),
            account,
            password,
            &[],
            None,
        )?;
        let body = self.check_status(r, MAX_EVENT_BYTES)?;
        let mut vevents = ical::parse_vevents(&body).map_err(ical_err)?;
        if vevents.is_empty() {
            return Err(CalError::Malformed("no VEVENT in response".into()));
        }
        let v = vevents.remove(0);
        Ok(EventDetail {
            href: event_href.to_string(),
            uid: v.uid,
            title: v.summary,
            start_utc: v.start_utc,
            end_utc: v.end_utc,
            all_day: v.all_day,
            description: v.description,
        })
    }

    // -- approval-gated create --------------------------------------------

    /// Validate a draft and stage it locally. Sends nothing.
    pub fn queue_create(&self, draft: CalendarDraft) -> Result<PendingCreate, CalError> {
        validate_draft(&draft)?;
        let hash = draft_hash(&draft);
        let tag = mix(hash, self.next_id.fetch_add(1, Ordering::SeqCst));
        let id = mix(tag, 0x9E37_79B9_7F4A_7C15);
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| CalError::Transport("creation staging lock poisoned".into()))?;
        pending.insert(id, StoredPending { tag, hash, draft });
        Ok(PendingCreate { id, tag })
    }

    /// Approve a staged creation: exactly one PUT with `If-None-Match: *`.
    /// The staged bytes are re-hashed and compared first; any mismatch fails
    /// closed with [`CalError::TamperedDraft`] and the staged entry is kept,
    /// so a forged guess cannot burn the legitimate draft. The entry is
    /// consumed only after verification, so replays and retries are
    /// impossible.
    pub fn approve_create(
        &self,
        account: &CalendarAccount,
        password: &str,
        create: &PendingCreate,
    ) -> Result<CreateReceipt, CalError> {
        validate_account(account)?;
        // Verify first, consume after: a failed integrity check must not
        // destroy the legitimate staged entry.
        let stored = {
            let pending = self
                .pending
                .lock()
                .map_err(|_| CalError::Transport("creation staging lock poisoned".into()))?;
            let stored = pending.get(&create.id).ok_or(CalError::UnknownCreate)?;
            if stored.tag != create.tag || stored.hash != draft_hash(&stored.draft) {
                return Err(CalError::TamperedDraft);
            }
            stored.clone()
        };
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| CalError::Transport("creation staging lock poisoned".into()))?;
            pending.remove(&create.id);
        }
        let uid = fresh_uid();
        let href = format!(
            "{}/{}.ics",
            stored.draft.calendar_path.trim_end_matches('/'),
            uid
        );
        let ics = render_ics(&uid, &stored.draft);
        let url = self.url(account, &href);
        let auth = Self::auth_value(&account.username, password);
        let headers: [(&str, &str); 3] = [
            ("authorization", &auth),
            ("if-none-match", "*"),
            ("content-type", "text/calendar; charset=utf-8"),
        ];
        // Any PUT transport failure is Uncertain: the bytes may be on the
        // server even if the response never arrived. Never retry.
        let r = self
            .transport
            .request("PUT", &url, &headers, Some(ics.as_bytes()))
            .map_err(|_| CalError::Uncertain)?;
        match r.status {
            200 | 201 | 204 => Ok(CreateReceipt {
                href,
                etag: r.header("etag").map(|s| s.to_string()),
            }),
            401 | 403 => Err(CalError::Auth),
            404 => Err(CalError::NotFound),
            412 => Err(CalError::Conflict),
            s => Err(CalError::Http(s)),
        }
    }

    /// Drop a staged creation without touching the network.
    pub fn deny_create(&self, create: &PendingCreate) -> bool {
        self.pending
            .lock()
            .map(|mut p| p.remove(&create.id).is_some())
            .unwrap_or(false)
    }
}

impl Default for CalendarClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn join_base(base: &str) -> Result<String, CalError> {
    if !clean_path(base) {
        return Err(CalError::InvalidArgument(
            "account base path must be absolute".into(),
        ));
    }
    Ok(base.to_string())
}

/// Control characters in any account/draft field would flow into the raw
/// HTTP request head or deform the URL, so they fail closed here.
fn has_controls(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// Absolute server path without traversal, query/fragment markers, or
/// control characters. Used for base paths, calendar paths, draft paths,
/// and event hrefs alike.
fn clean_path(p: &str) -> bool {
    p.starts_with('/')
        && !has_controls(p)
        && !p.contains(['?', '#'])
        && p.split('/').all(|seg| seg != "..")
}

/// Host without userinfo markers, whitespace, controls, or path characters.
/// `:` stays legal so IPv6 literals keep working; the port is separate.
fn clean_host(h: &str) -> bool {
    !h.is_empty()
        && !h
            .bytes()
            .any(|b| b <= 0x20 || b == 0x7f || b == b'@' || b == b'?' || b == b'#' || b == b'/')
}

fn clean_user(u: &str) -> bool {
    !u.is_empty() && !has_controls(u)
}

fn validate_account(a: &CalendarAccount) -> Result<(), CalError> {
    if !clean_host(&a.host) || !clean_user(&a.username) || !clean_path(&a.base_path) {
        return Err(CalError::InvalidArgument(
            "account host, username, or base path is malformed".into(),
        ));
    }
    Ok(())
}

/// Is `href` the calendar itself or an object directly beneath it?
/// A raw `starts_with` is not enough: `/a` must not match `/ab/x`.
fn href_in_calendar(calendar_path: &str, href: &str) -> bool {
    if !clean_path(calendar_path) || !clean_path(href) {
        return false;
    }
    let cal = calendar_path.trim_end_matches('/');
    if cal.is_empty() {
        return false;
    }
    href == cal
        || href
            .strip_prefix(cal)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn ical_err(e: ParseError) -> CalError {
    match e {
        ParseError::Malformed(m) => CalError::Malformed(m),
        ParseError::UnsupportedTimezone(z) => CalError::UnsupportedTimezone(z),
    }
}

fn validate_draft(d: &CalendarDraft) -> Result<(), CalError> {
    if !clean_path(&d.calendar_path) {
        return Err(CalError::InvalidDraft(
            "calendar path must be absolute".into(),
        ));
    }
    if d.title.trim().is_empty() {
        return Err(CalError::InvalidDraft("title must not be empty".into()));
    }
    for (name, v) in [
        ("calendar path", &d.calendar_path),
        ("title", &d.title),
        ("description", &d.description),
    ] {
        if v.len() > MAX_FIELD_BYTES {
            return Err(CalError::InvalidDraft(format!(
                "{name} exceeds {} bytes",
                MAX_FIELD_BYTES
            )));
        }
    }
    if d.end_utc <= d.start_utc {
        return Err(CalError::InvalidDraft(
            "event must end after it starts".into(),
        ));
    }
    if d.end_utc.saturating_sub(d.start_utc) > MAX_DURATION_SECS {
        return Err(CalError::InvalidDraft(
            "event duration exceeds 24 hours".into(),
        ));
    }
    Ok(())
}

/// FNV-1a/64 over the exact bound fields.
fn draft_hash(d: &CalendarDraft) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_4842_2225;
    for chunk in [
        d.calendar_path.as_bytes(),
        &[0],
        d.title.as_bytes(),
        &[0],
        &d.start_utc.to_le_bytes(),
        &d.end_utc.to_le_bytes(),
        d.description.as_bytes(),
    ] {
        for b in chunk {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01B3);
        }
    }
    h
}

fn mix(a: u64, b: u64) -> u64 {
    let mut x = a.wrapping_add(b).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 29;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 32;
    x
}

fn fresh_uid() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    format!(
        "muse-{:x}-{:x}",
        nanos,
        mix(n.wrapping_add(1), 0xD1B5_4A32_876E_0375)
    )
}

/// Escape iCalendar TEXT: backslash, newline, `;`, `,`.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            _ => out.push(c),
        }
    }
    out
}

fn render_ics(uid: &str, d: &CalendarDraft) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//muse-linux//calendar//EN\r\n\
         BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:{stamp}\r\nDTSTART:{start}\r\nDTEND:{end}\r\n\
         SUMMARY:{summary}\r\nDESCRIPTION:{desc}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        stamp = ical::format_utc(now_epoch()),
        start = ical::format_utc(d.start_utc),
        end = ical::format_utc(d.end_utc),
        summary = escape_text(&d.title),
        desc = escape_text(&d.description),
    )
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Minimal standard-alphabet base64 (for the Basic auth header only).
fn base64(input: &str) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 63) as usize] as char);
        out.push(ALPHA[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_draft() -> CalendarDraft {
        CalendarDraft {
            calendar_path: "/dav/calendars/user/work/".into(),
            title: "Planning".into(),
            start_utc: 1768461600,
            end_utc: 1768465200,
            description: "q1".into(),
        }
    }

    #[test]
    fn forged_handle_is_rejected() {
        let c = CalendarClient::new();
        let pending = c.queue_create(test_draft()).unwrap();
        // Forge: valid id, wrong integrity tag.
        let forged = PendingCreate {
            id: pending.id,
            tag: pending.tag.wrapping_add(1),
        };
        let err = c
            .approve_create(
                &CalendarAccount {
                    host: "127.0.0.1".into(),
                    port: 9,
                    username: "u".into(),
                    base_path: "/".into(),
                },
                "pw",
                &forged,
            )
            .unwrap_err();
        assert_eq!(err, CalError::TamperedDraft);
    }

    #[test]
    fn corrupted_store_is_rejected() {
        let c = CalendarClient::new();
        let pending = c.queue_create(test_draft()).unwrap();
        // Tamper with the staged bytes behind the handle's back.
        {
            let mut map = c.pending.lock().unwrap();
            let stored = map.get_mut(&pending.id).unwrap();
            stored.draft.title = "EVIL".into();
        }
        let err = c
            .approve_create(
                &CalendarAccount {
                    host: "127.0.0.1".into(),
                    port: 9,
                    username: "u".into(),
                    base_path: "/".into(),
                },
                "pw",
                &pending,
            )
            .unwrap_err();
        assert_eq!(err, CalError::TamperedDraft);
    }

    #[test]
    fn draft_bounds_are_checked() {
        let c = CalendarClient::new();
        let mut d = test_draft();
        d.end_utc = d.start_utc + 86401;
        assert!(matches!(
            c.queue_create(d).unwrap_err(),
            CalError::InvalidDraft(_)
        ));
    }

    #[test]
    fn account_debug_holds_no_secret() {
        let a = CalendarAccount {
            host: "h".into(),
            port: 443,
            username: "u".into(),
            base_path: "/".into(),
        };
        let dbg = format!("{a:?}");
        assert!(dbg.contains('h'));
        // There is no password field to leak; the Debug impl is hand-written.
        assert!(!dbg.contains("password"));
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(base64("user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64("a"), "YQ==");
        assert_eq!(base64("ab"), "YWI=");
        assert_eq!(base64("abc"), "YWJj");
    }

    /// Stub transport that answers every PUT with 201 and counts calls.
    struct OkPut {
        puts: std::sync::Mutex<usize>,
    }
    impl HttpTransport for OkPut {
        fn request(
            &self,
            _method: &str,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: Option<&[u8]>,
        ) -> Result<HttpResponse, TransportError> {
            *self.puts.lock().unwrap() += 1;
            Ok(HttpResponse {
                status: 201,
                headers: Default::default(),
                body: Vec::new(),
            })
        }
    }

    #[test]
    fn forged_guess_preserves_draft_for_legitimate_approve() {
        // Regression: verification used to remove the entry before checking
        // the tag, so one forged guess burned the legitimate staged draft.
        let transport = std::sync::Arc::new(OkPut {
            puts: std::sync::Mutex::new(0),
        });
        let c = CalendarClient::with_transport(transport.clone());
        let account = CalendarAccount {
            host: "127.0.0.1".into(),
            port: 9,
            username: "u".into(),
            base_path: "/".into(),
        };
        let pending = c.queue_create(test_draft()).unwrap();
        let forged = PendingCreate {
            id: pending.id,
            tag: pending.tag.wrapping_add(1),
        };
        assert_eq!(
            c.approve_create(&account, "pw", &forged).unwrap_err(),
            CalError::TamperedDraft
        );
        // The legitimate handle still works: exactly one PUT total.
        c.approve_create(&account, "pw", &pending).unwrap();
        assert_eq!(*transport.puts.lock().unwrap(), 1);
        // And the consumed entry cannot replay.
        assert_eq!(
            c.approve_create(&account, "pw", &pending).unwrap_err(),
            CalError::UnknownCreate
        );
        assert_eq!(*transport.puts.lock().unwrap(), 1);
    }

    #[test]
    fn href_must_stay_inside_its_calendar_segment() {
        // Regression: raw starts_with let /a match /ab/x and let an empty
        // calendar path skip the check entirely.
        assert!(href_in_calendar("/dav/cals/w/", "/dav/cals/w/e.ics"));
        assert!(href_in_calendar("/dav/cals/w", "/dav/cals/w/e.ics"));
        assert!(href_in_calendar("/dav/cals/w/", "/dav/cals/w/"));
        assert!(!href_in_calendar("/dav/cals/a", "/dav/cals/ab/x.ics"));
        assert!(
            !href_in_calendar("/dav/cals/a", "/dav/cals/attacker/e.ics"),
            "sibling prefix must not pass"
        );
        assert!(!href_in_calendar("", "/dav/cals/w/e.ics"));
        assert!(!href_in_calendar("/dav/cals/w/", ""));
        assert!(!href_in_calendar("/dav/cals/w/", "/dav/cals/w/../x.ics"));
        assert!(!href_in_calendar("/dav/cals/w/", "/dav/cals/w/e.ics?x=1"));
    }

    #[test]
    fn control_and_deforming_fields_fail_closed() {
        assert!(clean_host("127.0.0.1"));
        assert!(clean_host("::1"));
        assert!(clean_host("cal.example.com"));
        assert!(!clean_host(""));
        assert!(!clean_host("a b"));
        assert!(!clean_host("a/b"));
        assert!(!clean_host("u@h"));
        assert!(!clean_host("h?x"));
        assert!(!clean_host("h\r\nx: y"));
        assert!(clean_user("user"));
        assert!(!clean_user(""));
        assert!(!clean_user("u\r\nX: y"));
        assert!(clean_path("/dav/cals/w/"));
        assert!(!clean_path(""));
        assert!(!clean_path("relative/x"));
        assert!(!clean_path("/dav/../x"));
        assert!(!clean_path("/dav/x?y=1"));
        assert!(!clean_path("/dav/\r\nX: 1"));
        let bad = CalendarAccount {
            host: "127.0.0.1".into(),
            port: 9,
            username: "u\r\nX: y".into(),
            base_path: "/".into(),
        };
        assert!(matches!(
            validate_account(&bad).unwrap_err(),
            CalError::InvalidArgument(_)
        ));
    }

    #[test]
    fn extreme_epochs_fail_closed_without_overflow() {
        // Regression: plain i64 subtraction panicked on extreme epochs.
        let c = CalendarClient::new();
        let account = CalendarAccount {
            host: "127.0.0.1".into(),
            port: 9,
            username: "u".into(),
            base_path: "/".into(),
        };
        assert_eq!(
            c.events(&account, "pw", "/dav/cals/w/", i64::MIN, i64::MAX)
                .unwrap_err(),
            CalError::InvalidRange
        );
        let mut d = test_draft();
        d.start_utc = 0;
        d.end_utc = i64::MAX;
        assert!(matches!(
            c.queue_create(d).unwrap_err(),
            CalError::InvalidDraft(_)
        ));
    }
}
