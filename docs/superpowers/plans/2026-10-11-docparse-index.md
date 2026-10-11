# docparse Stage 1: Incremental Index — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `docparse index .` track a document directory in a SQLite index so re-runs re-parse only what changed.

**Architecture:** A new `index` module in the library holds the schema, a pure-function change detector, and the SQLite read/write layer. The CLI gains git-style subcommands (`index`, `status`, `export`) while bare `docparse <files>` keeps streaming JSONL. Extraction itself is untouched — only the destination of chunks changes.

**Tech Stack:** Rust 1.98, `rusqlite` (bundled SQLite + FTS5), `blake3`, `walkdir`, `rayon`, `serde_json`, `clap` 4 derive.

**Spec:** `docs/superpowers/specs/2026-10-11-docparse-index-design.md`

## Global Constraints

- Rust edition 2021, toolchain 1.98 or newer.
- **No network access at runtime.** No HTTP client crate may appear in `cargo tree`.
- No LLM, no embeddings, no vector index, no image captioning, no HTTP server.
- **No silent fallbacks.** A missing capability or an unreadable index is an error, never a placeholder or a silent rebuild.
- **An index belongs to exactly one root.** `meta.root_path` is written at creation and checked on every open.
- Peak heap bounded per document: chunks are written per file and dropped, never accumulated across files.
- Cargo features unchanged: `pdf`, `docx`, `image` default-on; `ocr` default-off.
- Exit codes: `0` all succeeded, `1` all failed or fatal, `2` partial.
- Both builds stay clean: `cargo test --workspace --features docparse/pdf` and `cargo test --workspace --features docparse/pdf,docparse/ocr -- --test-threads=1`, plus clippy `-D warnings` on both and `cargo fmt --check`.
- **Do not run any git command.** Every task omits a commit step; changes stay uncommitted in the working tree.

## Review Focus

Five input classes the spec implies but does not name, ordered by how likely they are to bite a real user. Each has a test assigned to the task that owns the code.

1. **Non-ASCII and spaced paths** (`Ürün raporu (2).pdf`). Real corpora are full of them; `rel_path` must round-trip exactly and `export` must resolve back to the real file. → Task 4.
2. **Case-insensitive filesystems.** On Windows, `Paper.pdf` and `paper.pdf` are the same file, but SQLite's `UNIQUE` on `rel_path` is case-sensitive, so a rename that only changes case could create two rows for one file. → Task 6.
3. **Symlinks and Windows junctions** in the walked tree. A cycle must not hang the walk, and a link must not index its target twice. → Task 6.
4. **A file modified during the run.** The hash is read, then the parse reads again; the two can disagree, which would store chunks under the wrong hash and make the file look current forever. → Task 6.
5. **Zero-byte and text-free files.** A valid file producing zero chunks must be recorded as indexed with `chunk_count = 0`, not as failed, and must not be re-parsed on every run. → Task 6.

---

## File Structure

```
docparse/docparse/
  Cargo.toml                    # + rusqlite (bundled, fts5), blake3
  src/
    lib.rs                      # + pub mod index;
    index/
      mod.rs                    # Index::open, index_dir, status, export — the public API
      schema.rs                 # DDL, SCHEMA_VERSION, EXTRACTION_VERSION, migrations
      detect.rs                 # Action enum + decide() — PURE, no I/O
      store.rs                  # SQLite reads and writes
      config.rs                 # IndexConfig + its fingerprint
  tests/
    index_schema_test.rs
    index_detect_test.rs        # (unit tests live in detect.rs; this covers wiring)
    index_cmd_test.rs           # the behavioural table from the spec
    index_export_test.rs        # round-trip against the streaming path

docparse/docparse-cli/
  src/
    main.rs                     # subcommand dispatch only
    stream.rs                   # the existing streaming path, moved out of main.rs
    cmd_index.rs
    cmd_status.rs
    cmd_export.rs
  tests/cli_test.rs             # existing 13 tests must keep passing
```

`detect.rs` deliberately contains no I/O: it decides, from facts it is handed, which action applies. That makes the heart of the feature testable without a filesystem or a database, and it is where mutation testing will aim.

