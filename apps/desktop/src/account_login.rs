//! Bounded, cancellable browser sign-in process; never returns raw helper output.

use std::{
    ffi::OsString,
    io::{Read, Write},
    os::fd::AsRawFd,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Cancellation {
    cancelled: AtomicBool,
    pid: Mutex<Option<u32>>,
}

impl Cancellation {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let pid = match self.pid.try_lock() {
            Ok(pid) => pid,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            // Never wait for subprocess cleanup on GTK's main thread. The
            // worker sees the atomic flag and sends SIGTERM itself as needed.
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        if let Some(pid) = *pid {
            // The lock prevents reaping/PID reuse while this signal is sent.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
    }
}

fn safe_environment(
    values: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    const ALLOWED: &[&str] = &[
        "HOME",
        "USER",
        "PATH",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_DATA_DIRS",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "XDG_SESSION_TYPE",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ];
    values
        .into_iter()
        .filter(|(key, _)| ALLOWED.iter().any(|allowed| key == allowed))
        .collect()
}

struct Guard<'a> {
    child: Child,
    cancellation: &'a Cancellation,
}
impl Guard<'_> {
    fn status(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let mut pid = self
            .cancellation
            .pid
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let status = self.child.try_wait()?;
        if status.is_some() {
            *pid = None;
        }
        Ok(status)
    }
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let mut pid = self
            .cancellation
            .pid
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if pid.is_some() {
            // Give our helper time to close its separately grouped browser and
            // remove its private temporary profile before escalating.
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(6);
            while Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            *pid = None;
        }
    }
}

