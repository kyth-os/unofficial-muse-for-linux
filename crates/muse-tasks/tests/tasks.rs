use muse_tasks::{TaskStatus, TaskStore};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "muse-tasks-test-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs()
}

fn open(tag: &str) -> (TaskStore, PathBuf) {
    let dir = tmpdir(tag);
    let store = TaskStore::open(&dir).expect("open should succeed");
    (store, dir)
}

#[test]
fn create_get_list_roundtrip() {
    let (mut store, _dir) = open("roundtrip");
    let t = now();
    let a = store.create("buy milk", "2% gallon", t + 3600).unwrap();
    let b = store.create("call mom", "", t + 7200).unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(a.status, TaskStatus::Pending);
    assert!(!a.notified);

    let got = store.get(a.id).unwrap();
    assert_eq!(got, a);

    let list = store.list();
    assert_eq!(list.len(), 2);

    // Persists across reopen.
    let dir = store.dir().to_path_buf();
    drop(store);
    let store2 = TaskStore::open(&dir).unwrap();
    assert_eq!(store2.list().len(), 2);
}

#[test]
fn get_missing_is_not_found() {
    let (store, _dir) = open("missing");
    assert_eq!(store.get(12345), Err(muse_tasks::TaskError::NotFound));
}

#[test]
fn rejects_empty_and_long_titles() {
    let (mut store, _dir) = open("titles");
    let t = now();
    assert!(store.create("", "b", t + 60).is_err());
    assert!(store.create(&"x".repeat(200), "b", t + 60).is_ok());
    assert!(store.create(&"x".repeat(201), "b", t + 60).is_err());
}

#[test]
fn rejects_big_body() {
    let (mut store, _dir) = open("body");
    let t = now();
    assert!(store.create("ok", &"y".repeat(4096), t + 60).is_ok());
    assert!(store.create("ok", &"y".repeat(4097), t + 60).is_err());
}

#[test]
fn rejects_control_chars() {
    let (mut store, _dir) = open("control");
    let t = now();
    assert!(store.create("has\0nul", "b", t + 60).is_err());
    assert!(store.create("ok", "has\nnewline", t + 60).is_err());
    assert!(store.create("has\tescape", "b", t + 60).is_err());
    assert!(store.create("fine title!", "plain body.", t + 60).is_ok());
}

#[test]
fn rejects_out_of_range_due() {
    let (mut store, _dir) = open("due-bounds");
    let t = now();
    assert!(store.create("a", "", t.saturating_sub(3600)).is_err()); // at boundary, exclusive
    assert!(store.create("a", "", t.saturating_sub(3599)).is_ok());
    let five_y = 5 * 365 * 86400u64;
    assert!(store.create("a", "", t + five_y).is_ok());
    assert!(store.create("a", "", t + five_y + 1).is_err());
}

#[test]
fn transition_happy_paths() {
    let (mut store, _dir) = open("transitions");
    let t = now();

    let a = store.create("pausable", "", t + 60).unwrap();
    assert_eq!(store.pause(a.id).unwrap().status, TaskStatus::Paused);
    assert_eq!(store.resume(a.id).unwrap().status, TaskStatus::Pending);
    assert_eq!(store.complete(a.id).unwrap().status, TaskStatus::Done);

    let b = store.create("cancellable", "", t + 60).unwrap();
    assert_eq!(store.cancel(b.id).unwrap().status, TaskStatus::Cancelled);

    let c = store.create("cancel-paused", "", t + 60).unwrap();
    store.pause(c.id).unwrap();
    assert_eq!(store.cancel(c.id).unwrap().status, TaskStatus::Cancelled);
}

#[test]
fn transition_violations_rejected() {
    let (mut store, _dir) = open("violations");
    let t = now();

    let a = store.create("task", "", t + 60).unwrap();
    assert!(store.resume(a.id).is_err()); // resume only from Paused
    store.pause(a.id).unwrap();
    assert!(store.pause(a.id).is_err()); // already paused
    assert!(store.complete(a.id).is_err()); // complete only from Pending
    store.resume(a.id).unwrap();

    // Terminal states are immutable.
    let d = store.create("done", "", t + 60).unwrap();
    store.complete(d.id).unwrap();
    assert!(store.pause(d.id).is_err());
    assert!(store.resume(d.id).is_err());
    assert!(store.cancel(d.id).is_err());
    assert!(store.complete(d.id).is_err());
    assert!(store.mark_notified(d.id).is_err());

    let c = store.create("cancelled", "", t + 60).unwrap();
    store.cancel(c.id).unwrap();
    assert!(store.pause(c.id).is_err());
    assert!(store.resume(c.id).is_err());
    assert!(store.cancel(c.id).is_err());
    assert!(store.complete(c.id).is_err());
}

#[test]
fn due_filter_and_mark_notified() {
    let (mut store, _dir) = open("due");
    let t = now();
    let past = t.saturating_sub(10);
    let soon = store.create("soon", "", past).unwrap();
    let _future = store.create("future", "", t + 3600).unwrap();
    let paused = store.create("paused", "", past).unwrap();
    store.pause(paused.id).unwrap();
    let notified_task = store.create("pinged", "", past).unwrap();
    store.mark_notified(notified_task.id).unwrap();

    let due = store.due(t);
    assert_eq!(due.iter().map(|x| x.id).collect::<Vec<_>>(), vec![soon.id]);

    store.mark_notified(soon.id).unwrap();
    assert!(store.due(t).is_empty());
    assert!(store.get(soon.id).unwrap().notified);
}

