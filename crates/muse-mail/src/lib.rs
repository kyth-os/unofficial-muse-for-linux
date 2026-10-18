//! Generic IMAP (read) + approval-gated SMTP (send) mail connector.
//!
//! Account-agnostic: works with any provider via explicit host/port/username.
//! No OAuth, no credential persistence, no background sync. Passwords are
//! passed per call as `&str`, held in memory only for the duration of the
//! call, never logged, never embedded in errors, and the owned handshake
//! buffer is zeroed (volatile write) before release. No struct in this crate
//! stores a password, so there is nothing to redact in `Debug` output.
//!
//! TLS is mandatory on every production path: implicit TLS on port 993
//! (IMAP) / 465 (SMTP), otherwise STARTTLS upgrade with system certificate
//! verification. Plaintext login or send is refused. `TlsMode::LoopbackPlaintext`
//! exists solely for offline tests against in-process `127.0.0.1` fixtures and
//! still refuses non-loopback endpoints.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Maximum plain-text body accepted or returned (64 KiB).
pub const MAX_BODY: usize = 65536;
/// Maximum message count per [`MailClient::list`] call.
pub const MAX_LIST: u32 = 50;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RW_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LINE: usize = 1024 * 1024;

/// Connection identity. The password travels per call and is never stored here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailAccount {
    pub host: String,
    pub port: u16,
    pub username: String,
}

impl MailAccount {
    pub fn new(host: &str, port: u16, username: &str) -> Self {
        Self {
            host: host.to_string(),
            port,
            username: username.to_string(),
        }
    }
}

/// One mailbox from `LIST` + `STATUS`: headers only, no message content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderStat {
    pub name: String,
    pub total: u32,
    pub unseen: u32,
}

/// Headers-only envelope: id/uid, from, subject, date.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub uid: u32,
    pub from: String,
    pub subject: String,
    pub date: String,
}

/// Plain-text body of one message, capped at [`MAX_BODY`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageText {
    pub uid: u32,
    pub text: String,
    /// Reserved for partial fetch; currently always false because literals
    /// over [`MAX_BODY`] fail closed with [`MailError::TooLarge`].
    pub truncated: bool,
}

/// Validated outbound message. Plain text only, no attachments.
/// Build with [`MailDraft::new`], which enforces the exact-recipient format,
/// non-empty subject/body, and the [`MAX_BODY`] cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailDraft {
    pub to: String,
    pub subject: String,
    pub body: String,
}

impl MailDraft {
    pub fn new(to: &str, subject: &str, body: &str) -> Result<Self, MailError> {
        validate_draft_fields(to, subject, body)?;
        Ok(Self {
            to: to.to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
        })
    }
}

/// Single validation gate for outbound drafts, used by both
/// [`MailDraft::new`] and [`queue_send`] so a literally-constructed draft
/// (all fields are `pub`) cannot bypass any check. In particular the
/// subject must be free of CR/LF (it is interpolated into SMTP headers)
/// and respect the 998-byte line cap.
fn validate_draft_fields(to: &str, subject: &str, body: &str) -> Result<(), MailError> {
    if !valid_email(to) {
        return Err(MailError::Validation(
            "Recipient must be one exact address like name@example.com (no display names, no lists).".into(),
        ));
    }
    if subject.trim().is_empty() {
        return Err(MailError::Validation("Subject must not be empty.".into()));
    }
    if subject.bytes().any(|b| b == b'\r' || b == b'\n') {
        return Err(MailError::Validation(
            "Subject must not contain line breaks.".into(),
        ));
    }
    if subject.len() > 998 {
        return Err(MailError::Validation(
            "Subject exceeds the 998-byte line limit.".into(),
        ));
    }
    if body.is_empty() {
        return Err(MailError::Validation("Body must not be empty.".into()));
    }
    if body.len() > MAX_BODY {
        return Err(MailError::Validation(format!(
            "Body exceeds the {}-byte cap.",
            MAX_BODY
        )));
    }
    Ok(())
}

fn valid_email(to: &str) -> bool {
    if to.is_empty() || to.len() > 320 {
        return false;
    }
    if to.bytes().any(|b| b <= 32 || b == 127) {
        return false;
    }
    let mut parts = to.split('@');
    let (local, domain, end) = (parts.next(), parts.next(), parts.next());
    if end.is_some() {
        return false; // more than one '@'
    }
    let (Some(local), Some(domain)) = (local, domain) else {
        return false;
    };
    if local.is_empty()
        || local.len() > 64
        || !local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(&b))
    {
        return false;
    }
    if domain.is_empty() || domain.len() > 253 {
        return false;
    }
    domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// A staged draft awaiting fresh per-send approval. `bytes` is the exact
/// recipient + subject + body serialization that [`approve_send`] hash-checks
/// before any socket is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDraft {
    pub id: u64,
    bytes: Vec<u8>,
}

impl PendingDraft {
    /// Exact bytes bound to the approval. Shown verbatim in the UI preview.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Test-only: forge approval bytes to prove tampering fails closed.
    #[doc(hidden)]
    pub fn bytes_mut_for_test(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }
}

/// Proof that SMTP accepted (or did not accept) a send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendReceipt {
    pub draft_id: u64,
    pub accepted: bool,
    pub server_reply: String,
}

/// Every failure in this crate. Never carries passwords or draft bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailError {
    /// Invalid draft, folder name, limit, or endpoint.
    Validation(String),
    /// Server rejected the credentials (password never included).
    Auth,
    /// TCP/TLS/IO failure description (no secrets).
    Connection(String),
    /// Bounded connect/read/write timeout elapsed.
    Timeout,
    /// Server would require plaintext auth/send; refused. No fallback exists.
    TlsRequired(String),
    /// Server spoke unexpected protocol bytes.
    Protocol(String),
    /// Body or literal exceeded [`MAX_BODY`].
    TooLarge(String),
    /// Message has no usable plain-text part.
    HtmlOnly,
    /// Unknown, denied, or already-consumed draft id.
    NotFound(String),
    /// Presented approval bytes differ from the queued bytes. Fails closed.
    Tampered,
    /// Send outcome unknown (e.g. link dropped after DATA). Never auto-retried.
    Uncertain(String),
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(m)
            | Self::Connection(m)
            | Self::TlsRequired(m)
            | Self::Protocol(m)
            | Self::TooLarge(m)
            | Self::NotFound(m)
            | Self::Uncertain(m) => f.write_str(m),
            Self::Auth => {
                f.write_str("IMAP/SMTP authentication failed; check host, port and username.")
            }
            Self::Timeout => f.write_str("Mail connection timed out; nothing was sent or changed."),
            Self::HtmlOnly => {
                f.write_str("Message has no plain-text part (HTML-only); not displayed.")
            }
            Self::Tampered => {
                f.write_str("Approval bytes differ from the queued draft; send refused.")
            }
        }
    }
}

