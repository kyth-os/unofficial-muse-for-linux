# muse-local-tools API

Linux-only local file capabilities for the **same Muse personal agent**. This crate does not connect to Muse, execute shell commands, use a developer-model API, or depend on a Mac relay. Account access alone is not a filesystem grant.

## Public surface

The agreed names and fields are implemented unchanged:

```rust
#[derive(Clone)]
pub struct ToolBroker { /* shared thread-safe state */ }
impl ToolBroker {
    pub fn new() -> Self;
    pub fn grant(&self, root: &std::path::Path) -> Result<GrantInfo, ToolError>;
    pub fn grants(&self) -> Vec<GrantInfo>;
    pub fn manifest(&self) -> serde_json::Value;
    pub fn invoke(&self, invocation: Invocation) -> Decision;
    pub fn approve(&self, approval_id: &str) -> ToolReply;
    pub fn deny(&self, approval_id: &str) -> ToolReply;
    pub fn revoke_all(&self) -> Vec<ToolReply>;
    pub fn expire(&self) -> Vec<ToolReply>;
}
```

- `GrantInfo { id: String, label: String, root: PathBuf }`
- `Invocation { id: String, command: String, params: Value, timeout_ms: u64 }`
- `Decision::Reply(ToolReply)` or `Decision::Approval(ApprovalRequest)`
- `ToolReply { id: String, ok: bool, output: Value, error: Option<String> }`
- `ApprovalRequest { approval_id: String, invocation_id: String, title: String, path: String, content: String, expires_in_seconds: u64 }`

All listed fields are public. These data types implement `Clone` and `Debug`; do **not** log them: proposals/replies deliberately contain granted user data. `ToolError(pub &'static str)` implements `Display`, `Debug`, and `Error`, with generic codes only. `ToolBroker` also implements `Default`, equivalent to `new()`. No other required API changes.

`new()` obtains a 128-bit nonce with Linux `getrandom`; each proposal independently obtains another 128-bit random approval token, never derived from advertised grant IDs. Entropy failure panics with a generic message rather than permitting reusable/guessable approval IDs. All clones share grants, invocation IDs, pending approvals, and revocation state. IDs are local, opaque capabilities; approval methods must only be wired to the trusted local UI, never exposed as remote commands.

## Permission and results

Calling `grant` is explicit permission to disclose filenames and text within that root to Muse. It must come from a trusted local user action, not a remote invocation. `GrantInfo.root` is **local UI data only**, and must never be sent in registration or tool replies. Generic labels intentionally avoid revealing a folder's real name.

Exact parameter objects (all values must be strings; missing/extra fields are rejected):

| Command | Required parameters | Successful `output` |
|---|---|---|
| `files.search` | `grant_id`, `query` | `{ "paths": ["relative/name"], "scanned": 1, "truncated": false }` |
| `files.read_text` | `grant_id`, `path` | `{ "path": "relative/name", "content": "UTF-8 text" }` |
| `files.create_text` | `grant_id`, `path`, `content` | After approval only: `{ "path": "relative/name", "bytes": 12 }` |

Search is a case-sensitive **regular-file basename substring** search, not content search. Empty query matches every regular filename. Query is at most 256 UTF-8 bytes and cannot contain slash or NUL. Results are relative paths. Complete searches sort paths; bounded/truncated subsets have filesystem traversal order. Limits: 100 returned files, 10,000 scanned entries (excluding dot entries), and 64 directory levels below root. Hitting the result/scan limit yields `truncated: true`; encountering depth overflow, a symlink, a special file, invalid UTF-8 filename, or an I/O error yields failure with no partial results. A truncated scan does not inspect unseen entries.

Reads return only regular UTF-8 files at most 65,536 bytes. Oversized files are rejected, never silently truncated; a bounded 65,537-byte read catches concurrent growth. No binary/base64 mode is provided. Paths must be nonempty UTF-8 relative paths, at most 4096 bytes, with no NUL, empty components, `.` or `..`; absolute paths and trailing/repeated slashes fail closed.

`manifest()` returns `{ "commands_v2": { ... }, "grants": [{ "id": ..., "label": ... }] }`. Each command uses the official gadget SDK's **`description`, `required`, `optional` objects**, with each parameter carrying `type` and `description`, plus `timeout_ms: 120000`. A supplemental strict JSON Schema is under `parameters`. The transport must pass `manifest()["commands_v2"]` as registration `commands_v2`. Each command's required `grant_id` description includes the current ID/label pairs and its enum contains the valid IDs, mirrored into the supplemental schema; discovery therefore works from the registered commands alone. The top-level `grants` array is local supplemental metadata, not a server discovery field. No absolute root path is included. Schema inspected locally: official SDK `linux/src/musegadget/executor.py` and `link_client.py`, source revision provided by parent: `f60347aeb851a1fe9438cfc388721776aa24f275`.

## Approval lifecycle

A valid create invocation always returns `Decision::Approval`, even if the destination already exists or its parent is missing. There is **no filesystem mutation before approval**. The UI must display exact immutable path/content (not an editable substitute) and associate the invocation with its selected grant. The returned proposal is a copy; altering it cannot change what is approved. `approve` accepts only its opaque approval ID, consumes it once, and commits only the stored path/content. No overwrite, parent creation, shell, delete/trash, browser, or arbitrary tool dispatch exists.

