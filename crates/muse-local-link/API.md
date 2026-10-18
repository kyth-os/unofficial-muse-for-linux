# Account-linked local-host transport

Status: implemented and tested with fake/loopback-only helpers and the selected
community runtime's real protobuf classes. **Not authenticated, live-registered,
or proven accepted by a personal-account `/link-control` endpoint.** Nothing
starts automatically. No gadget token, browser, authentication flow, command
executor, raw route interface, reconnect, or replay is provided here.

## Rust API

```rust
pub struct HostConfig {
    pub python: std::path::PathBuf,
    pub script: std::path::PathBuf,
    pub config_dir: std::path::PathBuf,
    pub node_id: String,
    pub display_name: String,
    pub commands: serde_json::Value,
}

HostSession::spawn(config: HostConfig) -> Result<HostSession, HostError>;
HostSession::try_recv(&self) -> Result<HostEvent, std::sync::mpsc::TryRecvError>;
HostSession::send_result(&self, id: &str, ok: bool,
                        output: serde_json::Value, error: Option<String>)
    -> Result<(), HostError>;
HostSession::stop(&self); // nonblocking and idempotent
// Drop invokes stop; it never joins an IO thread.

pub enum HostEvent {
    Connecting,
    Registered,
    Invoke(HostInvocation),
    Closed { code: String },
}
pub struct HostInvocation {
    pub id: String,
    pub command: String,
    pub params: serde_json::Value,
    pub timeout_ms: u64,
}
pub struct HostError { pub code: &'static str }
```

All requested API signatures are preserved. `spawn` validates only in memory and
starts a worker; unavailable executables, pipe errors, and helper failure arrive
as `Closed`, not synchronous spawn errors. The UI must retain/poll the session
and must not treat successful `spawn` as a connection or registration.

`send_result` accepts an outstanding unexpired ID exactly once. It uses a
nonblocking `try_lock` and bounded `try_send`; `busy`/`queue_full` mean nothing was
queued and do not consume the ID. An accepted response is queue acceptance, **not
remote delivery**. Do not replay an accepted response after failure. Oversized
local responses return `request_too_large` without consuming the ID. Send a small
safe error instead. Rust never logs helper output or stderr.

`Closed` supersedes queued invocations, is returned exactly once, then
`try_recv` returns `Disconnected`. During cancellation, pending event delivery is
suppressed; invocations that expired while queued are discarded before dispatch.
Both receive and result paths use nonblocking `try_lock`. Stop the broker's outstanding work on closure/disconnect; still apply
its independent permissions, schemas, consent, and cancellation checks before any
local operation. Transport allowlisting alone does not authorize execution.

The worker owns spawn, nonblocking stdin/stdout, graceful SIGTERM, process-group
SIGKILL, and reaping. Linux `PR_SET_PDEATHSIG=SIGKILL`, with the parent race check,
prevents an abrupt GUI/worker death from leaving an active host process. The
Python sidecar never starts child processes. Subprocess environments match the
account bridge's allowlist: provider keys, Python overrides, AppImage/loader
variables, and arbitrary MUSE overrides are removed; `-I` and
`MUSE_MCP_ENABLE_RAW=0` are explicit.

## Sidecar pipe protocol

Run the selected community interpreter with `-I` and the absolute sidecar path.
Send newline-delimited JSON on persistent stdin:

```json
{"op":"start","config_dir":"/absolute/private/config","node_id":"persisted-linux-node","display_name":"Linux","commands_v2":{"safe.read":{"description":"Permission-gated read","required":{"path":{"type":"string","description":"Granted relative path"}},"optional":{}}}}
{"op":"result","id":"server-invocation-id","ok":true,"output":{"text":"result"}}
{"op":"result","id":"another-server-id","ok":false,"output":null,"error":"permission_denied"}
{"op":"stop"}
```

`start` is mandatory and once only. Unknown operations, unknown extra start/result
fields, duplicate JSON keys, nonobjects, NaN, malformed input, unknown command
names, duplicate IDs, invalid timeouts, and responses without outstanding IDs
fail closed. Params must be objects. Command values are SDK `description` /
`required` / `optional` manifests, **not generic JSON Schema**. The transport
passes manifest objects unchanged; the separate Rust broker owns detailed
manifest construction and parameter validation.

Stdout contains only bounded JSON lines:

```json
{"type":"state","state":"connecting"}
{"type":"state","state":"registered"}
{"type":"invoke","id":"i","command":"safe.read","params":{},"timeout_ms":30000}
{"type":"state","state":"closed","code":"stopped"}
```

Terminal states may instead be `auth_required` or `error`, with a safe code.
Dependency Python/native stdout and stderr are redirected to `/dev/null`; a saved
nonblocking descriptor carries only pipe events. No traceback, credentials,
account identifier, tool result, or private error is logged. Tool arguments occur
only in the Rust-owned invocation pipe, not diagnostic output.