---

### Task 1: Subcommand restructure, no new behaviour

Pure refactor, done first because it carries the highest regression risk and zero new features. If this task breaks something, it is obvious.

**Files:**
- Modify: `docparse/docparse-cli/src/main.rs`
- Create: `docparse/docparse-cli/src/stream.rs`
- Modify: `docparse/docparse-cli/tests/cli_test.rs` (add two tests, change none)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `enum Commands { Index(IndexArgs), Status(StatusArgs), Export(ExportArgs) }`
  - `struct Args { command: Option<Commands>, /* existing flat args */ }`
  - `stream::run(args: &Args, out: &mut (dyn Write + Send)) -> Outcome` — the existing `run`, moved verbatim
  - `stream::{FileError, Outcome, open_source, collect_inputs, default_jobs, SUPPORTED}` — moved, made `pub(crate)` so the new commands reuse them

**Rust notes for this task:**

- `clap` has no concept of a "default subcommand". The idiom that preserves both shapes is `command: Option<Commands>` alongside the existing positional `paths`: when `command` is `None`, run the streaming path exactly as today.
- `pub(crate)` means "public within this crate, private outside" — the right visibility for helpers the new command modules need but nothing external should see.
- Moving code between files in Rust changes nothing about its behaviour, but every item the other module uses needs its visibility widened. Expect a cascade of small `pub(crate)` additions; that is normal, not a design smell.

**A real gotcha to verify, not assume:** a positional path whose name collides with a subcommand. `docparse index` is the subcommand; a *file* called `index.pdf` must still stream, because clap matches subcommands on the exact word. A file named exactly `index` with no extension is genuinely ambiguous and resolves as the subcommand — acceptable, and documented in `--help`.

- [ ] **Step 1: Write the failing tests**

Append to `docparse/docparse-cli/tests/cli_test.rs`:

```rust
#[test]
fn subcommands_are_listed_in_help() {
    let out = Command::new(binary()).arg("--help").output().unwrap();
    let help = String::from_utf8(out.stdout).unwrap();
    for sub in ["index", "status", "export"] {
        assert!(help.contains(sub), "--help should list `{sub}`:\n{help}");
    }
}

#[test]
fn a_file_named_like_a_subcommand_still_streams() {
    // `index` is a subcommand, but `index.pdf` is a filename. clap matches
    // subcommands on the exact word, so this must stream, not index.
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(dir.path(), "index.docx", &["Content in a file named index."]);

    let out = Command::new(binary()).arg(&path).output().unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("Content in a file named index."),
        "expected streamed JSONL, got: {stdout}"
    );
    assert!(!dir.path().join(".docparse").exists(), "must not create an index");
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cd docparse && cargo test -p docparse-cli --test cli_test subcommands_are_listed
```

Expected: FAIL — `--help` does not mention `index`.

- [ ] **Step 3: Move the streaming path into `stream.rs`**

Move, **verbatim**, from `main.rs` into a new `stream.rs`: `Emit`, `Outcome`, `FileError`, its `From`/`Display` impls, `SUPPORTED`, `run`, `run_parallel`, `process_file`, `open_source`, `collect_inputs`, `is_supported`, `default_jobs`, and the `default_jobs` unit tests. Change each item's visibility to `pub(crate)`. Add `mod stream;` to `main.rs`.

Do not alter any logic. If a test fails after this step, something was changed that should not have been.

- [ ] **Step 4: Add the subcommand enum**

In `main.rs`:

```rust
#[derive(Debug, Subcommand)]
enum Commands {
    /// Index a directory, parsing only what changed since the last run.
    Index(IndexArgs),
    /// Report what is new, changed, deleted, failed or unchanged.
    Status(StatusArgs),
    /// Write the indexed chunks out as JSONL.
    Export(ExportArgs),
}

#[derive(Debug, clap::Args)]
struct IndexArgs {
    /// Directory to index. Defaults to the current directory.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Put or find `.docparse` somewhere other than inside PATH.
    #[arg(long)]
    index_dir: Option<PathBuf>,
    #[arg(short, long)]
    jobs: Option<usize>,
    /// Reattempt files previously recorded as failed.
    #[arg(long)]
    retry_failed: bool,
    /// Ignore the cache and rebuild from scratch.
    #[arg(long)]
    reindex: bool,
}

#[derive(Debug, clap::Args)]
struct StatusArgs {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    index_dir: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct ExportArgs {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    index_dir: Option<PathBuf>,
    #[arg(short, long)]
    output: Option<PathBuf>,
}
```

