use muse_local_tools::*;
use serde_json::{json, Value};
use std::{fs, os::unix::fs::symlink};
fn call(b: &ToolBroker, id: &str, command: &str, params: Value) -> Decision {
    b.invoke(Invocation {
        id: id.into(),
        command: command.into(),
        params,
        timeout_ms: 10000,
    })
}
fn read(b: &ToolBroker, g: &GrantInfo, id: &str, path: &str) -> ToolReply {
    match call(
        b,
        id,
        "files.read_text",
        json!({"grant_id":g.id,"path":path}),
    ) {
        Decision::Reply(r) => r,
        _ => panic!(),
    }
}
fn propose(b: &ToolBroker, g: &GrantInfo, id: &str, path: &str, content: &str) -> ApprovalRequest {
    match call(
        b,
        id,
        "files.create_text",
        json!({"grant_id":g.id,"path":path,"content":content}),
    ) {
        Decision::Approval(p) => p,
        _ => panic!(),
    }
}
#[test]
fn paths_symlinks_and_fifo_fail_closed() {
    let d = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "must not disclose").unwrap();
    symlink(outside.path(), d.path().join("escape")).unwrap();
    symlink(outside.path().join("secret"), d.path().join("link")).unwrap();
    let fifo = std::ffi::CString::new(d.path().join("fifo").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    for (i, path) in [
        "../secret",
        "/etc/passwd",
        "escape/secret",
        "link",
        "fifo",
        "./link",
        "a//b",
        "a/../b",
        "a\0b",
        "",
    ]
    .into_iter()
    .enumerate()
    {
        let r = read(&b, &g, &format!("bad{i}"), path);
        assert!(!r.ok, "{path}");
        assert!(r.output.is_null());
        assert!(!r.error.unwrap().contains("secret"));
    }
    let p = propose(&b, &g, "create", "escape/new", "not outside");
    assert!(!b.approve(&p.approval_id).ok);
    assert!(!outside.path().join("new").exists());
    let Decision::Reply(r) = call(
        &b,
        "search",
        "files.search",
        json!({"grant_id":g.id,"query":""}),
    ) else {
        panic!()
    };
    assert!(!r.ok);
    assert!(r.output.is_null());
    assert!(b.grant(&d.path().join("escape")).is_err());
}
#[test]
fn root_replacement_cannot_redirect_read_search_or_create() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("root");
    let original = fixture.path().join("original");
    let outside = fixture.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("file"), "original").unwrap();
    fs::write(outside.join("file"), "outside").unwrap();
    let b = ToolBroker::new();
    let g = b.grant(&root).unwrap();
    let p = propose(&b, &g, "new", "new", "approved");
    fs::rename(&root, &original).unwrap();
    symlink(&outside, &root).unwrap();
    assert_eq!(read(&b, &g, "read", "file").output["content"], "original");
    assert!(b.approve(&p.approval_id).ok);
    assert_eq!(
        fs::read_to_string(original.join("new")).unwrap(),
        "approved"
    );
    assert!(!outside.join("new").exists());
    let Decision::Reply(r) = call(
        &b,
        "s",
        "files.search",
        json!({"grant_id":g.id,"query":"new"}),
    ) else {
        panic!()
    };
    assert_eq!(r.output["paths"], json!(["new"]));
}
#[test]
fn existing_files_and_symlinks_never_overwritten_and_failures_leave_no_debris() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("existing"), "keep").unwrap();
    symlink("existing", d.path().join("alias")).unwrap();
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    for (id, path) in [
        ("existing", "existing"),
        ("alias", "alias"),
        ("missing", "missing/new"),
    ] {
        let p = propose(&b, &g, id, path, "replace");
        assert!(!b.approve(&p.approval_id).ok);
    }
    assert_eq!(
        fs::read_to_string(d.path().join("existing")).unwrap(),
        "keep"
    );
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 2);
}
#[test]
fn read_utf8_and_size_bounds() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("exact"), "x".repeat(65536)).unwrap();
    fs::write(d.path().join("large"), "x".repeat(65537)).unwrap();
    fs::write(d.path().join("binary"), [255]).unwrap();
    fs::create_dir(d.path().join("dir")).unwrap();
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    assert!(read(&b, &g, "ok", "exact").ok);
    assert_eq!(
        read(&b, &g, "large", "large").error.as_deref(),
        Some("too_large")
    );
    assert_eq!(
        read(&b, &g, "binary", "binary").error.as_deref(),
        Some("invalid_utf8")
    );
    assert!(!read(&b, &g, "dir", "dir").ok);
    let Decision::Reply(r) = call(
        &b,
        "content",
        "files.create_text",
        json!({"grant_id":g.id,"path":"large_new","content":"x".repeat(65537)}),
    ) else {
        panic!()
    };
    assert!(!r.ok);
    assert!(!d.path().join("large_new").exists());
}
#[test]
fn malformed_unknown_and_unsupported_operations_are_rejected() {
    let d = tempfile::tempdir().unwrap();
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    for (i, (command, params)) in [
        (
            "system.run",
            json!({"grant_id":g.id,"command":"touch pwned"}),
        ),
        ("files.read_text", Value::Null),
        ("files.read_text", json!({"grant_id":g.id,"path":1})),
        (
            "files.read_text",
            json!({"grant_id":g.id,"path":"x","extra":true}),
        ),
        ("files.search", json!({"grant_id":g.id,"query":"a/b"})),
        (
            "files.create_text",
            json!({"grant_id":"absent","path":"new","content":"x"}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(matches!(call(&b,&format!("m{i}"),command,params),Decision::Reply(r) if !r.ok));
    }
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0);
}
#[test]
fn empty_grant_does_not_silently_authorize_cwd() {
    let b = ToolBroker::new();
    assert!(b.grant(std::path::Path::new("")).is_err());
    assert!(b.grants().is_empty());
}
