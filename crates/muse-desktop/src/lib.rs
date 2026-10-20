//! Portal-based screen context + harmless desktop actions for Muse on Linux.
//!
//! Three operations against the XDG Desktop Portal, all consent-gated by the
//! portal itself: one-shot screenshot, one-shot OpenURI, and a stateful
//! RemoteDesktop input session. The portal/compositor owns approval; this crate
//! does not store approvals or bypass the portal.
//!
//! URLs are caller-supplied and echoed only in the portal approval binding,
//! never logged and never embedded in errors.

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Fail-closed cap on screenshot bytes accepted per call (8 MiB).
pub const MAX_SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;

/// RemoteDesktop device bit for keyboard input.
pub const REMOTE_DEVICE_KEYBOARD: u32 = 1;
/// RemoteDesktop device bit for pointer input.
pub const REMOTE_DEVICE_POINTER: u32 = 2;

const MAX_URL_LEN: usize = 2048;
const MAX_TOKEN_LEN: usize = 255;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_DEADLINE: Duration = Duration::from_secs(20);

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE_SCREENSHOT: &str = "org.freedesktop.portal.Screenshot";
const IFACE_OPEN_URI: &str = "org.freedesktop.portal.OpenURI";
const IFACE_REQUEST: &str = "org.freedesktop.portal.Request";
const DBUS_DEST: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_IFACE: &str = "org.freedesktop.DBus";
const IFACE_REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
const IFACE_SESSION: &str = "org.freedesktop.portal.Session";
const MAX_PARENT_WINDOW_LEN: usize = 255;
const MAX_REMOTE_DELTA: f64 = 20_000.0;
const MAX_REMOTE_STEPS: i32 = 100;
const REMOTE_KEYCODE_MAX: i32 = 0x2ff;
const REMOTE_BUTTON_MIN: i32 = 0x100;
const REMOTE_BUTTON_MAX: i32 = 0x2ff;
const REMOTE_KEYSYM_MAX: i32 = 0x1fff_ffff;
const REMOTE_DEVICE_MASK: u32 = REMOTE_DEVICE_KEYBOARD | REMOTE_DEVICE_POINTER;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Injected session-bus endpoint.
///
/// `UnixSocket` is the production path (e.g. `$XDG_RUNTIME_DIR/bus`).
/// `LoopbackTcp` exists solely for offline tests against in-process fakes and
/// dials `127.0.0.1` only — it cannot address a remote host by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusEndpoint {
    UnixSocket(String),
    LoopbackTcp(u16),
}

impl BusEndpoint {
    pub fn unix_socket(path: &str) -> Self {
        Self::UnixSocket(path.to_string())
    }

    pub fn loopback_tcp(port: u16) -> Self {
        Self::LoopbackTcp(port)
    }
}

/// One-shot screenshot payload. Dimensions are best-effort (parsed from a
/// PNG IHDR when present); `bytes` always holds the full capped payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub bytes: Vec<u8>,
}

/// Typed portal failures. Denial and cancel are distinct variants and never
/// fall back to another capture path. Detail strings are static reasons only:
/// they never contain the URL, token, bus path, or screenshot location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopError {
    PermissionDenied,
    Cancelled,
    UnsupportedPortal(String),
    TooLarge,
    Transport(String),
    InvalidArgument(String),
}

impl fmt::Display for DesktopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => write!(f, "portal request denied"),
            Self::Cancelled => write!(f, "portal request cancelled"),
            Self::UnsupportedPortal(d) => write!(f, "unsupported portal: {d}"),
            Self::TooLarge => write!(f, "screenshot exceeds size cap"),
            Self::Transport(d) => write!(f, "portal transport failure: {d}"),
            Self::InvalidArgument(d) => write!(f, "invalid argument: {d}"),
        }
    }
}

impl std::error::Error for DesktopError {}

/// Namespace for the portal operations. No state, no approval store.
pub struct DesktopPortal;

impl DesktopPortal {
    /// One-shot screenshot through the portal Screenshot interface.
    ///
    /// `token` identifies this request (`[A-Za-z0-9_]{1,255}`; it is embedded
    /// in the portal request path). The portal owns the consent dialog; a
    /// denial or dismiss maps to [`DesktopError::PermissionDenied`] /
    /// [`DesktopError::Cancelled`].
    pub fn screenshot_via(bus: &BusEndpoint, token: &str) -> Result<Screenshot, DesktopError> {
        validate_token(token)?;
        let mut conn = Conn::connect(bus)?;
        conn.hello()?;
        conn.add_match()?;
        let req_path = conn.portal_request(
            IFACE_SCREENSHOT,
            "Screenshot",
            "sa{sv}",
            &screenshot_body(token),
        )?;
        let (code, uri) = conn.wait_response(&req_path)?;
        match code {
            0 => {
                let uri = uri.ok_or_else(|| {
                    DesktopError::Transport("screenshot response missing location".into())
                })?;
                let bytes = read_screenshot_file(&uri)?;
                let (width, height, format) = png_dims(&bytes);
                Ok(Screenshot {
                    width,
                    height,
                    format: format.to_string(),
                    bytes,
                })
            }
            1 => Err(DesktopError::Cancelled),
            _ => Err(DesktopError::PermissionDenied),
        }
    }

    /// Open a caller-supplied URL via the portal OpenURI interface.
    ///
    /// Only `http`/`https` URLs are accepted; everything else is rejected
    /// with [`DesktopError::InvalidArgument`] before any bus traffic. The
    /// portal owns the approval; results are never read (no scraping).
    pub fn open_url_via(bus: &BusEndpoint, url: &str) -> Result<(), DesktopError> {
        validate_url(url)?;
        let mut conn = Conn::connect(bus)?;
        conn.hello()?;
        conn.add_match()?;
        let req_path =
            conn.portal_request(IFACE_OPEN_URI, "OpenURI", "ssa{sv}", &open_uri_body(url))?;
        match conn.wait_response(&req_path)?.0 {
            0 => Ok(()),
            1 => Err(DesktopError::Cancelled),
            _ => Err(DesktopError::PermissionDenied),
        }
    }

    /// Start a consent-gated, stateful RemoteDesktop input session.
    pub fn start_remote_desktop_via(
        bus: &BusEndpoint,
        handle_token: &str,
        session_token: &str,
        parent_window: &str,
        requested_devices: u32,
    ) -> Result<RemoteDesktopSession, DesktopError> {
        Self::start_remote_desktop_via_with_cancel(
            bus,
            handle_token,
            session_token,
            parent_window,
            requested_devices,
            &|| false,
        )
    }

    /// Cancellable variant used by the GTK worker so Stop can interrupt a
    /// consent wait and close any request/session handle already created.
    pub fn start_remote_desktop_via_with_cancel(
        bus: &BusEndpoint,
        handle_token: &str,
        session_token: &str,
        parent_window: &str,
        requested_devices: u32,
        cancel: &dyn Fn() -> bool,
    ) -> Result<RemoteDesktopSession, DesktopError> {
        validate_token(handle_token)?;
        validate_token(session_token)?;
        validate_parent_window(parent_window)?;
        validate_requested_devices(requested_devices)?;

        let mut conn = Conn::connect(bus)?;
        conn.hello_with_cancel(cancel)?;
        conn.add_match_with_cancel(cancel)?;

        let create_request = conn.portal_request_with_cancel(
            IFACE_REMOTE_DESKTOP,
            "CreateSession",
            "a{sv}",
            &remote_create_body(handle_token, session_token),
            cancel,
        )?;
        let create = match conn.wait_remote_response_with_cancel(&create_request, cancel) {
            Ok(response) => response,
            Err(error) => {
                conn.close_request(&create_request);
                return Err(error);
            }
        };
        if create.code != 0 {
            conn.close_request(&create_request);
            return Err(response_code_error(create.code));
        }
        let session_handle = match create.session_handle {
            Some(handle) => handle,
            None => {
                conn.close_request(&create_request);
                return Err(DesktopError::Transport(
                    "remote session response missing session_handle".into(),
                ));
            }
        };
        if let Err(error) = validate_object_path(&session_handle) {
            conn.close_request(&create_request);
            return Err(error);
        }
        if cancel() {
            conn.close_session(&session_handle);
            return Err(DesktopError::Cancelled);
        }

        let select_request = match conn.portal_request_with_cancel(
            IFACE_REMOTE_DESKTOP,
            "SelectDevices",
            "oa{sv}",
            &remote_select_body(&session_handle, handle_token, requested_devices),
            cancel,
        ) {
            Ok(request) => request,
            Err(error) => {
                conn.close_session(&session_handle);
                return Err(error);
            }
        };
        let select = match conn.wait_remote_response_with_cancel(&select_request, cancel) {
            Ok(response) => response,
            Err(error) => {
                conn.close_request(&select_request);
                conn.close_session(&session_handle);
                return Err(error);
            }
        };
        if select.code != 0 {
            conn.close_request(&select_request);
            conn.close_session(&session_handle);
            return Err(response_code_error(select.code));
        }

        let start_request = match conn.portal_request_with_cancel(
            IFACE_REMOTE_DESKTOP,
            "Start",
            "osa{sv}",
            &remote_start_body(&session_handle, handle_token, parent_window),
            cancel,
        ) {
            Ok(request) => request,
            Err(error) => {
                conn.close_session(&session_handle);
                return Err(error);
            }
        };
        let start = match conn.wait_remote_response_with_cancel(&start_request, cancel) {
            Ok(response) => response,
            Err(error) => {
                conn.close_request(&start_request);
                conn.close_session(&session_handle);
                return Err(error);
            }
        };
        if start.code != 0 {
            conn.close_request(&start_request);
            conn.close_session(&session_handle);
            return Err(response_code_error(start.code));
        }
        let granted_devices = match start.devices {
            Some(devices) => devices,
            None => {
                conn.close_request(&start_request);
                conn.close_session(&session_handle);
                return Err(DesktopError::Transport(
                    "remote start response missing devices".into(),
                ));
            }
        };
        validate_granted_devices(requested_devices, granted_devices)
            .inspect_err(|_| conn.close_session(&session_handle))?;

        Ok(RemoteDesktopSession {
            conn: Some(conn),
            session_handle,
            granted_devices,
            stopped: false,
        })
    }
}