EOF, `{ "op": "stop" }`, SIGTERM and SIGINT shut down. The first signal ignores
subsequent termination signals before cleanup. Network connect/send/read work is
off the control thread; bounded queues fail closed rather than dropping work or
blocking cancellation. Auth construction that cannot be interrupted remains a
daemon thread, exits with the sidecar, and cannot send registration after stop.

## Wire compatibility

Inspected primary SDK revision: `f60347aeb851a1fe9438cfc388721776aa24f275`.
No SDK source is copied or vendored. Relevant primary files are
`linux/src/musegadget/link_client.py`, `noise/transport.py`, and `executor.py`.
The installed community `muse.py` and `muse_mcp/client.py` were inspected as source
only; no saved authentication/configuration was read for development.

* Reuse `scripts/muse_account_bridge.py` via an exact sibling-path importlib
  import, which also works under `python -I`. Reuse its `secure_config`, sealed
  HttpOnly memfd adaptation, and `community_client`; no duplicate auth logic.
* Open an exclusively owned `client._gateway(config)` session, overriding the
  in-memory node ID with `HostConfig.node_id`; do not modify saved settings.
* The actual installed counter is `Gateway.stream`, not `stream_id`. Consume one
  counter value; use real `muse.ServiceFrame` protobuf fields `request`,
  `body_chunk`, `response`, `reset`, and `stream_id`.
* Route through `muse.SERVICE_IDS["daemon"]` (currently **0**). No guessed route
  table insertion or silent numeric fallback. `_send_envelope` constructs the
  ServiceRequest/Noise envelope. Its existing lock encloses encryption **and**
  socket send, preserving nonce order. Exactly one `_read_frame` reader exists.
* Send a **headerless** `POST /link-control` application request with empty body
  and `end_body=False`, matching the public SDK's control-stream open. Do not
  invent an `x-app-id` or introduce any new auth headers. The account Gateway's
  existing authenticated WebSocket/Noise transport is reused unchanged.
* Body chunks have `end_body=False`; each contains little-endian u32-prefixed
  JSON. Register with `{type:"req", id:uuid, method:"link.register", params:...}`.
  Params include node/display name, `platform:"linux"`, `version:"0.1.0"`,
  `device_family:"homehub"`, `model_id:"linux"`,
  `is_wakeup_supported:false`, and `commands_v2`.
* `Registered` requires **both HTTP 2xx and the matching nonerror registration
  acknowledgement**. Neither a successful socket write nor a successful account
  read proves host registration. HTTP 401 yields `auth_required`, HTTP 403 yields
  `link_forbidden`, other HTTP or ack rejection yields `link_rejected`.
* `link.invoke` is a top-level method with `id`, `command`, `params`, `timeout_ms`.
  Local `output` is translated to wire **`payload`**, not `output`.
  Safe failures use `ok:false,error:"safe_code"`. The complete encoded wire
  message, including four-byte prefix, must fit 256 KiB. JSON escape inflation
  produces a small once-only `result_too_large` failure without private output.
* `link.unpaired` / `node.unpaired`, reset, or EOF are terminal. Never reconnect
  or replay automatically. A personal-token rejection is surfaced, never
  substituted with a gadget token.

### Idle behavior and unresolved live integration

The selected community Gateway connects with `timeout=20`. Inspection of its
installed curl_cffi `WebSocket.recv` shows indefinite AGAIN/select polling after
connection, **not a 20-second established idle deadline**. The sidecar installs a
bounded fragment assembler into that socket's `recv` before control-stream IO;
it retains idle waiting, bounds encrypted-frame assembly, and leaves the single
community Noise reader in place. Timeout exceptions become `transport_timeout`,
never guessed authentication failure. Peer close/reset still terminates. There
is no keepalive/reconnect policy beyond libcurl's existing ping/pong handling.

Actual personal-account authorization for this route and required server-side
app/device entitlements remain unverified. Header/service shape above is
source-derived and protobuf-tested, not a claim of an observed live exchange.
External auth/handshake stays owned by the community library. Its Noise envelope
reader's upstream limitations (including any future multipart envelope protocol
drift) are not represented as live compatibility proof.

The SDK's chat sender includes `device_id` equal to the registered `node_id` to
route follow-up commands to that device. Existing account bridge chat sends only
`node_id` and no `device_id`. **Parent integration must reconcile this:** persist
one explicitly chosen node ID, coordinate the account send path and registered
host identity, and verify device attribution with user authorization later.
This lane does not edit or silently enable account chat sends.

## Limits

