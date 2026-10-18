use muse_local_tools::*;
use serde_json::json;
fn create(b: &ToolBroker, g: &GrantInfo, id: &str, path: &str, content: &str, ms: u64) -> Decision {
    b.invoke(Invocation {
        id: id.into(),
        command: "files.create_text".into(),
        params: json!({"grant_id":g.id,"path":path,"content":content}),
        timeout_ms: ms,
    })
}
#[test]
fn create_requires_exact_once_approval() {
    let dir = tempfile::tempdir().unwrap();
    let b = ToolBroker::new();
    let g = b.grant(dir.path()).unwrap();
    let Decision::Approval(mut proposal) = create(&b, &g, "new", "note", "exact", 1000) else {
        panic!("must prompt")
    };
    assert!(!dir.path().join("note").exists());
    proposal.content = "tampered".into();
    proposal.path = "other".into();
    assert!(b.approve(&proposal.approval_id).ok);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note")).unwrap(),
        "exact"
    );
    assert!(!b.approve(&proposal.approval_id).ok);
    assert!(matches!(create(&b,&g,"new","again","x",1000),Decision::Reply(r) if !r.ok));
    assert!(!dir.path().join("again").exists());
}
#[test]
fn denied_expired_and_revoked_proposals_cannot_commit() {
    let dir = tempfile::tempdir().unwrap();
    let b = ToolBroker::new();
    let g = b.grant(dir.path()).unwrap();
    let Decision::Approval(p) = create(&b, &g, "deny", "denied", "secret", 1000) else {
        panic!()
    };
    assert_eq!(b.deny(&p.approval_id).error.as_deref(), Some("denied"));
    assert!(!b.approve(&p.approval_id).ok);
    let Decision::Approval(p) = create(&b, &g, "expire", "expired", "secret", 20) else {
        panic!()
    };
    std::thread::sleep(std::time::Duration::from_millis(30));
    let expired = b.expire();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].id, "expire");
    assert!(!b.approve(&p.approval_id).ok);
    let Decision::Approval(p) = create(&b, &g, "revoke", "revoked", "secret", 1000) else {
        panic!()
    };
    let revoked = b.revoke_all();
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].id, "revoke");
    assert!(!b.approve(&p.approval_id).ok);
    assert!(b.grants().is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
#[test]
fn stale_approval_from_other_broker_cannot_authorize_new_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let old = ToolBroker::new();
    let new = ToolBroker::new();
    let a = old.grant(dir.path()).unwrap();
    let b = new.grant(dir.path()).unwrap();
    let Decision::Approval(p) = create(&old, &a, "a", "old", "old", 1000) else {
        panic!()
    };
    let Decision::Approval(q) = create(&new, &b, "b", "new", "new", 1000) else {
        panic!()
    };
    assert_ne!(p.approval_id, q.approval_id);
    assert!(!new.approve(&p.approval_id).ok);
    assert!(!dir.path().join("new").exists());
    assert!(q.expires_in_seconds > 0);
}
#[test]
fn expiry_is_enforced_even_without_timer_and_budgets_are_capped() {
    let dir = tempfile::tempdir().unwrap();
    let b = ToolBroker::new();
    let g = b.grant(dir.path()).unwrap();
    let Decision::Approval(p) = create(&b, &g, "expire-direct", "a", "x", 20) else {
        panic!()
    };
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(b.approve(&p.approval_id).error.as_deref(), Some("expired"));
    assert!(
        matches!(create(&b,&g,"zero","a","x",0),Decision::Reply(r) if r.error.as_deref()==Some("expired"))
    );
    let Decision::Approval(p) = create(&b, &g, "cap", "a", "x", u64::MAX) else {
        panic!()
    };
    assert!(p.expires_in_seconds <= 120);
    assert!(p.expires_in_seconds > 0);
    assert!(b.clone().deny(&p.approval_id).error.is_some());
    assert!(!b.approve(&p.approval_id).ok);
    assert!(!dir.path().join("a").exists());
}
#[test]
fn new_file_can_be_exactly_64k_in_existing_parent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let b = ToolBroker::new();
    let g = b.grant(dir.path()).unwrap();
    let text = "é".repeat(32768);
    let Decision::Approval(p) = create(&b, &g, "max", "sub/file", &text, 10000) else {
        panic!()
    };
    assert!(b.approve(&p.approval_id).ok);
    assert_eq!(
        std::fs::read(dir.path().join("sub/file")).unwrap(),
        text.as_bytes()
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(dir.path().join("sub/file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
#[test]
fn concurrent_creates_never_replace_winner() {
    let d = tempfile::tempdir().unwrap();
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    let Decision::Approval(a) = create(&b, &g, "one", "file", "one", 10000) else {
        panic!()
    };
    let Decision::Approval(z) = create(&b, &g, "two", "file", "two", 10000) else {
        panic!()
    };
    let worker = b.clone();
    let t = std::thread::spawn(move || worker.approve(&a.approval_id));
    let second = b.approve(&z.approval_id);
    let first = t.join().unwrap();
    assert_ne!(first.ok, second.ok);
    assert_eq!(
        std::fs::read_to_string(d.path().join("file")).unwrap(),
        if first.ok { "one" } else { "two" }
    );
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
}
#[test]
fn bounded_pending_and_id_cache_fail_closed_without_eviction() {
    let d = tempfile::tempdir().unwrap();
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    for i in 0..128 {
        assert!(matches!(
            create(&b, &g, &format!("p{i}"), "new", "x", 120000),
            Decision::Approval(_)
        ));
    }
    assert!(
        matches!(create(&b,&g,"full","new","x",120000),Decision::Reply(r) if r.error.as_deref()==Some("capacity"))
    );
    assert_eq!(b.revoke_all().len(), 128);
    for i in 129..4096 {
        let _ = create(&b, &g, &format!("p{i}"), "new", "x", 1000);
    }
    assert!(
        matches!(create(&b,&g,"p0","new","x",1000),Decision::Reply(r) if r.error.as_deref()==Some("duplicate_id"))
    );
    assert!(
        matches!(create(&b,&g,"unseen","new","x",1000),Decision::Reply(r) if r.error.as_deref()==Some("capacity"))
    );
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
}
