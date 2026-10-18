# muse-calendar — API

Generic CalDAV connector (any provider, no OAuth): read + local draft +
fresh per-create approval. No UI. Sync + threads (`Mutex`-guarded pending
store, `Arc` transport).

## Frozen contract surface

```rust
pub struct CalendarAccount { pub host: String, pub port: u16, pub username: String, pub base_path: String }
impl CalendarClient {
    pub fn new() -> Self;  // production: TLS-only
    pub fn calendars(&self, account: &CalendarAccount, password: &str) -> Result<Vec<CalendarInfo>, CalError>;
    pub fn events(&self, account: &CalendarAccount, password: &str, calendar_path: &str,
                  from_utc: i64, to_utc: i64) -> Result<Vec<EventSummary>, CalError>;
    pub fn event_detail(&self, account: &CalendarAccount, password: &str, calendar_path: &str,
                        event_href: &str) -> Result<EventDetail, CalError>;
    pub fn queue_create(&self, draft: CalendarDraft) -> Result<PendingCreate, CalError>;
    pub fn approve_create(&self, account: &CalendarAccount, password: &str,
                          create: &PendingCreate) -> Result<CreateReceipt, CalError>;
    pub fn deny_create(&self, create: &PendingCreate) -> bool;
}
pub struct CalendarInfo { pub path: String, pub name: String }
pub struct EventSummary { pub id: String, pub uid: String, pub title: String,
    pub start_utc: i64, pub end_utc: i64, pub all_day: bool }
pub struct EventDetail { pub href: String, pub uid: String, pub title: String,
    pub start_utc: i64, pub end_utc: i64, pub all_day: bool, pub description: String }
pub struct CalendarDraft { pub calendar_path: String, pub title: String,
    pub start_utc: i64, pub end_utc: i64, pub description: String }
pub struct PendingCreate { pub id: u64 /* + private integrity tag */ }
pub struct CreateReceipt { pub href: String, pub etag: Option<String> }
```

Deviations/clarifications vs the brief (all additive, no renames):

* `approve_create` / `deny_create` take `&PendingCreate` (the `create_id` token)
  rather than a bare `u64`: the token carries a private integrity tag so a
  forged or mixed-up id fails closed with `TamperedDraft`. `pending.id`
  remains the plain id.
* `event_detail` requires `event_href` to be the calendar itself or an
  object directly beneath it (segment-boundary match): sibling prefixes
  (`/a` vs `/ab/x`), empty scopes, `..`, query and fragment forms are
  rejected with `InvalidArgument` before any request.
* Account/draft fields are validated up front: hosts reject userinfo
  markers, whitespace, controls and path characters; usernames and all
  paths reject control/CRLF characters; paths must be absolute with no
  `..`, `?` or `#`. Violations fail with `InvalidArgument`/`InvalidDraft`
  and no request is sent.
* `CalError` adds `InvalidArgument`, `TlsRequired`, `Transport`, `Http(u16)`
  alongside the specified `Auth/NotFound/Conflict/Uncertain/TamperedDraft/
  UnknownCreate/InvalidRange/InvalidDraft/Malformed/TooLarge/
  UnsupportedTimezone/Discovery`.
* `CalendarClient::with_transport` is `#[doc(hidden)]`, test-only
  (plaintext loopback). Production `new()` is https-only, enforced in
  `ReqwestTransport` (any non-https URL is refused before socket use),
  never follows redirects (`Policy::none`, so a 302 cannot downgrade to
  http or carry credentials elsewhere; 3xx surfaces for the caller),
  and streams response bodies through an 8 MiB transport cap
  (declared lengths are pre-checked, undeclared streams use `take`).

## Bounds enforced

| Rule | Value | Failure |
|---|---|---|
| `events` range | ordered, ≤ 31 days | `InvalidRange`, no request sent |
| `events` results | ≤ 200 (excess dropped) | — |
| `event_detail` body | ≤ 64 KiB | `TooLarge` |
| discovery PROPFIND body | ≤ 256 KiB each, 3 requests | `TooLarge` / `Discovery` |
| REPORT body | ≤ 4 MiB | `TooLarge` |
| draft fields | each ≤ 4 KiB, title non-empty, path absolute | `InvalidDraft` |
| draft times | end > start, duration ≤ 24 h | `InvalidDraft` |
| PUT | `If-None-Match: *` only; 412 → `Conflict`, never overwrite/retry | `Conflict` |
| PUT transport error | any failure after issue → `Uncertain`, entry already consumed | `Uncertain` |

Approval binding: `queue_create` stores an FNV-1a hash over
`calendar_path ␀ title ␀ start ␀ end ␀ description`; `approve_create`
re-checks tag + hash first and fails closed with `TamperedDraft` on
mismatch (the staged entry is kept, so a forged guess cannot burn the
legitimate draft), then consumes the entry so replays and retries are
impossible.

## Timezones

UTC (`Z`), numeric offsets (`±HHMM[SS]`), floating (= UTC), `TZID` limited
to UTC aliases and `GMT/UTC±H[MM]`. Any other named `TZID`
(`America/New_York`, …) → `UnsupportedTimezone`, fails closed. Named-zone
support needs a tz database (not bundled); see Gaps.

## Credentials

Passwords travel only as `&str` call args into the `Authorization` header.
Never stored, never logged, never rendered in `Display`/`Debug`. `Debug`
for `CalendarAccount` is hand-written. `401/403` → `Auth` with a static
message.

## Dependencies (no new crates vs tree)

* `reqwest 0.13.5` (`blocking`, `rustls`) — already used by
  `muse-meta-api` with the same feature set; sync style matches the tree.
  iCalendar/XML parsing is hand-rolled std-only (`ical.rs`, `xml.rs`;
  ~1000 lines) precisely to avoid pulling `ical`/`minreq`/`chrono` crates.
  Rejected alternative: `ureq` (would add a second HTTP stack) and
  `chrono`/`chrono-tz` (heavy for three datetime forms; reconsider when
  named-TZID support is required).

## Tests

`cargo test` (offline fake CalDAV server on 127.0.0.1, real socket I/O):
18 integration (`tests/caldav.rs`) + 23 unit. Covers discovery,
auth redaction, range/result bounds, VTODO/VJOURNAL ignored, malformed iCal,
oversize detail, draft validation, PUT-once + `If-None-Match`, replay,
deny-silence, 412, uncertain-hangup, forged/corrupted approval binding
(including forged-guess-keeps-draft), href segment confinement, control-field
and epoch-extreme rejection without network, streaming caps, non-char-boundary
inputs, duration overflow/sign, timezones (unit), XML edge cases (unit).

## Honest gaps

* Live TLS against a real server is untested (fake is plaintext loopback;
  only URL-scheme rejection is unit-tested). Needs a staging account.
* No OAuth (per scope), no invites/attendee email, no iTIP scheduling.
* No recurrence expansion (bounded instances out of scope); recurring
  `VEVENT`s return their `DTSTART`/`DTEND` as-is.
* Named timezones fail closed (no tz db). VALARM/VTIMEZONE ignored.
* No shared/delegated calendars beyond principal → home-set discovery.
* `events` silently truncates past 200 instead of paging (no `truncated`
  flag in the frozen signature).
