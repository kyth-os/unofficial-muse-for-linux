//! Durable local reminder-task store.
//!
//! Single-file JSON-lines persistence with atomic writes (tmp file + rename),
//! strict directory permissions, and validated state transitions.
//!
//! Scope: durable reminders only. This crate performs no command execution,
//! no network access, no notification delivery, and spawns no threads.
//! Delivery and UI are the parent's job. Single-process use only: no file
//! locking is performed.
//!
//! Times are Unix seconds (`u64`). The caller supplies `now`; the crate also
//! reads the system clock for `created_unix_secs` and due-bound checks.

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// File name of the JSON-lines store inside the store directory.
pub const STORE_FILE_NAME: &str = "tasks.jsonl";
/// Maximum title length in Unicode scalar values.
pub const TITLE_MAX_CHARS: usize = 200;
/// Maximum body length in bytes.
pub const BODY_MAX_BYTES: usize = 4096;
/// A due timestamp may lie at most this far in the past (seconds).
pub const PAST_GRACE_SECS: u64 = 3600;
/// A due timestamp may lie at most this far in the future (seconds, 5 years).
pub const FUTURE_WINDOW_SECS: u64 = 5 * 365 * 86400;

/// Reminder-task lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Paused,
    Done,
    Cancelled,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Paused => "paused",
            TaskStatus::Done => "done",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Option<TaskStatus> {
        match s {
            "pending" => Some(TaskStatus::Pending),
            "paused" => Some(TaskStatus::Paused),
            "done" => Some(TaskStatus::Done),
            "cancelled" => Some(TaskStatus::Cancelled),
            _ => None,
        }
    }
}

/// A single reminder task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: u64,
    pub title: String,
    pub body: String,
    pub due_unix_secs: u64,
    pub status: TaskStatus,
    pub created_unix_secs: u64,
    pub notified: bool,
}

/// Typed store error.
///
/// Payloads are fixed static reason strings, except `CorruptStore`, which
/// carries a static reason plus the offending 1-based line number. Task
/// titles, bodies, and filesystem paths are never included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskError {
    InvalidArgument(&'static str),
    NotFound,
    CorruptStore(String),
    Io(&'static str),
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskError::InvalidArgument(r) => write!(f, "invalid argument: {r}"),
            TaskError::NotFound => write!(f, "task not found"),
            TaskError::CorruptStore(r) => write!(f, "corrupt store: {r}"),
            TaskError::Io(r) => write!(f, "i/o error: {r}"),
        }
    }
}

impl std::error::Error for TaskError {}

fn now_secs() -> Result<u64, TaskError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| TaskError::Io("clock unavailable"))
}

