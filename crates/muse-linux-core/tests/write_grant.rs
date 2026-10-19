use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use muse_linux_core::filesystem::{GrantError, MAX_WRITE_BYTES, WriteGrant};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(10_000);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "muse-linux-write-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create isolated test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn no_tmp_files(root: &Path) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).expect("read test dir");
        for entry in entries {
            let entry = entry.expect("read test entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(".tmp") {
                return false;
            }
            if entry.file_type().expect("file type").is_dir() {
                stack.push(entry.path());
            }
        }
    }
    true
}

#[test]
fn write_grant_creates_and_overwrites_bytes() {
    let root = TempDir::new("roundtrip");
    let grant = WriteGrant::new(root.path()).unwrap();

    // Byte-oriented: no UTF-8 requirement.
    grant
        .create_file(Path::new("blob.bin"), &[0xFF, 0xFE, 0x00, b'a'])
        .unwrap();
    assert_eq!(
        fs::read(root.path().join("blob.bin")).unwrap(),
        vec![0xFF, 0xFE, 0x00, b'a']
    );

    grant
        .overwrite_file(Path::new("blob.bin"), b"second")
        .unwrap();
    assert_eq!(fs::read(root.path().join("blob.bin")).unwrap(), b"second");
}

#[test]
fn write_grant_new_rejects_non_directory_root() {
    let root = TempDir::new("bad-root");
    fs::write(root.path().join("file.txt"), b"x").unwrap();
    assert!(matches!(
        WriteGrant::new(root.path().join("file.txt")),
        Err(GrantError::InvalidRoot)
    ));
    assert!(matches!(
        WriteGrant::new(root.path().join("missing")),
        Err(GrantError::Io(_))
    ));
}

#[test]
fn create_refuses_when_target_exists() {
    let root = TempDir::new("create-exists");
    let grant = WriteGrant::new(root.path()).unwrap();
    grant.create_file(Path::new("note.txt"), b"first").unwrap();

    assert!(matches!(
        grant.create_file(Path::new("note.txt"), b"second"),
        Err(GrantError::AlreadyExists { .. })
    ));
    // Original untouched.
    assert_eq!(fs::read(root.path().join("note.txt")).unwrap(), b"first");
    assert!(no_tmp_files(root.path()));
}

#[test]
fn create_refuses_oversize_content() {
    let root = TempDir::new("create-large");
    let grant = WriteGrant::new(root.path()).unwrap();
    let big = vec![b'x'; MAX_WRITE_BYTES as usize + 1];

    assert!(matches!(
        grant.create_file(Path::new("big.bin"), &big),
        Err(GrantError::ContentTooLarge { .. })
    ));
    assert!(!root.path().join("big.bin").exists());
    assert!(no_tmp_files(root.path()));
}

#[test]
fn create_accepts_exactly_max_bytes() {
    let root = TempDir::new("create-max");
    let grant = WriteGrant::new(root.path()).unwrap();
    let max = vec![b'y'; MAX_WRITE_BYTES as usize];
    grant.create_file(Path::new("max.bin"), &max).unwrap();
    assert_eq!(
        fs::read(root.path().join("max.bin")).unwrap().len(),
        max.len()
    );
}

#[test]
fn overwrite_refuses_missing_target() {
    let root = TempDir::new("overwrite-missing");
    let grant = WriteGrant::new(root.path()).unwrap();
    assert!(matches!(
        grant.overwrite_file(Path::new("ghost.txt"), b"data"),
        Err(GrantError::NotFound { .. })
    ));
    assert!(no_tmp_files(root.path()));
}