| Resource | Bound / behavior |
| --- | --- |
| Wire JSON + u32 prefix | 256 KiB, including escaping |
| Python stdin / decoder buffer | 256 KiB each; oversize fails closed |
| Encrypted websocket assembly | 320 KiB after external handshake |
| Decoded application frame | 256 KiB + 8 KiB envelope allowance |
| Messages per decoder feed | 64 |
| Command manifest | 64 named object entries, bounded serialized start |
| Node/display/command/invoke ID | 128 characters in Python; Rust byte limit is stricter |
| Outstanding invocations | 8 |
| Seen invocation IDs per session | 4096, retained against replay; then close |
| Invocation timeout | 1..120000 ms; absent server timeout defaults to 30000 ms |
| Rust result/event queues | 32 entries each |
| Python incoming/outgoing queues | 16 entries each |
| Initial registration | Python 30 s; outer Rust 35 s |
| Pipe backpressure | Rust write 500 ms; Python output 500 ms |
| Rust cancellation grace | 400 ms, then process-group kill + reap on worker |
| Python cleanup joins | Up to 200 ms each for socket close and network worker |

Python expires pending IDs and emits one safe timeout result; late/duplicate
caller responses are rejected. Rust separately validates deadlines/IDs. Rust's
capped serializer bounds allocated pipe-output bytes before sending. Python's
bounded input size also bounds JSON escape expansion before its wire-length
check. Pending IDs can be accepted by Rust then expire before remote delivery;
that is delivery uncertainty, not permission to replay.

## Parent integration / packaging (not changed here)

1. Add `muse-local-link` to the reconciled workspace and dependent desktop/broker
   crates. This isolated worktree adds only this crate to Cargo.toml/Cargo.lock.
2. Package `muse_local_host.py` **alongside** `muse_account_bridge.py`; use the
   selected external community interpreter and existing private config directory.
   Do not run it with the bundle's plain stdlib interpreter if it lacks the
   user-selected external community runtime.
3. Connect explicit host consent to spawn; connect disconnect/window-close to
   `stop`; consume `Closed` even when a queue was saturated. Do not expose
   Registered before the event actually arrives.
4. The broker must advertise only enabled capabilities, validate required/optional
   manifests/arguments, enforce permission scopes and deadlines, cancel local
   work on disconnect, and return bounded results. No Python command execution.
5. Reconcile `node_id` / chat `device_id` as described above. Real account auth and
   live registration/invocation require a later explicitly authorized test.
6. Existing account bridge, account login, setup, and package scripts are untouched.

## Verification and observed RED/GREEN

Development proceeded in vertical failing-test → implementation → green slices:
missing decoder; malformed/oversized decoder; missing Link/register framing;
HTTP/ack/reset/EOF/unpair rejection; invocation/result correlation and budgets;
missing lifecycle/validation; isolated CLI/exact bridge import; bounded websocket
assembly/unknown methods; Rust missing crate; Rust idle cancellation/reaping;
configuration/environment/output protocol; outstanding results/backpressure;
parent-death survival; partial EOF/path validation; decoder flood/initial EOF;
headerless SDK control-open correction; once-only native close; and suppression
of expired queued invocations. Each missing feature/reproduction was
executed and observed failing before its corresponding production change.

Latest observed results:

* New Python transport suite: **16 passed**, including real installed protobuf
  serialization/deserialization with fake Gateway, fragmented/multiple prefixes,
  registration/invoke/result lifecycle, cancellation and wire escape inflation.
* Full Python suite with the selected external interpreter: **51 passed**.
  Plain stdlib interpreter: 51 discovered, 10 selected-runtime tests skipped.
* New Rust process suite: **13 passed**, 1 ignored subprocess fixture. The
  parent-death test explicitly executes that fixture in its child harness.
* Full workspace `cargo test --workspace --locked`: all non-display tests pass;
  the pre-existing GTK-display test remains ignored.
* `cargo fmt --all -- --check` and full workspace Clippy `-D warnings`: pass.
* The standard private SELinux `:Z` scratch/cache mounts encountered a relabel
  permission collision during parallel work. Re-running the same image with
  `--security-opt label=disable` and explicit worktree/cache/scratch mounts passed.
  No authentication or browser was used to work around the build issue.
* One parallel full-suite run hit the pre-existing login test
  `test_devtools_file_is_fresh_nofollow_and_scoped_to_live_process_listener`
  (`unsafe_devtools`). Its standalone suite and both subsequent complete Python
  runs passed without code changes; treat this as an observed intermittent test,
  not a transport regression proven fixed here.

Safe local commands (tests never initiate a live host):

```sh
# Plain decoder/validation/CLI tests; runtime-specific tests skip if unavailable.
python3 -m unittest discover -s scripts/tests -p test_local_host.py -v
# Use the selected community Python to exercise all protobuf tests, still fake IO.
/path/to/selected/community/python -m unittest discover -s scripts/tests -v
# Run Rust using the repository's standard builder and explicit worktree mounts.
cargo test -p muse-local-link --locked
```