Add to `Args`:

```rust
    #[command(subcommand)]
    command: Option<Commands>,
```

- [ ] **Step 5: Dispatch, with the three commands stubbed**

In `main`, before the existing streaming logic:

```rust
    match &args.command {
        Some(Commands::Index(_)) | Some(Commands::Status(_)) | Some(Commands::Export(_)) => {
            // Tasks 6, 7 and 8 replace these. Exit 1 rather than 0 so a
            // half-built binary cannot look like a success in a script.
            eprintln!("error: this subcommand is not implemented yet");
            std::process::exit(1);
        }
        None => { /* fall through to the existing streaming path */ }
    }
```

- [ ] **Step 6: Run the full suite**

```bash
cd docparse && cargo test --workspace --features docparse/pdf && cargo clippy --workspace --all-targets --features docparse/pdf -- -D warnings && cargo fmt --check
```

Expected: all 88 existing tests plus the 2 new ones pass, clippy and fmt clean. **If any of the 13 pre-existing CLI tests fails, the move was not verbatim** — revert and redo it rather than adjusting the test.

---

### Task 2: Schema, `Index::open`, and every refusal

**Files:**
- Modify: `docparse/docparse/Cargo.toml`
- Modify: `docparse/docparse/src/lib.rs`
- Create: `docparse/docparse/src/index/mod.rs`, `src/index/schema.rs`
- Create: `docparse/docparse/tests/index_schema_test.rs`

**Interfaces:**
- Consumes: `Error`, `Result` from `error.rs`.
- Produces:
  - `pub const SCHEMA_VERSION: i64 = 1;`
  - `pub const EXTRACTION_VERSION: u32 = 1;`
  - `pub struct Index { conn: rusqlite::Connection, root: PathBuf }`
  - `pub fn Index::open(root: &Path, index_dir: Option<&Path>) -> Result<Index>` — creates on first use, opens and validates afterwards
  - `pub fn Index::meta(&self, key: &str) -> Result<Option<String>>`
  - `pub fn Index::set_meta(&self, key: &str, value: &str) -> Result<()>`
  - `Error::Index { detail: String }` — a new variant in `error.rs`

**Rust notes for this task:**

- `rusqlite` is the standard SQLite binding. The `bundled` feature compiles SQLite from source into your binary, so there is **no runtime system dependency** — unlike PDFium, which needs its DLL beside the executable. The `fts5` feature enables full-text search.
- A `Connection` is one open database handle. It is **not** `Sync`, so it cannot be shared across threads. That is why Task 6 parses in parallel but writes from one thread.
- `conn.execute_batch(sql)` runs several statements at once — right for DDL. `conn.execute(sql, params![..])` runs one with bound parameters. **Always bind parameters**; never format values into SQL strings.
- `PRAGMA user_version` is a SQLite-native integer slot for schema versioning. We additionally keep `schema_version` in our own `meta` table, because `meta` is where the other versioning lives and one place to look beats two.

- [ ] **Step 1: Add the dependencies**

In `docparse/docparse/Cargo.toml`:

```toml
rusqlite = { version = "0.32", features = ["bundled", "fts5"] }
blake3 = "1"
```

Both are non-optional: the index is part of the library's core from now on.

- [ ] **Step 2: Add the error variant**

In `docparse/docparse/src/error.rs`, alongside the existing variants:

```rust
    /// The index itself is unusable — wrong schema version, wrong root,
    /// corrupt file, or an unwritable location. Never a silent rebuild.
    #[error("index unusable: {detail}")]
    Index { detail: String },
```

- [ ] **Step 3: Write the failing tests**

`docparse/docparse/tests/index_schema_test.rs`:

```rust
use docparse::index::{Index, SCHEMA_VERSION};

#[test]
fn opening_a_fresh_directory_creates_the_index_and_a_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(dir.path(), None).unwrap();

    assert!(dir.path().join(".docparse/index.db").exists());

    // So an index inside a git repository never pollutes it.
    let ignore = std::fs::read_to_string(dir.path().join(".docparse/.gitignore")).unwrap();
    assert_eq!(ignore.trim(), "*");

    assert_eq!(
        index.meta("schema_version").unwrap().as_deref(),
        Some(SCHEMA_VERSION.to_string().as_str())
    );
    assert!(index.meta("root_path").unwrap().is_some());
    assert!(index.meta("created_at").unwrap().is_some());
}

#[test]
fn reopening_an_existing_index_does_not_reset_it() {
    let dir = tempfile::tempdir().unwrap();
    {
        let index = Index::open(dir.path(), None).unwrap();
        index.set_meta("probe", "survived").unwrap();
    }
    let index = Index::open(dir.path(), None).unwrap();
    assert_eq!(index.meta("probe").unwrap().as_deref(), Some("survived"));
}

#[test]
fn a_schema_from_the_future_is_refused_not_written_to() {
    let dir = tempfile::tempdir().unwrap();
    {
        let index = Index::open(dir.path(), None).unwrap();
        index.set_meta("schema_version", "9999").unwrap();
    }
    let err = Index::open(dir.path(), None).unwrap_err();
    assert!(matches!(err, docparse::Error::Index { .. }), "got {err:?}");
    let msg = err.to_string();
    assert!(msg.contains("9999"), "message should name the version: {msg}");

    // And the index must be untouched: still 9999, not downgraded.
    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let v: String = conn
        .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, "9999");
}

#[test]
fn an_index_belonging_to_another_root_is_refused() {
    let shared = tempfile::tempdir().unwrap();
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();

    Index::open(root_a.path(), Some(shared.path())).unwrap();
    let err = Index::open(root_b.path(), Some(shared.path())).unwrap_err();

    assert!(matches!(err, docparse::Error::Index { .. }), "got {err:?}");
    let msg = err.to_string();
    assert!(msg.contains("root"), "message should mention the root: {msg}");
}

#[test]
fn a_corrupt_index_is_refused_rather_than_silently_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".docparse")).unwrap();
    std::fs::write(dir.path().join(".docparse/index.db"), b"this is not a database").unwrap();

    let err = Index::open(dir.path(), None).unwrap_err();
    assert!(matches!(err, docparse::Error::Index { .. }), "got {err:?}");
    assert!(
        err.to_string().contains("--reindex"),
        "the message must tell the user how to recover: {err}"
    );
}

#[test]
fn index_dir_relocates_the_database() {
    let docs = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();

    Index::open(docs.path(), Some(elsewhere.path())).unwrap();

    assert!(elsewhere.path().join("index.db").exists());
    assert!(!docs.path().join(".docparse").exists(), "must not touch the document dir");
}

#[test]
fn the_fts_table_and_triggers_exist() {
    let dir = tempfile::tempdir().unwrap();
    Index::open(dir.path(), None).unwrap();
    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();

    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE name LIKE 'chunks_fts%' OR type='trigger'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert!(names.iter().any(|n| n == "chunks_fts"), "got {names:?}");
    assert!(names.len() >= 4, "expected fts table plus 3 triggers, got {names:?}");
}
```

Add `rusqlite = { version = "0.32", features = ["bundled"] }` to `[dev-dependencies]` so the tests can inspect the database directly.

- [ ] **Step 4: Run to verify failure**

```bash
cd docparse && cargo test -p docparse --test index_schema_test
```

Expected: FAIL to compile — `unresolved import docparse::index`.

- [ ] **Step 5: Write `schema.rs`**

