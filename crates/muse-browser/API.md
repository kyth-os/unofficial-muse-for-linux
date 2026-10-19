# `muse-browser` API

Native-messaging-host protocol core for the browser tab bridge.
`std`-only, zero dependencies. No network, no credentials, no content
reading — tab metadata (`id`, `title`, `url`) only.

## Entry points

```rust
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024; // 1 MiB, fail-closed
pub const NATIVE_HOST_NAME: &str = "org.unofficial_muse_for_linux";

pub fn handle_message(body: &[u8], backend: &dyn TabBackend) -> Vec<u8>;
pub fn run_loop<R: io::Read, W: io::Write>(
    reader: &mut R, writer: &mut W, backend: &dyn TabBackend,
) -> io::Result<()>;
pub fn frame(payload: &[u8]) -> Vec<u8>;
```

* `handle_message` takes the raw message **body** (bytes after the 4-byte
  length prefix) and returns the full framed reply
  (4-byte little-endian length + UTF-8 JSON). Pure — tests inject a fake
  backend and assert exact reply bytes.
* `run_loop` owns the stdio framing: reads `u32LE len + body` records until
  clean EOF. A declared length above `MAX_MESSAGE_BYTES` gets one
  `too_large` reply and the loop **stops** (fail-closed: the body is not
  drained, so a hostile length prefix cannot force unbounded reads).
  Truncated header/body stops the loop with no reply.
* `frame` prefixes any payload with its `u32` little-endian length.

## Backend

```rust
pub struct TabInfo { pub id: u64, pub title: String, pub url: String }
pub struct BackendError { pub detail: &'static str } // static-only detail

pub trait TabBackend {
    fn list_tabs(&self) -> Result<Vec<TabInfo>, BackendError>;
    fn open_tab(&self, url: &str) -> Result<TabInfo, BackendError>;
    fn close_tab(&self, id: u64) -> Result<(), BackendError>;
}
```

The real browser side implements `TabBackend`; tests fake it.
`BackendError.detail` is `&'static str` so request bytes can never leak back
through an error reply.

## Request schema (strict — unknown ops/fields rejected)

```json
{"id": 7, "op": "tabs.list", "params": {}}
{"id": 8, "op": "tabs.open",  "params": {"url": "https://example.com/"}}
{"id": 9, "op": "tabs.close", "params": {"id": 2}}
```

* Top level must be an object with **exactly** `id`, `op`, `params`.
  Anything else (missing keys, extra keys, duplicate keys, non-object)
  is `invalid_request`.
* `id` must be an unsigned integer fitting `u64` (no strings, floats,
  negatives, or overflow). It is echoed in every reply whose request
  carried a usable one; otherwise the reply uses `"id": null`.
* `op` must be one of `tabs.list` / `tabs.open` / `tabs.close`.
  Unknown string ops are `unsupported_op`; non-string ops are
  `invalid_request`.
* `params` must be an object: `{}` for `tabs.list`, `{"url": …}` for
  `tabs.open`, `{"id": …}` for `tabs.close`. Extra params fields are
  rejected; `tabs.close` requires a `u64` tab id.

### URL rules (`tabs.open`)

Reimplemented small from the desktop crate's OpenURI validation (no
cross-crate dependency): non-empty, ≤ 2048 bytes, no whitespace/control
bytes, an `http`/`https` scheme (case-insensitive) followed by `://`,
and a non-empty authority with no userinfo (`@`). This rejects
`javascript:`, `file:`, `data:`, `chrome:`, `about:` and schemeless
strings. Any violation is `invalid_request` with the static detail
`"url rejected"` — the submitted URL is never echoed.

## Replies

Success: `{"id":7,"ok":true,"result":{"tabs":[{...}]}}`
(`tabs.open` nests the tab as `{"tab":{...}}`; `tabs.close` returns `{}`.)

Error: `{"id":7,"ok":false,"error":{"code":"…","detail":"…"}}`

| `code`           | When                                                        |
| ---------------- | ----------------------------------------------------------- |
| `invalid_request`| malformed JSON/UTF-8, schema violation, bad URL, bad tab id |
| `unsupported_op` | unknown string `op`                                         |
| `backend_error`  | the `TabBackend` failed (`detail` = its static string)      |
| `too_large`      | body (or declared length) above 1 MiB                       |

## Worked framing example

Request body (29 bytes):

```
{"id":1,"op":"tabs.list","params":{}}
```

stdin bytes (hex): `1d 00 00 00` + the UTF-8 body (`0x1d` = 29).
A two-tab reply body is framed the same way: `u32LE(len) + JSON`.

## Explicit non-goals

No tab-body/content API by design, no cookies/history/bookmarks APIs,
no form filling, no per-action remote approval protocol. Every action in
the companion extension originates from an explicit popup click;
see `docs/browser-tabs.md`.
