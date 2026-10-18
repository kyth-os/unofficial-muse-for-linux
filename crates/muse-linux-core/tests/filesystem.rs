use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use muse_linux_core::filesystem::{FilesystemGrant, GrantError};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("muse-linux-{label}-{}-{id}", std::process::id()));
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

#[test]
fn granted_root_lists_and_reads_files_under_that_root() {
    let root = TempDir::new("read");
    fs::write(root.path().join("note.txt"), "hello from Muse Linux").unwrap();
    fs::create_dir(root.path().join("subdir")).unwrap();
    fs::write(root.path().join("subdir/inside.txt"), "nested").unwrap();

    let grant = FilesystemGrant::new(root.path()).unwrap();
    let entries = grant.list(Path::new(".")).unwrap();

    assert!(
        entries
            .iter()
            .any(|entry| entry.relative_path == Path::new("note.txt"))
    );
    assert_eq!(
        grant.read_text(Path::new("note.txt")).unwrap(),
        "hello from Muse Linux"
    );
    assert_eq!(
        grant.read_text(Path::new("subdir/inside.txt")).unwrap(),
        "nested"
    );
}

#[test]
fn granted_root_rejects_parent_traversal() {
    let root = TempDir::new("traversal");
    let outside = TempDir::new("outside");
    fs::write(outside.path().join("secret.txt"), "must not be read").unwrap();

    let grant = FilesystemGrant::new(root.path()).unwrap();

    assert!(matches!(
        grant.read_text(Path::new("../outside/secret.txt")),
        Err(GrantError::PathOutsideGrant)
    ));
}

#[test]
fn granted_root_rejects_absolute_paths() {
    let root = TempDir::new("absolute");
    let outside = TempDir::new("absolute-outside");
    fs::write(outside.path().join("secret.txt"), "must not be read").unwrap();

    let grant = FilesystemGrant::new(root.path()).unwrap();

    assert!(matches!(
        grant.read_text(&outside.path().join("secret.txt")),
        Err(GrantError::PathOutsideGrant)
    ));
}

#[cfg(unix)]
#[test]
fn granted_root_rejects_symlinks_that_escape_the_root() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new("symlink-root");
    let outside = TempDir::new("symlink-outside");
    fs::write(outside.path().join("secret.txt"), "must not be read").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        root.path().join("link.txt"),
    )
    .unwrap();

    let grant = FilesystemGrant::new(root.path()).unwrap();

    assert!(matches!(
        grant.read_text(Path::new("link.txt")),
        Err(GrantError::PathOutsideGrant)
    ));
}

#[test]
fn granted_root_limits_text_reads() {
    let root = TempDir::new("large-file");
    let payload = vec![b'x'; 1_048_577];
    fs::write(root.path().join("large.txt"), payload).unwrap();

    let grant = FilesystemGrant::new(root.path()).unwrap();

    assert!(matches!(
        grant.read_text(Path::new("large.txt")),
        Err(GrantError::FileTooLarge { .. })
    ));
}

#[test]
fn name_search_finds_case_insensitive_nested_matches() {
    let root = TempDir::new("search");
    let outside = TempDir::new("search-outside");
    fs::create_dir_all(root.path().join("Research")).unwrap();
    fs::write(root.path().join("Research/ClimateNotes.txt"), "inside").unwrap();
    fs::write(root.path().join("Research/unrelated.md"), "not a match").unwrap();
    fs::write(outside.path().join("ClimateOutside.txt"), "outside").unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), root.path().join("external")).unwrap();

    let grant = FilesystemGrant::new(root.path()).unwrap();
    let result = grant.search_names("climate", 10).unwrap();

    assert_eq!(result.entries.len(), 1);
    assert_eq!(
        result.entries[0].relative_path,
        Path::new("Research/ClimateNotes.txt")
    );
    assert!(!result.truncated);
}

#[test]
fn name_search_rejects_a_zero_result_limit() {
    let root = TempDir::new("search-zero-limit");
    let grant = FilesystemGrant::new(root.path()).unwrap();

    assert!(matches!(
        grant.search_names("note", 0),
        Err(GrantError::InvalidSearchLimit)
    ));
}

#[test]
fn name_search_reports_when_results_are_truncated() {
    let root = TempDir::new("search-truncated");
    fs::write(root.path().join("note-one.txt"), "one").unwrap();
    fs::write(root.path().join("note-two.txt"), "two").unwrap();
    let grant = FilesystemGrant::new(root.path()).unwrap();

    let result = grant.search_names("note", 1).unwrap();

    assert_eq!(result.entries.len(), 1);
    assert!(result.truncated);
}
