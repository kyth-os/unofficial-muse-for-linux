use muse_local_link::{HostConfig, HostEvent, HostSession};
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static SEQ: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(body: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "muse-link-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("helper.py"), body).unwrap();
        Self(dir)
    }
    fn config(&self) -> HostConfig {
        HostConfig {
            python: "/usr/bin/python3".into(),
            script: self.0.join("helper.py"),
            config_dir: self.0.clone(),
            node_id: "test-node".into(),
            display_name: "Test".into(),
            commands: json!({"safe.read": {"description": "test"}}),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn next(session: &HostSession) -> HostEvent {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(event) = session.try_recv() {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for host event"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn closed(session: &HostSession) -> String {
    loop {
        if let HostEvent::Closed { code } = next(session) {
            return code;
        }
    }
}

#[test]
fn rejects_malformed_oversized_and_out_of_order_helper_output() {
    for (body, expected) in [
        ("print('not-json',flush=True)", "protocol_error"),
        ("print('x'*270000,flush=True)", "output_too_large"),
        (
            "print('{\"type\":\"state\",\"state\":\"registered\"}',flush=True)",
            "protocol_error",
        ),
        (
            "print('{\"type\":\"invoke\",\"id\":\"i\",\"command\":\"shell\",\"params\":{},\"timeout_ms\":10}',flush=True)",
            "protocol_error",
        ),
        (
            "print('{\"type\":\"state\",\"state\":\"error\",\"code\":\"private secret\"}',flush=True)",
            "protocol_error",
        ),
        (
            "print('{\"type\":\"state\",\"state\":\"connecting\",\"private\":\"secret\"}',flush=True)",
            "protocol_error",
        ),
    ] {
        let fixture = Fixture::new(&format!(
            "import sys,time\nsys.stdin.readline()\n{body}\ntime.sleep(10)\n"
        ));
        let session = HostSession::spawn(fixture.config()).unwrap();
        assert_eq!(closed(&session), expected);
    }
}

#[test]
fn environment_is_allowlisted_and_python_is_isolated() {
    let fixture = Fixture::new(
        r#"
import os,sys,time,json
sys.stdin.readline()
blocked=['OPENAI_API_KEY','ANTHROPIC_API_KEY','MUSE_API_KEY','PYTHONPATH','PYTHONHOME','LD_LIBRARY_PATH','LD_PRELOAD','APPDIR','APPIMAGE','MUSE_MCP_CONFIG']
assert not any(k in os.environ for k in blocked)
assert os.environ['MUSE_MCP_ENABLE_RAW']=='0'
assert sys.flags.isolated
print('{"type":"state","state":"connecting"}',flush=True)
time.sleep(10)
"#,
    );
    let session = HostSession::spawn(fixture.config()).unwrap();
    assert!(matches!(next(&session), HostEvent::Connecting));
}

#[test]
fn configuration_is_validated_before_spawning() {
    let fixture = Fixture::new("raise Exception('must not run')");
    let mut config = fixture.config();
    config.python = "python3".into();
    assert!(HostSession::spawn(config).is_err());
    let mut config = fixture.config();
    config.node_id = "".into();
    assert!(HostSession::spawn(config).is_err());
    let mut config = fixture.config();
    config.commands = json!(["raw"]);
    assert!(HostSession::spawn(config).is_err());
}

#[test]
fn repeated_stop_and_drop_are_nonblocking_and_reap_idle_child() {
    let fixture = Fixture::new(
        r#"
import json,sys,time,os,signal,pathlib
start=json.loads(sys.stdin.readline())
pathlib.Path(start['config_dir'],'pid').write_text(str(os.getpid()))
signal.signal(signal.SIGTERM,signal.SIG_IGN)
print('{"type":"state","state":"connecting"}',flush=True)
time.sleep(30)
"#,
    );
    let session = HostSession::spawn(fixture.config()).unwrap();
    assert!(matches!(next(&session), HostEvent::Connecting));
    let pid: i32 = fs::read_to_string(fixture.0.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    let start = Instant::now();
    for _ in 0..10 {
        session.stop();
    }
    drop(session);
    assert!(start.elapsed() < Duration::from_millis(50));
    let deadline = Instant::now() + Duration::from_secs(2);
    while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "idle helper survived stop"
    );
}

#[test]
fn result_ids_are_outstanding_once_only_and_expire() {
    let fixture = Fixture::new(
        r#"
import sys,time,json
sys.stdin.readline()
for state in ['connecting','registered']:
 print(json.dumps({'type':'state','state':state}),flush=True)
for id,timeout in [('i',1000),('late',50)]:
 print(json.dumps({'type':'invoke','id':id,'command':'safe.read','params':{},'timeout_ms':timeout}),flush=True)
time.sleep(10)
"#,
    );
    let session = HostSession::spawn(fixture.config()).unwrap();
    assert!(
        session
            .send_result("unknown", true, json!({}), None)
            .is_err()
    );
    assert!(matches!(next(&session), HostEvent::Connecting));
    assert!(matches!(next(&session), HostEvent::Registered));
    assert!(matches!(next(&session), HostEvent::Invoke(_)));
    assert!(matches!(next(&session), HostEvent::Invoke(_)));
    assert!(
        session
            .send_result("i", true, json!("x".repeat(270000)), None)
            .is_err()
    );
    session.send_result("i", true, json!({}), None).unwrap();
    assert!(session.send_result("i", true, json!({}), None).is_err());
    std::thread::sleep(Duration::from_millis(80));
    assert!(session.send_result("late", true, json!({}), None).is_err());
    session.stop();
    assert!(session.send_result("late", true, json!({}), None).is_err());
}

#[test]
fn rejects_unadvertised_duplicate_and_over_budget_invocations() {
    for (command, count, expected) in [
        ("shell", 1, "invalid_invoke"),
        ("safe.read", 2, "invalid_invoke"),
        ("safe.read", 9, "budget_exceeded"),
    ] {
        let body = format!(
            r#"
import sys,time,json
sys.stdin.readline()
for state in ['connecting','registered']:
 print(json.dumps({{'type':'state','state':state}}),flush=True)
for i in range({count}):
 print(json.dumps({{'type':'invoke','id':str(i) if {count}==9 else 'duplicate','command':'{command}','params':{{}},'timeout_ms':10000}}),flush=True)
time.sleep(10)
"#
        );
        let fixture = Fixture::new(&body);
        let session = HostSession::spawn(fixture.config()).unwrap();
        assert_eq!(closed(&session), expected);
    }
}

#[test]
fn write_backpressure_times_out_and_missing_helpers_are_redacted() {
    let fixture = Fixture::new("import time\ntime.sleep(20)\n");
    let mut config = fixture.config();
    config.commands = json!({"safe.read":{"description":"x".repeat(100000)}});
    let session = HostSession::spawn(config).unwrap();
    assert_eq!(closed(&session), "helper_timeout");
    let mut config = fixture.config();
    config.python = "/nonexistent-private-test-path".into();
    let session = HostSession::spawn(config).unwrap();
    assert_eq!(closed(&session), "helper_unavailable");
}

#[test]
#[ignore = "subprocess fixture invoked by parent_death_cleanup_kills_host"]
fn parent_death_fixture() {
    let fixture = Fixture(PathBuf::from(
        std::env::var_os("MUSE_LINK_TEST_DIR").unwrap(),
    ));
    let session = HostSession::spawn(fixture.config()).unwrap();
    assert!(matches!(next(&session), HostEvent::Connecting));
    std::process::exit(0); // No Drop: simulate abrupt GUI exit.
}

#[test]
fn parent_death_cleanup_kills_host() {
    let fixture = Fixture::new(
        r#"
import json,sys,time,os,pathlib
start=json.loads(sys.stdin.readline())
pathlib.Path(start['config_dir'],'pid').write_text(str(os.getpid()))
print('{"type":"state","state":"connecting"}',flush=True)
time.sleep(10)
"#,
    );
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "parent_death_fixture"])
        .env("MUSE_LINK_TEST_DIR", &fixture.0)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let pid: i32 = fs::read_to_string(fixture.0.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    let live = || {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            !stat
                .split(')')
                .nth(1)
                .unwrap()
                .trim_start()
                .starts_with('Z')
        })
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    while live() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let survived = live();
    if survived {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    assert!(!survived, "host was capable of acting after GUI death");
}

#[test]
fn partial_eof_and_exited_leaders_do_not_leave_helpers_active() {
    let fixture = Fixture::new("import sys\nsys.stdin.readline()\nprint('{',end='',flush=True)\n");
    let session = HostSession::spawn(fixture.config()).unwrap();
    assert_eq!(closed(&session), "protocol_error");
    let fixture = Fixture::new(
        r#"
import sys,os,time
sys.stdin.readline()
if os.fork()==0:
 time.sleep(10)
 os._exit(0)
os._exit(0)
"#,
    );
    let session = HostSession::spawn(fixture.config()).unwrap();
    assert_eq!(closed(&session), "helper_eof");
}

#[test]
fn nul_and_traversal_configuration_are_rejected_in_memory() {
    let fixture = Fixture::new("raise Exception('not run')");
    for path in ["/absolute/../traversal", "/nul\0path"] {
        let mut config = fixture.config();
        config.config_dir = path.into();
        assert!(HostSession::spawn(config).is_err());
    }
}

#[test]
fn expired_queued_invocations_are_not_dispatched() {
    let fixture = Fixture::new(
        r#"
import sys,time,json
sys.stdin.readline()
for state in ['connecting','registered']:
 print(json.dumps({'type':'state','state':state}),flush=True)
time.sleep(.05)
print(json.dumps({'type':'invoke','id':'expired','command':'safe.read','params':{},'timeout_ms':1}),flush=True)
time.sleep(10)
"#,
    );
    let session = HostSession::spawn(fixture.config()).unwrap();
    assert!(matches!(next(&session), HostEvent::Connecting));
    assert!(matches!(next(&session), HostEvent::Registered));
    std::thread::sleep(Duration::from_millis(150));
    assert!(matches!(
        session.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
}

#[test]
fn saturated_event_queue_preserves_terminal_closure() {
    let fixture = Fixture::new(
        r#"
import sys,time,json
sys.stdin.readline()
for state in ['connecting','registered']:
 print(json.dumps({'type':'state','state':state}),flush=True)
for i in range(64):
 print(json.dumps({'type':'invoke','id':str(i),'command':'safe.read','params':{},'timeout_ms':1}),flush=True)
 time.sleep(.01)
time.sleep(10)
"#,
    );
    let session = HostSession::spawn(fixture.config()).unwrap();
    std::thread::sleep(Duration::from_millis(650));
    assert_eq!(closed(&session), "queue_full");
    assert!(matches!(
        session.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
}

#[test]
fn states_and_invocation_result_roundtrip() {
    let fixture = Fixture::new(
        r#"
import json,sys,time
start=json.loads(sys.stdin.readline())
assert start['op']=='start' and start['node_id']=='test-node'
for state in ['connecting','registered']:
 print(json.dumps({'type':'state','state':state}),flush=True)
print(json.dumps({'type':'invoke','id':'i','command':'safe.read','params':{'x':1},'timeout_ms':1000}),flush=True)
result=json.loads(sys.stdin.readline())
assert result=={'op':'result','id':'i','ok':True,'output':{'answer':42}}
print(json.dumps({'type':'state','state':'closed','code':'stopped'}),flush=True)
time.sleep(10)
"#,
    );
    let session = HostSession::spawn(fixture.config()).unwrap();
    assert!(matches!(next(&session), HostEvent::Connecting));
    assert!(matches!(next(&session), HostEvent::Registered));
    let HostEvent::Invoke(invocation) = next(&session) else {
        panic!("missing invoke")
    };
    assert_eq!(invocation.id, "i");
    assert_eq!(invocation.params, json!({"x": 1}));
    assert_eq!(invocation.timeout_ms, 1000);
    session
        .send_result("i", true, json!({"answer":42}), None)
        .unwrap();
    assert!(matches!(next(&session), HostEvent::Closed {code} if code == "stopped"));
}
