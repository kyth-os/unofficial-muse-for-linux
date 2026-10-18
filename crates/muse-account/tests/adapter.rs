use muse_account::{AccountClient, BridgeConfig};
use std::{
    fs,
    path::PathBuf,
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
                "muse-account-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bridge.py"), script).unwrap();
        Self(dir)
    }
    fn client(&self) -> AccountClient {
        AccountClient::new(BridgeConfig {
            python: PathBuf::from("/usr/bin/python3"),
            script: self.0.join("bridge.py"),
            config_dir: self.0.join("config"),
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn history_preserves_selected_scope_ids_roles_timestamps_and_status() {
    let f = Fixture::new(
        r#"
import json, sys
r = json.load(sys.stdin)
assert r['op'] == 'history' and r['thread'] == ' exact-id ' and r['limit'] == 20
print(json.dumps({'ok':True,'data':[{'message_id':'msg', 'seq':9, 'role':'assistant',
    'text':'response','timestamp':'123','status':'complete'}]}))
"#,
    );
    let history = f.client().history(Some(" exact-id "), 20).unwrap();
    assert_eq!(history[0].message_id, "msg");
    assert_eq!(history[0].seq, 9);
    assert_eq!(history[0].role, "assistant");
    assert_eq!(history[0].timestamp.as_deref(), Some("123"));
    assert_eq!(history[0].status.as_deref(), Some("complete"));
    assert!(f.client().history(None, 0).is_err());
    assert!(f.client().history(None, 201).is_err());
    assert!(f.client().history(Some("  "), 1).is_err());
}

#[test]
fn tasks_parses_schedule_and_run_types() {
    let f = Fixture::new(
        r#"
import json, sys
assert json.load(sys.stdin)['op'] == 'tasks'
print(json.dumps({'ok':True,'data':{'schedules':[{'id':'job-1','title':'Daily','enabled':True,
 'timezone':'UTC','next_run_at_utc':123}],'runs':[{'run_id':'run-1','job_id':'job-1','status':'success',
 'result_summary':'Done','error_text':None,'scheduled_for_utc':120}]}}))
"#,
    );
    let result = f.client().tasks().unwrap();
    assert_eq!(result.schedules[0].id, "job-1");
    assert_eq!(result.schedules[0].next_run_at_utc, Some(123));
    assert_eq!(result.runs[0].status, "success");
    assert_eq!(result.runs[0].scheduled_for_utc, Some(120));
}

#[test]
fn send_does_not_treat_dispatched_as_confirmed_or_retry() {
    let f = Fixture::new(
        r#"
import json, sys, pathlib
assert len(sys.argv) == 1
r = json.load(sys.stdin)
assert r['op'] == 'send' and r['thread'] == 'thread-1' and r['message'] == 'exact prompt'
marker = pathlib.Path(__file__).with_suffix('.sent')
assert not marker.exists(), 'must not retry'
marker.touch()
print(json.dumps({'ok':True,'data':{'sent':True,'confirmed':False,'message_id':None}}))
"#,
    );
    let receipt = f.client().send(Some("thread-1"), "exact prompt").unwrap();
    assert!(receipt.sent);
    assert!(!receipt.confirmed);
    assert!(receipt.message_id.is_none());
    assert!(f.client().send(None, "  ").is_err());
}

#[test]
fn probe_is_read_only_by_default_and_prints_only_counts() {
    let f = Fixture::new(
        r#"
import json, sys
r = json.load(sys.stdin)
if r['op'] == 'connect':
 data = {'identity_name':'PRIVATE_NAME','vm_id':'PRIVATE_VM','threads':[]}
elif r['op'] == 'history':
 data = [{'message_id':'PRIVATE_ID','seq':1,'role':'user','text':'PRIVATE_MESSAGE','timestamp':None,'status':None}]
elif r['op'] == 'tasks':
 data = {'schedules':[],'runs':[]}
else:
 raise AssertionError('must not send')
print(json.dumps({'ok':True,'data':data}))
"#,
    );
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_account-probe"))
        .args([
            "/usr/bin/python3",
            f.0.join("bridge.py").to_str().unwrap(),
            f.0.to_str().unwrap(),
        ])
        .env_remove("MUSE_ACCOUNT_PROBE_ALLOW_SEND")
        .env("MUSE_ACCOUNT_PROBE_SEND_TEXT", "must not implicitly send")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        text,
        "connected=true threads=0 history_messages=1 schedules=0 runs=0\n"
    );
    assert!(!text.contains("PRIVATE"));
}

#[test]
fn structured_auth_vm_and_timeout_errors_remain_typed_and_safe() {
    use muse_account::ErrorCode;
    for (wire, expected) in [
        ("auth_required", ErrorCode::AuthRequired),
        ("vm_unavailable", ErrorCode::VmUnavailable),
        ("timeout", ErrorCode::Timeout),
    ] {
        let f = Fixture::new(&format!(
            r#"
import json, sys
json.load(sys.stdin)
print(json.dumps({{'ok':False,'error':{{'code':'{wire}','uncertain':False,'message':'SECRET'}}}}))
"#
        ));
        let error = f.client().connect().unwrap_err();
        assert_eq!(error.code, expected);
        assert!(!error.uncertain);
        assert!(!format!("{error} {error:?}").contains("SECRET"));
    }
}

#[test]
fn malformed_output_and_failed_child_never_echo_stdout_or_stderr() {
    use muse_account::ErrorCode;
    for (script, expected) in [
        ("print('SECRET malformed JSON')", ErrorCode::ProtocolError),
        (
            "import sys\nsys.stderr.write('SECRET credential')\nsys.exit(1)",
            ErrorCode::BridgeFailure,
        ),
        (
            "print('{\"ok\":false,\"error\":{\"code\":\"SECRET\",\"uncertain\":false}}')",
            ErrorCode::ProtocolError,
        ),
        (
            "print('{\"ok\":true,\"data\":{\"identity_name\":true}}')",
            ErrorCode::ProtocolError,
        ),
    ] {
        let f = Fixture::new(script);
        let error = f.client().send(None, "prompt").unwrap_err();
        assert_eq!(error.code, expected);
        assert!(error.uncertain);
        assert!(!format!("{error} {error:?}").contains("SECRET"));
    }
}

#[test]
fn construction_is_inert_and_invalid_inputs_never_spawn_a_child() {
    use muse_account::ErrorCode;
    fn send_sync<T: Send + Sync>() {}
    send_sync::<AccountClient>();
    let f = Fixture::new("raise AssertionError('must not execute')");
    let mut config = BridgeConfig {
        python: PathBuf::from("/usr/bin/python3"),
        script: f.0.join("bridge.py"),
        config_dir: PathBuf::from("relative"),
    };
    let client = AccountClient::new(config.clone());
    let _clone = client.clone();
    assert_eq!(
        client.connect().unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        client
            .send(None, &"x".repeat(512 * 1024 + 1))
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        client.history(Some("bad\0thread"), 1).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    config.config_dir = f.0.clone();
    config.python = f.0.join("missing-python");
    let error = AccountClient::new(config).connect().unwrap_err();
    assert_eq!(error.code, ErrorCode::BridgeUnavailable);
    assert!(!error.uncertain);
}

#[test]
fn inconsistent_confirmation_receipt_is_rejected_as_uncertain() {
    use muse_account::ErrorCode;
    let f = Fixture::new(
        "print('{\"ok\":true,\"data\":{\"sent\":true,\"confirmed\":true,\"message_id\":null}}')",
    );
    let error = f.client().send(None, "prompt").unwrap_err();
    assert_eq!(error.code, ErrorCode::ProtocolError);
    assert!(error.uncertain);
}

#[test]
fn bridge_receives_only_allowlisted_environment() {
    let f = Fixture::new(
        r#"
import json, os, sys
assert os.environ['MUSE_MCP_ENABLE_RAW'] == '0'
assert os.environ['PYTHONDONTWRITEBYTECODE'] == '1'
for forbidden in ('OPENAI_API_KEY','ANTHROPIC_API_KEY','HERMES_SECRET','PYTHONPATH','PYTHONHOME','MUSE_CONFIG_DIR','MUSE_VM_ID'):
 assert forbidden not in os.environ
r = json.load(sys.stdin)
if r['op'] == 'connect': data = {'identity_name':'User','vm_id':'VM','threads':[]}
elif r['op'] == 'history': data = []
elif r['op'] == 'tasks': data = {'schedules':[],'runs':[]}
else: raise AssertionError('must not send')
print(json.dumps({'ok':True,'data':data}))
"#,
    );
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_account-probe"));
    command
        .env_clear()
        .env("HOME", &f.0)
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("MUSE_MCP_ENABLE_RAW", "1")
        .args([
            "/usr/bin/python3",
            f.0.join("bridge.py").to_str().unwrap(),
            f.0.to_str().unwrap(),
        ]);
    for key in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "HERMES_SECRET",
        "PYTHONPATH",
        "PYTHONHOME",
        "MUSE_CONFIG_DIR",
        "MUSE_VM_ID",
    ] {
        command.env(key, "SYNTHETIC_SECRET");
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "probe rejected safe fake bridge");
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "connected=true threads=0 history_messages=0 schedules=0 runs=0\n"
    );
}

#[test]
fn connect_sends_json_via_stdin_and_parses_typed_snapshot() {
    let f = Fixture::new(
        r#"
import json, sys
assert len(sys.argv) == 1
request = json.load(sys.stdin)
assert request['op'] == 'connect'
assert request['config_dir'].endswith('/config')
print(json.dumps({'ok': True, 'data': {'identity_name': 'User', 'vm_id': 'vm-1',
    'threads': [{'session_id':'thread-1', 'title':'Hello', 'archived':False, 'updated_at':None}]}}))
"#,
    );
    let snapshot = f.client().connect().unwrap();
    assert_eq!(snapshot.identity_name, "User");
    assert_eq!(snapshot.vm_id, "vm-1");
    assert_eq!(snapshot.threads[0].session_id, "thread-1");
}