fn has_forbidden_chars(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

fn validate_title(title: &str) -> Result<(), TaskError> {
    let n = title.chars().count();
    if n == 0 || n > TITLE_MAX_CHARS {
        return Err(TaskError::InvalidArgument("title must be 1-200 chars"));
    }
    if has_forbidden_chars(title) {
        return Err(TaskError::InvalidArgument(
            "text contains forbidden characters",
        ));
    }
    Ok(())
}

fn validate_body(body: &str) -> Result<(), TaskError> {
    if body.len() > BODY_MAX_BYTES {
        return Err(TaskError::InvalidArgument("body exceeds size limit"));
    }
    if has_forbidden_chars(body) {
        return Err(TaskError::InvalidArgument(
            "text contains forbidden characters",
        ));
    }
    Ok(())
}

fn validate_due(due: u64, now: u64) -> Result<(), TaskError> {
    let oldest = now.saturating_sub(PAST_GRACE_SECS);
    let latest = now.saturating_add(FUTURE_WINDOW_SECS);
    if due <= oldest || due > latest {
        return Err(TaskError::InvalidArgument("due timestamp out of range"));
    }
    Ok(())
}

/// Nonzero pseudo-random id. Prefers `/dev/urandom`; falls back to a
/// time/pid/counter hash so id generation never fails without entropy.
fn random_id() -> u64 {
    if let Ok(mut f) = File::open("/dev/urandom") {
        use std::io::Read;
        let mut buf = [0u8; 8];
        if f.read_exact(&mut buf).is_ok() {
            let v = u64::from_le_bytes(buf);
            if v != 0 {
                return v;
            }
        }
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    // FNV-1a mix over time, pid, address entropy, and counter.
    let mut h: u64 = 0xcbf2_9ce4_4842_2225;
    let pid = std::process::id() as u64;
    for word in [
        t,
        pid,
        n ^ 0x9e37_79b9_7f4a_7c15,
        std::ptr::addr_of!(h) as u64,
    ] {
        for b in word.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    if h == 0 {
        1
    } else {
        h
    }
}

// ---------------------------------------------------------------------------
// Minimal JSON writer (known field set only).
// ---------------------------------------------------------------------------

fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

fn task_to_json(t: &Task) -> String {
    let mut s = String::new();
    s.push_str("{\"id\":");
    s.push_str(&t.id.to_string());
    s.push_str(",\"title\":\"");
    escape_into(&mut s, &t.title);
    s.push_str("\",\"body\":\"");
    escape_into(&mut s, &t.body);
    s.push_str("\",\"due_unix_secs\":");
    s.push_str(&t.due_unix_secs.to_string());
    s.push_str(",\"status\":\"");
    s.push_str(t.status.as_str());
    s.push_str("\",\"created_unix_secs\":");
    s.push_str(&t.created_unix_secs.to_string());
    s.push_str(",\"notified\":");
    s.push_str(if t.notified { "true" } else { "false" });
    s.push('}');
    s
}

// ---------------------------------------------------------------------------
// Minimal JSON reader (defensive; accepts what the writer emits plus the
// standard string escapes and surrounding whitespace).
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum JsonVal {
    Str(String),
    Num(u64),
    Bool(bool),
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Parser<'a> {
        Parser {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), ()> {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(())
        }
    }

    fn parse_string(&mut self) -> Result<String, ()> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let c = *self.b.get(self.i).ok_or(())?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or(())?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            if self.i + 4 > self.b.len() {
                                return Err(());
                            }
                            let hex =
                                std::str::from_utf8(&self.b[self.i..self.i + 4]).map_err(|_| ())?;
                            let mut cp = u32::from_str_radix(hex, 16).map_err(|_| ())?;
                            self.i += 4;
                            // Combine UTF-16 surrogate pairs.
                            if (0xD800..0xDC00).contains(&cp) {
                                if self.i + 6 <= self.b.len()
                                    && self.b[self.i] == b'\\'
                                    && self.b[self.i + 1] == b'u'
                                {
                                    let hex2 = std::str::from_utf8(&self.b[self.i + 2..self.i + 6])
                                        .map_err(|_| ())?;
                                    let lo = u32::from_str_radix(hex2, 16).map_err(|_| ())?;
                                    if (0xDC00..0xE000).contains(&lo) {
                                        cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                        self.i += 6;
                                    } else {
                                        return Err(());
                                    }
                                } else {
                                    return Err(());
                                }
                            }
                            out.push(char::from_u32(cp).ok_or(())?);
                        }
                        _ => return Err(()),
                    }
                }
                0x00..=0x1F => return Err(()),
                _ => {
                    // Copy the full UTF-8 sequence starting here.
                    let rest = std::str::from_utf8(&self.b[self.i - 1..]).map_err(|_| ())?;
                    let ch = rest.chars().next().ok_or(())?;
                    out.push(ch);
                    self.i += ch.len_utf8() - 1;
                }
            }
        }
    }

    fn parse_number(&mut self) -> Result<u64, ()> {
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        if self.i == start {
            return Err(());
        }
        std::str::from_utf8(&self.b[start..self.i])
            .map_err(|_| ())?
            .parse::<u64>()
            .map_err(|_| ())
    }

    fn parse_literal(&mut self) -> Result<JsonVal, ()> {
        if self.b[self.i..].starts_with(b"true") {
            self.i += 4;
            Ok(JsonVal::Bool(true))
        } else if self.b[self.i..].starts_with(b"false") {
            self.i += 5;
            Ok(JsonVal::Bool(false))
        } else {
            Err(())
        }
    }

    fn parse_value(&mut self) -> Result<JsonVal, ()> {
        match self.b.get(self.i) {
            Some(b'"') => Ok(JsonVal::Str(self.parse_string()?)),
            Some(c) if c.is_ascii_digit() => Ok(JsonVal::Num(self.parse_number()?)),
            Some(_) => self.parse_literal(),
            None => Err(()),
        }
    }

    fn parse_object(&mut self) -> Result<Vec<(String, JsonVal)>, ()> {
        self.ws();
        self.expect(b'{')?;
        let mut fields = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(fields);
        }
        loop {
            self.ws();
            let key = self.parse_string()?;
            self.ws();
            self.expect(b':')?;
            self.ws();
            let val = self.parse_value()?;
            fields.push((key, val));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(fields);
                }
                _ => return Err(()),
            }
        }
    }
}

