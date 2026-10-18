# muse-tasks API

Durable **local reminder-task store**. Std only, no dependencies.
Delivery of reminders (notifications, UI) is the parent's job; this crate
only stores tasks and answers "what is due?".

## Scope (frozen)

- Local durable reminders only. **No shell-command execution** — running
  arbitrary commands stays behind the local-host approval system and is
  explicitly out of scope for this crate.
- No network, no notification delivery, no credentials.
- Single-process use only: no file locking, no background threads, no
  polling inside the crate. Concurrent writers from two processes may
  interleave; last rename wins.
- Unix only (permission model relies on Unix mode bits).

## Types

```rust
pub const STORE_FILE_NAME: &str = "tasks.jsonl";
pub const TITLE_MAX_CHARS: usize = 200;
pub const BODY_MAX_BYTES: usize = 4096;
pub const PAST_GRACE_SECS: u64 = 3600;
pub const FUTURE_WINDOW_SECS: u64 = 157_680_000; // 5 * 365 * 86400

pub enum TaskStatus { Pending, Paused, Done, Cancelled }

pub struct Task {
    pub id: u64,               // random nonzero, collision-checked
    pub title: String,         // 1..=200 chars, no NUL/control chars
    pub body: String,          // <= 4096 bytes, no NUL/control chars
    pub due_unix_secs: u64,
    pub status: TaskStatus,
    pub created_unix_secs: u64,
    pub notified: bool,
}

pub enum TaskError {
    InvalidArgument(&'static str), // fixed static reason strings
    NotFound,
    CorruptStore(String),          // static reason + 1-based line number only
    Io(&'static str),              // fixed static reason strings
}
```

Error values never echo task titles, bodies, or filesystem paths. The
`CorruptStore` payload names only the offending line number.

## Operations

| Op | Signature | Notes |
|----|-----------|-------|
| open | `TaskStore::open(dir: &Path) -> Result<TaskStore, TaskError>` | Creates `dir` `0700` if missing; refuses symlink / non-dir / group- or world-accessible dirs (fail closed). Loads `tasks.jsonl`; any corrupt line fails the open naming its line number. |
| create | `create(title, body, due_unix_secs) -> Result<Task, TaskError>` | Validates bounds; `due` must be in `(now-1h, now+5y]`. |
| list | `list() -> Vec<Task>` | Ordered by `id`. Infallible (corruption surfaces at `open`). |
| get | `get(id) -> Result<Task, TaskError>` | `NotFound` when absent. |
| pause | `pause(id) -> Result<Task, TaskError>` | `Pending → Paused`. |
| resume | `resume(id) -> Result<Task, TaskError>` | `Paused → Pending`. |
| cancel | `cancel(id) -> Result<Task, TaskError>` | `Pending|Paused → Cancelled`. |
| complete | `complete(id) -> Result<Task, TaskError>` | `Pending → Done`. |
| due | `due(now_unix_secs: u64) -> Vec<Task>` | `Pending` + `due <= now` + `!notified`, ordered by `(due, id)`. |
| mark_notified | `mark_notified(id) -> Result<Task, TaskError>` | Sets `notified = true`; allowed from `Pending`/`Paused`. |
| dir | `dir() -> &Path` | Backing directory. |

## Transition table

| From \ To | Pending | Paused | Done | Cancelled |
|-----------|---------|--------|------|-----------|
| Pending | — | `pause` | `complete` | `cancel` |
| Paused | `resume` | — | ✗ | `cancel` |
| Done | ✗ | ✗ | ✗ (terminal) | ✗ (terminal) |
| Cancelled | ✗ | ✗ | ✗ (terminal) | ✗ (terminal) |

Any op on `Done`/`Cancelled` (including `mark_notified`) → `InvalidArgument("task is terminal")`.
Wrong-state op → `InvalidArgument("invalid state transition")`.

## Persistence

- One JSON object per line in `<dir>/tasks.jsonl` (hand-rolled minimal JSON;
  control chars are rejected at the boundary so records are always
  single-line). Store file mode `0600`.
- Every mutation rewrites the whole file via tmp-file + fsync + rename, so
  dropping the store without any close step loses nothing
  (list-after-crash = drop, reopen, list).
- Stored records are re-validated on load (bounds, status spelling, duplicate
  ids); violations are `CorruptStore`, never silently skipped.

## Verification gaps (known, accepted)

- No file locking: two processes writing concurrently can interleave
  (last-rename-wins). Single-process assumption is documented, not enforced.
- No fsync of the parent directory after rename: on a kernel/power crash the
  rename itself could be lost (data still consistent, just older).
- Tmp file name contains the pid; a stale tmp file from a killed process is
  left behind and ignored (never read).
- Ids come from `/dev/urandom` with a time/pid/counter-hash fallback; ids are
  unpredictable, not UUIDs, and uniqueness is checked only against loaded
  tasks.
- "5 years" is fixed at `5*365` days (leap days not accounted).
- Control-character rejection includes `\n`/tab, so multi-line bodies are not
  supported (keeps the JSONL invariant trivially true).
- Store-file permission repair is write-side only: a pre-existing
  world-readable `tasks.jsonl` is overwritten `0600` on the next mutation,
  not refused at open (the directory gate is the fail-closed boundary).