impl std::error::Error for MailError {}

/// Transport policy for a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsMode {
    /// Production: implicit TLS on 993/465, else STARTTLS with system certs.
    /// Plaintext is refused; there is no insecure fallback.
    Require,
    /// Test-only: plaintext is allowed, but ONLY to loopback addresses
    /// (`127.0.0.0/8`, `::1`, `localhost`). Non-loopback is still refused.
    LoopbackPlaintext,
}

/// Explicit SMTP endpoint. Production code uses
/// [`smtp_endpoint_for`] (frozen `approve_send` path); tests use
/// [`MailClient::approve_send_via`] with a loopback fixture endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmtpEndpoint {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
}

/// Production SMTP endpoint for an account: same host, port 587 with
/// mandatory STARTTLS (port 465 with implicit TLS when the IMAP port is 465).
pub fn smtp_endpoint_for(account: &MailAccount) -> SmtpEndpoint {
    SmtpEndpoint {
        host: account.host.clone(),
        port: if account.port == 465 { 465 } else { 587 },
        tls: TlsMode::Require,
    }
}

/// Stateless IMAP/SMTP client. Credentials travel per call; nothing is stored.
pub struct MailClient;

impl MailClient {
    /// List mailboxes (folders) over IMAP with mandatory TLS.
    pub fn connect_imap(
        account: &MailAccount,
        password: &str,
    ) -> Result<Vec<FolderStat>, MailError> {
        Self::connect_imap_via(account, password, TlsMode::Require)
    }

    /// Test hook: same as [`Self::connect_imap`] with an explicit [`TlsMode`].
    pub fn connect_imap_via(
        account: &MailAccount,
        password: &str,
        tls: TlsMode,
    ) -> Result<Vec<FolderStat>, MailError> {
        check_account(account)?;
        let secret = Secret::new(password);
        let mut session = ImapSession::connect(account, &secret, tls)?;
        let result = session.folders();
        let _ = session.logout();
        result
    }

    /// Headers-only envelopes from a folder, bounded to `limit` (1..=50).
    pub fn list(
        account: &MailAccount,
        password: &str,
        folder: &str,
        limit: u32,
    ) -> Result<Vec<Envelope>, MailError> {
        Self::list_via(account, password, folder, limit, TlsMode::Require)
    }

    /// Test hook: same as [`Self::list`] with an explicit [`TlsMode`].
    pub fn list_via(
        account: &MailAccount,
        password: &str,
        folder: &str,
        limit: u32,
        tls: TlsMode,
    ) -> Result<Vec<Envelope>, MailError> {
        check_account(account)?;
        check_limit(limit)?;
        check_folder(folder)?;
        let secret = Secret::new(password);
        let mut session = ImapSession::connect(account, &secret, tls)?;
        let result = session.envelopes(folder, limit);
        let _ = session.logout();
        result
    }

    /// Plain-text body of one message by UID, capped at [`MAX_BODY`].
    pub fn read_text(
        account: &MailAccount,
        password: &str,
        folder: &str,
        uid: u32,
    ) -> Result<MessageText, MailError> {
        Self::read_text_via(account, password, folder, uid, TlsMode::Require)
    }

    /// Test hook: same as [`Self::read_text`] with an explicit [`TlsMode`].
    pub fn read_text_via(
        account: &MailAccount,
        password: &str,
        folder: &str,
        uid: u32,
        tls: TlsMode,
    ) -> Result<MessageText, MailError> {
        check_account(account)?;
        check_folder(folder)?;
        let secret = Secret::new(password);
        let mut session = ImapSession::connect(account, &secret, tls)?;
        let result = session.message_text(folder, uid);
        let _ = session.logout();
        result
    }

    /// SMTP send. Only here does anything touch the network for sending.
    /// Test hook with an explicit endpoint (loopback fixtures).
    pub fn approve_send_via(
        account: &MailAccount,
        password: &str,
        draft: &PendingDraft,
        endpoint: &SmtpEndpoint,
    ) -> Result<SendReceipt, MailError> {
        check_account(account)?;
        // Bind the approval BEFORE any socket opens: unknown/consumed id fails,
        // and presented bytes must hash-match the queued bytes (fails closed).
        // The entry is consumed here, so transport failure or uncertainty can
        // never silently double-send through this API.
        let (to, subject, body) = claim_queued(draft)?;
        let secret = Secret::new(password);
        smtp_send(account, &secret, &to, &subject, &body, endpoint).map(|reply| SendReceipt {
            draft_id: draft.id,
            accepted: true,
            server_reply: reply,
        })
    }
}

fn check_limit(limit: u32) -> Result<(), MailError> {
    if limit == 0 || limit > MAX_LIST {
        return Err(MailError::Validation(format!(
            "Limit must be 1..={MAX_LIST}."
        )));
    }
    Ok(())
}

fn check_folder(folder: &str) -> Result<(), MailError> {
    if folder.is_empty() || folder.len() > 128 || folder.contains(['\r', '\n']) {
        return Err(MailError::Validation(
            "Folder name must be non-empty without line breaks.".into(),
        ));
    }
    Ok(())
}

// ---------- draft store ----------

struct StoredDraft {
    hash: u64,
    bytes: Vec<u8>,
}