Approval deadline starts at invocation entry, uses monotonic time, honors `timeout_ms`, and is capped at 120 seconds. Zero budget immediately fails. `expires_in_seconds` is remaining time rounded upward for UI display; the exact internal deadline remains authoritative. Both `approve` and `expire` enforce expiry; a timer is not required for correctness. Call `expire` regularly to release expired pending memory and deliver their failure replies. `deny` consumes one pending proposal. `revoke_all` clears grants and pending approvals and returns pending failures. `expire`/`revoke_all` reply vectors are sorted by invocation ID. A stale or already-consumed approval returns `stale_approval` with an empty reply ID because there is no remaining invocation to complete.

All invocation IDs, including failed ones, are retained up to 4096 unique IDs for the broker's lifetime. A duplicate returns `duplicate_id` and can never replay a write or create a second approval. On reaching 4096, new IDs fail `capacity`; IDs are **never evicted**, including on revoke. Replace a broker only at an intentional new local session boundary, not as a replay-cache bypass. At most 128 proposals are pending simultaneously. Expired proposals need `expire()` to reclaim their slots. Each content proposal is bounded to 64 KiB.

## Filesystem and concurrency boundary

- Roots are directory-fd capabilities pinned at grant time. Every supplied root component is opened without symlink traversal; remote paths use Linux `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`. No canonicalize-then-open race or user-controlled `/proc` reopening exists. Unsupported kernel operations fail closed.
- Replacing the root pathname with a symlink or different directory cannot redirect the capability. The **original pinned directory inode** remains authorized, including if renamed; `root` is a display reference, not a pathname subsequently reopened. Subdirectory/file descriptors likewise refer to the inodes securely opened beneath that capability. This is inode-capability confinement, not a sandbox against a same-UID process that can rearrange authorized inodes, existing hard links, or mounts. A grant must not include untrusted mounted filesystems if those are outside the user's intended scope.
- Reads use nonblocking opens and verify regular-file metadata before reading; FIFOs/devices/directories are not read. Recursive search enumerates fd-owned directory streams and uses `fstatat(..., AT_SYMLINK_NOFOLLOW)`, then reopens descendants beneath the pinned root.
- After approval, staging uses an anonymous owner-only `O_TMPFILE` inode inside the already-existing destination parent. Its complete written bytes are synced and read back for exact comparison. A single `linkat(..., AT_EMPTY_PATH)` publishes that verified inode with **no replacement**. Existing names, including symlinks/FIFOs, cause failure. Closing the anonymous inode cleans failed/expired/revoked staging without named debris. Filesystems lacking `O_TMPFILE`/link support fail `create_denied`; no weaker fallback is attempted. Successful new files have mode 0600 (or stricter if the process umask removes bits).
- Slow reads, search, grant acquisition, and staging occur outside the shared state lock. Completion rechecks the captured authorization epoch/deadline under that lock. Mutation holds the same lock across the no-replace link commit so revoke cannot win authorization first and still permit a later commit. In-flight grants are also invalidated if revoke wins first. Anonymous staged inodes are discarded on invalidation.
- Completion/revocation are linearized at the final state-lock check/commit. Results already returned to a caller cannot be recalled; the transport must still drop queued replies on disconnect/revoke and must not send an already-buffered reply after that boundary. A revoke-winning in-flight operation returns `revoked` without its data. An in-flight `approve` already removed from pending returns its own failure to its caller rather than appearing in `revoke_all()`'s drained vector.
- Timeouts prevent stale output/commit, not hard cancellation of a blocking filesystem syscall. A hung network filesystem can delay the caller; it does not hold the state lock during reads/staging. No remote user can opt into shell execution or broader paths.

The crate emits no logs and never includes OS error text, private root paths, or file content in error codes. Error replies have `output: null`. Codes include `invalid_id`, `duplicate_id`, `capacity`, `unknown_command`, `invalid_params`, `not_granted`, `invalid_path`, `io_denied`, `special_file`, `too_large`, `invalid_utf8`, `depth_limit`, `create_denied`, `verify_failed`, `expired`, `denied`, `revoked`, and `stale_approval`.

## Transport obligations and verification

Local `ToolReply.output` is intentionally not renamed; the gateway transport maps it to its wire `payload`. The transport must bound the **encoded** JSON control message (256 KiB), not just raw text: 64 KiB of control characters can JSON-escape above the wire cap. No live gateway integration is claimed by this crate.

All tests use disposable scratch fixtures. Verified slices included real RED failures for missing read/approval/lifecycle/manifest APIs, `files.search` returning `unknown_command`, approval IDs colliding across independent brokers, and an in-flight grant surviving revoke. GREEN then exercised real reads, anonymous staging/link, symlink/root replacement confinement, no-overwrite, exact UTF-8 byte limits, search bounds, strict input validation, duplicate/capacity policy, and deterministic barrier-controlled revoke races. Test-only barriers do not exist in production builds.

Run within the assigned worktree, using the established builder mounts/environment:

```sh
cargo fmt -p muse-local-tools -- --check
cargo test -p muse-local-tools --locked
cargo clippy -p muse-local-tools --all-targets --locked -- -D warnings
```
