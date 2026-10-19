use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::Write as _;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum amount of text the first read-only connector will return per file.
pub const MAX_TEXT_FILE_BYTES: u64 = 1_048_576;
/// Maximum bytes accepted by a single [`WriteGrant`] file write.
pub const MAX_WRITE_BYTES: u64 = 1_048_576;
pub const MAX_SEARCH_RESULTS: usize = 500;
const MAX_SEARCH_ENTRIES: usize = 10_000;
const MAX_SEARCH_DEPTH: usize = 64;

/// A user-selected directory, held as a canonical path and usable only for reads.
#[derive(Debug, Clone)]
pub struct FilesystemGrant {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub relative_path: PathBuf,
    pub is_directory: bool,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

#[derive(Debug)]
pub enum GrantError {
    InvalidRoot,
    PathOutsideGrant,
    NotAFile,
    NotADirectory,
    #[rustfmt::skip]
    FileTooLarge { size_bytes: u64, limit_bytes: u64 },
    EmptySearchQuery,
    InvalidSearchLimit,
    NotUtf8,
    Io(io::Error),
    /// A create/rename/mkdir target already exists. Carries only the validated
    /// relative name, never an absolute path or file content.
    AlreadyExists {
        name: String,
    },
    /// An overwrite/rename-source/delete target is missing. Relative name only.
    NotFound {
        name: String,
    },
    /// Write content exceeds [`MAX_WRITE_BYTES`]. Sizes only, never content.
    ContentTooLarge {
        size_bytes: u64,
        limit_bytes: u64,
    },
    /// The relative path itself is unusable (empty, NUL byte). Sanitized
    /// relative name only.
    InvalidPath {
        name: String,
    },
}

impl fmt::Display for GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot => write!(f, "the selected grant root is not a directory"),
            Self::PathOutsideGrant => write!(f, "the requested path escapes the granted directory"),
            Self::NotAFile => write!(f, "the requested path is not a regular file"),
            Self::NotADirectory => write!(f, "the requested path is not a directory"),
            Self::FileTooLarge {
                size_bytes,
                limit_bytes,
            } => write!(
                f,
                "file is {size_bytes} bytes; the read limit is {limit_bytes} bytes"
            ),
            Self::EmptySearchQuery => write!(f, "search query must not be empty"),
            Self::InvalidSearchLimit => write!(f, "search result limit must be greater than zero"),
            Self::NotUtf8 => write!(f, "file is not valid UTF-8 text"),
            Self::Io(error) => write!(f, "filesystem operation failed: {error}"),
            Self::AlreadyExists { name } => write!(f, "file already exists: {name}"),
            Self::NotFound { name } => write!(f, "file not found: {name}"),
            Self::ContentTooLarge {
                size_bytes,
                limit_bytes,
            } => write!(
                f,
                "content is {size_bytes} bytes; the write limit is {limit_bytes} bytes"
            ),
            Self::InvalidPath { name } => write!(f, "invalid file name: {name}"),
        }
    }
}