fn nonblocking(fd: i32) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn run_sign_in(
    python: &Path,
    script: &Path,
    config_dir: &Path,
    cancellation: &Cancellation,
) -> Result<(), String> {
    let cancelled_message = || "Browser sign-in was cancelled.".to_string();
    if cancellation.cancelled.load(Ordering::SeqCst) {
        return Err(cancelled_message());
    }
    if !python.is_absolute() || !script.is_file() || !config_dir.is_absolute() {
        return Err("The browser sign-in helper or selected configuration is unavailable.".into());
    }
    let child = Command::new(python)
        .env_clear()
        .envs(safe_environment(std::env::vars_os()))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-I")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Could not start the browser sign-in helper.".to_string())?;
    {
        let mut pid = cancellation.pid.lock().unwrap_or_else(|e| e.into_inner());
        *pid = Some(child.id());
        if cancellation.cancelled.load(Ordering::SeqCst) {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
        }
    }
    let mut guard = Guard {
        child,
        cancellation,
    };
    let mut input = guard.child.stdin.take();
    let mut stdout = guard
        .child
        .stdout
        .take()
        .ok_or_else(|| "Sign-in response pipe is unavailable.".to_string())?;
    let request = serde_json::to_vec(&serde_json::json!({"config_dir":config_dir}))
        .map_err(|_| "The selected sign-in directory is invalid.".to_string())?;
    let io_error = || "The sign-in helper connection failed; raw output is hidden.".to_string();
    if let Some(input) = &input {
        nonblocking(input.as_raw_fd()).map_err(|_| io_error())?;
    }
    nonblocking(stdout.as_raw_fd()).map_err(|_| io_error())?;
    let mut written = 0;
    let mut output = Vec::new();
    let mut eof = false;
    let deadline = Instant::now() + Duration::from_secs(330);
    let mut cancel_deadline = None;
    let status = loop {
        if cancellation.cancelled.load(Ordering::SeqCst) && cancel_deadline.is_none() {
            cancellation.cancel();
            cancel_deadline = Some(Instant::now() + Duration::from_secs(6));
        }
        if cancel_deadline.is_some_and(|limit| Instant::now() >= limit) {
            return Err(cancelled_message());
        }
        if Instant::now() >= deadline {
            return Err("Browser sign-in timed out. The helper was stopped.".into());
        }
        if let Some(pipe) = &mut input {
            match pipe.write(&request[written..]) {
                Ok(0) => input = None,
                Ok(n) => {
                    written += n;
                    if written == request.len() {
                        input = None;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(io_error()),
            }
        }
        for _ in 0..16 {
            let mut buffer = [0; 4096];
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    if output.len() + n > 65_536 {
                        return Err("Sign-in helper exceeded its safe response limit.".into());
                    }
                    output.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(io_error()),
            }
        }
        if let Some(status) = guard.status().map_err(|_| io_error())?
            && eof
        {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if cancellation.cancelled.load(Ordering::SeqCst) {
        return Err(cancelled_message());
    }
    let result: serde_json::Value = serde_json::from_slice(&output)
        .map_err(|_| "Sign-in helper returned invalid data; raw output is hidden.".to_string())?;
    if status.success() && result.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(());
    }
    let code = result
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .or_else(|| error.get("code").and_then(serde_json::Value::as_str))
        })
        .unwrap_or("internal_error");
    Err(crate::account_ui::login_error_message(code).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(script: &str) -> Self {
            let root = std::env::var_os("TMPDIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let path = root.join(format!(
                "muse-login-runner-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(path.join("helper.py"), script).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn disconnect_cancels_sign_in_and_allows_helper_cleanup() {
        // Regression: Disconnect used to discard only the GTK result while the
        // helper could continue authenticating and publish credentials later.
        let f = Fixture::new(
            "import json, signal, sys, time\nfrom pathlib import Path\nr=json.load(sys.stdin)\np=Path(r['config_dir'])\ndef stop(*_):\n (p/'cleaned').write_text('yes')\n print(json.dumps({'ok':False,'error':'cancelled'}),flush=True)\n sys.exit(1)\nsignal.signal(signal.SIGTERM,stop)\n(p/'ready').write_text('yes')\ntime.sleep(30)\n",
        );
        let cancelled = Arc::new(Cancellation::default());
        let signal = Arc::clone(&cancelled);
        let script = f.0.join("helper.py");
        let directory = f.0.clone();
        let worker = std::thread::spawn(move || {
            run_sign_in(
                std::path::Path::new("/usr/bin/python3"),
                &script,
                &directory,
                &signal,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f.0.join("ready").exists() {
            assert!(Instant::now() < deadline, "helper did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        cancelled.cancel();
        assert!(worker.join().unwrap().unwrap_err().contains("cancelled"));
        assert!(f.0.join("cleaned").is_file(), "SIGTERM cleanup was skipped");
    }

    #[test]
    fn cancelled_before_start_does_not_execute_the_helper() {
        let f = Fixture::new("raise Exception('must never run')\n");
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(
            run_sign_in(
                std::path::Path::new("/usr/bin/python3"),
                &f.0.join("helper.py"),
                &f.0,
                &cancelled
            )
            .unwrap_err()
            .contains("cancelled")
        );
    }

    #[test]
    fn arbitrary_helper_errors_and_oversized_output_are_not_reflected() {
        for body in [
            "print('{\"ok\":false,\"error\":\"hatch_sess=SECRET\",\"message\":\"SECRET\"}')",
            "print('SECRET'*20000)",
        ] {
            let f = Fixture::new(body);
            let error = run_sign_in(
                std::path::Path::new("/usr/bin/python3"),
                &f.0.join("helper.py"),
                &f.0,
                &Cancellation::default(),
            )
            .unwrap_err();
            assert!(!error.contains("SECRET"));
            assert!(!error.contains("hatch_sess"));
        }
    }

    #[test]
    fn cancel_never_blocks_the_gtk_thread_on_worker_cleanup() {
        let cancellation = Arc::new(Cancellation::default());
        let lock = cancellation.pid.lock().unwrap();
        let handle = Arc::clone(&cancellation);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            handle.cancel();
            tx.send(()).unwrap();
        });
        let responsive = rx.recv_timeout(Duration::from_millis(200)).is_ok();
        drop(lock);
        worker.join().unwrap();
        assert!(responsive, "Disconnect blocked on sign-in worker cleanup");
    }

    #[test]
    fn valid_helper_success_is_accepted_without_returning_its_paths() {
        let f = Fixture::new("print('{\"ok\":true,\"cookie_file\":\"/private/path\"}')");
        assert!(
            run_sign_in(
                std::path::Path::new("/usr/bin/python3"),
                &f.0.join("helper.py"),
                &f.0,
                &Cancellation::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn browser_environment_excludes_provider_secrets_and_appimage_libraries() {
        let input = [
            ("HOME", "/home/example"),
            ("DISPLAY", ":1"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/example"),
            ("OPENAI_API_KEY", "SECRET"),
            ("MUSE_CONFIG_DIR", "/wrong"),
            ("LD_LIBRARY_PATH", "/appimage/lib"),
            ("APPDIR", "/appimage"),
            ("PYTHONPATH", "/wrong"),
        ];
        let result = safe_environment(input.into_iter().map(|(k, v)| (k.into(), v.into())));
        assert!(result.iter().any(|(key, _)| key == "DISPLAY"));
        assert!(
            result
                .iter()
                .any(|(key, _)| key == "DBUS_SESSION_BUS_ADDRESS")
        );
        for forbidden in [
            "OPENAI_API_KEY",
            "MUSE_CONFIG_DIR",
            "LD_LIBRARY_PATH",
            "APPDIR",
            "PYTHONPATH",
        ] {
            assert!(!result.iter().any(|(key, _)| key == forbidden));
        }
    }
}
