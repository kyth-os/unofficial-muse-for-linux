# Write API (`WriteGrant`)

Confined file-mutation ops for a user-selected directory
(`muse-linux-core::filesystem::WriteGrant`). Mac-parity row:
create / edit / move / delete, each separately granted by the caller holding
a `WriteGrant`, with exact preview/confirmation owned by the parent UI.

## The two-grant rule

- `FilesystemGrant` is read-only and can **never** authorize writes.
- `WriteGrant` is a **distinct** type with only mutation methods
  (`create_file`, `overwrite_file`, `rename`, `delete_file`, `create_dir`).
- There is no conversion between the two. Code holding only a
  `FilesystemGrant` has no write surface at all — this is enforced by the
  type system, not by convention.

## No approval store here

Every op executes **immediately** when called. This crate stores no pending
approvals and performs no preview. Callers (desktop UI / agent runtime) must:

1. Read the current bytes (`FilesystemGrant::read_text` or bytes via `std::fs`),
2. Show the exact diff/preview and get explicit user confirmation,
3. Only then call the matching `WriteGrant` op.

## Operations

| Op | Refuses when |
|----|--------------|
| `create_file(rel, bytes)` | target exists (`AlreadyExists`); `bytes` > 1 MiB (`ContentTooLarge`) |
| `overwrite_file(rel, bytes)` | target missing (`NotFound`); target is a dir (`NotAFile`); symlink (`PathOutsideGrant`); > 1 MiB (`ContentTooLarge`) |
| `rename(from, to)` | source missing (`NotFound`); dest exists (`AlreadyExists`); either side a symlink (`PathOutsideGrant`) |
| `delete_file(rel)` | target missing (`NotFound`); target is a dir (`NotAFile` — directories are never deleted, no recursive delete); symlink (`PathOutsideGrant`) |
| `create_dir(rel)` | target exists (`AlreadyExists`); parent missing (`NotFound` — single level only, never recursive) |

## Confinement (all ops, fail-closed)

- Relative paths only: absolute paths and `..` are rejected lexically via
  `Component` matching, then rechecked against the canonical root prefix
  after resolution (`PathOutsideGrant`).
- NUL bytes and empty paths are rejected (`InvalidPath`).
- Symlinks are never traversed or overwritten: a `symlink_metadata` gate on
  the final component **and** every resolved parent turns any symlink into
  `PathOutsideGrant`. No cross-grant moves are possible: both `rename` sides
  resolve under the same root.
- File writes use tmp-file + atomic rename inside the same directory, so a
  failure or crash leaves either the old bytes or the new bytes — never a
  partial file and never a stray `.tmp` file (removed on any failure path).
  The tmp handle from creation is reused for the write, never reopened by
  path, so a concurrently planted symlink cannot redirect the bytes.
- **Single-writer assumption**: destination-existence checks
  (`create_file`/`rename` refusing an existing target) are check-then-act
  and can lose to a concurrent in-grant writer (plain `rename` replaces).
  Callers serialize mutations (the native UI executes synchronously,
  single-flight); concurrent writers must coordinate externally.

## Contracts

- **Bytes, not text**: `&[u8]` in, no UTF-8 requirement. UI previews text.
- **Size cap**: `MAX_WRITE_BYTES` = 1 MiB per write.
- **Errors never leak**: typed `GrantError` variants carry at most the
  validated *relative* name (NUL-sanitized); absolute paths and file content
  never appear in error values or their `Display`.
- **New error variants are additive only**: `AlreadyExists { name }`,
  `NotFound { name }`, `ContentTooLarge { size_bytes, limit_bytes }`,
  `InvalidPath { name }`. All pre-existing `GrantError` variants and the
  read path (`FilesystemGrant::new/list/read_text/search_names`) are
  unchanged.
