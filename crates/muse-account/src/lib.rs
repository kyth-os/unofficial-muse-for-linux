//! Opt-in account integration via a separately installed community Python bridge.
//! Constructors are inert; methods block and belong on a worker thread.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fmt,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_REQUEST: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(180);

#[cfg(test)]
mod process_tests;

#[derive(Clone, Debug)]
pub struct BridgeConfig {
    pub python: PathBuf,
    pub script: PathBuf,
    pub config_dir: PathBuf,
}
#[derive(Clone, Debug)]
pub struct AccountClient {
    config: BridgeConfig,
    active: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub identity_name: String,
    pub vm_id: String,
    pub threads: Vec<Thread>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub session_id: String,
    pub title: String,
    pub archived: bool,
    pub updated_at: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub message_id: String,
    pub seq: u64,
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
    pub status: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasksSnapshot {
    pub schedules: Vec<Schedule>,
    pub runs: Vec<TaskRun>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub title: String,
    pub enabled: bool,
    pub timezone: Option<String>,
    pub next_run_at_utc: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRun {
    pub run_id: String,
    pub job_id: String,
    pub status: String,
    pub result_summary: Option<String>,
    pub error_text: Option<String>,
    pub scheduled_for_utc: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchReceipt {
    pub sent: bool,
    pub confirmed: bool,
    pub message_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    AuthRequired,
    VmUnavailable,
    Timeout,
    InvalidRequest,
    InsecureConfig,
    BridgeUnavailable,
    BridgeFailure,
    ProtocolError,
    ResponseTooLarge,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountError {
    pub code: ErrorCode,
    pub uncertain: bool,
}
impl AccountError {
    fn new(code: ErrorCode, uncertain: bool) -> Self {
        Self { code, uncertain }
    }
}
impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.code {
            ErrorCode::AuthRequired => "Muse authentication is missing or expired; sign in again.",
            ErrorCode::VmUnavailable => "The Muse account VM is unavailable.",
            ErrorCode::Timeout => "The account bridge exceeded its time limit.",
            ErrorCode::InvalidRequest => "The account request or bridge configuration is invalid.",
            ErrorCode::InsecureConfig => "Account configuration permissions are unsafe.",
            ErrorCode::BridgeUnavailable => {
                "The selected community account bridge is not installed or executable."
            }
            ErrorCode::BridgeFailure => "The community account gateway request failed.",
            ErrorCode::ProtocolError => "The account bridge returned an invalid response.",
            ErrorCode::ResponseTooLarge => "The account bridge response exceeded its size limit.",
        })?;
        if self.uncertain {
            f.write_str(" Delivery is uncertain; do not automatically resend.")?;
        }
        Ok(())
    }
}
impl std::error::Error for AccountError {}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    data: Option<T>,
    error: Option<AccountError>,
}

impl AccountClient {
    pub fn new(config: BridgeConfig) -> Self {
        Self {
            config,
            active: None,
        }
    }
    /// Bound this clone's subprocesses to a session lease. False cancels locally;
    /// an already dispatched remote action cannot be recalled and is uncertain.
    pub fn cancellable(&self, active: Arc<AtomicBool>) -> Self {
        Self {
            config: self.config.clone(),
            active: Some(active),
        }
    }
    pub fn connect(&self) -> Result<AccountSnapshot, AccountError> {
        self.request(serde_json::json!({"op": "connect"}), false)
    }
    pub fn history(
        &self,
        thread: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HistoryMessage>, AccountError> {
        validate_thread(thread)?;
        if !(1..=200).contains(&limit) {
            return Err(AccountError::new(ErrorCode::InvalidRequest, false));
        }
        self.request(
            serde_json::json!({"op": "history", "thread": thread, "limit": limit}),
            false,
        )
    }
    pub fn tasks(&self) -> Result<TasksSnapshot, AccountError> {
        self.request(serde_json::json!({"op": "tasks"}), false)
    }
    /// Dispatch once; an unconfirmed receipt or uncertain error must not be
    /// automatically retried. Confirmation proves only prompt persistence.
    pub fn send(
        &self,
        thread: Option<&str>,
        message: &str,
    ) -> Result<DispatchReceipt, AccountError> {
        self.send_optional_device(thread, message, None)
    }
    /// Explicit registration attribution; never changes saved account settings.
    pub fn send_from_device(
        &self,
        thread: Option<&str>,
        message: &str,
        device_id: &str,
    ) -> Result<DispatchReceipt, AccountError> {
        if device_id.is_empty()
            || device_id.len() > 128
            || !device_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(AccountError::new(ErrorCode::InvalidRequest, false));
        }
        self.send_optional_device(thread, message, Some(device_id))
    }
    fn send_optional_device(
        &self,
        thread: Option<&str>,
        message: &str,
        device_id: Option<&str>,
    ) -> Result<DispatchReceipt, AccountError> {
        validate_thread(thread)?;
        if message.trim().is_empty() || message.len() > 512 * 1024 {
            return Err(AccountError::new(ErrorCode::InvalidRequest, false));
        }
        let mut request = serde_json::json!({"op": "send", "thread": thread, "message": message});
        if let Some(device_id) = device_id {
            request["device_id"] = device_id.into();
        }
        let receipt: DispatchReceipt = self.request(request, true)?;
        if receipt.confirmed
            && (!receipt.sent || receipt.message_id.as_deref().is_none_or(str::is_empty))
        {
            return Err(AccountError::new(ErrorCode::ProtocolError, true));
        }
        Ok(receipt)
    }
    fn request<T: DeserializeOwned>(
        &self,
        mut request: serde_json::Value,
        sending: bool,
    ) -> Result<T, AccountError> {
        if !self.config.python.is_absolute()
            || !self.config.script.is_absolute()
            || !self.config.config_dir.is_absolute()
        {
            return Err(AccountError::new(ErrorCode::InvalidRequest, false));
        }
        if !self.config.python.is_file() || !self.config.script.is_file() {
            return Err(AccountError::new(ErrorCode::BridgeUnavailable, false));
        }
        let config_dir = self
            .config
            .config_dir
            .to_str()
            .ok_or_else(|| AccountError::new(ErrorCode::InvalidRequest, false))?;
        request["config_dir"] = serde_json::Value::String(config_dir.to_owned());
        let bytes = serde_json::to_vec(&request)
            .map_err(|_| AccountError::new(ErrorCode::InvalidRequest, false))?;
        if bytes.len() > MAX_REQUEST {
            return Err(AccountError::new(ErrorCode::InvalidRequest, false));
        }
        let output = run_bridge_active(
            &self.config,
            &bytes,
            DEADLINE,
            sending,
            self.active.as_deref(),
        )?;
        let envelope: Envelope<T> = serde_json::from_slice(&output)
            .map_err(|_| AccountError::new(ErrorCode::ProtocolError, sending))?;
        if envelope.ok {
            if envelope.error.is_some() {
                return Err(AccountError::new(ErrorCode::ProtocolError, sending));
            }
            envelope
                .data
                .ok_or_else(|| AccountError::new(ErrorCode::ProtocolError, sending))
        } else {
            Err(envelope
                .error
                .unwrap_or_else(|| AccountError::new(ErrorCode::ProtocolError, sending)))
        }
    }
}