```rust
//! The index schema, its version, and the DDL that creates it.

/// Bumped when the table layout changes. An index with a higher value than
/// the binary understands is refused, never written to.
pub const SCHEMA_VERSION: i64 = 1;

/// Bumped BY HAND when a change alters extraction output for identical
/// input, which invalidates every stored chunk.
///
/// Deliberately not the crate version: a DOCX bug fix must not re-index a
/// PDF corpus. The case this exists for is real — a segment-merging fix
/// turned 2,860 chunks into 2,976 for an unchanged corpus.
pub const EXTRACTION_VERSION: u32 = 1;

pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id           INTEGER PRIMARY KEY,
    content_hash TEXT    NOT NULL UNIQUE,
    byte_size    INTEGER NOT NULL,
    page_count   INTEGER,
    chunk_count  INTEGER NOT NULL DEFAULT 0,
    status       TEXT    NOT NULL,
    error        TEXT,
    indexed_at   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS paths (
    id        INTEGER PRIMARY KEY,
    file_id   INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    rel_path  TEXT    NOT NULL UNIQUE,
    mtime_ns  INTEGER NOT NULL,
    byte_size INTEGER NOT NULL,
    seen_at   TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_paths_file ON paths(file_id);

CREATE TABLE IF NOT EXISTS chunks (
    id         INTEGER PRIMARY KEY,
    file_id    INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    ordinal    INTEGER NOT NULL,
    page_start INTEGER NOT NULL,
    page_end   INTEGER NOT NULL,
    origin     TEXT    NOT NULL,
    confidence REAL,
    hard_split INTEGER NOT NULL,
    blocks     TEXT    NOT NULL,
    text       TEXT    NOT NULL,
    UNIQUE(file_id, ordinal)
);
CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    text,
    content       = 'chunks',
    content_rowid = 'id',
    tokenize      = 'unicode61 remove_diacritics 2'
);

-- External-content FTS indexes are not maintained automatically; these
-- triggers are what keep `chunks_fts` in step with `chunks`.
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.id, old.text);
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
"#;
```

- [ ] **Step 6: Write `mod.rs` with `open` and the refusals**

```rust
//! The incremental index: a SQLite database tracking which files have been
//! parsed, keyed by content hash.

mod schema;

pub use schema::{EXTRACTION_VERSION, SCHEMA_VERSION};

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{Error, Result};

pub struct Index {
    conn: Connection,
    root: PathBuf,
}

impl Index {
    /// Opens the index for `root`, creating it on first use.
    ///
    /// Refuses rather than repairing: a schema from the future, an index
    /// belonging to a different root, and a corrupt file are all errors
    /// naming what to do next.
    pub fn open(root: &Path, index_dir: Option<&Path>) -> Result<Index> {
        let root = root.canonicalize().map_err(|source| Error::Io {
            path: root.to_path_buf(),
            source,
        })?;

        let dir = match index_dir {
            Some(d) => d.to_path_buf(),
            None => root.join(".docparse"),
        };

        let fresh = !dir.join("index.db").exists();

        std::fs::create_dir_all(&dir).map_err(|e| Error::Index {
            detail: format!(
                "cannot create {}: {e}. Use --index-dir to put it elsewhere.",
                dir.display()
            ),
        })?;

        if fresh {
            // `*` so an index inside a git repository never pollutes it.
            let _ = std::fs::write(dir.join(".gitignore"), "*\n");
        }

        let conn = Connection::open(dir.join("index.db")).map_err(|e| Error::Index {
            detail: format!("cannot open {}: {e}", dir.join("index.db").display()),
        })?;

        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
            .map_err(|e| Error::Index {
                detail: format!(
                    "{} is not a usable database ({e}). Use --reindex to rebuild it.",
                    dir.join("index.db").display()
                ),
            })?;

        conn.execute_batch(schema::DDL).map_err(|e| Error::Index {
            detail: format!(
                "cannot initialise schema in {} ({e}). Use --reindex to rebuild it.",
                dir.display()
            ),
        })?;

        let index = Index { conn, root };

        match index.meta("schema_version")? {
            None => {
                index.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
                index.set_meta("root_path", &index.root.display().to_string())?;
                index.set_meta("created_at", &now_iso8601())?;
            }
            Some(found) => {
                let found_n: i64 = found.parse().unwrap_or(i64::MAX);
                if found_n > SCHEMA_VERSION {
                    return Err(Error::Index {
                        detail: format!(
                            "schema version {found_n} is newer than this build understands \
                             ({SCHEMA_VERSION}). Upgrade docparse; this index was not modified."
                        ),
                    });
                }
                if found_n < SCHEMA_VERSION {
                    return Err(Error::Index {
                        detail: format!(
                            "schema version {found_n} predates this build ({SCHEMA_VERSION}) \
                             and no migration exists. Use --reindex to rebuild it."
                        ),
                    });
                }
                let stored_root = index.meta("root_path")?.unwrap_or_default();
                let this_root = index.root.display().to_string();
                if stored_root != this_root {
                    return Err(Error::Index {
                        detail: format!(
                            "this index belongs to root {stored_root}, not {this_root}. \
                             An index tracks exactly one directory."
                        ),
                    });
                }
            }
        }

        Ok(index)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .ok()
            .map(Ok)
            .transpose()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )
            .map_err(|e| Error::Index {
                detail: format!("cannot write meta `{key}`: {e}"),
            })?;
        Ok(())
    }
}

fn now_iso8601() -> String {
    // No chrono dependency for one timestamp: seconds since the epoch is
    // sortable, unambiguous, and enough for "when was this indexed".
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}
```