#[test]
fn overwrite_refuses_directories_and_oversize() {
    let root = TempDir::new("overwrite-dir");
    let grant = WriteGrant::new(root.path()).unwrap();
    fs::create_dir(root.path().join("subdir")).unwrap();

    assert!(matches!(
        grant.overwrite_file(Path::new("subdir"), b"data"),
        Err(GrantError::NotAFile)
    ));

    grant.create_file(Path::new("ok.txt"), b"keep").unwrap();
    let big = vec![b'z'; MAX_WRITE_BYTES as usize + 1];
    assert!(matches!(
        grant.overwrite_file(Path::new("ok.txt"), &big),
        Err(GrantError::ContentTooLarge { .. })
    ));
    // Failed overwrite leaves the committed bytes intact and no tmp files.
    assert_eq!(fs::read(root.path().join("ok.txt")).unwrap(), b"keep");
    assert!(no_tmp_files(root.path()));
}

#[test]
fn rename_moves_file_and_refuses_onto_existing() {
    let root = TempDir::new("rename");
    let grant = WriteGrant::new(root.path()).unwrap();
    grant.create_file(Path::new("a.txt"), b"payload").unwrap();
    grant.create_file(Path::new("b.txt"), b"other").unwrap();

    assert!(matches!(
        grant.rename(Path::new("a.txt"), Path::new("b.txt")),
        Err(GrantError::AlreadyExists { .. })
    ));
    assert_eq!(fs::read(root.path().join("a.txt")).unwrap(), b"payload");
    assert_eq!(fs::read(root.path().join("b.txt")).unwrap(), b"other");

    grant
        .rename(Path::new("a.txt"), Path::new("moved.txt"))
        .unwrap();
    assert!(!root.path().join("a.txt").exists());
    assert_eq!(fs::read(root.path().join("moved.txt")).unwrap(), b"payload");
}

#[test]
fn rename_refuses_missing_source() {
    let root = TempDir::new("rename-missing");
    let grant = WriteGrant::new(root.path()).unwrap();
    assert!(matches!(
        grant.rename(Path::new("nope.txt"), Path::new("dest.txt")),
        Err(GrantError::NotFound { .. })
    ));
}

#[test]
fn delete_removes_only_regular_files() {
    let root = TempDir::new("delete");
    let grant = WriteGrant::new(root.path()).unwrap();
    grant.create_file(Path::new("gone.txt"), b"bye").unwrap();
    fs::create_dir(root.path().join("keepdir")).unwrap();

    // Directories are never deleted: no recursive delete.
    assert!(matches!(
        grant.delete_file(Path::new("keepdir")),
        Err(GrantError::NotAFile)
    ));
    assert!(root.path().join("keepdir").is_dir());

    assert!(matches!(
        grant.delete_file(Path::new("missing.txt")),
        Err(GrantError::NotFound { .. })
    ));

    grant.delete_file(Path::new("gone.txt")).unwrap();
    assert!(!root.path().join("gone.txt").exists());
}

#[test]
fn create_dir_is_single_level() {
    let root = TempDir::new("mkdir");
    let grant = WriteGrant::new(root.path()).unwrap();

    grant.create_dir(Path::new("solo")).unwrap();
    assert!(root.path().join("solo").is_dir());

    assert!(matches!(
        grant.create_dir(Path::new("solo")),
        Err(GrantError::AlreadyExists { .. })
    ));
    // No recursive creation: missing parent refuses.
    assert!(matches!(
        grant.create_dir(Path::new("no-parent/child")),
        Err(GrantError::NotFound { .. })
    ));
    assert!(!root.path().join("no-parent").exists());
}

