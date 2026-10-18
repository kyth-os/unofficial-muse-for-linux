//! Opt-in local-host transport. No command execution lives in this crate.
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Component, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct HostConfig {
    pub python: PathBuf,
    pub script: PathBuf,
    pub config_dir: PathBuf,
    pub node_id: String,
    pub display_name: String,
    pub commands: Value,
}
#[derive(Debug, Clone, PartialEq)]
pub struct HostInvocation {
    pub id: String,
    pub command: String,
    pub params: Value,
    pub timeout_ms: u64,
}
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    Connecting,
    Registered,
    Invoke(HostInvocation),
    Closed { code: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub code: &'static str,
}
impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for HostError {}
fn error(code: &'static str) -> HostError {
    HostError { code }
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    pending: Mutex<HashMap<String, Instant>>,
    terminal: OnceLock<String>,
    terminal_read: AtomicBool,
}
pub struct HostSession {
    events: Receiver<HostEvent>,
    input: SyncSender<Vec<u8>>,
    shared: Arc<Shared>,
}
impl HostSession {
    pub fn spawn(config: HostConfig) -> Result<Self, HostError> {
        if !config.python.is_absolute()
            || !config.script.is_absolute()
            || !config.config_dir.is_absolute()
            || [&config.python, &config.script, &config.config_dir]
                .iter()
                .any(|path| {
                    path.to_str().is_none_or(|p| p.contains('\0'))
                        || path.components().any(|c| c == Component::ParentDir)
                })
            || !valid_text(&config.node_id, 128)
            || !valid_text(&config.display_name, 128)
            || config.commands.as_object().is_none_or(|commands| {
                commands.len() > 64
                    || commands
                        .iter()
                        .any(|(key, value)| !valid_text(key, 128) || !value.is_object())
            })
        {
            return Err(error("invalid_request"));
        }
        let _ = line(&config.commands)?;
        let (tx, events) = mpsc::sync_channel(32);
        let (input, rx) = mpsc::sync_channel(32);
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("muse-local-link".into())
            .spawn(move || {
                let code = match run(config, rx, &tx, &worker) {
                    Ok(code) => code,
                    Err(e) => e.code.to_owned(),
                };
                worker.stop.store(true, Ordering::Release);
                if let Ok(mut pending) = worker.pending.lock() {
                    pending.clear();
                }
                let _ = worker.terminal.set(code);
            })
            .map_err(|_| error("worker_unavailable"))?;
        Ok(Self {
            events,
            input,
            shared,
        })
    }
    /// Closure supersedes queued invocations so no stale work executes after unpair.
    pub fn try_recv(&self) -> Result<HostEvent, TryRecvError> {
        if let Some(code) = self.shared.terminal.get() {
            if !self.shared.terminal_read.swap(true, Ordering::AcqRel) {
                return Ok(HostEvent::Closed { code: code.clone() });
            }
            return Err(TryRecvError::Disconnected);
        }
        if self.shared.stop.load(Ordering::Acquire) {
            return Err(TryRecvError::Empty);
        }
        // Acquire before dequeueing, so a contended worker never loses an event.
        let pending = self
            .shared
            .pending
            .try_lock()
            .map_err(|_| TryRecvError::Empty)?;
        for _ in 0..32 {
            let event = self.events.try_recv()?;
            if let HostEvent::Invoke(ref invoke) = event
                && pending
                    .get(&invoke.id)
                    .is_none_or(|deadline| Instant::now() >= *deadline)
            {
                continue;
            }
            return Ok(event);
        }
        Err(TryRecvError::Empty)
    }
    pub fn send_result(
        &self,
        id: &str,
        ok: bool,
        output: Value,
        error: Option<String>,
    ) -> Result<(), HostError> {
        if self.shared.stop.load(Ordering::Acquire) || !valid_text(id, 128) {
            return Err(HostError {
                code: "invalid_result",
            });
        }
        // Move caller-owned output instead of cloning it before the capped serializer.
        let mut fields = serde_json::Map::new();
        fields.insert("op".into(), Value::String("result".into()));
        fields.insert("id".into(), Value::String(id.into()));
        fields.insert("ok".into(), Value::Bool(ok));
        fields.insert("output".into(), output);
        if let Some(error) = error {
            fields.insert("error".into(), Value::String(error));
        }
        let bytes = line(&Value::Object(fields))?;
        // Only try_lock: the caller never waits for worker-owned locks.
        let mut pending = self
            .shared
            .pending
            .try_lock()
            .map_err(|_| HostError { code: "busy" })?;
        if pending
            .get(id)
            .is_none_or(|deadline| Instant::now() >= *deadline)
        {
            return Err(HostError {
                code: "invalid_result",
            });
        }
        self.input
            .try_send(bytes)
            .map_err(|_| HostError { code: "queue_full" })?;
        pending.remove(id);
        Ok(())
    }
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}
impl Drop for HostSession {
    fn drop(&mut self) {
        self.stop();
    }
}
struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: the subprocess owns this process group.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn nonblocking(fd: i32) -> Result<(), HostError> {
    // SAFETY: caller owns the valid pipe descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(error("helper_io"));
    }
    Ok(())
}
const MAX_LINE: usize = 256 * 1024;
struct Capped(Vec<u8>);
impl Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + bytes.len() >= MAX_LINE - 4 {
            return Err(std::io::Error::other("size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn line(value: &Value) -> Result<Vec<u8>, HostError> {
    let mut output = Capped(Vec::new());
    serde_json::to_writer(&mut output, value).map_err(|_| error("request_too_large"))?;
    output.0.push(b'\n');
    Ok(output.0)
}
fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Wire {
    State {
        state: String,
        code: Option<String>,
    },
    Invoke {
        id: String,
        command: String,
        params: Value,
        timeout_ms: u64,
    },
}
fn parse(raw: &[u8], phase: &mut u8) -> Result<HostEvent, HostError> {
    let invalid = || error("protocol_error");
    let value: Wire = serde_json::from_slice(raw).map_err(|_| invalid())?;
    match value {
        Wire::State { state, code } => match state.as_str() {
            "connecting" if *phase == 0 && code.is_none() => {
                *phase = 1;
                Ok(HostEvent::Connecting)
            }
            "registered" if *phase == 1 && code.is_none() => {
                *phase = 2;
                Ok(HostEvent::Registered)
            }
            "closed" | "error" | "auth_required" => {
                let code = code.unwrap_or_else(|| "helper_closed".into());
                if ![
                    "helper_closed",
                    "stopped",
                    "eof",
                    "auth_required",
                    "invalid_request",
                    "invalid_result",
                    "invalid_invoke",
                    "protocol_error",
                    "budget_exceeded",
                    "result_too_large",
                    "transport_error",
                    "transport_timeout",
                    "stream_closed",
                    "stream_reset",
                    "unpaired",
                    "link_forbidden",
                    "link_rejected",
                    "registration_timeout",
                    "output_blocked",
                    "insecure_config",
                    "bridge_unavailable",
                    "bridge_failure",
                    "vm_unavailable",
                    "timeout",
                    "response_too_large",
                ]
                .contains(&code.as_str())
                {
                    return Err(invalid());
                }
                Ok(HostEvent::Closed { code })
            }
            _ => Err(invalid()),
        },
        Wire::Invoke {
            id,
            command,
            params,
            timeout_ms,
        } if *phase == 2 => Ok(HostEvent::Invoke(HostInvocation {
            id,
            command,
            params,
            timeout_ms,
        })),
        _ => Err(invalid()),
    }
}
fn helper_command(config: &HostConfig) -> Command {
    let mut command = Command::new(&config.python);
    command.env_clear();
    for key in [
        "HOME",
        "PATH",
        "LANG",
        "LC_ALL",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "REQUESTS_CA_BUNDLE",
        "CURL_CA_BUNDLE",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("MUSE_MCP_ENABLE_RAW", "0")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-I")
        .arg(&config.script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: pre_exec uses only async-signal-safe libc calls; no allocation/locks.
    // Linux delivers PDEATHSIG if the spawning worker/GUI dies, including SIGKILL.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Cover death between fork and installing the signal.
            if libc::getppid() != parent {
                libc::_exit(127);
            }
            Ok(())
        });
    }
    command
}
fn run(
    config: HostConfig,
    input: Receiver<Vec<u8>>,
    events: &SyncSender<HostEvent>,
    shared: &Shared,
) -> Result<String, HostError> {
    let stop = &shared.stop;
    if stop.load(Ordering::Acquire) {
        return Ok("stopped".into());
    }
    let child = helper_command(&config)
        .spawn()
        .map_err(|_| error("helper_unavailable"))?;
    let mut guard = Guard(child);
    let mut stdin = guard.0.stdin.take().ok_or_else(|| error("helper_io"))?;
    let mut stdout = guard.0.stdout.take().ok_or_else(|| error("helper_io"))?;
    nonblocking(stdin.as_raw_fd())?;
    nonblocking(stdout.as_raw_fd())?;
    let request = serde_json::json!({"op":"start","config_dir":config.config_dir,"node_id":config.node_id,
        "display_name":config.display_name,"commands_v2":&config.commands});
    let mut sending = line(&request)?;
    let mut written = 0;
    let mut buffer = Vec::new();
    let mut phase = 0;
    let mut seen = HashSet::new();
    let started = Instant::now();
    let mut write_started = started;
    loop {
        if stop.load(Ordering::Acquire) {
            // Graceful signal first; a wedged helper is killed/reaped by Guard.
            unsafe {
                libc::kill(guard.0.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_millis(400);
            while Instant::now() < deadline {
                if guard
                    .0
                    .try_wait()
                    .map_err(|_| error("helper_io"))?
                    .is_some()
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            return Ok("stopped".into());
        }
        if phase != 2 && started.elapsed() >= Duration::from_secs(35) {
            return Err(error("registration_timeout"));
        }
        if written == sending.len()
            && let Ok(message) = input.try_recv()
        {
            sending = message;
            written = 0;
            write_started = Instant::now();
        }
        if written < sending.len() {
            if write_started.elapsed() >= Duration::from_millis(500) {
                return Err(error("helper_timeout"));
            }
            match stdin.write(&sending[written..]) {
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(error("helper_io")),
            }
        }
        let mut raw = [0; 8192];
        match stdout.read(&mut raw) {
            Ok(0) if buffer.is_empty() => return Ok("helper_eof".into()),
            Ok(0) => return Err(error("protocol_error")),
            Ok(n) => buffer.extend_from_slice(&raw[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return Err(error("helper_io")),
        }
        while let Some(end) = buffer.iter().position(|c| *c == b'\n') {
            if end > MAX_LINE {
                return Err(error("output_too_large"));
            }
            let event = parse(&buffer[..end], &mut phase)?;
            buffer.drain(..=end);
            if let HostEvent::Closed { code } = event {
                return Ok(code);
            }
            if let HostEvent::Invoke(ref invoke) = event {
                if !valid_text(&invoke.id, 128)
                    || seen.contains(&invoke.id)
                    || !valid_text(&invoke.command, 128)
                    || config.commands.get(&invoke.command).is_none()
                    || !invoke.params.is_object()
                    || !(1..=120000).contains(&invoke.timeout_ms)
                {
                    return Err(error("invalid_invoke"));
                }
                let mut pending = shared.pending.lock().map_err(|_| error("internal_error"))?;
                pending.retain(|_, deadline| Instant::now() < *deadline);
                if pending.len() >= 8 || seen.len() >= 4096 {
                    return Err(error("budget_exceeded"));
                }
                seen.insert(invoke.id.clone());
                pending.insert(
                    invoke.id.clone(),
                    Instant::now() + Duration::from_millis(invoke.timeout_ms),
                );
            }
            events.try_send(event).map_err(|_| error("queue_full"))?;
        }
        if buffer.len() > MAX_LINE {
            return Err(error("output_too_large"));
        }
        // Descendants must not hold this session open after its leader exits.
        if guard
            .0
            .try_wait()
            .map_err(|_| error("helper_io"))?
            .is_some()
        {
            return if buffer.is_empty() {
                Ok("helper_eof".into())
            } else {
                Err(error("protocol_error"))
            };
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