Add `pub mod index;` to `lib.rs`.

- [ ] **Step 7: Run the tests**

```bash
cd docparse && cargo test -p docparse --test index_schema_test
```

Expected: all 7 PASS.

If `a_corrupt_index_is_refused_rather_than_silently_rebuilt` fails because SQLite accepted the garbage file, note that SQLite defers header validation until first read — the `PRAGMA journal_mode` batch is what forces it. Keep that call before the DDL for exactly this reason.

- [ ] **Step 8: Confirm the no-network invariant still holds**

```bash
cd docparse && cargo tree --features pdf | grep -Ei "reqwest|hyper|ureq|curl" || echo "clean: no network stack"
```

Expected: `clean`. `rusqlite` with `bundled` compiles SQLite from source and pulls in no network crate.

- [ ] **Step 9: Lint**

```bash
cd docparse && cargo clippy --workspace --all-targets --features docparse/pdf -- -D warnings && cargo fmt --check
```

---

---

### Task 3: Change detection as pure functions

The heart of the feature. No I/O, so it is fully testable at a table, and it is where mutation testing aims — a wrong answer here silently corrupts the index rather than crashing.

**Files:**
- Create: `docparse/docparse/src/index/detect.rs`
- Modify: `docparse/docparse/src/index/mod.rs` (add `pub mod detect;`)

**Interfaces:**
- Consumes: nothing — this module is deliberately dependency-free.
- Produces:
  - `pub enum FileStatus { Indexed, Failed }`
  - `pub struct KnownPath { pub file_id: i64, pub byte_size: u64, pub mtime_ns: i128, pub status: FileStatus }`
  - `pub struct WalkedFile { pub byte_size: u64, pub mtime_ns: i128 }`
  - `pub enum Action { Unchanged, TouchOnly { file_id: i64 }, Parse, LinkExisting { file_id: i64 }, SkipFailed }`
  - `pub enum Decision { Settled(Action), NeedsHash }`
  - `pub fn decide_cheap(known: Option<&KnownPath>, walked: &WalkedFile, retry_failed: bool) -> Decision`
  - `pub fn decide_after_hash(known_file_id: Option<i64>, hash_owner: Option<i64>) -> Action`

**Rust notes for this task:**

- This is "pure function" design: the functions take facts and return a decision, touching no filesystem and no database. In Python you might reach for mocks to test this; in Rust the cheaper move is to make the function take plain data so there is nothing to mock.
- `i128` for `mtime_ns` because nanoseconds since the Unix epoch overflow `i64` only past the year 2262, but a *negative* timestamp (a file dated before 1970, which archives do contain) must also fit. `u64` would reject those.
- Two functions rather than one because **hashing is expensive and lazy.** `decide_cheap` answers without reading the file; only when it returns `NeedsHash` does the caller pay to hash. Folding both into one function would force a hash on every file and destroy the whole point.

**Why the split matters for the headline requirement:** success criterion 1 is "a re-run re-parses zero files". That is delivered entirely by `decide_cheap` returning `Settled(Unchanged)` from size and mtime alone, with no read. If this function is wrong, the feature has no value even when the output is correct.

