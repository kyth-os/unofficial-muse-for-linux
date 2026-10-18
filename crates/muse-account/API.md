# muse-account API (GUI integration contract)

All operations are synchronous/blocking: call off GTK's main thread. No account files or network are accessed by construction, only by explicit method invocation. Uses an existing community Muse account session, **not** official Mac-device pairing. The external installed `muse-mcp` is imported through the selected Python interpreter; no MCP server runs.

```rust
pub struct BridgeConfig {
    pub python: std::path::PathBuf,
    pub script: std::path::PathBuf,
    pub config_dir: std::path::PathBuf,
}
impl AccountClient {
    pub fn new(config: BridgeConfig) -> Self;
    pub fn connect(&self) -> Result<AccountSnapshot, AccountError>;
    pub fn history(&self, thread: Option<&str>, limit: usize)
        -> Result<Vec<HistoryMessage>, AccountError>;
    pub fn tasks(&self) -> Result<TasksSnapshot, AccountError>;
    pub fn send(&self, thread: Option<&str>, message: &str)
        -> Result<DispatchReceipt, AccountError>;
}
pub struct AccountSnapshot {
    pub identity_name: String, pub vm_id: String, pub threads: Vec<Thread>,
}
pub struct Thread {
    pub session_id: String, pub title: String, pub archived: bool,
    pub updated_at: Option<String>,
}
pub struct HistoryMessage {
    pub message_id: String, pub seq: u64, pub role: String, pub text: String,
    pub timestamp: Option<String>, pub status: Option<String>,
}
pub struct TasksSnapshot { pub schedules: Vec<Schedule>, pub runs: Vec<TaskRun> }
pub struct Schedule {
    pub id: String, pub title: String, pub enabled: bool,
    pub timezone: Option<String>, pub next_run_at_utc: Option<i64>,
}
pub struct TaskRun {
    pub run_id: String, pub job_id: String, pub status: String,
    pub result_summary: Option<String>, pub error_text: Option<String>,
    pub scheduled_for_utc: Option<i64>,
}
pub struct DispatchReceipt {
    pub sent: bool, pub confirmed: bool, pub message_id: Option<String>,
}
pub struct AccountError { pub code: ErrorCode, pub uncertain: bool }
// ErrorCode: AuthRequired, VmUnavailable, Timeout, InvalidRequest,
// InsecureConfig, BridgeUnavailable, BridgeFailure, ProtocolError, ResponseTooLarge.
```

Public records derive Clone/Debug/PartialEq/Eq and serde Deserialize/Serialize. `BridgeConfig` and `AccountClient` derive Clone/Debug. AccountError implements safe fixed Display/Error (never subprocess output). Dropping the client only drops a local handle, never revokes the upstream account.

History: limit 1–200 upstream events, sorted by sequence, duplicate message IDs retain highest-sequence update. ID-less nonmessage events are excluded. Roles and IDs preserved; timestamp numbers converted to strings. None selects main conversation, not all threads. Explicit IDs pass unchanged (no alias expansion).

Send: baseline scoped history first; exactly one fire-and-forget dispatch; bounded scoped-history confirmation. `confirmed` only when a newer user-role message with the **exact original text** appears; `sent=true, confirmed=false` means uncertain. A timeout/transport/protocol failure while invoking send has `AccountError.uncertain=true`. Never auto-retry any send. Confirmation means prompt persistence, not assistant completion. If baseline history fails, no send occurs.

Sidecar: one JSON stdin request (`op`, `config_dir`, optional `thread`, `limit`, `message`), one bounded stdout envelope: `{"ok":true,"data":...}` or `{"ok":false,"error":{"code":"auth_required","uncertain":false}}`. Operation allowlist `connect|history|tasks|send`; only tasks.list/tasks.runs use the community gateway escape hatch. No raw route interface, shell or remote file actions. stderr discarded. Credentials never enter argv, Rust, logs, errors or responses.

Sidecar subprocesses inherit only HOME/PATH/locale, XDG paths, TLS CA settings and proxy variables; unrelated provider credentials, Python overrides and MUSE_VM_ID/MUSE_CONFIG_DIR are not forwarded. MUSE_MCP_ENABLE_RAW=0 and PYTHONDONTWRITEBYTECODE=1 are set explicitly. Cookie credentials are confined to the child, using a sealed private memfd to normalize Netscape HttpOnly rows for the installed community loader without modifying the original jar.

Limits: 1 MiB request, 4 MiB stdout, 180-second process deadline, kill/reap on timeout or overflow. Selected Python and script must be absolute files (virtualenv interpreter symlink allowed). Config dir/cookie parent owned/private (0700 or tighter); cookies owned regular files, no group/other permissions. Nonsecret config JSON may be 0644 but not group/other-writable. Config/cookie symlinks rejected. Cookie read uses pinned validated descriptor. No persistent writes or chmod.

Read-only counts-only CLI:

```text
account-probe /absolute/venv/bin/python /absolute/scripts/muse_account_bridge.py /absolute/config_dir
```

Optional deliberate single send only if caller sets both `MUSE_ACCOUNT_PROBE_ALLOW_SEND=1` and `MUSE_ACCOUNT_PROBE_SEND_TEXT`; optional `MUSE_ACCOUNT_PROBE_THREAD`. Never retries. Probe prints counts/booleans only, no names/IDs/titles/messages.
