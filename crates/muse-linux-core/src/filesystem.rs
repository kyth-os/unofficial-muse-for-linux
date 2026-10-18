use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

/// Maximum amount of text the first read-only connector will return per file.
pub const MAX_TEXT_FILE_BYTES: u64 = 1_048_576;
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
    FileTooLarge { size_bytes: u64, limit_bytes: u64 },
    EmptySearchQuery,
    InvalidSearchLimit,
    NotUtf8,
    Io(io::Error),
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
