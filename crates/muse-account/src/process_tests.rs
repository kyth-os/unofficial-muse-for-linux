use super::*;
use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(script: &str) -> Self {
        let dir = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "muse-account-process-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bridge.py"), script).unwrap();
        Self(dir)
    }
    fn config(&self) -> BridgeConfig {
        BridgeConfig {
            python: PathBuf::from("/usr/bin/python3"),
            script: self.0.join("bridge.py"),
            config_dir: self.0.clone(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn timeout_while_stdin_is_blocked_kills_and_reaps_child() {
    let fixture = Fixture::new(
        r#"
import os, pathlib, time
pathlib.Path(__file__).with_suffix('.pid').write_text(str(os.getpid()))
time.sleep(60)
"#,
    );
    let start = Instant::now();
    let error = run_bridge(
        &fixture.config(),
        &vec![b'x'; MAX_REQUEST],
        Duration::from_millis(500),
        true,
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
    assert!(error.uncertain);
    assert!(start.elapsed() < Duration::from_secs(5));
    let pid: i32 = fs::read_to_string(fixture.0.join("bridge.pid"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal zero only checks process existence; waitpid is nonblocking.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn oversized_stdout_is_bounded_and_terminated() {
    let fixture = Fixture::new(
        "import sys\nsys.stdout.buffer.write(b'x' * (5 * 1024 * 1024))\nsys.stdout.flush()\n",
    );
    let error = run_bridge(&fixture.config(), b"{}", Duration::from_secs(3), false).unwrap_err();
    assert_eq!(error.code, ErrorCode::ResponseTooLarge);
    assert!(!error.uncertain);
}

#[test]
fn stdin_and_stdout_pump_concurrently_without_pipe_deadlock() {
    let fixture = Fixture::new(
        r#"
import sys
sys.stdout.buffer.write(b'x' * 131072)
sys.stdout.flush()
request = sys.stdin.buffer.read()
assert len(request) == 262144
"#,
    );
    let output = run_bridge(
        &fixture.config(),
        &vec![b'a'; 262144],
        Duration::from_secs(3),
        false,
    )
    .unwrap();
    assert_eq!(output.len(), 131072);
}
