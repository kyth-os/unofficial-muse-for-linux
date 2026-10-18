# muse-mail API

Generic IMAP-read + approval-gated SMTP-send connector. Account-agnostic
(any provider via explicit host/port/username), no OAuth, no credential
persistence, no background sync.

## Frozen contract (names kept)

```rust
pub struct MailAccount { pub host: String, pub port: u16, pub username: String }
impl MailClient {
    pub fn connect_imap(account: &MailAccount, password: &str) -> Result<Vec<FolderStat>, MailError>;
    pub fn list(account: &MailAccount, password: &str, folder: &str, limit: u32) -> Result<Vec<Envelope>, MailError>;
    pub fn read_text(account: &MailAccount, password: &str, folder: &str, uid: u32) -> Result<MessageText, MailError>;
}
pub struct MailDraft { pub to: String, pub subject: String, pub body: String }
pub fn queue_send(draft: MailDraft) -> Result<PendingDraft, MailError>;
pub fn approve_send(account: &MailAccount, password: &str, draft: &PendingDraft) -> Result<SendReceipt, MailError>;
pub fn deny_send(id: u64);
```

Supporting types: `FolderStat { name, total, unseen }`,
`Envelope { uid, from, subject, date }` (headers only),
`MessageText { uid, text, truncated }`,
`PendingDraft { id, bytes() }` (exact bytes bound to the approval),
`SendReceipt { draft_id, accepted, server_reply }`,
`MailError::{Validation, Auth, Connection, Timeout, TlsRequired, Protocol,
TooLarge, HtmlOnly, NotFound, Tampered, Uncertain}`.

## Diffs from the frozen sketch (additive, justified)

- `approve_send` takes `&PendingDraft` instead of a bare `draft_id`. The
  draft carries the id **plus the exact bytes**, so the FNV-1a hash + byte
  binding can be compared at send time. A bare id alone cannot detect
  tampering. `deny_send(id: u64)` keeps the frozen bare-id shape.
- Added test-only hooks with explicit transport policy, never used by
  production paths: `MailClient::{connect_imap_via, list_via, read_text_via,
  approve_send_via}`, `TlsMode::{Require, LoopbackPlaintext}`,
  `SmtpEndpoint`, `smtp_endpoint_for`. `LoopbackPlaintext` still refuses
  non-loopback endpoints before dialing.
- Added `DraftController` + `SendStage`: the pure, UI-agnostic
  stage → approve/deny state machine with an actionable notice in every
  state. The GTK view (`apps/desktop/src/mail_ui.rs`) is a thin shell over it.

## Rules enforced

- `MailDraft::new`: one exact `local@domain` recipient (single `@`, no
  spaces/controls, total ≤ 320 bytes; single-label domains allowed so
  loopback fixtures work), non-empty subject ≤ 998 bytes, non-empty body ≤
  64 KiB. Plain text only; attachments rejected by construction (no API).
- `list` limit is 1..=50; folder names must be non-empty without line breaks.
- `read_text`: plain-text part preferred (first `text/plain` in MIME order,
  `quoted-printable`/`base64` decoded, other charsets lossy-UTF-8);
  HTML-only (or no plain part) → `MailError::HtmlOnly`; literals over
  64 KiB → `MailError::TooLarge` before reading further. `truncated` is
  reserved for partial fetch and currently always false.
- Approval binding: unknown/consumed id → `NotFound`; presented bytes
  hash/byte mismatch → `Tampered`, fails closed before any socket opens, and
  the queued entry is kept. On a passing bind the entry is consumed **before**
  network IO, so a second approval (or any retry through this API) fails with
  `NotFound`: at most one send per approval, never silent double-send.
- SMTP sends only inside `approve_send_via`. A dropped link after DATA with
  no `250` → `MailError::Uncertain`, never auto-retried. `5xx` on RCPT/DATA
  → `Validation` (server refused; nothing confirmed delivered).
- SMTP endpoint: same host, port 587 + mandatory STARTTLS, or port 465 +
  implicit TLS when the IMAP port is 465.

## Credentials & transport

- Passwords are per-call `&str`, copied into a `Secret` buffer used only for
  the LOGIN/AUTH handshake, then zeroed with volatile writes on drop. The
  IMAP LOGIN line and the SMTP AUTH LOGIN/PLAIN credential buffers are
  likewise built as raw byte vectors and zeroed after the write (verified
  by code; heap-dump observation is out of scope). No struct stores a
  password, so stock derived `Debug` is safe. Passwords never
  appear in errors, logs, or server transcripts (verified by the
  auth-failure redaction test).
- TLS only: implicit TLS on 993/465, else STARTTLS with system cert
  verification (`native-tls`). Missing STARTTLS → `TlsRequired`; there is no
  plaintext fallback. Timeouts: 10 s connect, 30 s read/write.

## Known gaps (not claimed)

- Live-TLS interop (real 993/587 handshake, SNI, STARTTLS upgrade) is
  implemented but exercised only by code path, not by test: the offline suite
  proves the TLS-required refusal against a plaintext fixture, not a real
  handshake. Needs an integration run against a local TLS fixture or a real
  provider before any live-account claim.
- No OAuth (out of scope), no attachments, no HTML compose/render, no
  background sync/polling, no RFC 2047 encoded-word decoding (raw header
  values shown), no non-ASCII Subject encoding (UTF-8 passthrough).
- `MessageText.truncated` flags MIME-decoded bodies cut at the 64 KiB cap;
  the native UI labels truncated reads explicitly. Raw FETCH literals over
  the cap fail closed with `TooLarge` before buffering.

## Dependencies (need parent approval at merge)

- `native-tls 0.2` — TLS + system cert verification without vendoring a TLS
  stack. Everything else (IMAP/SMTP/MIME/base64/QP) is hand-rolled `std` so
  no `imap`/`lettre`/`mailparse` deps are needed. Rationale vs the tree's
  reqwest+rustls stack: IMAP/SMTP need a raw TLS *stream* (STARTTLS
  upgrade mid-connection), which reqwest does not expose; driving rustls
  directly would add a second hand-rolled handshake on top of the existing
  one. `native-tls` uses the OS trust store (consistent with a desktop
  client honoring system CAs) and resolves to a single `openssl 0.10.81`
  in `Cargo.lock`. It builds in the Debian bookworm builder with no extra
  system packages. Revisit only if the tree ever needs a uniform TLS
  implementation.