impl Error for GrantError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for GrantError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl FilesystemGrant {
    /// Creates a read-only grant from a directory explicitly selected by the user.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, GrantError> {
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(GrantError::InvalidRoot);
        }
        Ok(Self { root })
    }

    /// Returns the canonical root for display in the grant-management UI.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lists immediate children. Symlink entries are omitted rather than followed.
    pub fn list(&self, relative_dir: &Path) -> Result<Vec<FileEntry>, GrantError> {
        let directory = self.resolve(relative_dir)?;
        if !directory.is_dir() {
            return Err(GrantError::NotADirectory);
        }

        let mut entries = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }

            let metadata = entry.metadata()?;
            let path = entry.path();
            let relative_path = path
                .strip_prefix(&self.root)
                .map_err(|_| GrantError::PathOutsideGrant)?
                .to_path_buf();
            let name = entry.file_name().to_string_lossy().into_owned();
            entries.push(FileEntry {
                name,
                relative_path,
                is_directory: metadata.is_dir(),
                size_bytes: if metadata.is_file() {
                    metadata.len()
                } else {
                    0
                },
            });
        }
        entries.sort_by(|left, right| {
            right
                .is_directory
                .cmp(&left.is_directory)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(entries)
    }

    /// Reads a bounded UTF-8 text file within the granted root.
    pub fn read_text(&self, relative_file: &Path) -> Result<String, GrantError> {
        let file_path = self.resolve(relative_file)?;
        let metadata = fs::metadata(&file_path)?;
        if !metadata.is_file() {
            return Err(GrantError::NotAFile);
        }
        if metadata.len() > MAX_TEXT_FILE_BYTES {
            return Err(GrantError::FileTooLarge {
                size_bytes: metadata.len(),
                limit_bytes: MAX_TEXT_FILE_BYTES,
            });
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(file_path)?
            .take(MAX_TEXT_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_TEXT_FILE_BYTES {
            return Err(GrantError::FileTooLarge {
                size_bytes: bytes.len() as u64,
                limit_bytes: MAX_TEXT_FILE_BYTES,
            });
        }
        String::from_utf8(bytes).map_err(|_| GrantError::NotUtf8)
    }

    /// Searches names recursively inside the granted root, without following symlinks.
    ///
    /// Search is case-insensitive and bounded by the result, entry, and depth limits.
    /// The result marks when any bound omitted part of the search.
    pub fn search_names(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<SearchResult, GrantError> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Err(GrantError::EmptySearchQuery);
        }

        let result_limit = max_results.min(MAX_SEARCH_RESULTS);
        if result_limit == 0 {
            return Err(GrantError::InvalidSearchLimit);
        }

        let mut directories = VecDeque::from([(PathBuf::new(), 0usize)]);
        let mut results = Vec::new();
        let mut scanned_entries = 0usize;
        let mut truncated = false;

        'search: while let Some((relative_directory, depth)) = directories.pop_front() {
            let directory = self.resolve(&relative_directory)?;
            for entry in fs::read_dir(directory)? {
                if scanned_entries >= MAX_SEARCH_ENTRIES {
                    truncated = true;
                    break 'search;
                }
                scanned_entries += 1;

                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_symlink() || (!file_type.is_file() && !file_type.is_dir()) {
                    continue;
                }

                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                let relative_path = path
                    .strip_prefix(&self.root)
                    .map_err(|_| GrantError::PathOutsideGrant)?
                    .to_path_buf();

                if name.to_lowercase().contains(&query) {
                    if results.len() >= result_limit {
                        truncated = true;
                        break 'search;
                    }
                    let size_bytes = if file_type.is_file() {
                        entry.metadata()?.len()
                    } else {
                        0
                    };
                    results.push(FileEntry {
                        name,
                        relative_path: relative_path.clone(),
                        is_directory: file_type.is_dir(),
                        size_bytes,
                    });
                }

                if file_type.is_dir() {
                    if depth < MAX_SEARCH_DEPTH {
                        directories.push_back((relative_path, depth + 1));
                    } else {
                        truncated = true;
                    }
                }
            }
        }

        results.sort_by(|left, right| {
            left.relative_path
                .to_string_lossy()
                .to_lowercase()
                .cmp(&right.relative_path.to_string_lossy().to_lowercase())
        });

        Ok(SearchResult {
            entries: results,
            truncated,
        })
    }

    fn resolve(&self, relative_path: &Path) -> Result<PathBuf, GrantError> {
        let mut safe_relative = PathBuf::new();
        for component in relative_path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => safe_relative.push(part),
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(GrantError::PathOutsideGrant);
                }
            }
        }

        let resolved = fs::canonicalize(self.root.join(safe_relative))?;
        if !resolved.starts_with(&self.root) {
            return Err(GrantError::PathOutsideGrant);
        }
        Ok(resolved)
    }
}

/// Monotonic counter used to mint collision-resistant temporary file names.
static WRITE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A user-selected directory, held as a canonical path, usable for confined
/// file mutations.
///
/// This is a separate type from [`FilesystemGrant`] on purpose: a read grant
/// can never authorize writes, and a write grant carries no read methods.
/// There is no approval store in this crate — every op executes immediately
/// when called, so per-op preview/confirmation is the parent's job (see
/// `WRITE_API.md`).
///
/// All paths are relative to the grant root and confined to it:
/// absolute paths, `..` escapes (lexical [`Component`] check plus a
/// canonical-prefix recheck after resolution), NUL bytes, and empty paths are
/// rejected fail-closed. Symlinks are never traversed or overwritten: if the
/// final component or any resolved parent is a symlink the op fails with
/// [`GrantError::PathOutsideGrant`]. File writes go through a temporary file
/// in the same directory followed by an atomic rename, so a failure or crash
/// never leaves a partial file behind.
///
/// Writes are byte-oriented with no UTF-8 requirement; each write is capped
/// at [`MAX_WRITE_BYTES`] bytes.
#[derive(Debug, Clone)]
pub struct WriteGrant {
    root: PathBuf,
}