/// A consented XDG RemoteDesktop input session.
pub struct RemoteDesktopSession {
    conn: Option<Conn>,
    session_handle: String,
    granted_devices: u32,
    stopped: bool,
}

impl RemoteDesktopSession {
    pub fn granted_devices(&self) -> u32 {
        self.granted_devices
    }

    pub fn pointer_motion(&mut self, dx: f64, dy: f64) -> Result<(), DesktopError> {
        validate_finite(dx, "dx")?;
        validate_finite(dy, "dy")?;
        self.require_input(REMOTE_DEVICE_POINTER)?;
        let mut w = W::new();
        w.object(&self.session_handle);
        w.dict_strs(&[]);
        w.f64(dx);
        w.f64(dy);
        self.notify("NotifyPointerMotion", "oa{sv}dd", &w.b)
    }

    pub fn pointer_button(&mut self, button: i32, state: u32) -> Result<(), DesktopError> {
        validate_button(button)?;
        validate_key_state(state)?;
        self.require_input(REMOTE_DEVICE_POINTER)?;
        let mut w = W::new();
        w.object(&self.session_handle);
        w.dict_strs(&[]);
        w.i32(button);
        w.u32(state);
        self.notify("NotifyPointerButton", "oa{sv}iu", &w.b)
    }

    pub fn pointer_axis(&mut self, dx: f64, dy: f64, finish: bool) -> Result<(), DesktopError> {
        validate_finite(dx, "dx")?;
        validate_finite(dy, "dy")?;
        self.require_input(REMOTE_DEVICE_POINTER)?;
        let mut w = W::new();
        w.object(&self.session_handle);
        w.dict_bool("finish", finish);
        w.f64(dx);
        w.f64(dy);
        self.notify("NotifyPointerAxis", "oa{sv}dd", &w.b)
    }

    pub fn pointer_axis_discrete(&mut self, axis: u32, steps: i32) -> Result<(), DesktopError> {
        validate_axis(axis)?;
        if steps == 0 || steps.unsigned_abs() > MAX_REMOTE_STEPS as u32 {
            return Err(DesktopError::InvalidArgument(
                "steps must be nonzero".into(),
            ));
        }
        self.require_input(REMOTE_DEVICE_POINTER)?;
        let mut w = W::new();
        w.object(&self.session_handle);
        w.dict_strs(&[]);
        w.u32(axis);
        w.i32(steps);
        self.notify("NotifyPointerAxisDiscrete", "oa{sv}ui", &w.b)
    }

    pub fn keyboard_keysym(&mut self, keysym: i32, state: u32) -> Result<(), DesktopError> {
        validate_keysym(keysym)?;
        validate_key_state(state)?;
        self.require_input(REMOTE_DEVICE_KEYBOARD)?;
        let mut w = W::new();
        w.object(&self.session_handle);
        w.dict_strs(&[]);
        w.i32(keysym);
        w.u32(state);
        self.notify("NotifyKeyboardKeysym", "oa{sv}iu", &w.b)
    }

    pub fn keyboard_keycode(&mut self, keycode: i32, state: u32) -> Result<(), DesktopError> {
        validate_keycode(keycode)?;
        validate_key_state(state)?;
        self.require_input(REMOTE_DEVICE_KEYBOARD)?;
        let mut w = W::new();
        w.object(&self.session_handle);
        w.dict_strs(&[]);
        w.i32(keycode);
        w.u32(state);
        self.notify("NotifyKeyboardKeycode", "oa{sv}iu", &w.b)
    }