#[test]
fn corrupt_line_fails_with_line_number() {
    let (mut store, _dir) = open("corrupt");
    let dir = store.dir().to_path_buf();
    let t = now();
    store.create("one", "", t + 60).unwrap();
    store.create("two", "", t + 60).unwrap();
    drop(store);

    // Append garbage as line 3.
    use std::io::Write;
    let path = dir.join(muse_tasks::STORE_FILE_NAME);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "this is not json").unwrap();
    drop(f);

    let err = TaskStore::open(&dir).expect_err("open must fail on corrupt line");
    match err {
        muse_tasks::TaskError::CorruptStore(msg) => {
            assert!(msg.contains("line 3"), "must name line 3, got: {msg}")
        }
        other => panic!("expected CorruptStore, got {other:?}"),
    }
}

#[test]
fn corrupt_middle_line_names_it() {
    let (mut store, _dir) = open("corrupt-mid");
    let dir = store.dir().to_path_buf();
    let t = now();
    store.create("one", "", t + 60).unwrap();
    store.create("two", "", t + 60).unwrap();
    drop(store);

    let path = dir.join(muse_tasks::STORE_FILE_NAME);
    let content = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = content.lines().collect();
    lines[0] = "{broken";
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let err = TaskStore::open(&dir).expect_err("open must fail");
    match err {
        muse_tasks::TaskError::CorruptStore(msg) => {
            assert!(msg.contains("line 1"), "must name line 1, got: {msg}")
        }
        other => panic!("expected CorruptStore, got {other:?}"),
    }
}

#[test]
fn refuses_symlink_dir() {
    let target = tmpdir("symlink-target");
    std::fs::create_dir_all(&target).unwrap();
    let link = tmpdir("symlink-link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(TaskStore::open(&link).is_err());
    let _ = std::fs::remove_dir_all(&target);
    let _ = std::fs::remove_file(&link);
}

#[test]
fn refuses_world_readable_dir() {
    let dir = tmpdir("world");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(TaskStore::open(&dir).is_err());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(TaskStore::open(&dir).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refuses_group_accessible_dir() {
    let dir = tmpdir("group");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(TaskStore::open(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crash_simulation_list_after_reopen() {
    // Drop the store without any close call; data must survive via atomic writes.
    let dir = tmpdir("crash");
    let t = now();
    {
        let mut store = TaskStore::open(&dir).unwrap();
        store.create("survive", "crash", t + 60).unwrap();
        // No close API: just drop.
    }
    let store2 = TaskStore::open(&dir).unwrap();
    let list = store2.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].title, "survive");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn store_file_is_0600() {
    let (mut store, _dir) = open("mode");
    let dir = store.dir().to_path_buf();
    store.create("x", "", now() + 60).unwrap();
    let path = dir.join(muse_tasks::STORE_FILE_NAME);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "store file must be 0600, got {mode:o}");
}

#[test]
fn ids_are_unique() {
    let (mut store, _dir) = open("ids");
    let t = now();
    let mut ids = std::collections::HashSet::new();
    for i in 0..200 {
        let task = store.create(&format!("task {i}"), "", t + 60 + i).unwrap();
        assert!(ids.insert(task.id), "duplicate id generated");
    }
}

#[test]
fn error_values_never_echo_content() {
    let (mut store, _dir) = open("no-echo");
    let secret = "s3cr3t-title-payload";
    store.create(secret, "", now() + 60).unwrap();
    let err = store.get(u64::MAX).unwrap_err();
    let dbg = format!("{err:?}");
    assert!(!dbg.contains(secret), "error leaked task content: {dbg}");
    // Corrupt-store error must not echo the bad line either.
    let dir = store.dir().to_path_buf();
    drop(store);
    let path = dir.join(muse_tasks::STORE_FILE_NAME);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    use std::io::Write;
    writeln!(f, "{{\"evil\": \"{secret}-in-file\"}}").unwrap();
    drop(f);
    let err = TaskStore::open(&dir).expect_err("must fail");
    assert!(!format!("{err:?}").contains(secret));
}

#[test]
fn quote_backslash_and_unicode_titles_round_trip_byte_exact() {
    // Coverage: writer escaping must survive reopen byte-exact.
    let (mut store, _dir) = open("escapes");
    let title = "say \"hi\" \\ bye ☃ café";
    let task = store
        .create(title, "body \\ \"quoted\"", now() + 3600)
        .unwrap();
    let dir = store.dir().to_path_buf();
    drop(store);
    let store2 = TaskStore::open(&dir).unwrap();
    assert_eq!(store2.get(task.id).unwrap(), task);
}

#[test]
fn duplicate_and_unknown_fields_are_corrupt_not_skipped() {
    let (mut store, _dir) = open("dupfields");
    let task = store.create("t", "", now() + 60).unwrap();
    let dir = store.dir().to_path_buf();
    drop(store);
    let path = dir.join(muse_tasks::STORE_FILE_NAME);
    let raw = std::fs::read_to_string(&path).unwrap();
    // Duplicate field.
    std::fs::write(&path, raw.replace("\"body\"", "\"body\",\"body\"")).unwrap();
    assert!(matches!(
        TaskStore::open(&dir).unwrap_err(),
        muse_tasks::TaskError::CorruptStore(_)
    ));
    // Unknown field.
    std::fs::write(&path, raw.replace('}', ",\"zzz\":1}")).unwrap();
    assert!(matches!(
        TaskStore::open(&dir).unwrap_err(),
        muse_tasks::TaskError::CorruptStore(_)
    ));
    let _ = task;
}
