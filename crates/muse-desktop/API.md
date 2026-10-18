# muse-desktop — portal screen-context + OpenURI

Two operations against the XDG Desktop Portal (`org.freedesktop.portal.Desktop`),
both consent-gated by the portal itself. No background capture, no persistence,
no approve/deny store in this crate — the approval UI is the parent's job.

## API (frozen)

```rust
DesktopPortal::screenshot_via(bus: &BusEndpoint, token: &str) -> Result<Screenshot, DesktopError>
DesktopPortal::open_url_via(bus: &BusEndpoint, url: &str) -> Result<(), DesktopError>
```

- `BusEndpoint::unix_socket(path)` — real session bus (e.g. `$XDG_RUNTIME_DIR/bus`).
- `BusEndpoint::loopback_tcp(port)` — test fakes only; dials `127.0.0.1`, never a remote host.
- `Screenshot { width: u32, height: u32, format: String, bytes: Vec<u8> }`
  (`format` is `"png"` when the payload parses as PNG, else `"unknown"` with zero dims;
  bytes are always returned).
- `DesktopError`: `PermissionDenied` · `Cancelled` · `UnsupportedPortal(String)` ·
  `TooLarge` · `Transport(String)` · `InvalidArgument(String)`.

## Protocol

Minimal std-only D-Bus wire client (little-endian, `AUTH EXTERNAL` with the uid
read from `/proc/self/status` on Linux): `Hello` → `AddMatch` (broad
signal subscription, pre-call to avoid a reply/signal race) → portal method call
→ wait for `org.freedesktop.portal.Request.Response(u response, a{sv} results)`
filtered by request path, 20 s deadline:

- `Screenshot("", {"handle_token": token})` → reply is a request path; on
  response code 0 the `uri` result must be a `file://` URI, read one-shot and
  capped at `MAX_SCREENSHOT_BYTES` (8 MiB, fail closed → `TooLarge`).
- `OpenURI("", url, {})` → on code 0 returns `Ok(())`; results are never read
  (no response scraping).

## Error mapping

| Portal outcome | `DesktopError` |
|---|---|
| Response code 1 (user dismissed) / `...Error.Cancelled` | `Cancelled` |
| Response code ≠ 0/1, `...NotAllowed`, `...AccessDenied`, `...PermissionDenied`, `...NotAuthorized` | `PermissionDenied` |
| `...UnknownMethod`, `...UnknownInterface`, `...UnknownObject`, `...ServiceUnknown`, `...NameHasNoOwner`, `...NotSupported` | `UnsupportedPortal` |
| Screenshot over 8 MiB (declared or actual) | `TooLarge` |
| Non-`http(s)` URL, bad token, malformed input | `InvalidArgument` |
| Anything else (I/O, framing, timeout, non-`file:` screenshot URI, non-regular screenshot file) | `Transport` |

Matching is on the last dotted segment with equality, never substrings: a
custom `org.example.NotCancelledEver` is `Transport`, not `Cancelled`.

Denial/cancel is always a typed error — there is no fallback capture path.
X11 SHM, compositor bypass, and global input are explicitly out of scope and
are not implemented.

## Redaction

URLs are caller-supplied and echoed only in the portal approval binding, never
logged. Error variants carry static reasons, never the URL, token, bus path, or
screenshot location.

## Validation

- URL: ≤ 2048 chars, no whitespace/control bytes, scheme `http`/`https`
  (case-insensitive) with a non-empty authority and no `@`. Everything else
  (`file:`, `javascript:`, `data:`, …) is rejected **before** any bus traffic.
- Token: 1–255 chars, `[A-Za-z0-9_]` only (it is embedded in the request path,
  where `-` etc. are illegal).

## Verification gap (honest)

Covered by loopback fakes that speak the real D-Bus wire framing (auth, header
alignment, reply-serial matching, error names, Response signals), so framing
bugs are caught. **Not** covered: interop with a live `xdg-desktop-portal`
(no portal runs in this environment), compositor consent dialogs, and real
screenshot payload shapes beyond PNG IHDR sniffing. Promoting this crate to
production use requires a live-portal run (GNOME/KDE session) and parent review
of the std-only framing versus adopting `zbus` (new dependency, needs parent
approval at merge). Zero dependencies were added; `cargo test -p muse-desktop`
resolves once the parent adds `crates/muse-desktop` to workspace `members`.