    pub fn stop(&mut self) -> Result<(), DesktopError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        let Some(mut conn) = self.conn.take() else {
            return Ok(());
        };
        let result = conn
            .transact(
                PORTAL_DEST,
                &self.session_handle,
                IFACE_SESSION,
                "Close",
                "",
                &[],
            )
            .and_then(|reply| {
                if reply.ty == 3 {
                    Err(map_dbus_error(reply.err.as_deref().unwrap_or("")))
                } else {
                    Ok(())
                }
            });
        drop(conn);
        result
    }

    fn require_input(&self, device: u32) -> Result<(), DesktopError> {
        if self.stopped || self.conn.is_none() {
            return Err(DesktopError::InvalidArgument(
                "remote session is stopped".into(),
            ));
        }
        if self.granted_devices & device == 0 {
            return Err(DesktopError::PermissionDenied);
        }
        Ok(())
    }

    fn notify(&mut self, member: &str, sig: &str, body: &[u8]) -> Result<(), DesktopError> {
        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| DesktopError::InvalidArgument("remote session is stopped".into()))?;
        let reply = conn.transact(
            PORTAL_DEST,
            &self.session_handle,
            IFACE_REMOTE_DESKTOP,
            member,
            sig,
            body,
        )?;
        if reply.ty == 3 {
            Err(map_dbus_error(reply.err.as_deref().unwrap_or("")))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Validation (run before any I/O)
// ---------------------------------------------------------------------------

fn validate_url(url: &str) -> Result<(), DesktopError> {
    if url.is_empty() || url.len() > MAX_URL_LEN {
        return Err(DesktopError::InvalidArgument(
            "url length out of range".into(),
        ));
    }
    if url.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(DesktopError::InvalidArgument(
            "url contains whitespace or control characters".into(),
        ));
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(DesktopError::InvalidArgument(
            "only http(s) urls can be opened".into(),
        ));
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Err(DesktopError::InvalidArgument(
            "only http(s) urls can be opened".into(),
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(DesktopError::InvalidArgument(
            "url has no valid host".into(),
        ));
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), DesktopError> {
    if token.is_empty() || token.len() > MAX_TOKEN_LEN {
        return Err(DesktopError::InvalidArgument(
            "token length out of range".into(),
        ));
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(DesktopError::InvalidArgument(
            "token must be [A-Za-z0-9_]+".into(),
        ));
    }
    Ok(())
}

fn validate_parent_window(parent_window: &str) -> Result<(), DesktopError> {
    if parent_window.len() > MAX_PARENT_WINDOW_LEN
        || parent_window
            .bytes()
            .any(|b| b == 0 || b < 0x20 || b == 0x7f)
    {
        return Err(DesktopError::InvalidArgument(
            "parent window is malformed".into(),
        ));
    }
    Ok(())
}

fn validate_requested_devices(devices: u32) -> Result<(), DesktopError> {
    if devices == 0 || devices & !REMOTE_DEVICE_MASK != 0 {
        return Err(DesktopError::InvalidArgument(
            "requested devices must be keyboard and/or pointer".into(),
        ));
    }
    Ok(())
}

fn validate_granted_devices(requested: u32, granted: u32) -> Result<(), DesktopError> {
    if granted == 0 || granted & !REMOTE_DEVICE_MASK != 0 || granted & !requested != 0 {
        return Err(DesktopError::Transport(
            "remote start returned invalid devices".into(),
        ));
    }
    Ok(())
}

fn validate_object_path(path: &str) -> Result<(), DesktopError> {
    if path.is_empty()
        || path.len() > MAX_TOKEN_LEN
        || !path.starts_with('/')
        || path.ends_with('/')
        || path.split('/').skip(1).any(|part| {
            part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
    {
        return Err(DesktopError::Transport(
            "remote session handle is malformed".into(),
        ));
    }
    Ok(())
}

fn validate_finite(value: f64, name: &str) -> Result<(), DesktopError> {
    if !value.is_finite() || value.abs() > MAX_REMOTE_DELTA {
        return Err(DesktopError::InvalidArgument(format!(
            "{name} must be finite and bounded"
        )));
    }
    Ok(())
}

fn validate_button(button: i32) -> Result<(), DesktopError> {
    if !(REMOTE_BUTTON_MIN..=REMOTE_BUTTON_MAX).contains(&button) {
        return Err(DesktopError::InvalidArgument(
            "button must be positive".into(),
        ));
    }
    Ok(())
}

fn validate_key_state(state: u32) -> Result<(), DesktopError> {
    if state > 1 {
        return Err(DesktopError::InvalidArgument("state must be 0 or 1".into()));
    }
    Ok(())
}

fn validate_axis(axis: u32) -> Result<(), DesktopError> {
    if axis > 1 {
        return Err(DesktopError::InvalidArgument("axis must be 0 or 1".into()));
    }
    Ok(())
}

fn validate_keysym(keysym: i32) -> Result<(), DesktopError> {
    if !(0..=REMOTE_KEYSYM_MAX).contains(&keysym) {
        return Err(DesktopError::InvalidArgument(
            "keysym must be nonnegative".into(),
        ));
    }
    Ok(())
}

fn validate_keycode(keycode: i32) -> Result<(), DesktopError> {
    if !(0..=REMOTE_KEYCODE_MAX).contains(&keycode) {
        return Err(DesktopError::InvalidArgument(
            "keycode must be nonnegative".into(),
        ));
    }
    Ok(())
}

fn response_code_error(code: u32) -> DesktopError {
    match code {
        1 => DesktopError::Cancelled,
        _ => DesktopError::PermissionDenied,
    }
}

fn remote_create_body(handle_token: &str, session_token: &str) -> Vec<u8> {
    let mut w = W::new();
    w.dict_strs(&[
        ("handle_token", handle_token),
        ("session_handle_token", session_token),
    ]);
    w.b
}

fn remote_select_body(session_handle: &str, handle_token: &str, requested_devices: u32) -> Vec<u8> {
    let mut w = W::new();
    w.object(session_handle);
    let mut entries = W::new();
    entries.align(8);
    entries.text("handle_token");
    entries.sig("s");
    entries.text(handle_token);
    entries.align(8);
    entries.text("types");
    entries.sig("u");
    entries.u32(requested_devices);
    w.u32(entries.b.len() as u32);
    w.align(8);
    w.b.extend_from_slice(&entries.b);
    w.b
}

fn remote_start_body(session_handle: &str, handle_token: &str, parent_window: &str) -> Vec<u8> {
    let mut w = W::new();
    w.object(session_handle);
    w.text(parent_window);
    w.dict_strs(&[("handle_token", handle_token)]);
    w.b
}

// ---------------------------------------------------------------------------
// Screenshot payload helpers
// ---------------------------------------------------------------------------

fn uri_to_path(uri: &str) -> Result<String, DesktopError> {
    let after = uri
        .strip_prefix("file://")
        .ok_or_else(|| DesktopError::Transport("unsupported screenshot location".into()))?;
    let path = after.strip_prefix("localhost/").unwrap_or(after);
    if !path.starts_with('/') {
        return Err(DesktopError::Transport(
            "unsupported screenshot location".into(),
        ));
    }
    let mut out = Vec::with_capacity(path.len());
    let mut bytes = path.as_bytes().iter();
    while let Some(&b) = bytes.next() {
        if b == b'%' {
            let bad_location = || DesktopError::Transport("unsupported screenshot location".into());
            let hi = bytes.next().copied().ok_or_else(bad_location)?;
            let lo = bytes.next().copied().ok_or_else(bad_location)?;
            let hex = |c: u8| {
                (c as char).to_digit(16).map(|d| d as u8).ok_or_else(|| {
                    DesktopError::Transport("unsupported screenshot location".into())
                })
            };
            let v = (hex(hi)? << 4) | hex(lo)?;
            if v == 0 {
                return Err(DesktopError::Transport(
                    "unsupported screenshot location".into(),
                ));
            }
            out.push(v);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out)
        .map_err(|_| DesktopError::Transport("unsupported screenshot location".into()))
}

fn read_screenshot_file(uri: &str) -> Result<Vec<u8>, DesktopError> {
    let path = uri_to_path(uri)?;
    let unavailable = || DesktopError::Transport("screenshot unavailable".into());
    let md = std::fs::metadata(&path).map_err(|_| unavailable())?;
    // Fail closed on non-regular files: opening a FIFO with no writer would
    // block the worker thread indefinitely (no deadline covers file I/O).
    if !md.file_type().is_file() {
        return Err(unavailable());
    }
    if md.len() > MAX_SCREENSHOT_BYTES as u64 {
        return Err(DesktopError::TooLarge);
    }
    let file =
        File::open(&path).map_err(|_| DesktopError::Transport("screenshot unavailable".into()))?;
    let mut bytes = Vec::new();
    file.take(MAX_SCREENSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| DesktopError::Transport("screenshot unreadable".into()))?;
    if bytes.len() > MAX_SCREENSHOT_BYTES {
        return Err(DesktopError::TooLarge);
    }
    Ok(bytes)
}

/// Best-effort `(width, height, format)` from a PNG IHDR; anything else is
/// `("unknown", 0, 0)` with the bytes still returned to the caller.
fn png_dims(bytes: &[u8]) -> (u32, u32, &'static str) {
    const MAGIC: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if bytes.len() >= 33 && bytes[..8] == MAGIC && bytes[12..16] == *b"IHDR" {
        let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap_or([0; 4]));
        let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap_or([0; 4]));
        (w, h, "png")
    } else {
        (0, 0, "unknown")
    }
}

// ---------------------------------------------------------------------------
// Minimal D-Bus wire protocol (little-endian subset)
// ---------------------------------------------------------------------------

struct W {
    b: Vec<u8>,
}

impl W {
    fn new() -> Self {
        Self { b: Vec::new() }
    }

    fn align(&mut self, n: usize) {
        while !self.b.len().is_multiple_of(n) {
            self.b.push(0);
        }
    }

    fn u8(&mut self, v: u8) {
        self.b.push(v);
    }

    fn u32(&mut self, v: u32) {
        self.align(4);
        self.b.extend_from_slice(&v.to_le_bytes());
    }

    fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }

    fn f64(&mut self, v: f64) {
        self.align(8);
        self.b.extend_from_slice(&v.to_le_bytes());
    }

    fn text(&mut self, s: &str) {
        self.align(4);
        self.b.extend_from_slice(&(s.len() as u32).to_le_bytes());
        self.b.extend_from_slice(s.as_bytes());
        self.b.push(0);
    }

    fn object(&mut self, s: &str) {
        self.text(s);
    }

    fn sig(&mut self, s: &str) {
        self.b.push(s.len() as u8);
        self.b.extend_from_slice(s.as_bytes());
        self.b.push(0);
    }

    /// Dict-entry array `a{sv}` with string values, appended in place so
    /// alignment stays relative to the enclosing buffer.
    fn dict_strs(&mut self, entries: &[(&str, &str)]) {
        let mut e = W::new();
        for (k, v) in entries {
            e.align(8);
            e.text(k);
            e.sig("s");
            e.text(v);
        }
        self.u32(e.b.len() as u32);
        self.align(8);
        self.b.extend_from_slice(&e.b);
    }

    #[allow(dead_code)]
    fn dict_objects(&mut self, entries: &[(&str, &str)]) {
        let mut e = W::new();
        for (k, v) in entries {
            e.align(8);
            e.text(k);
            e.sig("o");
            e.object(v);
        }
        self.u32(e.b.len() as u32);
        self.align(8);
        self.b.extend_from_slice(&e.b);
    }

    fn dict_bool(&mut self, key: &str, value: bool) {
        let mut e = W::new();
        e.align(8);
        e.text(key);
        e.sig("b");
        e.u32(u32::from(value));
        self.u32(e.b.len() as u32);
        self.align(8);
        self.b.extend_from_slice(&e.b);
    }
}

fn screenshot_body(token: &str) -> Vec<u8> {
    let mut w = W::new();
    w.text("");
    w.dict_strs(&[("handle_token", token)]);
    w.b
}

fn open_uri_body(url: &str) -> Vec<u8> {
    let mut w = W::new();
    w.text("");
    w.text(url);
    w.dict_strs(&[]);
    w.b
}

fn rule_body(rule: &str) -> Vec<u8> {
    let mut w = W::new();
    w.text(rule);
    w.b
}

// Header field codes.
const F_PATH: u8 = 1;
const F_IFACE: u8 = 2;
const F_MEMBER: u8 = 3;
const F_ERROR: u8 = 4;
const F_REPLY: u8 = 5;
const F_DEST: u8 = 6;
const F_SENDER: u8 = 7;
const F_SIG: u8 = 8;

enum FV<'a> {
    S(&'a str),
    O(&'a str),
    /// Only encoded by the `#[cfg(test)]` fake bus (reply serials); kept on
    /// the shared encoder so fakes speak identical framing.
    #[allow(dead_code)]
    U(u32),
    G(&'a str),
}

fn encode_message(ty: u8, serial: u32, fields: &[(u8, FV<'_>)], body: &[u8]) -> Vec<u8> {
    let mut f = W::new();
    for (code, v) in fields {
        f.align(8);
        f.u8(*code);
        match v {
            FV::S(s) => {
                f.sig("s");
                f.text(s);
            }
            FV::O(s) => {
                f.sig("o");
                f.text(s);
            }
            FV::U(n) => {
                f.sig("u");
                f.u32(*n);
            }
            FV::G(s) => {
                f.sig("g");
                f.sig(s);
            }
        }
    }
    // Fields buffer base (offset 16) is 8-aligned, so its internal alignment
    // matches message-relative alignment.
    let mut m = vec![b'l', ty, 0, 1];
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(&serial.to_le_bytes());
    m.extend_from_slice(&(f.b.len() as u32).to_le_bytes());
    debug_assert_eq!(m.len(), 16);
    m.extend_from_slice(&f.b);
    while m.len() % 8 != 0 {
        m.push(0);
    }
    m.extend_from_slice(body);
    m
}

struct R<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, p: 0 }
    }

    fn fail() -> DesktopError {
        DesktopError::Transport("malformed bus message".into())
    }

    fn align(&mut self, n: usize) -> Result<(), DesktopError> {
        let adv = (n - (self.p % n)) % n;
        self.p = self.p.checked_add(adv).ok_or_else(Self::fail)?;
        if self.p > self.d.len() {
            return Err(Self::fail());
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, DesktopError> {
        let b = *self.d.get(self.p).ok_or_else(Self::fail)?;
        self.p += 1;
        Ok(b)
    }

    fn u32(&mut self) -> Result<u32, DesktopError> {
        self.align(4)?;
        let end = self.p.checked_add(4).ok_or_else(Self::fail)?;
        if end > self.d.len() {
            return Err(Self::fail());
        }
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&self.d[self.p..end]);
        self.p = end;
        Ok(u32::from_le_bytes(raw))
    }

    fn text(&mut self) -> Result<String, DesktopError> {
        self.align(4)?;
        let len = self.u32()? as usize;
        let end = self.p.checked_add(len).ok_or_else(Self::fail)?;
        if end >= self.d.len() || self.d[end] != 0 {
            return Err(Self::fail());
        }
        let s = std::str::from_utf8(&self.d[self.p..end]).map_err(|_| Self::fail())?;
        let out = s.to_string();
        self.p = end + 1;
        Ok(out)
    }

    fn sigstr(&mut self) -> Result<String, DesktopError> {
        let len = self.u8()? as usize;
        let end = self.p.checked_add(len).ok_or_else(Self::fail)?;
        if end >= self.d.len() || self.d[end] != 0 {
            return Err(Self::fail());
        }
        let s = std::str::from_utf8(&self.d[self.p..end]).map_err(|_| Self::fail())?;
        let out = s.to_string();
        self.p = end + 1;
        Ok(out)
    }

    /// Skip one value of the given single complete type signature.
    fn skip_value(&mut self, sig: &str) -> Result<(), DesktopError> {
        match sig {
            "s" | "o" => {
                self.text().map(|_| ())?;
            }
            "g" => {
                self.sigstr().map(|_| ())?;
            }
            "u" | "i" | "b" => {
                self.u32().map(|_| ())?;
            }
            "d" => {
                self.align(8)?;
                let end = self.p.checked_add(8).ok_or_else(Self::fail)?;
                if end > self.d.len() {
                    return Err(Self::fail());
                }
                self.p = end;
            }
            "y" => {
                self.u8().map(|_| ())?;
            }
            "v" => {
                let inner = self.sigstr()?;
                self.skip_value(&inner)?;
            }
            "as" | "ao" => {
                let len = self.u32()? as usize;
                let end = self.p.checked_add(len).ok_or_else(Self::fail)?;
                while self.p < end {
                    self.text().map(|_| ())?;
                }
                if self.p != end {
                    return Err(Self::fail());
                }
            }
            "ay" => {
                let len = self.u32()? as usize;
                self.p = self.p.checked_add(len).ok_or_else(Self::fail)?;
                if self.p > self.d.len() {
                    return Err(Self::fail());
                }
            }
            "a{sv}" => {
                let len = self.u32()? as usize;
                let end = self.p.checked_add(len).ok_or_else(Self::fail)?;
                while self.p < end {
                    self.align(8)?;
                    self.text().map(|_| ())?;
                    let inner = self.sigstr()?;
                    self.skip_value(&inner)?;
                }
                if self.p != end {
                    return Err(Self::fail());
                }
            }
            _ => return Err(Self::fail()),
        }
        Ok(())
    }
}

struct Msg {
    ty: u8,
    /// Read by the `#[cfg(test)]` fake bus to address replies; the client
    /// itself keys replies off `reply_to`.
    #[allow(dead_code)]
    serial: u32,
    reply_to: Option<u32>,
    path: Option<String>,
    iface: Option<String>,
    member: Option<String>,
    sender: Option<String>,
    err: Option<String>,
    sig: String,
    body: Vec<u8>,
}

fn decode_message(buf: &[u8]) -> Result<Msg, DesktopError> {
    if buf.len() < 16 || buf[0] != b'l' || buf[3] != 1 {
        return Err(DesktopError::Transport("unsupported bus message".into()));
    }
    let body_len = u32::from_le_bytes(buf[4..8].try_into().unwrap_or([0; 4])) as usize;
    let serial = u32::from_le_bytes(buf[8..12].try_into().unwrap_or([0; 4]));
    let fields_len = u32::from_le_bytes(buf[12..16].try_into().unwrap_or([0; 4])) as usize;
    let fields_end = 16 + fields_len;
    let header_end = fields_end + ((8 - (fields_end % 8)) % 8);
    if buf.len() < header_end + body_len {
        return Err(DesktopError::Transport("truncated bus message".into()));
    }
    let mut r = R::new(&buf[16..fields_end]);
    let mut msg = Msg {
        ty: buf[1],
        serial,
        reply_to: None,
        path: None,
        iface: None,
        member: None,
        sender: None,
        err: None,
        sig: String::new(),
        body: buf[header_end..header_end + body_len].to_vec(),
    };
    while r.p < r.d.len() {
        r.align(8)?;
        let code = r.u8()?;
        let vsig = r.sigstr()?;
        match (code, vsig.as_str()) {
            (F_PATH, "o") => msg.path = Some(r.text()?),
            (F_IFACE, "s") => msg.iface = Some(r.text()?),
            (F_MEMBER, "s") => msg.member = Some(r.text()?),
            (F_ERROR, "s") => msg.err = Some(r.text()?),
            (F_REPLY, "u") => msg.reply_to = Some(r.u32()?),
            (F_SENDER, "s") => msg.sender = Some(r.text()?),
            (F_SIG, "g") => msg.sig = r.sigstr()?,
            (F_DEST, _) => {
                r.skip_value(&vsig)?;
            }
            _ => return Err(DesktopError::Transport("unknown bus header field".into())),
        }
    }
    Ok(msg)
}

fn validate_response_signature(signature: &str) -> Result<(), DesktopError> {
    if signature == "ua{sv}" {
        Ok(())
    } else {
        Err(DesktopError::Transport(
            "portal response returned wrong signature".into(),
        ))
    }
}

fn matching_response(
    message: &Msg,
    portal_sender: Option<&str>,
    request_path: &str,
) -> Result<bool, DesktopError> {
    if message.ty == 4
        && portal_sender == message.sender.as_deref()
        && message.iface.as_deref() == Some(IFACE_REQUEST)
        && message.member.as_deref() == Some("Response")
        && message.path.as_deref() == Some(request_path)
    {
        validate_response_signature(&message.sig)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Decode a `Response(u, a{sv})` body into the response code plus the `uri`
/// result when present. All other results are skipped, never surfaced.
fn decode_response(body: &[u8]) -> Result<(u32, Option<String>), DesktopError> {
    let mut r = R::new(body);
    let code = r.u32()?;
    r.align(4)?;
    let len = r.u32()? as usize;
    let end = r.p.checked_add(len).ok_or_else(R::fail)?;
    if end > r.d.len() {
        return Err(R::fail());
    }
    let mut uri = None;
    while r.p < end {
        r.align(8)?;
        let key = r.text()?;
        let vsig = r.sigstr()?;
        if key == "uri" {
            if vsig != "s" || uri.is_some() {
                return Err(R::fail());
            }
            uri = Some(r.text()?);
        } else {
            r.skip_value(&vsig)?;
        }
    }
    if r.p != end || end != r.d.len() {
        return Err(R::fail());
    }
    Ok((code, uri))
}

struct RemoteResponse {
    code: u32,
    session_handle: Option<String>,
    devices: Option<u32>,
}

fn decode_remote_response(body: &[u8]) -> Result<RemoteResponse, DesktopError> {
    let mut r = R::new(body);
    let code = r.u32()?;
    r.align(4)?;
    let len = r.u32()? as usize;
    let end = r.p.checked_add(len).ok_or_else(R::fail)?;
    if end > r.d.len() {
        return Err(R::fail());
    }
    let mut session_handle = None;
    let mut devices = None;
    while r.p < end {
        r.align(8)?;
        let key = r.text()?;
        let vsig = r.sigstr()?;
        match key.as_str() {
            "session_handle" => {
                if vsig != "s" || session_handle.is_some() {
                    return Err(R::fail());
                }
                session_handle = Some(r.text()?);
            }
            "devices" => {
                if vsig != "u" || devices.is_some() {
                    return Err(R::fail());
                }
                devices = Some(r.u32()?);
            }
            _ => r.skip_value(&vsig)?,
        }
    }
    if r.p != end || end != r.d.len() {
        return Err(R::fail());
    }
    Ok(RemoteResponse {
        code,
        session_handle,
        devices,
    })
}

/// Map a D-Bus error name to a typed error. Matches on the last dotted
/// segment with equality, never substrings: a custom name like
/// `org.example.NotCancelledEver` must not read as a cancel.
fn map_dbus_error(name: &str) -> DesktopError {
    let leaf = name.rsplit('.').next().unwrap_or(name);
    if matches!(leaf, "Cancelled" | "Canceled") {
        DesktopError::Cancelled
    } else if matches!(
        leaf,
        "NotAllowed" | "AccessDenied" | "PermissionDenied" | "NotAuthorized"
    ) {
        DesktopError::PermissionDenied
    } else if matches!(
        leaf,
        "UnknownMethod"
            | "UnknownInterface"
            | "UnknownObject"
            | "ServiceUnknown"
            | "NameHasNoOwner"
            | "NotSupported"
    ) {
        DesktopError::UnsupportedPortal("portal interface unavailable".into())
    } else {
        DesktopError::Transport("portal call failed".into())
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

enum Stream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        match self {
            Self::Unix(s) => s.read_exact(buf),
            Self::Tcp(s) => s.read_exact(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Unix(s) => s.write_all(buf),
            Self::Tcp(s) => s.write_all(buf),
        }
    }

    fn set_timeouts(&self) -> std::io::Result<()> {
        match self {
            Self::Unix(s) => {
                s.set_read_timeout(Some(IO_TIMEOUT))?;
                s.set_write_timeout(Some(IO_TIMEOUT))
            }
            Self::Tcp(s) => {
                s.set_read_timeout(Some(IO_TIMEOUT))?;
                s.set_write_timeout(Some(IO_TIMEOUT))
            }
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

struct Conn {
    s: Stream,
    serial: u32,
    pending: Vec<Msg>,
    portal_sender: Option<String>,
}

fn external_uid_hex(uid: &str) -> String {
    uid.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn uid_hex() -> String {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                if let Some(uid) = rest.split_whitespace().next() {
                    if uid.bytes().all(|byte| byte.is_ascii_digit()) {
                        return external_uid_hex(uid);
                    }
                }
            }
        }
    }
    external_uid_hex("0")
}

impl Conn {
    fn connect(bus: &BusEndpoint) -> Result<Self, DesktopError> {
        let stream = match bus {
            BusEndpoint::UnixSocket(path) => UnixStream::connect(path)
                .map(Stream::Unix)
                .map_err(|_| DesktopError::Transport("cannot reach session bus".into()))?,
            BusEndpoint::LoopbackTcp(port) => TcpStream::connect(format!("127.0.0.1:{port}"))
                .map(Stream::Tcp)
                .map_err(|_| DesktopError::Transport("cannot reach session bus".into()))?,
        };
        stream
            .set_timeouts()
            .map_err(|_| DesktopError::Transport("cannot configure session bus".into()))?;
        let mut conn = Self {
            s: stream,
            serial: 0,
            pending: Vec::new(),
            portal_sender: None,
        };
        conn.auth()?;
        Ok(conn)
    }

    fn send_bytes(&mut self, bytes: &[u8]) -> Result<(), DesktopError> {
        self.s
            .write_all(bytes)
            .map_err(|_| DesktopError::Transport("bus write failed".into()))
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), DesktopError> {
        match self.s.read_exact(buf) {
            Ok(()) => Ok(()),
            Err(e) if is_timeout(&e) => Err(DesktopError::Transport("bus read timeout".into())),
            Err(_) => Err(DesktopError::Transport("bus read failed".into())),
        }
    }

    fn read_line(&mut self) -> Result<String, DesktopError> {
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            self.read_exact(&mut b)?;
            line.push(b[0]);
            if b[0] == b'\n' {
                break;
            }
            if line.len() > 4096 {
                return Err(DesktopError::Transport("bus auth rejected".into()));
            }
        }
        String::from_utf8(line).map_err(|_| DesktopError::Transport("bus auth rejected".into()))
    }

    /// SASL `AUTH EXTERNAL`. Only EXTERNAL is implemented; the fake bus in
    /// tests accepts any uid, a live daemon checks it against the peer.
    fn auth(&mut self) -> Result<(), DesktopError> {
        let mut first = vec![0u8];
        first.extend_from_slice(format!("AUTH EXTERNAL {}\r\n", uid_hex()).as_bytes());
        self.send_bytes(&first)?;
        let ok = self.read_line()?;
        if ok != "OK" && !ok.starts_with("OK ") {
            return Err(DesktopError::Transport("bus auth rejected".into()));
        }
        self.send_bytes(b"BEGIN\r\n")?;
        Ok(())
    }

    /// Read one message; `Ok(None)` on read timeout so callers can enforce
    /// their own overall deadline.
    fn read_message(&mut self) -> Result<Option<Msg>, DesktopError> {
        let mut head = [0u8; 16];
        match self.s.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if is_timeout(&e) => return Ok(None),
            Err(_) => return Err(DesktopError::Transport("bus read failed".into())),
        }
        if head[0] != b'l' {
            return Err(DesktopError::Transport("unsupported bus byte order".into()));
        }
        let body_len = u32::from_le_bytes(head[4..8].try_into().unwrap_or([0; 4])) as usize;
        let fields_len = u32::from_le_bytes(head[12..16].try_into().unwrap_or([0; 4])) as usize;
        if body_len > 64 * 1024 * 1024 || fields_len > 1024 * 1024 {
            // Bus framing cap, not the screenshot cap: report Transport so
            // TooLarge keeps its documented screenshot-only meaning.
            return Err(DesktopError::Transport("oversized bus message".into()));
        }
        let fields_end = 16 + fields_len;
        let header_end = fields_end + ((8 - (fields_end % 8)) % 8);
        let mut rest = vec![0u8; header_end - 16 + body_len];
        match self.s.read_exact(&mut rest) {
            Ok(()) => {}
            Err(e) if is_timeout(&e) => return Ok(None),
            Err(_) => return Err(DesktopError::Transport("bus read failed".into())),
        }
        let mut buf = head.to_vec();
        buf.extend_from_slice(&rest);
        decode_message(&buf).map(Some)
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    /// Send a method call and block (until the deadline) for its reply.
    /// Signals arriving meanwhile are stashed for [`Conn::wait_response`].
    fn transact(
        &mut self,
        dest: &str,
        path: &str,
        iface: &str,
        member: &str,
        body_sig: &str,
        body: &[u8],
    ) -> Result<Msg, DesktopError> {
        self.transact_with_cancel(dest, path, iface, member, body_sig, body, &|| false)
    }

    #[allow(clippy::too_many_arguments)]
    fn transact_with_cancel(
        &mut self,
        dest: &str,
        path: &str,
        iface: &str,
        member: &str,
        body_sig: &str,
        body: &[u8],
        cancel: &dyn Fn() -> bool,
    ) -> Result<Msg, DesktopError> {
        let serial = self.next_serial();
        let mut fields = vec![
            (F_DEST, FV::S(dest)),
            (F_PATH, FV::O(path)),
            (F_IFACE, FV::S(iface)),
            (F_MEMBER, FV::S(member)),
        ];
        if !body_sig.is_empty() {
            fields.push((F_SIG, FV::G(body_sig)));
        }
        self.send_bytes(&encode_message(1, serial, &fields, body))?;
        let deadline = Instant::now() + RESPONSE_DEADLINE;
        loop {
            if cancel() {
                return Err(DesktopError::Cancelled);
            }
            match self.read_message()? {
                None => {
                    if cancel() {
                        return Err(DesktopError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(DesktopError::Transport("portal call timed out".into()));
                    }
                }
                Some(msg) if (msg.ty == 2 || msg.ty == 3) && msg.reply_to == Some(serial) => {
                    return Ok(msg);
                }
                Some(msg) => self.pending.push(msg),
            }
        }
    }

    fn hello(&mut self) -> Result<(), DesktopError> {
        self.hello_with_cancel(&|| false)
    }

    fn hello_with_cancel(&mut self, cancel: &dyn Fn() -> bool) -> Result<(), DesktopError> {
        let reply =
            self.transact_with_cancel(DBUS_DEST, DBUS_PATH, DBUS_IFACE, "Hello", "", &[], cancel)?;
        if reply.ty == 3 {
            return Err(map_dbus_error(reply.err.as_deref().unwrap_or("")));
        }
        if reply.sig != "s" {
            return Err(DesktopError::Transport(
                "bus hello returned wrong signature".into(),
            ));
        }
        let mut r = R::new(&reply.body);
        let unique_name = r
            .text()
            .map_err(|_| DesktopError::Transport("bus hello failed".into()))?;
        if !unique_name.starts_with(':') || r.p != r.d.len() {
            return Err(DesktopError::Transport(
                "bus hello returned malformed name".into(),
            ));
        }
        Ok(())
    }

    /// Broad pre-call subscription to Request Response signals. The request
    /// path is unknown until the method reply, so filtering happens
    /// client-side in [`Conn::wait_response`]; the connection is one-shot and
    /// closed after the call, so no `RemoveMatch` is needed.
    fn add_match(&mut self) -> Result<(), DesktopError> {
        self.add_match_with_cancel(&|| false)
    }

    fn add_match_with_cancel(&mut self, cancel: &dyn Fn() -> bool) -> Result<(), DesktopError> {
        let rule = format!("type='signal',interface='{IFACE_REQUEST}',member='Response'");
        let reply = self.transact_with_cancel(
            DBUS_DEST,
            DBUS_PATH,
            DBUS_IFACE,
            "AddMatch",
            "s",
            &rule_body(&rule),
            cancel,
        )?;
        if reply.ty == 3 {
            return Err(DesktopError::Transport("signal subscription failed".into()));
        }
        Ok(())
    }

    fn portal_request(
        &mut self,
        iface: &str,
        member: &str,
        body_sig: &str,
        body: &[u8],
    ) -> Result<String, DesktopError> {
        self.portal_request_with_cancel(iface, member, body_sig, body, &|| false)
    }

    fn portal_request_with_cancel(
        &mut self,
        iface: &str,
        member: &str,
        body_sig: &str,
        body: &[u8],
        cancel: &dyn Fn() -> bool,
    ) -> Result<String, DesktopError> {
        let reply = self.transact_with_cancel(
            PORTAL_DEST,
            PORTAL_PATH,
            iface,
            member,
            body_sig,
            body,
            cancel,
        )?;
        if reply.ty == 3 {
            return Err(map_dbus_error(reply.err.as_deref().unwrap_or("")));
        }
        let sender = reply
            .sender
            .clone()
            .ok_or_else(|| DesktopError::Transport("portal reply missing sender".into()))?;
        if let Some(existing) = &self.portal_sender {
            if existing != &sender {
                return Err(DesktopError::Transport("portal sender changed".into()));
            }
        } else {
            self.portal_sender = Some(sender);
        }
        if reply.sig != "o" {
            return Err(DesktopError::Transport(
                "portal returned wrong request signature".into(),
            ));
        }
        let mut r = R::new(&reply.body);
        let request_path = r
            .text()
            .map_err(|_| DesktopError::Transport("portal returned no request".into()))?;
        validate_object_path(&request_path)
            .map_err(|_| DesktopError::Transport("portal returned malformed request".into()))?;
        if r.p != r.d.len() {
            return Err(DesktopError::Transport(
                "portal returned trailing request data".into(),
            ));
        }
        Ok(request_path)
    }

    fn wait_response(&mut self, req_path: &str) -> Result<(u32, Option<String>), DesktopError> {
        let deadline = Instant::now() + RESPONSE_DEADLINE;
        let mut idx = 0;
        loop {
            while idx < self.pending.len() {
                if matching_response(&self.pending[idx], self.portal_sender.as_deref(), req_path)? {
                    let msg = self.pending.remove(idx);
                    return decode_response(&msg.body);
                }
                idx += 1;
            }
            idx = self.pending.len();
            match self.read_message()? {
                None => {
                    if Instant::now() >= deadline {
                        return Err(DesktopError::Transport("portal response timed out".into()));
                    }
                }
                Some(msg) => self.pending.push(msg),
            }
        }
    }

    fn wait_remote_response_with_cancel(
        &mut self,
        req_path: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<RemoteResponse, DesktopError> {
        let deadline = Instant::now() + RESPONSE_DEADLINE;
        let mut idx = 0;
        loop {
            if cancel() {
                return Err(DesktopError::Cancelled);
            }
            while idx < self.pending.len() {
                if matching_response(&self.pending[idx], self.portal_sender.as_deref(), req_path)? {
                    let msg = self.pending.remove(idx);
                    return decode_remote_response(&msg.body);
                }
                idx += 1;
            }
            idx = self.pending.len();
            match self.read_message()? {
                None => {
                    if cancel() {
                        return Err(DesktopError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(DesktopError::Transport("portal response timed out".into()));
                    }
                }
                Some(msg) => self.pending.push(msg),
            }
        }
    }

    fn close_request(&mut self, request_path: &str) {
        let _ = self.transact(PORTAL_DEST, request_path, IFACE_REQUEST, "Close", "", &[]);
    }

    fn close_session(&mut self, session_path: &str) {
        let _ = self.transact(PORTAL_DEST, session_path, IFACE_SESSION, "Close", "", &[]);
    }
}
// No network, no live portal, no credentials.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    static FILE_SEQ: AtomicU64 = AtomicU64::new(0);

    fn mini_png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13];
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
        v
    }

    fn stage_temp_file(bytes: &[u8]) -> String {
        let n = FILE_SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("muse-desktop-shot-{}-{n}.bin", std::process::id()));
        std::fs::write(&path, bytes).expect("stage temp screenshot");
        path.to_string_lossy().into_owned()
    }

    #[derive(Clone)]
    enum ShotResp {
        File {
            png: Vec<u8>,
        },
        BigFile,
        /// URI sent verbatim (for malformed-location and non-regular-file tests).
        RawUri(&'static str),
        Denied,
        Cancelled,
        CallError(&'static str),
    }

    #[derive(Clone)]
    enum OpenResp {
        Ok,
        Denied,
        Cancelled,
        CallError(&'static str),
    }

    #[derive(Clone)]
    struct Script {
        shot: ShotResp,
        open: OpenResp,
    }

    fn fake_read_message(s: &mut TcpStream) -> Msg {
        let mut head = [0u8; 16];
        s.read_exact(&mut head).expect("fake read header");
        assert_eq!(head[0], b'l', "client must speak little-endian");
        let body_len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
        let fields_len = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
        let fields_end = 16 + fields_len;
        let header_end = fields_end + ((8 - (fields_end % 8)) % 8);
        let mut rest = vec![0u8; header_end - 16 + body_len];
        s.read_exact(&mut rest).expect("fake read rest");
        let mut buf = head.to_vec();
        buf.extend_from_slice(&rest);
        decode_message(&buf).expect("fake decode")
    }

    fn fake_send(s: &mut TcpStream, bytes: &[u8]) {
        s.write_all(bytes).expect("fake send");
    }

    fn reply_return(s: &mut TcpStream, to: u32, sig: &str, body: &[u8]) {
        let mut fields = vec![(F_REPLY, FV::U(to)), (F_SENDER, FV::S(FAKE_PORTAL_SENDER))];
        if !sig.is_empty() {
            fields.push((F_SIG, FV::G(sig)));
        }
        fake_send(s, &encode_message(2, 1, &fields, body));
    }

    fn reply_error(s: &mut TcpStream, to: u32, name: &str) {
        let mut w = W::new();
        w.text("rejected by fake portal");
        let fields = vec![
            (F_ERROR, FV::S(name)),
            (F_REPLY, FV::U(to)),
            (F_SIG, FV::G("s")),
        ];
        fake_send(s, &encode_message(3, 1, &fields, &w.b));
    }

    fn send_response(s: &mut TcpStream, req_path: &str, code: u32, uri: Option<&str>) {
        let mut w = W::new();
        w.u32(code);
        match uri {
            Some(u) => w.dict_strs(&[("uri", u)]),
            None => w.dict_strs(&[]),
        }
        let fields = vec![
            (F_PATH, FV::O(req_path)),
            (F_IFACE, FV::S(IFACE_REQUEST)),
            (F_MEMBER, FV::S("Response")),
            (F_SENDER, FV::S(FAKE_PORTAL_SENDER)),
            (F_SIG, FV::G("ua{sv}")),
        ];
        fake_send(s, &encode_message(4, 1, &fields, &w.b));
    }

    const FAKE_PORTAL_SENDER: &str = ":1.42";

    fn fake_read_line(s: &mut TcpStream) -> String {
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            s.read_exact(&mut b).expect("fake read line");
            line.push(b[0]);
            if b[0] == b'\n' {
                break;
            }
        }
        String::from_utf8(line).expect("fake line utf8")
    }

    fn serve_one(mut s: TcpStream, script: &Script, seen_url: &mpsc::Sender<String>) {
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut zero = [0u8; 1];
        s.read_exact(&mut zero).expect("fake auth nul");
        assert_eq!(zero[0], 0);
        let auth = fake_read_line(&mut s);
        assert!(auth.starts_with("AUTH EXTERNAL"), "unexpected auth: {auth}");
        fake_send(&mut s, b"OK 00000000000000000000000000000000\r\n");
        let begin = fake_read_line(&mut s);
        assert!(begin.starts_with("BEGIN"), "unexpected begin: {begin}");
        loop {
            let msg = fake_read_message(&mut s);
            let member = msg.member.clone().unwrap_or_default();
            match member.as_str() {
                "Hello" => {
                    let mut w = W::new();
                    w.text(":1.99");
                    reply_return(&mut s, msg.serial, "s", &w.b);
                }
                "AddMatch" => reply_return(&mut s, msg.serial, "", &[]),
                "Screenshot" => {
                    // Body must be (parent_window, options with our token).
                    let mut r = R::new(&msg.body);
                    assert_eq!(r.text().unwrap(), "");
                    let code = {
                        let mut r2 = R::new(&msg.body);
                        let _ = r2.text().unwrap();
                        r2.align(4).unwrap();
                        let len = r2.u32().unwrap() as usize;
                        let end = r2.p + len;
                        let mut found = None;
                        while r2.p < end {
                            r2.align(8).unwrap();
                            let k = r2.text().unwrap();
                            let vs = r2.sigstr().unwrap();
                            if k == "handle_token" && vs == "s" {
                                found = Some(r2.text().unwrap());
                            } else {
                                r2.skip_value(&vs).unwrap();
                            }
                        }
                        found
                    };
                    assert!(code.is_some(), "handle_token missing");
                    match &script.shot {
                        ShotResp::CallError(name) => reply_error(&mut s, msg.serial, name),
                        resp => {
                            let mut w = W::new();
                            w.text("/org/freedesktop/portal/request/fake/shot1");
                            reply_return(&mut s, msg.serial, "o", &w.b);
                            match resp {
                                ShotResp::File { png } => {
                                    let path = stage_temp_file(png);
                                    let uri = format!("file://{path}");
                                    send_response(
                                        &mut s,
                                        "/org/freedesktop/portal/request/fake/shot1",
                                        0,
                                        Some(&uri),
                                    );
                                }
                                ShotResp::BigFile => {
                                    let path =
                                        stage_temp_file(&vec![0u8; MAX_SCREENSHOT_BYTES + 1]);
                                    let uri = format!("file://{path}");
                                    send_response(
                                        &mut s,
                                        "/org/freedesktop/portal/request/fake/shot1",
                                        0,
                                        Some(&uri),
                                    );
                                }
                                ShotResp::Denied => send_response(
                                    &mut s,
                                    "/org/freedesktop/portal/request/fake/shot1",
                                    2,
                                    None,
                                ),
                                ShotResp::RawUri(uri) => send_response(
                                    &mut s,
                                    "/org/freedesktop/portal/request/fake/shot1",
                                    0,
                                    Some(uri),
                                ),
                                ShotResp::Cancelled => send_response(
                                    &mut s,
                                    "/org/freedesktop/portal/request/fake/shot1",
                                    1,
                                    None,
                                ),
                                ShotResp::CallError(_) => unreachable!(),
                            }
                        }
                    }
                    return;
                }
                "OpenURI" => {
                    let mut r = R::new(&msg.body);
                    assert_eq!(r.text().unwrap(), "");
                    let uri = r.text().unwrap();
                    seen_url.send(uri).unwrap();
                    match &script.open {
                        OpenResp::CallError(name) => reply_error(&mut s, msg.serial, name),
                        resp => {
                            let mut w = W::new();
                            w.text("/org/freedesktop/portal/request/fake/open1");
                            reply_return(&mut s, msg.serial, "o", &w.b);
                            let code = match resp {
                                OpenResp::Ok => 0,
                                OpenResp::Denied => 2,
                                OpenResp::Cancelled => 1,
                                OpenResp::CallError(_) => unreachable!(),
                            };
                            send_response(
                                &mut s,
                                "/org/freedesktop/portal/request/fake/open1",
                                code,
                                None,
                            );
                        }
                    }
                    return;
                }
                other => panic!("unexpected method: {other}"),
            }
        }
    }

    fn spawn_fake(script: Script) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve_one(stream, &script, &tx);
            }
        });
        (port, rx)
    }

    fn bus(port: u16) -> BusEndpoint {
        BusEndpoint::loopback_tcp(port)
    }

    fn default_script() -> Script {
        Script {
            shot: ShotResp::Denied,
            open: OpenResp::Ok,
        }
    }

    // ----- screenshot -----

    #[test]
    fn screenshot_happy_path_returns_png_with_dims() {
        let png = mini_png(320, 200);
        let (port, _) = spawn_fake(Script {
            shot: ShotResp::File { png: png.clone() },
            open: OpenResp::Ok,
        });
        let shot = DesktopPortal::screenshot_via(&bus(port), "tok1").unwrap();
        assert_eq!(shot.width, 320);
        assert_eq!(shot.height, 200);
        assert_eq!(shot.format, "png");
        assert_eq!(shot.bytes, png);
    }

    #[test]
    fn screenshot_denial_is_permission_denied() {
        let (port, _) = spawn_fake(default_script());
        assert_eq!(
            DesktopPortal::screenshot_via(&bus(port), "tok1").unwrap_err(),
            DesktopError::PermissionDenied
        );
    }

    #[test]
    fn screenshot_cancel_is_cancelled() {
        let (port, _) = spawn_fake(Script {
            shot: ShotResp::Cancelled,
            open: OpenResp::Ok,
        });
        assert_eq!(
            DesktopPortal::screenshot_via(&bus(port), "tok1").unwrap_err(),
            DesktopError::Cancelled
        );
    }

    #[test]
    fn screenshot_oversized_is_too_large() {
        let (port, _) = spawn_fake(Script {
            shot: ShotResp::BigFile,
            open: OpenResp::Ok,
        });
        assert_eq!(
            DesktopPortal::screenshot_via(&bus(port), "tok1").unwrap_err(),
            DesktopError::TooLarge
        );
    }

    #[test]
    fn screenshot_error_names_map_to_typed_errors() {
        for (name, expected) in [
            (
                "org.freedesktop.portal.Error.NotAllowed",
                DesktopError::PermissionDenied,
            ),
            (
                "org.freedesktop.DBus.Error.AccessDenied",
                DesktopError::PermissionDenied,
            ),
            (
                "org.freedesktop.portal.Error.Cancelled",
                DesktopError::Cancelled,
            ),
            (
                "org.freedesktop.DBus.Error.ServiceUnknown",
                DesktopError::UnsupportedPortal("portal interface unavailable".into()),
            ),
            (
                "org.freedesktop.DBus.Error.UnknownMethod",
                DesktopError::UnsupportedPortal("portal interface unavailable".into()),
            ),
        ] {
            let (port, _) = spawn_fake(Script {
                shot: ShotResp::CallError(name),
                open: OpenResp::Ok,
            });
            assert_eq!(
                DesktopPortal::screenshot_via(&bus(port), "tok1").unwrap_err(),
                expected,
                "error name {name}"
            );
        }
    }

    #[test]
    fn screenshot_unknown_error_is_transport() {
        let (port, _) = spawn_fake(Script {
            shot: ShotResp::CallError("org.freedesktop.portal.Error.Failed"),
            open: OpenResp::Ok,
        });
        assert!(matches!(
            DesktopPortal::screenshot_via(&bus(port), "tok1").unwrap_err(),
            DesktopError::Transport(_)
        ));
    }

    #[test]
    fn error_names_match_by_equality_not_substring() {
        // Regression class: substring matching let a custom name containing
        // e.g. "Cancelled" read as a cancel. Only exact leaf names map.
        for (name, expected) in [
            ("org.example.NotCancelledEver", "Transport"),
            ("org.example.TokenNotAllowedX", "Transport"),
            ("org.example.OperationCanceled", "Transport"),
            ("org.example.NoSuchThing", "Transport"),
        ] {
            let (port, _) = spawn_fake(Script {
                shot: ShotResp::CallError(name),
                open: OpenResp::Ok,
            });
            let err = DesktopPortal::screenshot_via(&bus(port), "tok1").unwrap_err();
            assert!(
                matches!(err, DesktopError::Transport(_)),
                "substring name {name} must be Transport, got {err:?} ({expected})"
            );
        }
    }

    #[test]
    fn non_regular_screenshot_location_fails_closed_without_blocking() {
        // Regression: opening a FIFO with no writer blocks forever (no
        // deadline covers file I/O). /dev/null is a char device, so it
        // exercises the regular-file gate deterministically.
        let (port, _) = spawn_fake(Script {
            shot: ShotResp::RawUri("file:///dev/null"),
            open: OpenResp::Ok,
        });
        assert!(matches!(
            DesktopPortal::screenshot_via(&bus(port), "tok1").unwrap_err(),
            DesktopError::Transport(_)
        ));
    }

    #[test]
    fn truncated_percent_escape_in_location_fails_closed() {
        // Regression: `%4` (missing nibble) was decoded as `@` by fabricating
        // a zero nibble, landing on a different path than the URI names.
        let (port, _) = spawn_fake(Script {
            shot: ShotResp::RawUri("file:///tmp/a%4"),
            open: OpenResp::Ok,
        });
        assert!(matches!(
            DesktopPortal::screenshot_via(&bus(port), "tok1").unwrap_err(),
            DesktopError::Transport(_)
        ));
    }

    // ----- open_url -----

    #[test]
    fn open_url_happy_path_sends_exact_url() {
        let (port, seen) = spawn_fake(default_script());
        DesktopPortal::open_url_via(&bus(port), "https://example.com/a?b=c").unwrap();
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(5)).unwrap(),
            "https://example.com/a?b=c"
        );
    }

    #[test]
    fn open_url_uppercase_scheme_ok() {
        let (port, seen) = spawn_fake(default_script());
        DesktopPortal::open_url_via(&bus(port), "HTTPS://example.com/").unwrap();
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(5)).unwrap(),
            "HTTPS://example.com/"
        );
    }

    #[test]
    fn open_url_denial_and_cancel_map() {
        for (resp, expected) in [
            (OpenResp::Denied, DesktopError::PermissionDenied),
            (OpenResp::Cancelled, DesktopError::Cancelled),
            (
                OpenResp::CallError("org.freedesktop.portal.Error.NotAllowed"),
                DesktopError::PermissionDenied,
            ),
            (
                OpenResp::CallError("org.freedesktop.DBus.Error.ServiceUnknown"),
                DesktopError::UnsupportedPortal("portal interface unavailable".into()),
            ),
        ] {
            // Bind the receiver: the fake always reports the URL it saw.
            let (port, _rx) = spawn_fake(Script {
                shot: ShotResp::Denied,
                open: resp,
            });
            assert_eq!(
                DesktopPortal::open_url_via(&bus(port), "https://example.com/").unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn open_url_rejects_non_http_without_dialing() {
        // Unroutable endpoint proves validation precedes any bus traffic.
        let bus = BusEndpoint::unix_socket("/nonexistent-bus-path-xyz");
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "javascript:alert(1)",
            "data:text/plain,hi",
            "example.com/no-scheme",
            "",
            "FILE:///etc/passwd",
            "http://",
            "http:///path-only",
            "https://exa mple.com/",
            "https://user@example.com/",
        ] {
            assert!(
                matches!(
                    DesktopPortal::open_url_via(&bus, url).unwrap_err(),
                    DesktopError::InvalidArgument(_)
                ),
                "url rejected: {url:?}"
            );
        }
    }

    #[test]
    fn open_url_error_never_echoes_url() {
        let bus = BusEndpoint::unix_socket("/nonexistent-bus-path-xyz");
        let secret = "https://example.com/s3cret-path?tok=abc";
        let err = DesktopPortal::open_url_via(&bus, &format!("file://{secret}")).unwrap_err();
        assert!(!format!("{err:?}").contains("s3cret"));
    }

    // ----- tokens -----

    #[test]
    fn bad_tokens_rejected_without_dialing() {
        let bus = BusEndpoint::unix_socket("/nonexistent-bus-path-xyz");
        let long = "a".repeat(256);
        for token in [
            "",
            "has space",
            "dash-ed",
            "semi;colon",
            "dot.tok",
            long.as_str(),
        ] {
            assert!(
                matches!(
                    DesktopPortal::screenshot_via(&bus, token).unwrap_err(),
                    DesktopError::InvalidArgument(_)
                ),
                "token rejected: {token:?}"
            );
        }
    }

    // ----- framing -----

    #[test]
    fn message_round_trip_preserves_fields_and_body() {
        let body = screenshot_body("tok_9");
        let msg = encode_message(
            1,
            42,
            &[
                (F_DEST, FV::S(PORTAL_DEST)),
                (F_PATH, FV::O(PORTAL_PATH)),
                (F_IFACE, FV::S(IFACE_SCREENSHOT)),
                (F_MEMBER, FV::S("Screenshot")),
                (F_SIG, FV::G("sa{sv}")),
            ],
            &body,
        );
        let back = decode_message(&msg).unwrap();
        assert_eq!(back.ty, 1);
        assert_eq!(back.serial, 42);
        assert_eq!(back.member.as_deref(), Some("Screenshot"));
        assert_eq!(back.iface.as_deref(), Some(IFACE_SCREENSHOT));
        assert_eq!(back.path.as_deref(), Some(PORTAL_PATH));
        let mut r = R::new(&back.body);
        assert_eq!(r.text().unwrap(), "");
        r.align(4).unwrap();
        let len = r.u32().unwrap() as usize;
        let end = r.p + len;
        let mut token = None;
        while r.p < end {
            r.align(8).unwrap();
            let k = r.text().unwrap();
            let vs = r.sigstr().unwrap();
            if k == "handle_token" {
                token = Some(r.text().unwrap());
            } else {
                r.skip_value(&vs).unwrap();
            }
        }
        assert_eq!(token.as_deref(), Some("tok_9"));
    }

    #[test]
    fn response_decode_skips_unknown_results() {
        let mut w = W::new();
        w.u32(0);
        w.dict_strs(&[("uri", "file:///tmp/x.png"), ("other", "zzz")]);
        let (code, uri) = decode_response(&w.b).unwrap();
        assert_eq!(code, 0);
        assert_eq!(uri.as_deref(), Some("file:///tmp/x.png"));
    }

    #[test]
    fn response_decode_rejects_wrong_signatures_and_mixed_duplicates() {
        let mut wrong_uri = W::new();
        wrong_uri.u32(0);
        wrong_uri.dict_bool("uri", true);
        assert!(decode_response(&wrong_uri.b).is_err());

        let mut mixed_entries = W::new();
        mixed_entries.align(8);
        mixed_entries.text("session_handle");
        mixed_entries.sig("b");
        mixed_entries.u32(1);
        mixed_entries.align(8);
        mixed_entries.text("session_handle");
        mixed_entries.sig("s");
        mixed_entries.text("/org/freedesktop/portal/desktop/session/one");
        let mut mixed = W::new();
        mixed.u32(0);
        mixed.u32(mixed_entries.b.len() as u32);
        mixed.align(8);
        mixed.b.extend_from_slice(&mixed_entries.b);
        assert!(decode_remote_response(&mixed.b).is_err());
    }

    #[test]
    fn external_uid_is_hex_encoded_ascii() {
        assert_eq!(external_uid_hex("1000"), "31303030");
        assert_eq!(external_uid_hex("0"), "30");
    }

    #[test]
    fn response_signature_must_be_exact() {
        assert!(validate_response_signature("ua{sv}").is_ok());
        assert!(validate_response_signature("u").is_err());
        assert!(validate_response_signature("ua{sv} ").is_err());
    }

    #[test]
    fn response_correlation_rejects_wrong_outer_signature() {
        let message = Msg {
            ty: 4,
            serial: 1,
            reply_to: None,
            path: Some("/org/freedesktop/portal/desktop/request/one".into()),
            iface: Some(IFACE_REQUEST.into()),
            member: Some("Response".into()),
            sender: Some(":1.42".into()),
            err: None,
            sig: "u".into(),
            body: Vec::new(),
        };
        assert!(matching_response(
            &message,
            Some(":1.42"),
            "/org/freedesktop/portal/desktop/request/one"
        )
        .is_err());
    }

    #[test]
    fn non_png_payload_returns_unknown_format_with_bytes() {
        let (w, h, f) = png_dims(b"not an image");
        assert_eq!((w, h, f), (0, 0, "unknown"));
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    const FAKE_PORTAL_SENDER: &str = ":1.42";

    fn fake_read_line(s: &mut TcpStream) -> String {
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            s.read_exact(&mut b).unwrap();
            line.push(b[0]);
            if b[0] == b'\n' {
                return String::from_utf8(line).unwrap();
            }
        }
    }

    fn fake_read_message(s: &mut TcpStream) -> Msg {
        let mut head = [0u8; 16];
        s.read_exact(&mut head).unwrap();
        let body_len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
        let fields_len = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
        let fields_end = 16 + fields_len;
        let header_end = fields_end + ((8 - (fields_end % 8)) % 8);
        let mut rest = vec![0u8; header_end - 16 + body_len];
        s.read_exact(&mut rest).unwrap();
        let mut buf = head.to_vec();
        buf.extend_from_slice(&rest);
        decode_message(&buf).unwrap()
    }

    fn reply_return(s: &mut TcpStream, to: u32, sig: &str, body: &[u8]) {
        let mut fields = vec![(F_REPLY, FV::U(to)), (F_SENDER, FV::S(FAKE_PORTAL_SENDER))];
        if !sig.is_empty() {
            fields.push((F_SIG, FV::G(sig)));
        }
        s.write_all(&encode_message(2, 1, &fields, body)).unwrap();
    }

    fn send_response(
        s: &mut TcpStream,
        req_path: &str,
        code: u32,
        result: Option<(&str, &str, &str)>,
    ) {
        let mut w = W::new();
        w.u32(code);
        match result {
            Some((key, "s", value)) => w.dict_strs(&[(key, value)]),
            Some((key, "o", value)) => w.dict_objects(&[(key, value)]),
            Some((key, sig, value)) if sig == "u" => {
                let mut e = W::new();
                e.align(8);
                e.text(key);
                e.sig(sig);
                e.u32(value.parse().unwrap());
                w.u32(e.b.len() as u32);
                w.align(8);
                w.b.extend_from_slice(&e.b);
            }
            _ => w.dict_strs(&[]),
        }
        let fields = vec![
            (F_PATH, FV::O(req_path)),
            (F_IFACE, FV::S(IFACE_REQUEST)),
            (F_MEMBER, FV::S("Response")),
            (F_SENDER, FV::S(FAKE_PORTAL_SENDER)),
            (F_SIG, FV::G("ua{sv}")),
        ];
        s.write_all(&encode_message(4, 1, &fields, &w.b)).unwrap();
    }

    fn spawn_remote_fake() -> (u16, mpsc::Receiver<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut nul = [0u8; 1];
            s.read_exact(&mut nul).unwrap();
            assert_eq!(nul[0], 0);
            assert!(fake_read_line(&mut s).starts_with("AUTH EXTERNAL"));
            s.write_all(b"OK 00000000000000000000000000000000\r\n")
                .unwrap();
            assert!(fake_read_line(&mut s).starts_with("BEGIN"));
            let mut calls = Vec::new();
            loop {
                let msg = fake_read_message(&mut s);
                let member = msg.member.clone().unwrap();
                calls.push(format!(
                    "{member}:{}:{}",
                    msg.iface.as_deref().unwrap(),
                    msg.sig
                ));
                match member.as_str() {
                    "Hello" => {
                        let mut w = W::new();
                        w.text(":1.1");
                        reply_return(&mut s, msg.serial, "s", &w.b);
                    }
                    "AddMatch" => reply_return(&mut s, msg.serial, "", &[]),
                    "CreateSession" => {
                        assert!(msg
                            .body
                            .windows(b"handle_token".len())
                            .any(|window| window == b"handle_token"));
                        assert!(msg
                            .body
                            .windows(b"session_handle_token".len())
                            .any(|window| window == b"session_handle_token"));
                        let mut w = W::new();
                        w.text("/request/create");
                        reply_return(&mut s, msg.serial, "o", &w.b);
                        send_response(
                            &mut s,
                            "/request/create",
                            0,
                            Some((
                                "session_handle",
                                "s",
                                "/org/freedesktop/portal/desktop/session/abc",
                            )),
                        );
                    }
                    "SelectDevices" => {
                        let mut w = W::new();
                        w.text("/request/select");
                        reply_return(&mut s, msg.serial, "o", &w.b);
                        send_response(&mut s, "/request/select", 0, None);
                    }
                    "Start" => {
                        assert!(msg
                            .body
                            .windows(b"handle_token".len())
                            .any(|window| window == b"handle_token"));
                        let mut w = W::new();
                        w.text("/request/start");
                        reply_return(&mut s, msg.serial, "o", &w.b);
                        send_response(&mut s, "/request/start", 0, Some(("devices", "u", "3")));
                        tx.send(calls.clone()).unwrap();
                    }
                    "NotifyPointerMotion"
                    | "NotifyPointerButton"
                    | "NotifyPointerAxis"
                    | "NotifyPointerAxisDiscrete"
                    | "NotifyKeyboardKeysym"
                    | "NotifyKeyboardKeycode" => reply_return(&mut s, msg.serial, "", &[]),
                    "Close" => {
                        reply_return(&mut s, msg.serial, "", &[]);
                        tx.send(calls).unwrap();
                        return;
                    }
                    other => panic!("unexpected method {other}"),
                }
            }
        });
        (port, rx)
    }

    #[test]
    fn remote_response_rejects_duplicate_and_trailing_results() {
        let mut duplicate = W::new();
        duplicate.u32(0);
        duplicate.dict_strs(&[
            (
                "session_handle",
                "/org/freedesktop/portal/desktop/session/one",
            ),
            (
                "session_handle",
                "/org/freedesktop/portal/desktop/session/two",
            ),
        ]);
        assert!(decode_remote_response(&duplicate.b).is_err());

        let mut trailing = W::new();
        trailing.u32(0);
        trailing.dict_strs(&[]);
        trailing.b.push(0);
        assert!(decode_remote_response(&trailing.b).is_err());
    }

    #[test]
    fn remote_input_bounds_reject_before_bus_input() {
        let mut session = RemoteDesktopSession {
            conn: None,
            session_handle: "/session".into(),
            granted_devices: REMOTE_DEVICE_KEYBOARD | REMOTE_DEVICE_POINTER,
            stopped: false,
        };
        assert!(matches!(
            session.pointer_motion(MAX_REMOTE_DELTA + 1.0, 0.0),
            Err(DesktopError::InvalidArgument(_))
        ));
        assert!(matches!(
            session.pointer_axis_discrete(0, MAX_REMOTE_STEPS + 1),
            Err(DesktopError::InvalidArgument(_))
        ));
        assert!(matches!(
            session.pointer_button(REMOTE_BUTTON_MAX + 1, 1),
            Err(DesktopError::InvalidArgument(_))
        ));
        assert!(matches!(
            session.keyboard_keysym(REMOTE_KEYSYM_MAX + 1, 1),
            Err(DesktopError::InvalidArgument(_))
        ));
        assert!(matches!(
            session.keyboard_keycode(REMOTE_KEYCODE_MAX + 1, 1),
            Err(DesktopError::InvalidArgument(_))
        ));
    }

    #[test]
    fn remote_response_decodes_string_session_handle() {
        let mut w = W::new();
        w.u32(0);
        w.dict_strs(&[(
            "session_handle",
            "/org/freedesktop/portal/desktop/session/abc",
        )]);
        let result = decode_remote_response(&w.b).unwrap();
        assert_eq!(
            result.session_handle.as_deref(),
            Some("/org/freedesktop/portal/desktop/session/abc")
        );
    }

    #[test]
    fn remote_start_runs_ordered_lifecycle_and_grants_requested_devices() {
        let (port, calls) = spawn_remote_fake();
        let mut session = DesktopPortal::start_remote_desktop_via(
            &BusEndpoint::loopback_tcp(port),
            "handle",
            "session",
            "parent-window",
            REMOTE_DEVICE_KEYBOARD | REMOTE_DEVICE_POINTER,
        )
        .unwrap();
        assert_eq!(session.granted_devices(), 3);
        assert_eq!(
            calls.recv_timeout(Duration::from_secs(5)).unwrap(),
            vec![
                "Hello:org.freedesktop.DBus:".to_string(),
                "AddMatch:org.freedesktop.DBus:s".to_string(),
                "CreateSession:org.freedesktop.portal.RemoteDesktop:a{sv}".to_string(),
                "SelectDevices:org.freedesktop.portal.RemoteDesktop:oa{sv}".to_string(),
                "Start:org.freedesktop.portal.RemoteDesktop:osa{sv}".to_string(),
            ]
        );
        session.stop().unwrap();
    }

    #[test]
    fn remote_input_uses_exact_notify_signatures_and_stop_is_idempotent() {
        let (port, calls) = spawn_remote_fake();
        let mut session = DesktopPortal::start_remote_desktop_via(
            &BusEndpoint::loopback_tcp(port),
            "handle",
            "session",
            "parent-window",
            REMOTE_DEVICE_KEYBOARD | REMOTE_DEVICE_POINTER,
        )
        .unwrap();
        let _ = calls.recv_timeout(Duration::from_secs(5)).unwrap();
        session.pointer_motion(1.25, -2.5).unwrap();
        session.pointer_button(0x110, 1).unwrap();
        session.pointer_axis(0.0, 1.0, true).unwrap();
        session.pointer_axis_discrete(0, -2).unwrap();
        session.keyboard_keysym(0x41, 1).unwrap();
        session.keyboard_keycode(30, 0).unwrap();
        session.stop().unwrap();
        session.stop().unwrap();
        let calls = calls.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            &calls[5..],
            [
                "NotifyPointerMotion:org.freedesktop.portal.RemoteDesktop:oa{sv}dd",
                "NotifyPointerButton:org.freedesktop.portal.RemoteDesktop:oa{sv}iu",
                "NotifyPointerAxis:org.freedesktop.portal.RemoteDesktop:oa{sv}dd",
                "NotifyPointerAxisDiscrete:org.freedesktop.portal.RemoteDesktop:oa{sv}ui",
                "NotifyKeyboardKeysym:org.freedesktop.portal.RemoteDesktop:oa{sv}iu",
                "NotifyKeyboardKeycode:org.freedesktop.portal.RemoteDesktop:oa{sv}iu",
                "Close:org.freedesktop.portal.Session:",
            ]
        );
        assert!(matches!(
            session.pointer_motion(0.0, 0.0),
            Err(DesktopError::InvalidArgument(_))
        ));
    }
}