- [ ] **Step 1: Write the failing tests**

Append to `detect.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn known(file_id: i64, size: u64, mtime: i128, status: FileStatus) -> KnownPath {
        KnownPath { file_id, byte_size: size, mtime_ns: mtime, status }
    }

    fn walked(size: u64, mtime: i128) -> WalkedFile {
        WalkedFile { byte_size: size, mtime_ns: mtime }
    }

    // --- decide_cheap ---------------------------------------------------

    #[test]
    fn an_unchanged_indexed_file_needs_no_read() {
        let k = known(7, 100, 1_000, FileStatus::Indexed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, 1_000), false),
            Decision::Settled(Action::Unchanged)
        );
    }

    #[test]
    fn a_changed_size_demands_a_hash() {
        let k = known(7, 100, 1_000, FileStatus::Indexed);
        assert_eq!(decide_cheap(Some(&k), &walked(101, 1_000), false), Decision::NeedsHash);
    }

    #[test]
    fn a_changed_mtime_demands_a_hash() {
        // mtime alone is never trusted: sync clients and checkouts move it
        // without changing content, and coarse filesystems lie about it.
        let k = known(7, 100, 1_000, FileStatus::Indexed);
        assert_eq!(decide_cheap(Some(&k), &walked(100, 2_000), false), Decision::NeedsHash);
    }

    #[test]
    fn an_unknown_path_demands_a_hash() {
        assert_eq!(decide_cheap(None, &walked(100, 1_000), false), Decision::NeedsHash);
    }

    #[test]
    fn a_previously_failed_unchanged_file_is_skipped() {
        // A corrupt PDF must not cost parse time on every single run.
        let k = known(7, 100, 1_000, FileStatus::Failed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, 1_000), false),
            Decision::Settled(Action::SkipFailed)
        );
    }

    #[test]
    fn retry_failed_forces_a_reattempt_of_an_unchanged_failure() {
        let k = known(7, 100, 1_000, FileStatus::Failed);
        assert_eq!(decide_cheap(Some(&k), &walked(100, 1_000), true), Decision::NeedsHash);
    }

    #[test]
    fn a_failed_file_that_changed_is_reattempted_without_the_flag() {
        // The user fixed it; they should not need to remember a flag.
        let k = known(7, 100, 1_000, FileStatus::Failed);
        assert_eq!(decide_cheap(Some(&k), &walked(250, 9_000), false), Decision::NeedsHash);
    }

    #[test]
    fn a_negative_mtime_is_handled() {
        // Archives really do contain files dated before 1970.
        let k = known(7, 100, -86_400_000_000_000, FileStatus::Indexed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, -86_400_000_000_000), false),
            Decision::Settled(Action::Unchanged)
        );
    }

    // --- decide_after_hash ---------------------------------------------

    #[test]
    fn content_unchanged_despite_a_moved_mtime_only_touches() {
        assert_eq!(decide_after_hash(Some(7), Some(7)), Action::TouchOnly { file_id: 7 });
    }

    #[test]
    fn brand_new_content_is_parsed() {
        assert_eq!(decide_after_hash(None, None), Action::Parse);
    }

    #[test]
    fn a_renamed_or_copied_file_links_instead_of_parsing() {
        // This is what makes a rename free, and what stores a duplicated
        // corpus once instead of twice.
        assert_eq!(decide_after_hash(None, Some(3)), Action::LinkExisting { file_id: 3 });
    }

    #[test]
    fn a_path_overwritten_with_already_indexed_content_relinks() {
        // a.pdf was file 1; someone overwrote it with b.pdf's bytes, which
        // are already file 2. Relink, do not re-parse.
        assert_eq!(decide_after_hash(Some(1), Some(2)), Action::LinkExisting { file_id: 2 });
    }

    #[test]
    fn a_known_path_with_new_content_is_parsed() {
        assert_eq!(decide_after_hash(Some(1), None), Action::Parse);
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cd docparse && cargo test -p docparse --lib detect
```

Expected: FAIL to compile — the types and functions do not exist.