fn validate_thread(thread: Option<&str>) -> Result<(), AccountError> {
    if thread.is_some_and(|s| s.trim().is_empty() || s.len() > 4096 || s.contains('\0')) {
        return Err(AccountError::new(ErrorCode::InvalidRequest, false));
    }
    Ok(())
}

/// Kill the dedicated process group before reaping, including a descendant which
/// might otherwise keep stdout open. No wait/read can outlive the deadline.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        // SAFETY: the child was started as leader of its own process group.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn nonblocking(fd: i32) -> std::io::Result<()> {
    // SAFETY: fcntl accesses a valid pipe descriptor owned by this call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(test)]
fn run_bridge(
    config: &BridgeConfig,
    request: &[u8],
    timeout: Duration,
    sending: bool,
) -> Result<Vec<u8>, AccountError> {
    run_bridge_active(config, request, timeout, sending, None)
}
fn run_bridge_active(
    config: &BridgeConfig,
    request: &[u8],
    timeout: Duration,
    sending: bool,
    active: Option<&AtomicBool>,
) -> Result<Vec<u8>, AccountError> {
    if active.is_some_and(|a| !a.load(Ordering::SeqCst)) {
        return Err(AccountError::new(ErrorCode::InvalidRequest, false));
    }
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
    let child = command
        .env("MUSE_MCP_ENABLE_RAW", "0")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-I")
        .arg(&config.script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| AccountError::new(ErrorCode::BridgeUnavailable, false))?;
    let mut guard = ChildGuard(child);
    let io_error = || AccountError::new(ErrorCode::BridgeFailure, sending);
    let stdin = guard.0.stdin.take().ok_or_else(io_error)?;
    let mut stdout = guard.0.stdout.take().ok_or_else(io_error)?;
    nonblocking(stdin.as_raw_fd()).map_err(|_| io_error())?;
    nonblocking(stdout.as_raw_fd()).map_err(|_| io_error())?;
    let started = Instant::now();
    let mut input = Some(stdin);
    let mut written = 0;
    let mut output = Vec::new();
    let mut eof = false;
    loop {
        if active.is_some_and(|a| !a.load(Ordering::SeqCst)) {
            return Err(AccountError::new(ErrorCode::BridgeFailure, sending));
        }
        if started.elapsed() >= timeout {
            return Err(AccountError::new(ErrorCode::Timeout, sending));
        }
        if let Some(pipe) = input.as_mut() {
            match pipe.write(&request[written..]) {
                Ok(0) => {
                    input = None;
                }
                Ok(n) => {
                    written += n;
                    if written == request.len() {
                        input = None;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    input = None;
                }
                Err(_) => return Err(io_error()),
            }
        }
        // Read only bounded chunks and return to timeout checks even for a flood.
        for _ in 0..16 {
            let mut chunk = [0; 8192];
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    if output.len() + n > MAX_RESPONSE {
                        return Err(AccountError::new(ErrorCode::ResponseTooLarge, sending));
                    }
                    output.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(io_error()),
            }
        }
        if let Some(status) = guard.0.try_wait().map_err(|_| io_error())?
            && eof
        {
            if !status.success() {
                return Err(io_error());
            }
            return Ok(output);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