fn draft_store() -> &'static Mutex<HashMap<u64, StoredDraft>> {
    static STORE: OnceLock<Mutex<HashMap<u64, StoredDraft>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn draft_ids() -> &'static AtomicU64 {
    static IDS: OnceLock<AtomicU64> = OnceLock::new();
    IDS.get_or_init(|| AtomicU64::new(1))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn canonical_bytes(draft: &MailDraft) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(draft.to.len() + draft.subject.len() + draft.body.len() + 2);
    bytes.extend_from_slice(draft.to.as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(draft.subject.as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(draft.body.as_bytes());
    bytes
}

/// Zero a String's bytes in place. SAFETY: writing zeros preserves UTF-8
/// validity (NUL is a valid char) and never changes the length.
fn zero_str(s: &mut str) {
    // SAFETY: zero bytes are valid UTF-8 and the length is unchanged.
    unsafe {
        zero(s.as_bytes_mut());
    }
}

fn zero(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: volatile write to a valid exclusive borrow; prevents the
        // compiler from optimizing the wipe away.
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::hint::black_box(bytes.as_ptr());
}

/// Validate and stage a draft. Nothing is sent; the returned [`PendingDraft`]
/// must pass through [`approve_send`] (fresh approval per send) or
/// [`deny_send`].
pub fn queue_send(draft: MailDraft) -> Result<PendingDraft, MailError> {
    // Re-check through the same gate: the queue must never hold an invalid
    // draft even if constructed literally outside MailDraft::new.
    validate_draft_fields(&draft.to, &draft.subject, &draft.body)
        .map_err(|_| MailError::Validation("Invalid draft.".into()))?;
    let bytes = canonical_bytes(&draft);
    let id = draft_ids().fetch_add(1, Ordering::SeqCst);
    let hash = fnv1a(&bytes);
    draft_store()
        .lock()
        .map_err(|_| MailError::Connection("Draft store unavailable.".into()))?
        .insert(
            id,
            StoredDraft {
                hash,
                bytes: bytes.clone(),
            },
        );
    Ok(PendingDraft { id, bytes })
}

/// Remove a queued entry and return its exact recipient/subject/body after
/// verifying the binding. Entry is consumed (removed) on success.
fn claim_queued(draft: &PendingDraft) -> Result<(String, String, String), MailError> {
    let mut store = draft_store()
        .lock()
        .map_err(|_| MailError::Connection("Draft store unavailable.".into()))?;
    let stored = store.get(&draft.id).ok_or_else(|| {
        MailError::NotFound(format!(
            "Draft {} is unknown, denied or already sent.",
            draft.id
        ))
    })?;
    if stored.hash != fnv1a(draft.bytes()) || stored.bytes != draft.bytes() {
        return Err(MailError::Tampered);
    }
    let bytes = store.remove(&draft.id).expect("just looked up").bytes;
    drop(store);
    let text = String::from_utf8(bytes)
        .map_err(|_| MailError::Validation("Draft bytes are not valid text.".into()))?;
    let mut parts = text.splitn(3, '\n');
    let (Some(to), Some(subject), Some(body)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(MailError::Validation("Draft bytes are malformed.".into()));
    };
    Ok((to.to_string(), subject.to_string(), body.to_string()))
}

/// Send a queued draft over SMTP after binding the approval: the draft id must
/// be queued and unconsumed, and the presented exact bytes must hash-match the
/// queued bytes, else this fails closed before any socket opens. Consumes the
/// queue entry, so a second approval of the same draft fails with `NotFound`.
/// Uncertain transport outcomes are returned as `MailError::Uncertain` and are
/// never auto-retried.
///
/// Diff from the frozen sketch: takes `&PendingDraft` (which carries the frozen
/// `draft_id` plus the exact bytes) instead of a bare id, so the hash binding
/// can be compared at send time. A bare id alone cannot detect tampering.
pub fn approve_send(
    account: &MailAccount,
    password: &str,
    draft: &PendingDraft,
) -> Result<SendReceipt, MailError> {
    let endpoint = smtp_endpoint_for(account);
    MailClient::approve_send_via(account, password, draft, &endpoint)
}

/// Discard a queued draft without sending. Unknown ids are a no-op so UI
/// teardown can call this unconditionally.
pub fn deny_send(id: u64) {
    if let Ok(mut store) = draft_store().lock() {
        if let Some(mut stored) = store.remove(&id) {
            zero(&mut stored.bytes);
        }
    }
}

/// Test-only: number of staged drafts currently held. The lib unit-test
/// binary is the only suite that touches the draft store, so counts there
/// are deterministic.
#[doc(hidden)]
pub fn queued_count_for_test() -> usize {
    draft_store().lock().map(|s| s.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restage_leaves_no_orphaned_queue_entry() {
        // Regression: stage() replaced self.pending without denying the old
        // id, orphaning a queue entry that could never be consumed. Counts
        // are deterministic here: no other lib unit test touches the store
        // (integration suites run in separate processes).
        let base = queued_count_for_test();
        let mut ctl = DraftController::new();
        ctl.stage(MailDraft::new("a@b.c", "one", "b1").unwrap())
            .unwrap();
        assert_eq!(queued_count_for_test(), base + 1);
        ctl.stage(MailDraft::new("a@b.c", "two", "b2").unwrap())
            .unwrap();
        assert_eq!(
            queued_count_for_test(),
            base + 1,
            "restage must deny the previous entry"
        );
        ctl.deny();
        assert_eq!(queued_count_for_test(), base);
    }
}

// ---------- secrets ----------

/// Owned password bytes, zeroed (volatile) on drop. Never logged, never
/// cloned into errors, never stored in any long-lived struct.
struct Secret(Vec<u8>);

impl Secret {
    fn new(password: &str) -> Self {
        Self(password.as_bytes().to_vec())
    }

    fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}

// ---------- transport ----------

fn map_io(error: std::io::Error) -> MailError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => MailError::Timeout,
        std::io::ErrorKind::UnexpectedEof => {
            MailError::Connection("Server closed the connection.".into())
        }
        other => MailError::Connection(format!("Network error ({other:?}).")),
    }
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(addr) = host.parse::<std::net::IpAddr>() {
        return addr.is_loopback();
    }
    host.starts_with("127.")
}

/// Account fields must be free of control/CRLF characters before any socket
/// opens: the username is interpolated into IMAP LOGIN and SMTP AUTH/MAIL
/// FROM lines, where a line break would inject protocol commands.
fn check_account(account: &MailAccount) -> Result<(), MailError> {
    if account.host.is_empty()
        || account.host.bytes().any(|b| b <= 32 || b == 127)
        || account.username.is_empty()
        || account.username.bytes().any(|b| b == b'\r' || b == b'\n')
    {
        return Err(MailError::Validation(
            "Account host and username must be non-empty without line breaks.".into(),
        ));
    }
    Ok(())
}