- [ ] **Step 3: Write the implementation**

Prepend to `detect.rs`:

```rust
//! Change detection, as pure functions over plain data.
//!
//! No filesystem, no database. The caller supplies what it found on disk and
//! what the index already knows; these functions decide what to do. That
//! keeps the part of the feature most able to corrupt an index testable at a
//! table, with nothing to mock.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Indexed,
    Failed,
}

/// What the index already records for one path.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownPath {
    pub file_id: i64,
    pub byte_size: u64,
    /// Nanoseconds since the Unix epoch. Signed: archives contain files
    /// dated before 1970.
    pub mtime_ns: i128,
    pub status: FileStatus,
}

/// What the walk found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct WalkedFile {
    pub byte_size: u64,
    pub mtime_ns: i128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Size and mtime both match. Do not read the file.
    Unchanged,
    /// Content is identical but the timestamp moved. Refresh mtime only.
    TouchOnly { file_id: i64 },
    /// Content is new to this index. Parse it.
    Parse,
    /// This content is already indexed. Point the path at it; do not parse.
    LinkExisting { file_id: i64 },
    /// Recorded as failed and unchanged since. Skip unless asked to retry.
    SkipFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Settled(Action),
    /// Cannot decide without the content hash; the caller must hash the file.
    NeedsHash,
}

/// Decides what can be decided without reading the file.
///
/// Returning `Settled(Unchanged)` from size and mtime alone is what makes a
/// no-op run fast; everything else here exists to avoid trusting mtime.
pub fn decide_cheap(
    known: Option<&KnownPath>,
    walked: &WalkedFile,
    retry_failed: bool,
) -> Decision {
    let Some(known) = known else {
        return Decision::NeedsHash;
    };

    let same_stat = known.byte_size == walked.byte_size && known.mtime_ns == walked.mtime_ns;

    match known.status {
        FileStatus::Failed => {
            if same_stat && !retry_failed {
                Decision::Settled(Action::SkipFailed)
            } else {
                // Changed on disk, or the user asked for a retry.
                Decision::NeedsHash
            }
        }
        FileStatus::Indexed => {
            if same_stat {
                Decision::Settled(Action::Unchanged)
            } else {
                Decision::NeedsHash
            }
        }
    }
}

/// Decides once the content hash is known.
///
/// `known_file_id` is the file this path currently points at, if any.
/// `hash_owner` is the file that already holds this content hash, if any.
pub fn decide_after_hash(known_file_id: Option<i64>, hash_owner: Option<i64>) -> Action {
    match (known_file_id, hash_owner) {
        // Same path, same content: only the timestamp moved.
        (Some(a), Some(b)) if a == b => Action::TouchOnly { file_id: a },
        // This content exists elsewhere in the index: rename, copy, or an
        // overwrite with bytes we have already parsed.
        (_, Some(owner)) => Action::LinkExisting { file_id: owner },
        // Content new to the index.
        (_, None) => Action::Parse,
    }
}
```

Add `pub mod detect;` to `index/mod.rs`.

- [ ] **Step 4: Run the tests**

```bash
cd docparse && cargo test -p docparse --lib detect
```

Expected: all 13 PASS.

- [ ] **Step 5: Verify the tests have teeth by mutation**

For each, make the change, run `cargo test -p docparse --lib detect`, record which test fails by name, then **revert**:

1. In `decide_cheap`, compare only `byte_size` and ignore `mtime_ns` → a test must fail.
2. In `decide_cheap`, compare only `mtime_ns` and ignore `byte_size` → a test must fail.
3. In `decide_cheap`, return `NeedsHash` for a failed-and-unchanged file regardless of `retry_failed` → a test must fail.
4. In `decide_after_hash`, drop the `a == b` arm so an unchanged file re-parses → a test must fail.
5. In `decide_after_hash`, return `Parse` when `hash_owner` is `Some` → a test must fail.

Any mutation that breaks nothing means that behaviour is unverified — add the missing case before moving on.

- [ ] **Step 6: Lint**

```bash
cd docparse && cargo clippy -p docparse --all-targets --features pdf -- -D warnings && cargo fmt --check
```