#[test]
fn write_ops_reject_traversal_and_absolute_paths() {
    let root = TempDir::new("confine");
    let outside = TempDir::new("confine-outside");
    fs::write(outside.path().join("secret.txt"), b"outside").unwrap();
    let grant = WriteGrant::new(root.path()).unwrap();
    let absolute = outside.path().join("secret.txt");

    for rel in [
        PathBuf::from("../confine-outside/pwned.txt"),
        PathBuf::from("../../pwned.txt"),
        PathBuf::from("a/../../b.txt"),
        absolute.clone(),
    ] {
        assert!(
            matches!(
                grant.create_file(&rel, b"x"),
                Err(GrantError::PathOutsideGrant)
            ),
            "create {rel:?}"
        );
        assert!(
            matches!(
                grant.overwrite_file(&rel, b"x"),
                Err(GrantError::PathOutsideGrant)
            ),
            "overwrite {rel:?}"
        );
        assert!(
            matches!(grant.delete_file(&rel), Err(GrantError::PathOutsideGrant)),
            "delete {rel:?}"
        );
        assert!(
            matches!(grant.create_dir(&rel), Err(GrantError::PathOutsideGrant)),
            "mkdir {rel:?}"
        );
        assert!(
            matches!(
                grant.rename(&rel, Path::new("dest.txt")),
                Err(GrantError::PathOutsideGrant)
            ),
            "rename-src {rel:?}"
        );
        assert!(
            matches!(
                grant.rename(Path::new("dest.txt"), &rel),
                Err(GrantError::PathOutsideGrant)
            ),
            "rename-dst {rel:?}"
        );
    }

    // Escape left no trace outside the grant.
    assert_eq!(
        fs::read(outside.path().join("secret.txt")).unwrap(),
        b"outside"
    );
    assert!(!outside.path().join("pwned.txt").exists());
    assert!(no_tmp_files(root.path()));
}

#[test]
fn write_ops_reject_nul_and_empty_paths() {
    let root = TempDir::new("bad-names");
    let grant = WriteGrant::new(root.path()).unwrap();

    assert!(matches!(
        grant.create_file(Path::new("a\0b.txt"), b"x"),
        Err(GrantError::InvalidPath { .. })
    ));
    assert!(matches!(
        grant.create_file(Path::new(""), b"x"),
        Err(GrantError::InvalidPath { .. })
    ));
    assert!(matches!(
        grant.overwrite_file(Path::new(""), b"x"),
        Err(GrantError::InvalidPath { .. })
    ));
    assert!(no_tmp_files(root.path()));
}

#[cfg(unix)]
#[test]
fn write_ops_refuse_symlinked_directories_planted_inside() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new("linkdir-root");
    let outside = TempDir::new("linkdir-outside");
    symlink(outside.path(), root.path().join("linkdir")).unwrap();
    let grant = WriteGrant::new(root.path()).unwrap();

    assert!(matches!(
        grant.create_file(Path::new("linkdir/pwned.txt"), b"x"),
        Err(GrantError::PathOutsideGrant)
    ));
    assert!(matches!(
        grant.create_dir(Path::new("linkdir/newdir")),
        Err(GrantError::PathOutsideGrant)
    ));
    assert!(!outside.path().join("pwned.txt").exists());
    assert!(no_tmp_files(root.path()));
}

