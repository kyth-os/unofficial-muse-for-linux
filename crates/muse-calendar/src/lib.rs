//! Generic CalDAV connector: read + approval-gated event creation.
//!
//! * No OAuth. HTTP Basic over TLS only (production transport).
//! * Passwords are `&str` per call: never stored, never logged, never
//!   embedded in errors.
//! * Event creation is a two-step gate: [`CalendarClient::queue_create`]
//!   validates and stages a draft locally (no network), and
//!   [`CalendarClient::approve_create`] performs exactly one `PUT` with
//!   `If-None-Match: *`. The approval is bound to the exact staged bytes;
//!   any mismatch fails closed.

mod client;
mod ical;
mod transport;
mod xml;

pub use client::{
    CalError, CalendarAccount, CalendarClient, CalendarDraft, CalendarInfo, CreateReceipt,
    EventDetail, EventSummary, PendingCreate, MAX_EVENT_BYTES, MAX_RESULTS,
};
pub use transport::{HttpTransport, LoopbackTransport, ReqwestTransport};