enum Stream {
    Plain(TcpStream),
    Tls(native_tls::TlsStream<TcpStream>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

struct Conn {
    stream: Stream,
    buf: Vec<u8>,
}

impl Conn {
    fn plain(tcp: TcpStream) -> Self {
        Self {
            stream: Stream::Plain(tcp),
            buf: Vec::new(),
        }
    }

    fn upgrade_tls(&mut self, host: &str) -> Result<(), MailError> {
        let plain = match std::mem::replace(
            &mut self.stream,
            Stream::Plain(TcpStream::connect("127.0.0.1:1").map_err(map_io)?),
        ) {
            Stream::Plain(tcp) => tcp,
            Stream::Tls(_) => return Err(MailError::Protocol("Already using TLS.".into())),
        };
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| MailError::Connection(format!("TLS setup failed ({e}).")))?;
        let tls = connector
            .connect(host, plain)
            .map_err(|e| MailError::Connection(format!("TLS handshake failed ({e}).")))?;
        self.stream = Stream::Tls(tls);
        Ok(())
    }

    fn fill(&mut self) -> Result<usize, MailError> {
        let mut chunk = [0u8; 4096];
        let n = self.stream.read(&mut chunk).map_err(map_io)?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n)
    }

    fn read_line(&mut self) -> Result<String, MailError> {
        loop {
            if self.buf.len() > MAX_LINE {
                return Err(MailError::Protocol("Server line too long.".into()));
            }
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = self.buf.drain(..=pos).collect();
                let mut line = String::from_utf8(raw)
                    .map_err(|_| MailError::Protocol("Non-UTF8 protocol line.".into()))?;
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if self.fill()? == 0 {
                return Err(MailError::Connection(
                    "Server closed the connection.".into(),
                ));
            }
        }
    }

    fn read_exact(&mut self, len: usize) -> Result<Vec<u8>, MailError> {
        while self.buf.len() < len {
            if self.fill()? == 0 {
                return Err(MailError::Connection(
                    "Server closed the connection mid-literal.".into(),
                ));
            }
        }
        Ok(self.buf.drain(..len).collect())
    }

    fn write_line(&mut self, line: &str) -> Result<(), MailError> {
        self.write_bytes(line.as_bytes())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), MailError> {
        self.stream.write_all(bytes).map_err(map_io)?;
        self.stream.write_all(b"\r\n").map_err(map_io)?;
        self.stream.flush().map_err(map_io)?;
        Ok(())
    }
}

fn dial(host: &str, port: u16) -> Result<TcpStream, MailError> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|_| MailError::Connection(format!("Cannot resolve mail host {host}.")))?
        .next()
        .ok_or_else(|| MailError::Connection(format!("Cannot resolve mail host {host}.")))?;
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(map_io)?;
    tcp.set_read_timeout(Some(RW_TIMEOUT)).map_err(map_io)?;
    tcp.set_write_timeout(Some(RW_TIMEOUT)).map_err(map_io)?;
    Ok(tcp)
}

/// Status token of a tagged IMAP response (`tag SP status ...`), if the
/// line is addressed to `tag`. Compared by callers with equality, never
/// substring: `t1 NO ... bad token` must not read as success.
fn tagged_status<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(tag)?.strip_prefix(' ')?;
    rest.split_whitespace().next()
}

/// Second token of a tagged response line, whatever the tag was. Used where
/// the tag itself is no longer at hand (LOGIN error mapping).
fn response_status(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    words.next()?;
    words.next()
}

/// True for tagged NO/BAD rejections (compared as tokens, not substrings).
fn is_auth_rejection(line: &str) -> bool {
    response_status(line)
        .is_some_and(|s| s.eq_ignore_ascii_case("NO") || s.eq_ignore_ascii_case("BAD"))
}