#[cfg(unix)]
#[test]
fn overwrite_refuses_symlink_targets() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new("linkfile-root");
    let outside = TempDir::new("linkfile-outside");
    fs::write(outside.path().join("secret.txt"), b"do not touch").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        root.path().join("link.txt"),
    )
    .unwrap();
    let grant = WriteGrant::new(root.path()).unwrap();

    assert!(matches!(
        grant.overwrite_file(Path::new("link.txt"), b"pwned"),
        Err(GrantError::PathOutsideGrant)
    ));
    assert!(matches!(
        grant.delete_file(Path::new("link.txt")),
        Err(GrantError::PathOutsideGrant)
    ));
    // Symlink itself is untouched and the outside target is intact.
    assert!(
        fs::symlink_metadata(root.path().join("link.txt"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read(outside.path().join("secret.txt")).unwrap(),
        b"do not touch"
    );
    assert!(no_tmp_files(root.path()));
}

#[test]
fn failed_writes_leave_no_stray_tmp_files() {
    let root = TempDir::new("tmp-clean");
    let grant = WriteGrant::new(root.path()).unwrap();
    let big = vec![b'q'; MAX_WRITE_BYTES as usize + 1];

    let _ = grant.create_file(Path::new("fail.bin"), &big);
    let _ = grant.overwrite_file(Path::new("missing.txt"), b"x");
    let _ = grant.create_dir(Path::new("missing-parent/child"));

    assert!(no_tmp_files(root.path()));
}

#[test]
fn committed_state_survives_reopen_after_drop() {
    let root = TempDir::new("crash");
    {
        let grant = WriteGrant::new(root.path()).unwrap();
        grant
            .create_file(Path::new("durable.txt"), b"committed")
            .unwrap();
        drop(grant);
    }
    // Crash simulation: reopen the grant; the atomically renamed write is intact.
    let reopened = WriteGrant::new(root.path()).unwrap();
    reopened
        .overwrite_file(Path::new("durable.txt"), b"second commit")
        .unwrap();
    drop(reopened);

    assert_eq!(
        fs::read(root.path().join("durable.txt")).unwrap(),
        b"second commit"
    );
}

#[test]
fn files_written_are_readable_through_the_read_grant() {
    use muse_linux_core::filesystem::FilesystemGrant;

    let root = TempDir::new("interop");
    let writer = WriteGrant::new(root.path()).unwrap();
    writer
        .create_file(Path::new("hello.txt"), "hello from Muse Linux".as_bytes())
        .unwrap();

    // A read grant never authorizes writes: it only exposes the read surface.
    let reader = FilesystemGrant::new(root.path()).unwrap();
    assert_eq!(
        reader.read_text(Path::new("hello.txt")).unwrap(),
        "hello from Muse Linux"
    );
}

#[test]
fn write_errors_never_echo_absolute_paths_or_content() {
    let root = TempDir::new("no-leak");
    let outside = TempDir::new("no-leak-outside");
    let grant = WriteGrant::new(root.path()).unwrap();
    let absolute = outside.path().join("secret.txt");
    let root_str = root.path().to_string_lossy().into_owned();

    let errors = vec![
        grant
            .create_file(&absolute, b"supersecret-content")
            .unwrap_err(),
        grant
            .create_file(Path::new("../escape.txt"), b"supersecret-content")
            .unwrap_err(),
        grant
            .create_file(Path::new("a\0b"), b"supersecret-content")
            .unwrap_err(),
        grant
            .overwrite_file(Path::new("missing.txt"), b"supersecret-content")
            .unwrap_err(),
    ];
    for error in errors {
        let rendered = format!("{error}");
        assert!(!rendered.contains(&root_str), "leaked root in {rendered:?}");
        assert!(
            !rendered.contains("supersecret-content"),
            "leaked content in {rendered:?}"
        );
    }
}

#[test]
fn concurrent_creates_all_land_without_tmp_leftovers() {
    // Exercises the tmp mint + handle-reuse path under contention: distinct
    // names must all succeed and no .tmp files may survive.
    use std::sync::Barrier;
    let root = TempDir::new("contend");
    let grant = WriteGrant::new(root.path()).unwrap();
    let grant = std::sync::Arc::new(grant);
    let barrier = std::sync::Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for i in 0..8 {
        let (grant, barrier) = (
            std::sync::Arc::clone(&grant),
            std::sync::Arc::clone(&barrier),
        );
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for j in 0..10 {
                let name = format!("t{i}-{j}.txt");
                let bytes = format!("payload-{i}-{j}").into_bytes();
                grant.create_file(Path::new(&name), &bytes).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().expect("worker thread");
    }
    for i in 0..8 {
        for j in 0..10 {
            let name = format!("t{i}-{j}.txt");
            let bytes = std::fs::read(root.path().join(&name)).unwrap();
            assert_eq!(bytes, format!("payload-{i}-{j}").into_bytes());
        }
    }
    assert!(no_tmp_files(root.path()), "stray tmp files survived");
}