impl WriteGrant {
    /// Creates a write grant from a directory explicitly selected by the user.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, GrantError> {
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(GrantError::InvalidRoot);
        }
        Ok(Self { root })
    }

    /// Returns the canonical root for display in the grant-management UI.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates a new file with `bytes`. Refuses if the target already exists;
    /// `bytes` must be at most [`MAX_WRITE_BYTES`] bytes.
    pub fn create_file(&self, relative_file: &Path, bytes: &[u8]) -> Result<(), GrantError> {
        let relative = self.validate_relative(relative_file)?;
        self.check_size(bytes.len() as u64)?;
        let parent = self.resolve_parent_dir(&relative)?;
        let target = parent.join(file_name(&relative)?);
        match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(GrantError::PathOutsideGrant);
                }
                return Err(GrantError::AlreadyExists {
                    name: display_name(&relative),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(GrantError::Io(error)),
        }
        self.atomic_write(&parent, &target, bytes)
    }

    /// Replaces the content of an existing regular file. The target must
    /// exist; symlinks and directories are refused.
    pub fn overwrite_file(&self, relative_file: &Path, bytes: &[u8]) -> Result<(), GrantError> {
        let relative = self.validate_relative(relative_file)?;
        let parent = self.resolve_parent_dir(&relative)?;
        let target = parent.join(file_name(&relative)?);
        match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(GrantError::PathOutsideGrant);
                }
                if !metadata.is_file() {
                    return Err(GrantError::NotAFile);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(GrantError::NotFound {
                    name: display_name(&relative),
                });
            }
            Err(error) => return Err(GrantError::Io(error)),
        }
        self.check_size(bytes.len() as u64)?;
        self.atomic_write(&parent, &target, bytes)
    }

    /// Moves a file or directory within the grant. The destination must not
    /// exist; cross-grant moves are impossible by construction since both
    /// sides resolve under the same root. Symlink sources and destinations
    /// are refused.
    pub fn rename(&self, relative_from: &Path, relative_to: &Path) -> Result<(), GrantError> {
        let from_relative = self.validate_relative(relative_from)?;
        let to_relative = self.validate_relative(relative_to)?;
        let from_parent = self.resolve_parent_dir(&from_relative)?;
        let to_parent = self.resolve_parent_dir(&to_relative)?;
        let from_path = from_parent.join(file_name(&from_relative)?);
        let to_path = to_parent.join(file_name(&to_relative)?);
        match fs::symlink_metadata(&from_path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(GrantError::PathOutsideGrant);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(GrantError::NotFound {
                    name: display_name(&from_relative),
                });
            }
            Err(error) => return Err(GrantError::Io(error)),
        }
        match fs::symlink_metadata(&to_path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(GrantError::PathOutsideGrant);
                }
                return Err(GrantError::AlreadyExists {
                    name: display_name(&to_relative),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(GrantError::Io(error)),
        }
        fs::rename(&from_path, &to_path)?;
        Ok(())
    }

    /// Deletes a regular file. Directories are never deleted — there is no
    /// recursive delete — and symlinks are refused.
    pub fn delete_file(&self, relative_file: &Path) -> Result<(), GrantError> {
        let relative = self.validate_relative(relative_file)?;
        let parent = self.resolve_parent_dir(&relative)?;
        let target = parent.join(file_name(&relative)?);
        match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(GrantError::PathOutsideGrant);
                }
                if !metadata.is_file() {
                    return Err(GrantError::NotAFile);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(GrantError::NotFound {
                    name: display_name(&relative),
                });
            }
            Err(error) => return Err(GrantError::Io(error)),
        }
        fs::remove_file(&target)?;
        Ok(())
    }

    /// Creates a single directory level. Refuses if the target exists and
    /// never creates missing parents.
    pub fn create_dir(&self, relative_dir: &Path) -> Result<(), GrantError> {
        let relative = self.validate_relative(relative_dir)?;
        let parent = self.resolve_parent_dir(&relative)?;
        let target = parent.join(file_name(&relative)?);
        match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(GrantError::PathOutsideGrant);
                }
                return Err(GrantError::AlreadyExists {
                    name: display_name(&relative),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(GrantError::Io(error)),
        }
        fs::create_dir(&target)?;
        Ok(())
    }

    /// Lexical confinement: rejects absolute paths, `..`/root/prefix
    /// components, NUL bytes, and empty paths before touching the filesystem.
    fn validate_relative(&self, relative_path: &Path) -> Result<PathBuf, GrantError> {
        if relative_path.as_os_str().is_empty() {
            return Err(GrantError::InvalidPath {
                name: String::from("(empty)"),
            });
        }
        if relative_path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(GrantError::InvalidPath {
                name: sanitize_name(relative_path),
            });
        }
        let mut safe_relative = PathBuf::new();
        for component in relative_path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => safe_relative.push(part),
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(GrantError::PathOutsideGrant);
                }
            }
        }
        if safe_relative.as_os_str().is_empty() {
            return Err(GrantError::InvalidPath {
                name: sanitize_name(relative_path),
            });
        }
        Ok(safe_relative)
    }

    /// Resolves the canonical parent directory of a validated relative path,
    /// refusing symlinked or missing parents, then rechecks the canonical
    /// prefix so a planted symlink can never escape the grant.
    fn resolve_parent_dir(&self, relative: &Path) -> Result<PathBuf, GrantError> {
        let mut current = self.root.clone();
        let parent_count = relative.components().count().saturating_sub(1);
        for component in relative.components().take(parent_count) {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err(GrantError::PathOutsideGrant);
                    }
                    if !metadata.is_dir() {
                        return Err(GrantError::NotADirectory);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(GrantError::NotFound {
                        name: display_name(relative),
                    });
                }
                Err(error) => return Err(GrantError::Io(error)),
            }
        }
        let canonical = fs::canonicalize(&current)?;
        if !canonical.starts_with(&self.root) {
            return Err(GrantError::PathOutsideGrant);
        }
        Ok(canonical)
    }

    fn check_size(&self, size_bytes: u64) -> Result<(), GrantError> {
        if size_bytes > MAX_WRITE_BYTES {
            return Err(GrantError::ContentTooLarge {
                size_bytes,
                limit_bytes: MAX_WRITE_BYTES,
            });
        }
        Ok(())
    }

    /// Writes `bytes` to a temporary file in `directory`, then atomically
    /// renames it over `target`. The temporary file is removed on any
    /// failure, so no stray `.tmp` files or partial writes survive.
    /// The tmp handle from creation is reused for the write (never reopened
    /// by path) so a swapped-in symlink cannot redirect the bytes.
    fn atomic_write(
        &self,
        directory: &Path,
        target: &Path,
        bytes: &[u8],
    ) -> Result<(), GrantError> {
        let (temporary, mut file) = self.create_temporary(directory)?;
        let write_result = (|| -> Result<(), GrantError> {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, target)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }

    /// Creates a uniquely named temporary file in `directory` with
    /// create-new semantics so an existing file is never truncated, and
    /// returns the already-open handle: callers must write through it, not
    /// reopen the path (a concurrent swap to a symlink between creation and
    /// a second open would redirect the write outside the grant).
    fn create_temporary(&self, directory: &Path) -> Result<(PathBuf, File), GrantError> {
        for _ in 0..100 {
            let id = WRITE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = directory.join(format!(".tmp-{}-{id}", std::process::id()));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => return Ok((candidate, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(GrantError::Io(error)),
            }
        }
        Err(GrantError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not mint a temporary file name",
        )))
    }
}

/// Final single component of a validated relative path; always present since
/// [`WriteGrant::validate_relative`] rejects empty paths. Returns an error
/// instead of panicking so hostile input can never abort the caller.
fn file_name(relative: &Path) -> Result<&std::ffi::OsStr, GrantError> {
    relative.file_name().ok_or_else(|| GrantError::InvalidPath {
        name: String::from("(empty)"),
    })
}

/// Relative display name for error variants. The path is already validated
/// relative, so this never leaks an absolute path; content is never included.
fn display_name(relative: &Path) -> String {
    relative.to_string_lossy().into_owned()
}

/// Sanitized name for paths that failed validation (e.g. NUL bytes are
/// replaced so they cannot propagate into error strings or logs).
fn sanitize_name(path: &Path) -> String {
    path.to_string_lossy().replace('\0', "\u{FFFD}")
}