fn imap_quote(raw: &str) -> String {
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('"');
    for ch in raw.chars() {
        if ch == '"' || ch == '\\' {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

/// Byte-level IMAP quoting appended in place, for password-bearing buffers
/// that must be zeroed after the write (no intermediate String copies).
fn imap_quote_bytes(out: &mut Vec<u8>, raw: &[u8]) {
    out.push(b'"');
    for &b in raw {
        if b == b'"' || b == b'\\' {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b'"');
}

// ---------- IMAP ----------

struct ImapSession {
    conn: Conn,
    tag: u32,
}

impl ImapSession {
    fn connect(account: &MailAccount, password: &Secret, tls: TlsMode) -> Result<Self, MailError> {
        if tls == TlsMode::LoopbackPlaintext && !is_loopback_host(&account.host) {
            return Err(MailError::TlsRequired(
                "Test-mode plaintext is allowed only to loopback hosts.".into(),
            ));
        }
        if tls == TlsMode::LoopbackPlaintext {
            let loopback = (account.host.as_str(), account.port)
                .to_socket_addrs()
                .map_err(|_| MailError::Connection("Cannot resolve mail host.".into()))?
                .next()
                .is_some_and(|addr| addr.ip().is_loopback());
            if !loopback {
                return Err(MailError::TlsRequired(
                    "Test-mode plaintext is allowed only to loopback hosts.".into(),
                ));
            }
        }
        let implicit_tls = tls == TlsMode::Require && (account.port == 993 || account.port == 465);
        let tcp = dial(&account.host, account.port)?;
        let mut session = Self {
            conn: Conn::plain(tcp),
            tag: 0,
        };
        if implicit_tls {
            session.conn.upgrade_tls(&account.host)?;
        }
        let greeting = session.conn.read_line()?;
        if !greeting.starts_with("* OK") {
            return Err(MailError::Protocol("Bad IMAP greeting.".into()));
        }
        if tls == TlsMode::Require && !implicit_tls {
            let caps = session.command("CAPABILITY")?;
            let offers_starttls = caps
                .iter()
                .flat_map(|line| line.split_whitespace())
                .any(|token| token.eq_ignore_ascii_case("STARTTLS"));
            if !offers_starttls {
                return Err(MailError::TlsRequired(
                    "Server does not offer STARTTLS; plaintext login refused with no fallback."
                        .into(),
                ));
            }
            session.command("STARTTLS")?;
            session.conn.upgrade_tls(&account.host)?;
        }
        let mut password_text = String::from_utf8_lossy(password.expose()).into_owned();
        // Build the LOGIN line as raw bytes so every password-bearing buffer
        // can be zeroed after the write; String copies would retain the
        // secret on the heap until allocator reuse.
        let mut login: Vec<u8> = Vec::with_capacity(64 + password_text.len() + 2);
        login.extend_from_slice(b"LOGIN ");
        login.extend_from_slice(imap_quote(&account.username).as_bytes());
        login.push(b' ');
        imap_quote_bytes(&mut login, password_text.as_bytes());
        zero_str(&mut password_text);
        drop(password_text);
        let result = match session.command_raw_bytes(&login) {
            Ok(_) => Ok(session),
            Err(MailError::Protocol(line)) if is_auth_rejection(&line) => Err(MailError::Auth),
            Err(other) => Err(other),
        };
        zero(&mut login);
        result
    }

    fn next_tag(&mut self) -> String {
        self.tag = self.tag.wrapping_add(1);
        format!("t{}", self.tag)
    }

    /// Send a pre-built command line (no tag added); collect untagged lines;
    /// tagged OK required. Used for credential-bearing commands whose bytes
    /// the caller zeroes after the write.
    fn command_raw_bytes(&mut self, cmd: &[u8]) -> Result<Vec<String>, MailError> {
        let tag = self.next_tag();
        let mut line = Vec::with_capacity(tag.len() + 1 + cmd.len());
        line.extend_from_slice(tag.as_bytes());
        line.push(b' ');
        line.extend_from_slice(cmd);
        self.conn.write_bytes(&line)?;
        let mut untagged = Vec::new();
        loop {
            let line = self.conn.read_line()?;
            if let Some(status) = tagged_status(&line, &tag) {
                if status.eq_ignore_ascii_case("OK") {
                    return Ok(untagged);
                }
                return Err(MailError::Protocol(line));
            }
            untagged.push(line);
        }
    }

    /// Send a command; collect untagged lines; tagged OK required.
    /// The status token immediately after the tag must equal OK
    /// (case-insensitive): substring matching would misread e.g. a NO line
    /// containing "TOKEN" as success. Tagged failure surfaces as
    /// `Protocol(tagged line)` so callers (LOGIN) can distinguish auth
    /// rejection from transport errors.
    fn command_raw(&mut self, cmd: &str) -> Result<Vec<String>, MailError> {
        let tag = self.next_tag();
        self.conn.write_line(&format!("{tag} {cmd}"))?;
        let mut untagged = Vec::new();
        loop {
            let line = self.conn.read_line()?;
            if let Some(status) = tagged_status(&line, &tag) {
                if status.eq_ignore_ascii_case("OK") {
                    return Ok(untagged);
                }
                return Err(MailError::Protocol(line));
            }
            untagged.push(line);
        }
    }

    fn command(&mut self, cmd: &str) -> Result<Vec<String>, MailError> {
        self.command_raw(cmd)
    }

    fn logout(mut self) -> Result<(), MailError> {
        let _ = self.command("LOGOUT");
        Ok(())
    }

    fn folders(&mut self) -> Result<Vec<FolderStat>, MailError> {
        let mut names = Vec::new();
        for line in self.command("LIST \"\" \"*\"")? {
            if let Some(name) = parse_list_name(&line) {
                names.push(name);
            }
        }
        let mut folders = Vec::new();
        for name in names {
            let status =
                self.command(&format!("STATUS {} (MESSAGES UNSEEN)", imap_quote(&name)))?;
            let (total, unseen) = parse_status(&status).unwrap_or((0, 0));
            folders.push(FolderStat {
                name,
                total,
                unseen,
            });
        }
        Ok(folders)
    }

    fn select(&mut self, folder: &str) -> Result<u32, MailError> {
        let lines = self.command(&format!("SELECT {}", imap_quote(folder)))?;
        let mut total = 0;
        for line in lines {
            if let Some(n) = parse_exists(&line) {
                total = n;
            }
        }
        Ok(total)
    }

    fn fetch_raw(&mut self, cmd: &str) -> Result<Vec<(u32, Vec<u8>)>, MailError> {
        let tag = self.next_tag();
        self.conn.write_line(&format!("{tag} {cmd}"))?;
        let mut out = Vec::new();
        loop {
            let line = self.conn.read_line()?;
            if let Some(status) = tagged_status(&line, &tag) {
                if status.eq_ignore_ascii_case("OK") {
                    return Ok(out);
                }
                if status.eq_ignore_ascii_case("NO") {
                    return Err(MailError::NotFound("Message not found.".into()));
                }
                return Err(MailError::Protocol("FETCH failed.".into()));
            }
            if line.starts_with("* ") && line.contains("FETCH") {
                let uid = parse_uid(&line)
                    .ok_or_else(|| MailError::Protocol("Bad FETCH line.".into()))?;
                let len = parse_literal_len(&line)
                    .ok_or_else(|| MailError::Protocol("Bad FETCH literal.".into()))?;
                if len > MAX_BODY {
                    return Err(MailError::TooLarge(format!(
                        "Message literal ({len} bytes) exceeds the {MAX_BODY}-byte cap."
                    )));
                }
                let bytes = self.conn.read_exact(len)?;
                let _trailer = self.conn.read_line()?; // closing ")"
                out.push((uid, bytes));
            }
        }
    }

    fn envelopes(&mut self, folder: &str, limit: u32) -> Result<Vec<Envelope>, MailError> {
        let total = self.select(folder)?;
        if total == 0 {
            return Ok(Vec::new());
        }
        let low = total.saturating_sub(limit - 1).max(1);
        let raws = self.fetch_raw(&format!(
            "UID FETCH {low}:{total} (UID BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)])"
        ))?;
        let mut envelopes = Vec::new();
        for (uid, bytes) in raws.iter().take(limit as usize) {
            let headers = parse_rfc822_headers(bytes);
            envelopes.push(Envelope {
                uid: *uid,
                from: header_value(&headers, "from"),
                subject: header_value(&headers, "subject"),
                date: header_value(&headers, "date"),
            });
        }
        Ok(envelopes)
    }

    fn message_text(&mut self, folder: &str, uid: u32) -> Result<MessageText, MailError> {
        self.select(folder)?;
        let raws = self.fetch_raw(&format!("UID FETCH {uid} (UID BODY.PEEK[])"))?;
        let (_, bytes) = raws
            .into_iter()
            .find(|(u, _)| *u == uid)
            .ok_or_else(|| MailError::NotFound(format!("Message UID {uid} not found.")))?;
        let (text, truncated) = extract_text(&bytes)?;
        Ok(MessageText {
            uid,
            text,
            truncated,
        })
    }
}

fn parse_list_name(line: &str) -> Option<String> {
    // `* LIST (flags) "/" "INBOX"` or `* LIST (flags) / INBOX`
    let mut names: Vec<&str> = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        if let Some(end) = after.find('"') {
            names.push(&after[..end]);
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    if let Some(name) = names.last() {
        return Some((*name).to_string());
    }
    line.split_whitespace().last().map(str::to_string)
}

fn parse_status(lines: &[String]) -> Option<(u32, u32)> {
    for line in lines {
        let upper = line.to_ascii_uppercase();
        if !upper.contains("STATUS") {
            continue;
        }
        let tokens: Vec<&str> = line
            .split(|c: char| c == '(' || c == ')' || c.is_whitespace())
            .filter(|t| !t.is_empty())
            .collect();
        let mut total = None;
        let mut unseen = None;
        let mut iter = tokens.iter().peekable();
        while let Some(token) = iter.next() {
            if token.eq_ignore_ascii_case("MESSAGES") {
                total = iter.next().and_then(|n| n.parse().ok());
            } else if token.eq_ignore_ascii_case("UNSEEN") {
                unseen = iter.next().and_then(|n| n.parse().ok());
            }
        }
        if let (Some(total), Some(unseen)) = (total, unseen) {
            return Some((total, unseen));
        }
    }
    None
}

fn parse_exists(line: &str) -> Option<u32> {
    // `* 23 EXISTS`
    let line = line.strip_prefix("* ")?;
    let num = line.strip_suffix("EXISTS")?.trim();
    num.parse().ok()
}

fn parse_uid(line: &str) -> Option<u32> {
    let pos = line.find("UID ")?;
    line[pos + 4..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|s| !s.is_empty())?
        .parse()
        .ok()
}

fn parse_literal_len(line: &str) -> Option<usize> {
    let start = line.rfind('{')?;
    let end = line[start..].find('}')?;
    line[start + 1..start + end].parse().ok()
}

// ---------- message parsing ----------

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn split_head_body(message: &[u8]) -> (&[u8], &[u8]) {
    if let Some(pos) = find_bytes(message, b"\r\n\r\n") {
        return (&message[..pos], &message[pos + 4..]);
    }
    if let Some(pos) = find_bytes(message, b"\n\n") {
        return (&message[..pos], &message[pos + 2..]);
    }
    (message, &[])
}

fn parse_rfc822_headers(block: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(block);
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in text.split_inclusive('\n') {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some(colon) = line.find(':') {
            headers.push((
                line[..colon].trim().to_ascii_lowercase(),
                line[colon + 1..].trim().to_string(),
            ));
        }
    }
    headers
}

fn header_value(headers: &[(String, String)], name: &str) -> String {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn content_type_parts(headers: &[(String, String)]) -> (String, Option<String>) {
    let raw = header_value(headers, "content-type");
    if raw.is_empty() {
        return ("text/plain".into(), None);
    }
    let mut pieces = raw.split(';');
    let mime = pieces
        .next()
        .unwrap_or("text/plain")
        .trim()
        .to_ascii_lowercase();
    let mut boundary = None;
    for piece in pieces {
        let piece = piece.trim();
        if let Some(value) = piece.strip_prefix("boundary=") {
            let value = value.trim();
            let unquoted = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            boundary = Some(unquoted.trim_matches('"').to_string());
        } else if piece.to_ascii_lowercase().starts_with("boundary") {
            if let Some(eq) = piece.find('=') {
                let value = piece[eq + 1..].trim().trim_matches('"');
                boundary = Some(value.to_string());
            }
        }
    }
    (mime, boundary)
}

fn decode_cte(headers: &[(String, String)], body: &[u8]) -> Result<Vec<u8>, MailError> {
    let encoding = header_value(headers, "content-transfer-encoding").to_ascii_lowercase();
    match encoding.as_str() {
        "" | "7bit" | "8bit" | "binary" => Ok(body.to_vec()),
        "base64" => {
            let text = std::str::from_utf8(body)
                .map_err(|_| MailError::Protocol("Non-ASCII base64 body.".into()))?;
            b64_decode(text).ok_or_else(|| MailError::Protocol("Bad base64 body.".into()))
        }
        "quoted-printable" => {
            qp_decode(body).ok_or_else(|| MailError::Protocol("Bad quoted-printable body.".into()))
        }
        other => Err(MailError::Protocol(format!(
            "Unsupported transfer encoding {other}."
        ))),
    }
}

fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delimiter = format!("--{boundary}");
    let bytes = delimiter.as_bytes();
    let mut parts = Vec::new();
    let mut chunks: Vec<&[u8]> = Vec::new();
    let mut start = 0;
    while let Some(relative) = find_bytes(&body[start..], bytes) {
        chunks.push(&body[start..start + relative]);
        start += relative + bytes.len();
    }
    chunks.push(&body[start..]);
    // First chunk is the preamble; skip it.
    for chunk in chunks.into_iter().skip(1) {
        if chunk.starts_with(b"--") {
            break; // closing delimiter / epilogue
        }
        let mut part = chunk;
        while part.starts_with(b"\r\n") || part.starts_with(b"\n") {
            part = if part.starts_with(b"\r\n") {
                &part[2..]
            } else {
                &part[1..]
            };
        }
        while part.ends_with(b"\r\n") || part.ends_with(b"\n") {
            part = if part.ends_with(b"\r\n") {
                &part[..part.len() - 2]
            } else {
                &part[..part.len() - 1]
            };
        }
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts
}

/// Extract plain text from a full RFC822 message. Returns (text, truncated).
fn extract_text(message: &[u8]) -> Result<(String, bool), MailError> {
    let (head, body) = split_head_body(message);
    let headers = parse_rfc822_headers(head);
    extract_part(&headers, body)
}

fn extract_part(headers: &[(String, String)], body: &[u8]) -> Result<(String, bool), MailError> {
    let (mime, boundary) = content_type_parts(headers);
    if mime == "text/plain" {
        let decoded = decode_cte(headers, body)?;
        let mut text = String::from_utf8_lossy(&decoded).into_owned();
        let mut truncated = false;
        if text.len() > MAX_BODY {
            text.truncate(MAX_BODY);
            truncated = true;
        }
        return Ok((text, truncated));
    }
    if mime == "text/html" {
        return Err(MailError::HtmlOnly);
    }
    if mime.starts_with("multipart/") {
        let boundary = boundary
            .ok_or_else(|| MailError::Protocol("Multipart message without boundary.".into()))?;
        let mut saw_html = false;
        for part in split_multipart(body, &boundary) {
            let (head, part_body) = split_head_body(part);
            let part_headers = parse_rfc822_headers(head);
            let (part_mime, _) = content_type_parts(&part_headers);
            if part_mime == "text/html" {
                saw_html = true;
            }
            match extract_part(&part_headers, part_body) {
                Ok(found) => return Ok(found),
                Err(MailError::HtmlOnly) => saw_html = true,
                Err(_) => {}
            }
        }
        if saw_html {
            return Err(MailError::HtmlOnly);
        }
        return Err(MailError::Protocol(
            "Multipart message has no plain-text part.".into(),
        ));
    }
    Err(MailError::Protocol(format!(
        "No plain-text part (top-level {mime})."
    )))
}

fn b64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut values = [0u8; 4];
        let mut pad = 0;
        for (i, &byte) in chunk.iter().enumerate() {
            if byte == b'=' {
                pad += 1;
            } else {
                if pad > 0 {
                    return None;
                }
                values[i] = b64_value(byte)?;
            }
        }
        if pad > 2 {
            return None;
        }
        out.push(values[0] << 2 | values[1] >> 4);
        if pad < 2 {
            out.push(values[1] << 4 | values[2] >> 2);
        }
        if pad < 1 {
            out.push(values[2] << 6 | values[3]);
        }
    }
    Some(out)
}

fn b64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 15) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn qp_decode(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i] != b'=' {
            out.push(body[i]);
            i += 1;
            continue;
        }
        // Soft line break: "=" followed by CRLF or LF.
        if body.get(i + 1) == Some(&b'\r') || body.get(i + 1) == Some(&b'\n') {
            i += 1;
            if body.get(i) == Some(&b'\r') && body.get(i + 1) == Some(&b'\n') {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if i + 2 >= body.len() {
            return None;
        }
        let hex = std::str::from_utf8(&body[i + 1..i + 3]).ok()?;
        out.push(u8::from_str_radix(hex, 16).ok()?);
        i += 3;
    }
    Some(out)
}

// ---------- SMTP ----------

fn smtp_send(
    account: &MailAccount,
    password: &Secret,
    to: &str,
    subject: &str,
    body: &str,
    endpoint: &SmtpEndpoint,
) -> Result<String, MailError> {
    if endpoint.tls == TlsMode::LoopbackPlaintext && !is_loopback_host(&endpoint.host) {
        return Err(MailError::TlsRequired(
            "Test-mode plaintext SMTP is allowed only to loopback hosts.".into(),
        ));
    }
    if endpoint.tls == TlsMode::LoopbackPlaintext {
        let loopback = (endpoint.host.as_str(), endpoint.port)
            .to_socket_addrs()
            .map_err(|_| MailError::Connection("Cannot resolve SMTP host.".into()))?
            .next()
            .is_some_and(|addr| addr.ip().is_loopback());
        if !loopback {
            return Err(MailError::TlsRequired(
                "Test-mode plaintext SMTP is allowed only to loopback hosts.".into(),
            ));
        }
    }
    let implicit_tls = endpoint.tls == TlsMode::Require && endpoint.port == 465;
    let tcp = dial(&endpoint.host, endpoint.port)?;
    let mut conn = Conn::plain(tcp);
    if implicit_tls {
        conn.upgrade_tls(&endpoint.host)?;
    }
    let greeting = conn.read_line()?;
    if !greeting.starts_with("220") {
        return Err(MailError::Protocol("Bad SMTP greeting.".into()));
    }
    let mut extensions = smtp_ehlo(&mut conn)?;
    if endpoint.tls == TlsMode::Require && !implicit_tls {
        if !extensions
            .iter()
            .any(|ext| ext.eq_ignore_ascii_case("STARTTLS"))
        {
            return Err(MailError::TlsRequired(
                "SMTP server does not offer STARTTLS; plaintext send refused with no fallback."
                    .into(),
            ));
        }
        let (code, _) = smtp_command(&mut conn, "STARTTLS")?;
        if code != 220 {
            return Err(MailError::Protocol("STARTTLS rejected.".into()));
        }
        conn.upgrade_tls(&endpoint.host)?;
        extensions = smtp_ehlo(&mut conn)?;
    }
    smtp_auth(&mut conn, &extensions, &account.username, password)?;
    // Never emit a null sender on invalid input: fail closed instead.
    if !valid_email(&account.username) {
        return Err(MailError::Validation(
            "Account username must be a valid email address to send.".into(),
        ));
    }
    let sender = account.username.clone();
    let (code, text) = smtp_command(&mut conn, &format!("MAIL FROM:<{sender}>"))?;
    if code != 250 {
        return Err(MailError::Protocol(format!(
            "Server refused sender ({code} {text})."
        )));
    }
    let (code, text) = smtp_command(&mut conn, &format!("RCPT TO:<{to}>"))?;
    if code != 250 {
        return Err(MailError::Validation(format!(
            "Server refused recipient ({code} {text})."
        )));
    }
    let (code, _) = smtp_command(&mut conn, "DATA")?;
    if code != 354 {
        return Err(MailError::Protocol("Server refused DATA.".into()));
    }
    send_message_data(&mut conn, &account.username, to, subject, body)?;
    // The reply after "." is the ONLY proof of delivery. A dropped link here
    // means the outcome is unknown: surface Uncertain, never auto-retry.
    let reply = match conn.read_line() {
        Ok(line) => line,
        Err(_) => {
            return Err(MailError::Uncertain(
                "Connection lost after DATA with no server confirmation; delivery is unknown. Do not retry automatically — confirm with the recipient first.".into(),
            ));
        }
    };
    let code: u16 = reply.get(..3).and_then(|c| c.parse().ok()).unwrap_or(0);
    if code == 250 {
        let _ = smtp_command(&mut conn, "QUIT");
        return Ok(reply);
    }
    if (500..600).contains(&code) {
        return Err(MailError::Validation(format!(
            "Server rejected message ({reply})."
        )));
    }
    Err(MailError::Uncertain(format!(
        "Unexpected send reply ({reply}); delivery is unknown. Do not retry automatically."
    )))
}

fn smtp_command(conn: &mut Conn, cmd: &str) -> Result<(u16, String), MailError> {
    conn.write_line(cmd)?;
    let mut full = String::new();
    loop {
        let line = conn.read_line()?;
        if line.len() < 4 {
            return Err(MailError::Protocol("Short SMTP reply.".into()));
        }
        let code: u16 = line[..3]
            .parse()
            .map_err(|_| MailError::Protocol("Bad SMTP reply code.".into()))?;
        if !full.is_empty() {
            full.push(' ');
        }
        full.push_str(line.get(4..).unwrap_or(""));
        if line.as_bytes().get(3) == Some(&b' ') {
            return Ok((code, full));
        }
    }
}

fn smtp_ehlo(conn: &mut Conn) -> Result<Vec<String>, MailError> {
    conn.write_line("EHLO localhost")?;
    let mut extensions = Vec::new();
    loop {
        let line = conn.read_line()?;
        if line.len() < 4 {
            return Err(MailError::Protocol("Short SMTP reply.".into()));
        }
        let code: u16 = line[..3]
            .parse()
            .map_err(|_| MailError::Protocol("Bad SMTP reply code.".into()))?;
        if code != 250 {
            return Err(MailError::Protocol("EHLO rejected.".into()));
        }
        extensions.push(line.get(4..).unwrap_or("").to_string());
        if line.as_bytes().get(3) == Some(&b' ') {
            break;
        }
    }
    Ok(extensions)
}

fn smtp_auth(
    conn: &mut Conn,
    extensions: &[String],
    username: &str,
    password: &Secret,
) -> Result<(), MailError> {
    let offered: Vec<String> = extensions
        .iter()
        .filter_map(|ext| {
            let mut words = ext.split_whitespace();
            if words.next().is_some_and(|w| w.eq_ignore_ascii_case("AUTH")) {
                Some(words.map(str::to_string).collect::<Vec<_>>().join(" "))
            } else {
                None
            }
        })
        .collect();
    let mechanisms = offered.join(" ").to_ascii_uppercase();
    if mechanisms.contains("LOGIN") {
        let (code, _) = smtp_command(conn, "AUTH LOGIN")?;
        if code != 334 {
            return Err(MailError::Protocol("AUTH LOGIN rejected.".into()));
        }
        let (code, _) = smtp_command(conn, &b64_encode(username.as_bytes()))?;
        if code != 334 {
            return Err(MailError::Auth);
        }
        let mut password_text = String::from_utf8_lossy(password.expose()).into_owned();
        let mut encoded = b64_encode(password_text.as_bytes());
        zero_str(&mut password_text);
        drop(password_text);
        let (code, _) = smtp_command(conn, &encoded)?;
        zero_str(&mut encoded);
        drop(encoded);
        if code != 235 {
            return Err(MailError::Auth);
        }
        return Ok(());
    }
    if mechanisms.contains("PLAIN") {
        let mut material = vec![0u8];
        material.extend_from_slice(username.as_bytes());
        material.push(0);
        material.extend_from_slice(password.expose());
        let mut response = b64_encode(&material);
        zero(&mut material);
        let (code, _) = smtp_command(conn, &format!("AUTH PLAIN {response}"))?;
        zero_str(&mut response);
        drop(response);
        if code != 235 {
            return Err(MailError::Auth);
        }
        return Ok(());
    }
    Err(MailError::Protocol(
        "Server offers no supported AUTH mechanism (need LOGIN or PLAIN over TLS).".into(),
    ))
}

fn send_message_data(
    conn: &mut Conn,
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), MailError> {
    // Body lines are normalized to CRLF and dot-stuffed per RFC 5321 §4.5.2.
    let mut data = String::new();
    data.push_str(&format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\n"
    ));
    data.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
    for line in body.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with('.') {
            data.push('.');
        }
        data.push_str(line);
        data.push_str("\r\n");
    }
    data.push_str(".\r\n");
    conn.stream.write_all(data.as_bytes()).map_err(map_io)?;
    conn.stream.flush().map_err(map_io)?;
    Ok(())
}

// ---------- pure send-approval controller (UI-agnostic) ----------

/// Pure send-approval state machine for the native UI: every state carries an
/// actionable notice so sends never fail silently. Network IO stays in
/// [`approve_send`], run on a worker thread; the UI feeds the outcome back via
/// `finish_*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendStage {
    Idle,
    AwaitingApproval,
    Sent,
    Uncertain,
    Failed,
    Denied,
}