fn task_from_json(line: &str) -> Result<Task, ()> {
    let mut p = Parser::new(line);
    let fields = p.parse_object()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(());
    }
    let mut id: Option<u64> = None;
    let mut title: Option<String> = None;
    let mut body: Option<String> = None;
    let mut due: Option<u64> = None;
    let mut status: Option<TaskStatus> = None;
    let mut created: Option<u64> = None;
    let mut notified: Option<bool> = None;
    for (k, v) in fields {
        match (k.as_str(), v) {
            ("id", JsonVal::Num(n)) => {
                if n == 0 || id.is_some() {
                    return Err(());
                }
                id = Some(n);
            }
            ("title", JsonVal::Str(s)) => {
                if title.is_some() {
                    return Err(());
                }
                title = Some(s);
            }
            ("body", JsonVal::Str(s)) => {
                if body.is_some() {
                    return Err(());
                }
                body = Some(s);
            }
            ("due_unix_secs", JsonVal::Num(n)) => {
                if due.is_some() {
                    return Err(());
                }
                due = Some(n);
            }
            ("status", JsonVal::Str(s)) => {
                if status.is_some() {
                    return Err(());
                }
                status = Some(TaskStatus::parse(&s).ok_or(())?);
            }
            ("created_unix_secs", JsonVal::Num(n)) => {
                if created.is_some() {
                    return Err(());
                }
                created = Some(n);
            }
            ("notified", JsonVal::Bool(b)) => {
                if notified.is_some() {
                    return Err(());
                }
                notified = Some(b);
            }
            _ => return Err(()),
        }
    }
    let (id, title, body, due, status, created, notified) = (
        id.ok_or(())?,
        title.ok_or(())?,
        body.ok_or(())?,
        due.ok_or(())?,
        status.ok_or(())?,
        created.ok_or(())?,
        notified.ok_or(())?,
    );
    // Stored records must satisfy the same bounds as new ones (due checked
    // against creation time is unknowable, so only title/body are rechecked).
    validate_title(&title).map_err(|_| ())?;
    validate_body(&body).map_err(|_| ())?;
    Ok(Task {
        id,
        title,
        body,
        due_unix_secs: due,
        status,
        created_unix_secs: created,
        notified,
    })
}

// ---------------------------------------------------------------------------
// Store.
// ---------------------------------------------------------------------------

/// Durable local reminder-task store backed by a single JSON-lines file.
///
/// Open with [`TaskStore::open`]. All mutations are persisted atomically
/// (temporary file + rename) before the call returns, so dropping the store
/// without any close step loses nothing.
#[derive(Debug)]
pub struct TaskStore {
    dir: PathBuf,
    tasks: Vec<Task>,
}