#[derive(Debug)]
pub struct DraftController {
    stage: SendStage,
    pending: Option<PendingDraft>,
    notice: String,
    last_receipt: Option<SendReceipt>,
}

impl DraftController {
    pub fn new() -> Self {
        Self {
            stage: SendStage::Idle,
            pending: None,
            notice: "No draft staged. Compose a message to begin; nothing is sent without fresh approval.".into(),
            last_receipt: None,
        }
    }

    /// Validate + stage a draft for approval. Returns the exact bytes preview.
    /// Restaging first discards any previously staged draft so orphaned
    /// queue entries can never accumulate.
    pub fn stage(&mut self, draft: MailDraft) -> Result<Vec<u8>, MailError> {
        if let Some(old) = self.pending.take() {
            deny_send(old.id);
        }
        match queue_send(draft) {
            Ok(pending) => {
                let preview = pending.bytes().to_vec();
                self.pending = Some(pending);
                self.stage = SendStage::AwaitingApproval;
                self.notice = "Review the exact recipient, subject and body below. Approving sends once; denying discards without sending.".into();
                Ok(preview)
            }
            Err(error) => {
                self.stage = SendStage::Failed;
                self.notice = format!("Draft rejected: {error}");
                Err(error)
            }
        }
    }

    /// The staged draft awaiting approval, if any.
    pub fn pending(&self) -> Option<&PendingDraft> {
        self.pending.as_ref()
    }

    pub fn stage_state(&self) -> &SendStage {
        &self.stage
    }

    /// Current actionable notice (never empty).
    pub fn notice(&self) -> &str {
        &self.notice
    }

    pub fn last_receipt(&self) -> Option<&SendReceipt> {
        self.last_receipt.as_ref()
    }

    /// User approved: caller runs [`approve_send`] on a worker thread, then
    /// feeds the outcome into `finish_sent` / `finish_failed`.
    pub fn take_for_send(&mut self) -> Option<PendingDraft> {
        let pending = self.pending.take()?;
        self.notice =
            "Sending once. The staged approval is consumed; it cannot be approved again.".into();
        Some(pending)
    }

    pub fn finish_sent(&mut self, receipt: SendReceipt) {
        self.notice = format!("Sent. Server replied: {}", receipt.server_reply);
        self.last_receipt = Some(receipt);
        self.pending = None;
        self.stage = SendStage::Sent;
    }

    pub fn finish_failed(&mut self, error: MailError) {
        self.pending = None;
        match &error {
            MailError::Uncertain(detail) => {
                self.stage = SendStage::Uncertain;
                self.notice = format!("Send outcome uncertain: {detail}");
            }
            other => {
                self.stage = SendStage::Failed;
                self.notice = format!("Send failed and nothing was confirmed delivered: {other}");
            }
        }
    }

    /// User denied: discards the queued draft without sending.
    pub fn deny(&mut self) {
        if let Some(pending) = self.pending.take() {
            deny_send(pending.id);
            self.stage = SendStage::Denied;
            self.notice = "Draft discarded. Nothing was sent.".into();
        } else {
            self.notice = "Nothing staged to discard; last send state unchanged.".into();
        }
    }
}

impl Default for DraftController {
    fn default() -> Self {
        Self::new()
    }
}