impl TaskStore {
    /// Open (or create) the store in `dir`.
    ///
    /// The directory is created with mode 0700 when missing. Opening fails
    /// closed when `dir` is a symlink, is not a directory, or carries any
    /// group/world permission bits. A corrupt record fails the open with
    /// [`TaskError::CorruptStore`] naming the 1-based line number.
    pub fn open(dir: &Path) -> Result<TaskStore, TaskError> {
        match std::fs::symlink_metadata(dir) {
            Ok(md) => {
                if md.file_type().is_symlink() {
                    return Err(TaskError::InvalidArgument("directory refused"));
                }
                if !md.file_type().is_dir() {
                    return Err(TaskError::InvalidArgument("directory refused"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(dir)
                    .map_err(|_| TaskError::Io("directory create failed"))?;
                set_dir_private(dir)?;
            }
            Err(_) => return Err(TaskError::Io("directory unavailable")),
        }
        // Re-check permissions (fail closed) for pre-existing directories.
        let md = std::fs::metadata(dir).map_err(|_| TaskError::Io("directory unavailable"))?;
        if !md.file_type().is_dir() {
            return Err(TaskError::InvalidArgument("directory refused"));
        }
        if md.mode() & 0o077 != 0 {
            return Err(TaskError::InvalidArgument("directory refused"));
        }

        let store_path = dir.join(STORE_FILE_NAME);
        let mut tasks = Vec::new();
        if std::fs::symlink_metadata(&store_path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(TaskError::InvalidArgument("directory refused"));
        }
        if store_path.exists() {
            let content = std::fs::read_to_string(&store_path)
                .map_err(|_| TaskError::CorruptStore("store is not valid text".to_string()))?;
            let mut seen = HashSet::new();
            for (idx, line) in content.lines().enumerate() {
                let task = task_from_json(line).map_err(|_| corrupt_line(idx + 1))?;
                if !seen.insert(task.id) {
                    return Err(corrupt_line(idx + 1));
                }
                tasks.push(task);
            }
            tasks.sort_by_key(|t| t.id);
        }
        Ok(TaskStore {
            dir: dir.to_path_buf(),
            tasks,
        })
    }

    /// Directory backing this store.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Create a task due at `due_unix_secs`.
    pub fn create(
        &mut self,
        title: &str,
        body: &str,
        due_unix_secs: u64,
    ) -> Result<Task, TaskError> {
        let now = now_secs()?;
        validate_title(title)?;
        validate_body(body)?;
        validate_due(due_unix_secs, now)?;
        let mut id = random_id();
        while self.tasks.iter().any(|t| t.id == id) {
            id = random_id();
        }
        let task = Task {
            id,
            title: title.to_string(),
            body: body.to_string(),
            due_unix_secs,
            status: TaskStatus::Pending,
            created_unix_secs: now,
            notified: false,
        };
        self.insert_and_persist(task)
    }

    /// All tasks, ordered by id.
    pub fn list(&self) -> Vec<Task> {
        let mut out = self.tasks.clone();
        out.sort_by_key(|t| t.id);
        out
    }

    /// Fetch one task by id.
    pub fn get(&self, id: u64) -> Result<Task, TaskError> {
        self.tasks
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or(TaskError::NotFound)
    }

    /// Pending tasks with `due_unix_secs <= now` that are not yet notified,
    /// ordered by `(due_unix_secs, id)`.
    pub fn due(&self, now_unix_secs: u64) -> Vec<Task> {
        let mut out: Vec<Task> = self
            .tasks
            .iter()
            .filter(|t| {
                t.status == TaskStatus::Pending && !t.notified && t.due_unix_secs <= now_unix_secs
            })
            .cloned()
            .collect();
        out.sort_by_key(|t| (t.due_unix_secs, t.id));
        out
    }

    /// Pause a pending task.
    pub fn pause(&mut self, id: u64) -> Result<Task, TaskError> {
        self.transition(id, TaskStatus::Pending, TaskStatus::Paused)
    }

    /// Resume a paused task.
    pub fn resume(&mut self, id: u64) -> Result<Task, TaskError> {
        self.transition(id, TaskStatus::Paused, TaskStatus::Pending)
    }

    /// Cancel a pending or paused task.
    pub fn cancel(&mut self, id: u64) -> Result<Task, TaskError> {
        let task = self.get(id)?;
        match task.status {
            TaskStatus::Done | TaskStatus::Cancelled => {
                Err(TaskError::InvalidArgument("task is terminal"))
            }
            TaskStatus::Pending | TaskStatus::Paused => {
                self.apply_status(id, TaskStatus::Cancelled)
            }
        }
    }

    /// Complete a pending task.
    pub fn complete(&mut self, id: u64) -> Result<Task, TaskError> {
        self.transition(id, TaskStatus::Pending, TaskStatus::Done)
    }

    /// Flag a pending or paused task as notified.
    pub fn mark_notified(&mut self, id: u64) -> Result<Task, TaskError> {
        let idx = self.index_of(id)?;
        if matches!(
            self.tasks[idx].status,
            TaskStatus::Done | TaskStatus::Cancelled
        ) {
            return Err(TaskError::InvalidArgument("task is terminal"));
        }
        let mut updated = self.tasks[idx].clone();
        updated.notified = true;
        self.replace_and_persist(idx, updated)
    }

    // -- internals ----------------------------------------------------------

    fn index_of(&self, id: u64) -> Result<usize, TaskError> {
        self.tasks
            .iter()
            .position(|t| t.id == id)
            .ok_or(TaskError::NotFound)
    }

    fn transition(&mut self, id: u64, from: TaskStatus, to: TaskStatus) -> Result<Task, TaskError> {
        let idx = self.index_of(id)?;
        let cur = self.tasks[idx].status;
        if matches!(cur, TaskStatus::Done | TaskStatus::Cancelled) {
            return Err(TaskError::InvalidArgument("task is terminal"));
        }
        if cur != from {
            return Err(TaskError::InvalidArgument("invalid state transition"));
        }
        self.apply_status(id, to)
    }

    fn apply_status(&mut self, id: u64, status: TaskStatus) -> Result<Task, TaskError> {
        let idx = self.index_of(id)?;
        let mut updated = self.tasks[idx].clone();
        updated.status = status;
        self.replace_and_persist(idx, updated)
    }

    fn insert_and_persist(&mut self, task: Task) -> Result<Task, TaskError> {
        let mut next = self.tasks.clone();
        next.push(task.clone());
        next.sort_by_key(|t| t.id);
        persist(&self.dir, &next)?;
        self.tasks = next;
        Ok(task)
    }

    fn replace_and_persist(&mut self, idx: usize, task: Task) -> Result<Task, TaskError> {
        let mut next = self.tasks.clone();
        next[idx] = task.clone();
        persist(&self.dir, &next)?;
        self.tasks = next;
        Ok(task)
    }
}

fn corrupt_line(n: usize) -> TaskError {
    // Line numbers are store metadata, never task content: safe to include.
    TaskError::CorruptStore(format!("corrupt record at line {n}"))
}

fn set_dir_private(dir: &Path) -> Result<(), TaskError> {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| TaskError::Io("directory create failed"))
}

/// Atomically rewrite the store file: write tmp + fsync + rename, mode 0600.
fn persist(dir: &Path, tasks: &[Task]) -> Result<(), TaskError> {
    let mut content = String::new();
    for t in tasks {
        content.push_str(&task_to_json(t));
        content.push('\n');
    }
    let tmp = dir.join(format!("{STORE_FILE_NAME}.tmp.{}", std::process::id()));
    let target = dir.join(STORE_FILE_NAME);
    let result = (|| -> Result<(), TaskError> {
        {
            let mut f = File::create(&tmp).map_err(|_| TaskError::Io("store write failed"))?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|_| TaskError::Io("store write failed"))?;
            f.write_all(content.as_bytes())
                .map_err(|_| TaskError::Io("store write failed"))?;
            f.sync_all()
                .map_err(|_| TaskError::Io("store write failed"))?;
        }
        std::fs::rename(&tmp, &target).map_err(|_| TaskError::Io("store write failed"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
