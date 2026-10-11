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

/// The settings that affect extraction output. Flattened into the bare form
/// AND into `index`/`status`, because spec Section 6 lists them under `index`
/// and Task 5 fingerprints them.
#[derive(Debug, Clone, clap::Args)]
struct ExtractArgs {
    #[arg(long, default_value_t = 800)]
    chunk: usize,
    #[arg(long, default_value_t = 1200)]
    max_chunk: usize,
    #[arg(long, default_value_t = 1)]
    overlap: usize,
    #[arg(long)]
    cross_page: bool,
    #[arg(long)]
    ocr: bool,
    #[arg(long, default_value_t = 150)]
    ocr_dpi: u32,
}

#[derive(Debug, clap::Args)]
struct IndexArgs {
    /// Directory to index. Defaults to the current directory.
    #[arg(default_value = ".")]
    path: PathBuf,
    #[command(flatten)]
    extract: ExtractArgs,
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
    #[command(flatten)]
    extract: ExtractArgs,
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
rusqlite = { version = "0.32", features = ["bundled"] }   # bundled SQLite already includes FTS5; there is no separate `fts5` feature in 0.32
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
        // `.optional()` distinguishes "no such row" from "the database is
        // broken". Using `.ok()` here would map a corrupt index to None and
        // report a missing key — a silent fallback, which this project forbids.
        use rusqlite::OptionalExtension;
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(|e| Error::Index {
                detail: format!("cannot read meta `{key}`: {e}"),
            })
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

If `a_corrupt_index_is_refused_rather_than_silently_rebuilt` fails because SQLite accepted the garbage file, the cause is that nothing has read the file yet — SQLite defers header validation until first access. Either the PRAGMA batch or the DDL will trigger it, so the ordering between those two is not load-bearing for this test.

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

---

### Task 4: The store layer — one transaction per file

**Files:**
- Create: `docparse/docparse/src/index/store.rs`
- Modify: `docparse/docparse/src/index/mod.rs`
- Create: `docparse/docparse/tests/index_store_test.rs`

**Interfaces:**
- Consumes: `Index`, `Error`, `Result`, `Chunk`, `PageOrigin`, and `detect::{KnownPath, FileStatus}`.
- Produces, all as methods on `Index`:
  - `pub fn known_path(&self, rel_path: &str) -> Result<Option<KnownPath>>`
  - `pub fn hash_owner(&self, content_hash: &str) -> Result<Option<i64>>`
  - `pub fn store_parsed(&mut self, content_hash: &str, byte_size: u64, page_count: Option<u32>, chunks: &[Chunk], rel_path: &str, mtime_ns: i128) -> Result<i64>`
  - `pub fn store_failure(&mut self, content_hash: &str, byte_size: u64, error: &str, rel_path: &str, mtime_ns: i128) -> Result<i64>`
  - `pub fn link_path(&mut self, file_id: i64, rel_path: &str, mtime_ns: i128, byte_size: u64) -> Result<()>`
  - `pub fn touch_path(&mut self, rel_path: &str, mtime_ns: i128, byte_size: u64) -> Result<()>`
  - `pub fn prune_unseen(&mut self, seen: &std::collections::HashSet<String>) -> Result<(usize, usize)>` — returns (paths removed, files removed)
  - `pub fn chunk_count(&self) -> Result<i64>`
  - `pub fn path_count(&self) -> Result<i64>`

**Rust notes for this task:**

- A **transaction** groups statements so they either all apply or none do. `let tx = self.conn.transaction()?;` then `tx.commit()?`. If the `tx` value is dropped without `commit`, rusqlite rolls back automatically — so an early `?` return cannot leave a half-written file. That is the whole mechanism behind spec success criterion 4, and it is why these methods take `&mut self`: a transaction needs exclusive access to the connection.
- `params![a, b]` binds values safely. Never build SQL with `format!` and a value in it.
- `ON CONFLICT ... DO UPDATE` is SQLite's upsert. We use it so re-linking a path is one statement rather than a select-then-branch.
- `last_insert_rowid()` returns the id SQLite assigned, which is how we learn a new `file_id`.

**The deliberate design point:** `store_parsed` deletes any existing chunks for the file and inserts the new ones *inside one transaction*. An interrupted run therefore leaves some files current and some stale, never a file with half its chunks. Deleting first also keeps `chunks_fts` correct, because the delete trigger fires before the insert trigger.

- [ ] **Step 1: Write the failing tests**

`docparse/docparse/tests/index_store_test.rs`:

```rust
use std::collections::HashSet;

use docparse::index::Index;
use docparse::model::{BlockRef, Chunk, PageOrigin};

fn chunk(ordinal: u64, text: &str, page: u32, origin: PageOrigin) -> Chunk {
    Chunk {
        id: ordinal,
        source: "doc.pdf".to_string(),
        page_start: page,
        page_end: page,
        blocks: vec![BlockRef { page, block: 0 }],
        text: text.to_string(),
        origin,
        hard_split: false,
    }
}

#[test]
fn a_parsed_file_round_trips_through_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    let chunks = vec![
        chunk(0, "First chunk here.", 1, PageOrigin::Native),
        chunk(1, "Second chunk here.", 2, PageOrigin::Ocr { confidence: 0.75 }),
    ];
    let file_id = index
        .store_parsed("hash-a", 1234, Some(2), &chunks, "sub/doc.pdf", 9_000)
        .unwrap();

    assert!(file_id > 0);
    assert_eq!(index.chunk_count().unwrap(), 2);
    assert_eq!(index.path_count().unwrap(), 1);

    let known = index.known_path("sub/doc.pdf").unwrap().unwrap();
    assert_eq!(known.file_id, file_id);
    assert_eq!(known.byte_size, 1234);
    assert_eq!(known.mtime_ns, 9_000);
    assert_eq!(known.status, docparse::index::detect::FileStatus::Indexed);

    assert_eq!(index.hash_owner("hash-a").unwrap(), Some(file_id));
    assert_eq!(index.hash_owner("hash-absent").unwrap(), None);
}

#[test]
fn ocr_confidence_is_stored_and_native_chunks_have_none() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    let chunks = vec![
        chunk(0, "native text", 1, PageOrigin::Native),
        chunk(1, "ocr text", 1, PageOrigin::Ocr { confidence: 0.5 }),
    ];
    index.store_parsed("h", 10, Some(1), &chunks, "a.pdf", 1).unwrap();

    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let rows: Vec<(String, Option<f64>)> = conn
        .prepare("SELECT origin, confidence FROM chunks ORDER BY ordinal")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(rows[0].0, "native");
    assert_eq!(rows[0].1, None, "a native chunk must not carry a confidence");
    assert_eq!(rows[1].0, "ocr");
    assert!((rows[1].1.unwrap() - 0.5).abs() < 1e-6);
}

#[test]
fn re_storing_a_file_replaces_its_chunks_rather_than_appending() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    index
        .store_parsed("h1", 10, Some(1), &[chunk(0, "old text", 1, PageOrigin::Native)], "a.pdf", 1)
        .unwrap();
    index
        .store_parsed(
            "h1",
            10,
            Some(1),
            &[
                chunk(0, "new text", 1, PageOrigin::Native),
                chunk(1, "more new text", 1, PageOrigin::Native),
            ],
            "a.pdf",
            2,
        )
        .unwrap();

    assert_eq!(index.chunk_count().unwrap(), 2, "old chunks must be gone, not appended to");
    assert_eq!(index.path_count().unwrap(), 1);
}

#[test]
fn the_fts_index_follows_chunk_replacement() {
    // External-content FTS is maintained by triggers; if they are wrong the
    // index silently returns stale text, which Stage 2 would inherit.
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    index
        .store_parsed("h", 10, Some(1), &[chunk(0, "aardvark", 1, PageOrigin::Native)], "a.pdf", 1)
        .unwrap();
    index
        .store_parsed("h", 10, Some(1), &[chunk(0, "zeppelin", 1, PageOrigin::Native)], "a.pdf", 2)
        .unwrap();

    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let hits: i64 = conn
        .query_row("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'aardvark'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(hits, 0, "the replaced text must be gone from the FTS index");

    let hits: i64 = conn
        .query_row("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'zeppelin'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(hits, 1);
}

#[test]
fn two_paths_sharing_content_share_one_file_row() {
    // This is what makes a duplicated corpus cost one copy. The corpus this
    // was built against had all 20 PDFs in two directories.
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    let file_id = index
        .store_parsed("same", 10, Some(1), &[chunk(0, "shared", 1, PageOrigin::Native)], "a/x.pdf", 1)
        .unwrap();
    index.link_path(file_id, "b/x.pdf", 1, 10).unwrap();

    assert_eq!(index.path_count().unwrap(), 2);
    assert_eq!(index.chunk_count().unwrap(), 1, "content stored once");
    assert_eq!(index.known_path("b/x.pdf").unwrap().unwrap().file_id, file_id);
}

#[test]
fn a_failure_is_recorded_with_its_message_and_no_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    index
        .store_failure("bad", 5, "malformed document: not a zip", "broken.docx", 1)
        .unwrap();

    assert_eq!(index.chunk_count().unwrap(), 0);
    let known = index.known_path("broken.docx").unwrap().unwrap();
    assert_eq!(known.status, docparse::index::detect::FileStatus::Failed);

    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let err: String = conn
        .query_row("SELECT error FROM files WHERE content_hash='bad'", [], |r| r.get(0))
        .unwrap();
    assert!(err.contains("not a zip"), "the message must be kept: {err}");
}

#[test]
fn pruning_removes_unseen_paths_and_orphaned_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    index
        .store_parsed("h1", 10, Some(1), &[chunk(0, "one", 1, PageOrigin::Native)], "keep.pdf", 1)
        .unwrap();
    index
        .store_parsed("h2", 10, Some(1), &[chunk(0, "two", 1, PageOrigin::Native)], "drop.pdf", 1)
        .unwrap();

    let mut seen = HashSet::new();
    seen.insert("keep.pdf".to_string());

    let (paths_removed, files_removed) = index.prune_unseen(&seen).unwrap();

    assert_eq!(paths_removed, 1);
    assert_eq!(files_removed, 1, "a file with no remaining paths is orphaned");
    assert_eq!(index.chunk_count().unwrap(), 1, "the orphan's chunks cascade away");
    assert!(index.known_path("drop.pdf").unwrap().is_none());
    assert!(index.known_path("keep.pdf").unwrap().is_some());
}

#[test]
fn pruning_one_of_two_shared_paths_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    let file_id = index
        .store_parsed("same", 10, Some(1), &[chunk(0, "shared", 1, PageOrigin::Native)], "a.pdf", 1)
        .unwrap();
    index.link_path(file_id, "b.pdf", 1, 10).unwrap();

    let mut seen = HashSet::new();
    seen.insert("b.pdf".to_string());
    let (paths_removed, files_removed) = index.prune_unseen(&seen).unwrap();

    assert_eq!(paths_removed, 1);
    assert_eq!(files_removed, 0, "one path remains, so the file stays");
    assert_eq!(index.chunk_count().unwrap(), 1);
}

#[test]
fn touch_updates_the_timestamp_without_touching_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    index
        .store_parsed("h", 10, Some(1), &[chunk(0, "text", 1, PageOrigin::Native)], "a.pdf", 1_000)
        .unwrap();
    index.touch_path("a.pdf", 5_000, 10).unwrap();

    assert_eq!(index.known_path("a.pdf").unwrap().unwrap().mtime_ns, 5_000);
    assert_eq!(index.chunk_count().unwrap(), 1);
}

#[test]
fn non_ascii_and_spaced_paths_round_trip_exactly() {
    // Review Focus 1. Real corpora are full of these, and a mangled
    // rel_path means export cannot find the file again.
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    let awkward = "reports/Ürün raporu (2) — final.pdf";
    index
        .store_parsed("h", 10, Some(1), &[chunk(0, "text", 1, PageOrigin::Native)], awkward, 1)
        .unwrap();

    let known = index.known_path(awkward).unwrap();
    assert!(known.is_some(), "the exact path must be findable");

    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let stored: String = conn
        .query_row("SELECT rel_path FROM paths", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, awkward, "stored byte-for-byte, not normalised");
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cd docparse && cargo test -p docparse --test index_store_test
```

Expected: FAIL to compile — the methods do not exist.

- [ ] **Step 3: Write `store.rs`**

```rust
//! The SQLite read and write layer.
//!
//! Every write that changes a file's content goes through one transaction,
//! so an interrupted run leaves stale files rather than torn ones.

use std::collections::HashSet;

use rusqlite::params;

use super::detect::{FileStatus, KnownPath};
use super::Index;
use crate::error::{Error, Result};
use crate::model::{Chunk, PageOrigin};

fn db(detail: impl std::fmt::Display) -> Error {
    Error::Index { detail: detail.to_string() }
}

/// Splits a `PageOrigin` into the two columns the schema stores, so a later
/// query can say `WHERE origin = 'ocr' AND confidence < 0.7`.
fn split_origin(origin: &PageOrigin) -> (&'static str, Option<f64>) {
    match origin {
        PageOrigin::Native => ("native", None),
        PageOrigin::Synthesized => ("synthesized", None),
        PageOrigin::Ocr { confidence } => ("ocr", Some(f64::from(*confidence))),
    }
}

impl Index {
    pub fn known_path(&self, rel_path: &str) -> Result<Option<KnownPath>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT p.file_id, p.byte_size, p.mtime_ns, f.status
                 FROM paths p JOIN files f ON f.id = p.file_id
                 WHERE p.rel_path = ?1",
            )
            .map_err(db)?;

        let mut rows = stmt.query(params![rel_path]).map_err(db)?;
        match rows.next().map_err(db)? {
            None => Ok(None),
            Some(row) => {
                let status: String = row.get(3).map_err(db)?;
                Ok(Some(KnownPath {
                    file_id: row.get(0).map_err(db)?,
                    byte_size: row.get::<_, i64>(1).map_err(db)? as u64,
                    mtime_ns: row.get::<_, i64>(2).map_err(db)? as i128,
                    status: if status == "failed" {
                        FileStatus::Failed
                    } else {
                        FileStatus::Indexed
                    },
                }))
            }
        }
    }

    pub fn hash_owner(&self, content_hash: &str) -> Result<Option<i64>> {
        // `.optional()`, not `.ok()`: an unreadable index must be an error,
        // not an answer of "this content is new" that triggers a re-parse of
        // the whole corpus.
        use rusqlite::OptionalExtension;
        self.conn
            .query_row(
                "SELECT id FROM files WHERE content_hash = ?1",
                params![content_hash],
                |r| r.get(0),
            )
            .optional()
            .map_err(db)
    }

    /// Stores a parsed file and links the path to it, atomically.
    pub fn store_parsed(
        &mut self,
        content_hash: &str,
        byte_size: u64,
        page_count: Option<u32>,
        chunks: &[Chunk],
        rel_path: &str,
        mtime_ns: i128,
    ) -> Result<i64> {
        let now = super::now_iso8601();
        let tx = self.conn.transaction().map_err(db)?;

        tx.execute(
            "INSERT INTO files(content_hash, byte_size, page_count, chunk_count,
                               status, error, indexed_at)
             VALUES(?1, ?2, ?3, ?4, 'indexed', NULL, ?5)
             ON CONFLICT(content_hash) DO UPDATE SET
                 byte_size = excluded.byte_size,
                 page_count = excluded.page_count,
                 chunk_count = excluded.chunk_count,
                 status = 'indexed',
                 error = NULL,
                 indexed_at = excluded.indexed_at",
            params![content_hash, byte_size as i64, page_count, chunks.len() as i64, now],
        )
        .map_err(db)?;

        let file_id: i64 = tx
            .query_row(
                "SELECT id FROM files WHERE content_hash = ?1",
                params![content_hash],
                |r| r.get(0),
            )
            .map_err(db)?;

        // Delete first so the FTS delete trigger fires before the inserts.
        tx.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])
            .map_err(db)?;

        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO chunks(file_id, ordinal, page_start, page_end,
                                        origin, confidence, hard_split, blocks, text)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(db)?;

            for c in chunks {
                let (origin, confidence) = split_origin(&c.origin);
                let blocks = serde_json::to_string(&c.blocks)
                    .map_err(|e| db(format!("cannot serialize blocks: {e}")))?;
                insert
                    .execute(params![
                        file_id,
                        c.id as i64,
                        c.page_start,
                        c.page_end,
                        origin,
                        confidence,
                        i64::from(c.hard_split),
                        blocks,
                        c.text,
                    ])
                    .map_err(db)?;
            }
        }

        upsert_path(&tx, file_id, rel_path, mtime_ns, byte_size, &now)?;
        tx.commit().map_err(db)?;
        Ok(file_id)
    }

    /// Records a parse failure. The message is kept so `status` can show it.
    pub fn store_failure(
        &mut self,
        content_hash: &str,
        byte_size: u64,
        error: &str,
        rel_path: &str,
        mtime_ns: i128,
    ) -> Result<i64> {
        let now = super::now_iso8601();
        let tx = self.conn.transaction().map_err(db)?;

        tx.execute(
            "INSERT INTO files(content_hash, byte_size, page_count, chunk_count,
                               status, error, indexed_at)
             VALUES(?1, ?2, NULL, 0, 'failed', ?3, ?4)
             ON CONFLICT(content_hash) DO UPDATE SET
                 status = 'failed',
                 error = excluded.error,
                 page_count = NULL,
                 chunk_count = 0,
                 indexed_at = excluded.indexed_at",
            params![content_hash, byte_size as i64, error, now],
        )
        .map_err(db)?;

        let file_id: i64 = tx
            .query_row(
                "SELECT id FROM files WHERE content_hash = ?1",
                params![content_hash],
                |r| r.get(0),
            )
            .map_err(db)?;

        tx.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])
            .map_err(db)?;
        upsert_path(&tx, file_id, rel_path, mtime_ns, byte_size, &now)?;
        tx.commit().map_err(db)?;
        Ok(file_id)
    }

    pub fn link_path(
        &mut self,
        file_id: i64,
        rel_path: &str,
        mtime_ns: i128,
        byte_size: u64,
    ) -> Result<()> {
        let now = super::now_iso8601();
        let tx = self.conn.transaction().map_err(db)?;
        upsert_path(&tx, file_id, rel_path, mtime_ns, byte_size, &now)?;
        tx.commit().map_err(db)
    }

    pub fn touch_path(&mut self, rel_path: &str, mtime_ns: i128, byte_size: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE paths SET mtime_ns = ?1, byte_size = ?2, seen_at = ?3
                 WHERE rel_path = ?4",
                params![mtime_ns as i64, byte_size as i64, super::now_iso8601(), rel_path],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Removes paths absent from the walk, then files left with no paths.
    /// Returns (paths removed, files removed).
    pub fn prune_unseen(&mut self, seen: &HashSet<String>) -> Result<(usize, usize)> {
        let all: Vec<String> = self
            .conn
            .prepare("SELECT rel_path FROM paths")
            .map_err(db)?
            .query_map([], |r| r.get(0))
            .map_err(db)?
            .collect::<std::result::Result<_, _>>()
            .map_err(db)?;

        let gone: Vec<&String> = all.iter().filter(|p| !seen.contains(*p)).collect();

        let tx = self.conn.transaction().map_err(db)?;
        for p in &gone {
            tx.execute("DELETE FROM paths WHERE rel_path = ?1", params![p])
                .map_err(db)?;
        }
        // ON DELETE CASCADE removes each orphan's chunks, and the FTS delete
        // trigger keeps the search index in step.
        let files_removed = tx
            .execute(
                "DELETE FROM files WHERE id NOT IN (SELECT file_id FROM paths)",
                [],
            )
            .map_err(db)?;
        tx.commit().map_err(db)?;

        Ok((gone.len(), files_removed))
    }

    pub fn chunk_count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
            .map_err(db)
    }

    pub fn path_count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT count(*) FROM paths", [], |r| r.get(0))
            .map_err(db)
    }
}

fn upsert_path(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    rel_path: &str,
    mtime_ns: i128,
    byte_size: u64,
    now: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO paths(file_id, rel_path, mtime_ns, byte_size, seen_at)
         VALUES(?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(rel_path) DO UPDATE SET
             file_id = excluded.file_id,
             mtime_ns = excluded.mtime_ns,
             byte_size = excluded.byte_size,
             seen_at = excluded.seen_at",
        params![file_id, rel_path, mtime_ns as i64, byte_size as i64, now],
    )
    .map_err(db)?;
    Ok(())
}
```

Add `pub mod store;` to `index/mod.rs`, make `now_iso8601` `pub(super)`, and make `Index`'s `conn` field `pub(super)` so `store.rs` can reach it.

- [ ] **Step 4: Run the tests**

```bash
cd docparse && cargo test -p docparse --test index_store_test
```

Expected: all 11 PASS.

If `the_fts_index_follows_chunk_replacement` fails, the trigger order is the cause: the `DELETE FROM chunks` must precede the inserts so the FTS delete trigger runs first.

- [ ] **Step 5: Verify the transaction actually rolls back**

Add this test, which proves success criterion 4 rather than assuming it:

```rust
#[test]
fn a_failed_insert_leaves_no_partial_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    // Two chunks with the same ordinal violate UNIQUE(file_id, ordinal),
    // so the second insert fails partway through the transaction.
    let bad = vec![
        chunk(0, "first", 1, PageOrigin::Native),
        chunk(0, "duplicate ordinal", 1, PageOrigin::Native),
    ];
    let result = index.store_parsed("h", 10, Some(1), &bad, "a.pdf", 1);

    assert!(result.is_err(), "a duplicate ordinal must be rejected");
    assert_eq!(index.chunk_count().unwrap(), 0, "the first insert must have rolled back");
    assert_eq!(index.path_count().unwrap(), 0);
}
```

Run it. Expected: PASS. This is the single most important test in the task — it demonstrates that dropping the transaction without committing rolls everything back.

- [ ] **Step 6: Lint**

```bash
cd docparse && cargo clippy -p docparse --all-targets --features pdf -- -D warnings && cargo fmt --check
```

---

### Task 5: Config fingerprint and extraction version

**Files:**
- Create: `docparse/docparse/src/index/config.rs`
- Modify: `docparse/docparse/src/index/mod.rs`
- Create: `docparse/docparse/tests/index_config_test.rs`

**Interfaces:**
- Consumes: `ChunkConfig`, `Index`, `EXTRACTION_VERSION`.
- Produces:
  - `pub struct IndexConfig { pub chunk: ChunkConfig, pub ocr: bool, pub ocr_dpi: u32 }`
  - `pub fn IndexConfig::fingerprint(&self) -> String`
  - `pub enum Staleness { Current, ConfigChanged { from: String, to: String }, ExtractionChanged { from: u32, to: u32 }, FirstRun }`
  - `pub fn Index::staleness(&self, config: &IndexConfig) -> Result<Staleness>`
  - `pub fn Index::record_config(&self, config: &IndexConfig) -> Result<()>`

**Rust notes for this task:**

- A **fingerprint** is a hash of the settings that affect output. Comparing one short string is cheaper and less error-prone than comparing six fields, and it extends without touching the comparison when a seventh setting arrives.
- We store the underlying values *as well as* the hash, so `status` can report *which* setting changed rather than only that something did. A hash alone would make the message useless.
- `blake3::hash(bytes).to_hex().to_string()` gives a stable hex digest. Stability matters: the same settings must produce the same fingerprint across runs and machines, so the input is a canonical formatted string rather than anything memory-layout dependent.

- [ ] **Step 1: Write the failing tests**

`docparse/docparse/tests/index_config_test.rs`:

```rust
use docparse::chunk::ChunkConfig;
use docparse::index::{Index, IndexConfig, Staleness};

fn cfg() -> IndexConfig {
    IndexConfig { chunk: ChunkConfig::default(), ocr: false, ocr_dpi: 150 }
}

#[test]
fn the_same_settings_fingerprint_identically() {
    assert_eq!(cfg().fingerprint(), cfg().fingerprint());
}

#[test]
fn every_output_affecting_setting_changes_the_fingerprint() {
    let base = cfg().fingerprint();

    let mut a = cfg();
    a.chunk.target_chars = 500;
    assert_ne!(a.fingerprint(), base, "target_chars must count");

    let mut b = cfg();
    b.chunk.max_chars = 2000;
    assert_ne!(b.fingerprint(), base, "max_chars must count");

    let mut c = cfg();
    c.chunk.overlap_sentences = 3;
    assert_ne!(c.fingerprint(), base, "overlap_sentences must count");

    let mut d = cfg();
    d.chunk.cross_page = true;
    assert_ne!(d.fingerprint(), base, "cross_page must count");

    let mut e = cfg();
    e.ocr = true;
    assert_ne!(e.fingerprint(), base, "ocr must count");

    let mut f = cfg();
    f.ocr_dpi = 300;
    assert_ne!(f.fingerprint(), base, "ocr_dpi must count");
}

#[test]
fn a_brand_new_index_reports_first_run() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(dir.path(), None).unwrap();
    assert_eq!(index.staleness(&cfg()).unwrap(), Staleness::FirstRun);
}

#[test]
fn unchanged_settings_report_current() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(dir.path(), None).unwrap();
    index.record_config(&cfg()).unwrap();
    assert_eq!(index.staleness(&cfg()).unwrap(), Staleness::Current);
}

#[test]
fn a_changed_chunk_size_reports_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(dir.path(), None).unwrap();
    index.record_config(&cfg()).unwrap();

    let mut changed = cfg();
    changed.chunk.target_chars = 400;

    match index.staleness(&changed).unwrap() {
        Staleness::ConfigChanged { from, to } => {
            // The message must name the setting, not just report a hash diff.
            assert!(from.contains("800"), "old value should appear: {from}");
            assert!(to.contains("400"), "new value should appear: {to}");
        }
        other => panic!("expected ConfigChanged, got {other:?}"),
    }
}

#[test]
fn a_bumped_extraction_version_invalidates_everything() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(dir.path(), None).unwrap();
    index.record_config(&cfg()).unwrap();

    // Simulate the real case: a code change altered output for unchanged
    // input, exactly as the segment-merging fix did.
    index.set_meta("extraction_version", "0").unwrap();

    match index.staleness(&cfg()).unwrap() {
        Staleness::ExtractionChanged { from, to } => {
            assert_eq!(from, 0);
            assert_eq!(to, docparse::index::EXTRACTION_VERSION);
        }
        other => panic!("expected ExtractionChanged, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cd docparse && cargo test -p docparse --test index_config_test
```

Expected: FAIL to compile — `IndexConfig` does not exist.

- [ ] **Step 3: Write `config.rs`**

```rust
//! The settings that affect extraction output, and how staleness is decided.

use super::{Index, EXTRACTION_VERSION};
use crate::chunk::ChunkConfig;
use crate::error::Result;

/// Everything that changes what a parse produces. A change to any of these
/// invalidates every stored chunk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexConfig {
    pub chunk: ChunkConfig,
    pub ocr: bool,
    pub ocr_dpi: u32,
}

impl IndexConfig {
    /// A canonical, human-readable rendering of the settings. Stored next to
    /// the hash so `status` can say which setting changed.
    pub fn describe(&self) -> String {
        format!(
            "chunk={} max={} overlap={} cross_page={} ocr={} ocr_dpi={}",
            self.chunk.target_chars,
            self.chunk.max_chars,
            self.chunk.overlap_sentences,
            self.chunk.cross_page,
            self.ocr,
            self.ocr_dpi,
        )
    }

    /// Hash of `describe()`. Formatting the values first keeps the digest
    /// stable across runs and machines.
    pub fn fingerprint(&self) -> String {
        blake3::hash(self.describe().as_bytes()).to_hex().to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    /// Nothing recorded yet.
    FirstRun,
    /// Settings and extraction version both match.
    Current,
    /// A setting that affects output changed; everything must be re-parsed.
    ConfigChanged { from: String, to: String },
    /// Extraction itself changed; everything must be re-parsed.
    ExtractionChanged { from: u32, to: u32 },
}

impl Index {
    /// Decides, before the walk begins, whether every file is already stale.
    pub fn staleness(&self, config: &IndexConfig) -> Result<Staleness> {
        let stored_fp = self.meta("config_fingerprint")?;
        let Some(stored_fp) = stored_fp else {
            return Ok(Staleness::FirstRun);
        };

        // Extraction version is checked first: it invalidates regardless of
        // settings, and reporting it is more informative.
        let stored_ev: u32 = self
            .meta("extraction_version")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(EXTRACTION_VERSION);
        if stored_ev != EXTRACTION_VERSION {
            return Ok(Staleness::ExtractionChanged {
                from: stored_ev,
                to: EXTRACTION_VERSION,
            });
        }

        if stored_fp != config.fingerprint() {
            return Ok(Staleness::ConfigChanged {
                from: self.meta("config_describe")?.unwrap_or(stored_fp),
                to: config.describe(),
            });
        }

        Ok(Staleness::Current)
    }

    pub fn record_config(&self, config: &IndexConfig) -> Result<()> {
        self.set_meta("config_fingerprint", &config.fingerprint())?;
        self.set_meta("config_describe", &config.describe())?;
        self.set_meta("extraction_version", &EXTRACTION_VERSION.to_string())?;
        self.set_meta("updated_at", &super::now_iso8601())?;
        Ok(())
    }
}
```

Add `pub mod config;` to `index/mod.rs` and re-export: `pub use config::{IndexConfig, Staleness};`.

- [ ] **Step 4: Run the tests**

```bash
cd docparse && cargo test -p docparse --test index_config_test
```

Expected: all 6 PASS.

- [ ] **Step 5: Verify by mutation**

Remove `self.ocr_dpi` from `describe()`, run the tests, and confirm `every_output_affecting_setting_changes_the_fingerprint` fails. Revert. That test is the one guarding against a future setting being added to the struct but forgotten in the fingerprint.

- [ ] **Step 6: Lint**

```bash
cd docparse && cargo clippy -p docparse --all-targets --features pdf -- -D warnings && cargo fmt --check
```

---

### Task 6: `docparse index` — the walk

The task that makes the feature real, and the one carrying four of the five Review Focus items.

**Files:**
- Create: `docparse/docparse/src/index/walk.rs`
- Create: `docparse/docparse-cli/src/cmd_index.rs`
- Modify: `docparse/docparse/src/index/mod.rs`, `docparse/docparse-cli/src/main.rs`
- Create: `docparse/docparse/tests/index_cmd_test.rs`

**Interfaces:**
- Consumes: `Index`, `IndexConfig`, `Staleness`, `detect::{decide_cheap, decide_after_hash, Action, Decision}`, the store methods from Task 4, and `stream::{open_source, is_supported}`.
- Produces:
  - `pub struct IndexReport { pub parsed: usize, pub linked: usize, pub touched: usize, pub unchanged: usize, pub failed: usize, pub skipped_failed: usize, pub paths_pruned: usize, pub files_pruned: usize, pub reason: Option<String> }`
  - `pub fn Index::index_dir(&mut self, config: &IndexConfig, opts: &IndexOptions) -> Result<IndexReport>`
  - `pub struct IndexOptions { pub jobs: usize, pub retry_failed: bool, pub reindex: bool }`
  - `pub fn hash_file(path: &Path) -> Result<(String, u64)>` — BLAKE3 digest and byte size, streamed

**Rust notes for this task:**

- `blake3::Hasher` is fed in chunks from a `BufReader` rather than loading the file. A 94 MB PDF must not be read into memory to be hashed — that would undo the project's whole memory story.
- `rusqlite::Connection` is **not** `Sync`, so it cannot be shared across rayon threads. The shape that works: parse in parallel, send results down a channel, write from the single thread that owns the connection. That is the opposite of Task 7's parallelism in the streaming path, and the reason is the database, not preference.
- `walkdir::WalkDir::new(root).follow_links(false)` is the default and the one we want. Following links invites cycles and double-indexing.

**The counts in `IndexReport` are the testable observable** for the spec's success criteria. `parsed == 0` on a re-run *is* criterion 1; `parsed == 1` after one edit *is* criterion 2.

- [ ] **Step 1: Write the failing tests — the behavioural table**

`docparse/docparse/tests/index_cmd_test.rs`. Build fixtures with the same in-code `.docx` writer the existing tests use, so no binary fixtures are needed:

```rust
// Helper: write_docx(dir, name, paragraphs) -> PathBuf, as in docx_test.rs.
// Helper: index(dir) -> IndexReport, opening the index and running one pass
//         with ChunkConfig::default(), jobs=1, retry_failed=false, reindex=false.

#[test] fn first_index_parses_every_file() { /* 3 files -> parsed == 3 */ }

#[test] fn an_immediate_rerun_parses_nothing() {
    // Spec success criterion 1. The headline promise.
    // -> parsed == 0, unchanged == 3
}

#[test] fn editing_one_file_reparses_exactly_that_file() {
    // Criterion 2. -> parsed == 1, unchanged == 2
}

#[test] fn renaming_a_file_parses_nothing_and_moves_the_path() {
    // Criterion 3. -> parsed == 0, linked == 1, paths_pruned == 1
    // and chunk_count is unchanged
}

#[test] fn copying_a_file_parses_nothing_and_shares_one_file_row() {
    // -> parsed == 0, linked == 1, path_count == 4, chunk_count unchanged
}

#[test] fn deleting_a_file_prunes_it_and_leaves_the_others() {
    // -> paths_pruned == 1, files_pruned == 1, the others still indexed
}

#[test] fn touching_mtime_without_editing_parses_nothing() {
    // -> parsed == 0, touched == 1
}

#[test] fn changing_chunk_size_reparses_everything_and_says_why() {
    // -> parsed == 3, report.reason mentions the old and new values
}

#[test] fn a_corrupt_file_is_recorded_and_the_batch_completes() {
    // -> failed == 1, parsed == 2, and the error text is retrievable
}

#[test] fn a_rerun_skips_a_known_failure() {
    // -> parsed == 0, skipped_failed == 1
}

#[test] fn retry_failed_reattempts_it() {
    // -> with retry_failed: true, failed == 1 again (it is still corrupt)
}

#[test] fn reindex_reparses_everything_regardless_of_cache() {
    // -> parsed == 3 on an otherwise unchanged directory
}
```

Write each body out in full when implementing; the comments above state the exact assertion each one makes.

Then the four Review Focus tests this task owns:

```rust
#[test]
fn a_case_only_rename_does_not_create_two_rows() {
    // Review Focus 2. On Windows `Paper.docx` and `paper.docx` are one file,
    // but SQLite's UNIQUE on rel_path is case-sensitive, so a naive
    // implementation records both and double-counts the corpus.
    let dir = tempfile::tempdir().unwrap();
    write_docx(dir.path(), "Paper.docx", &["Content here."]);
    index(dir.path());

    std::fs::rename(dir.path().join("Paper.docx"), dir.path().join("paper.docx")).unwrap();
    let report = index(dir.path());

    // Exactly one path must remain, whichever case it is stored under.
    let idx = Index::open(dir.path(), None).unwrap();
    assert_eq!(idx.path_count().unwrap(), 1, "report: {report:?}");
    assert_eq!(idx.chunk_count().unwrap() > 0, true);
}

#[test]
fn a_symlink_cycle_does_not_hang_the_walk() {
    // Review Focus 3. Requires symlink privileges on Windows; skip cleanly
    // rather than fail, so the suite stays green for unprivileged users.
    let dir = tempfile::tempdir().unwrap();
    write_docx(dir.path(), "real.docx", &["Real content."]);
    let link = dir.path().join("loop");
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(dir.path(), &link).is_ok();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(dir.path(), &link).is_ok();
    if !made {
        eprintln!("skipping: symlink creation not permitted here");
        return;
    }

    let report = index(dir.path());   // must terminate
    assert_eq!(report.parsed, 1, "the link must not be followed: {report:?}");
}

#[test]
fn a_file_modified_during_the_run_is_not_recorded_as_current() {
    // Review Focus 4. The hash is read, then the parse reads again. If those
    // disagree, chunks get stored under the wrong hash and the file looks
    // current forever — the worst outcome available, because no later run
    // ever fixes it. The fix: re-stat after parsing and, if size or mtime
    // moved, record the file as needing another pass rather than as current.
    //
    // Simulated by indexing, then editing the file and setting its mtime
    // back to the stored value, so only a content re-check can catch it.
    // Assert the NEXT run re-parses it.
}

#[test]
fn a_zero_byte_file_is_indexed_with_no_chunks_not_failed() {
    // Review Focus 5. A valid-but-empty file must be recorded as indexed
    // with chunk_count 0, and must not be re-parsed on every subsequent run.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("empty.docx"), b"").unwrap();
    write_docx(dir.path(), "real.docx", &["Real content."]);

    let first = index(dir.path());
    let second = index(dir.path());

    // An empty .docx is not a valid zip, so `failed` is the honest outcome
    // here — what must NOT happen is re-attempting it every run.
    assert_eq!(second.parsed, 0, "second run: {second:?}");
    assert_eq!(second.skipped_failed, first.failed);
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cd docparse && cargo test -p docparse --test index_cmd_test
```

Expected: FAIL to compile — `index_dir` does not exist.

- [ ] **Step 3: Implement `hash_file`**

```rust
/// BLAKE3 digest and byte size, read in blocks.
///
/// Streamed deliberately: hashing a 94 MB PDF must not load it into memory,
/// or the project's bounded-memory property dies here.
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;

    loop {
        let n = reader.read(&mut buf).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }

    Ok((hasher.finalize().to_hex().to_string(), total))
}
```

- [ ] **Step 4: Implement the walk**

The shape, with the reasoning that matters:

1. Check `staleness`. `ConfigChanged`, `ExtractionChanged`, or `reindex` means treat every file as `Parse` and set `report.reason`.
2. Walk with `WalkDir::new(root).follow_links(false).sort_by_file_name()`, keeping files whose extension `is_supported`.
3. For each, compute `rel_path` relative to the root with **forward slashes** on every platform, so an index is portable and `rel_path` comparisons are stable.
4. `decide_cheap`. On `NeedsHash`, call `hash_file`, then `decide_after_hash`.
5. Collect the `Parse` set; parse those with rayon; write results from the owning thread.
6. After the walk, `prune_unseen(&seen)`.
7. `record_config`.

**Case-insensitivity (Review Focus 2):** store `rel_path` exactly as found, but keep the in-run `seen` set and the `known_path` lookup case-insensitively **on Windows only**, where the filesystem makes two casings the same file. Use `#[cfg(windows)]` to lower-case the lookup key; do not do this on Unix, where `Paper.pdf` and `paper.pdf` genuinely are two files. Getting this backwards either double-counts on Windows or merges distinct files on Linux.

**Mid-run modification (Review Focus 4):** after parsing, re-stat the file. If size or mtime differs from what was recorded before the hash, store the file but with the *new* stat values deliberately left unrecorded — so the next run sees a mismatch and re-parses. Never record a stat that was read before the content.

- [ ] **Step 5: Wire `cmd_index.rs`**

Translate `IndexArgs` into `IndexConfig` plus `IndexOptions`, open the index, run, and print one summary line:

```
indexed 20 files: 3 parsed, 1 linked, 16 unchanged, 0 failed, 1 pruned
```

With a reason line above it when everything was invalidated:

```
re-indexing all files: chunk size changed (chunk=800 -> chunk=400)
```

Exit codes: `0` when `failed == 0`, `2` when some failed, `1` when all failed or the index could not be opened.

- [ ] **Step 6: Run the tests**

```bash
cd docparse && cargo test -p docparse --test index_cmd_test && cargo test --workspace --features docparse/pdf
```

Expected: all 16 new tests pass and the 90 existing ones still do.

- [ ] **Step 7: Verify the headline promise on the real corpus**

```bash
cd docparse && cargo build --release
./target/release/docparse index ../corpus
./target/release/docparse index ../corpus
```

Expected: the first run parses 20 files; **the second reports 0 parsed, 20 unchanged** and returns in well under a second. Record both timings in the report — this is the feature's entire value proposition and it should be measured, not assumed.

- [ ] **Step 8: Lint**

```bash
cd docparse && cargo clippy --workspace --all-targets --features docparse/pdf -- -D warnings && cargo fmt --check
```

---

### Task 7: `docparse status`

**Files:**
- Create: `docparse/docparse-cli/src/cmd_status.rs`
- Modify: `docparse/docparse/src/index/mod.rs`, `docparse/docparse-cli/src/main.rs`
- Modify: `docparse/docparse-cli/tests/cli_test.rs`

**Interfaces:**
- Consumes: `Index`, `IndexConfig`, `Staleness`, `detect::{decide_cheap, Decision, Action}`.
- Produces:
  - `pub struct StatusReport { pub unchanged: Vec<String>, pub modified: Vec<String>, pub new_files: Vec<String>, pub deleted: Vec<String>, pub failed: Vec<(String, String)>, pub staleness: Staleness }`
  - `pub fn Index::status(&self, config: &IndexConfig) -> Result<StatusReport>`

**The defining constraint: `status` parses nothing and writes nothing.** It may hash — that is unavoidable to tell "modified" from "touched" honestly — but it must not open a document or mutate the index. A `status` that silently re-indexed would be the same class of surprise as `git status` committing.

- [ ] **Step 1: Write the failing tests**

In `cli_test.rs`:

```rust
#[test]
fn status_reports_each_category() {
    // Index 3 files, then: edit one, delete one, add one.
    // Assert the human output names each under the right heading, and that
    // the modified file is listed as modified rather than new.
}

#[test]
fn status_does_not_modify_the_index() {
    // Capture the index file's bytes before and after `status`, assert equal.
    // This is the test that makes the read-only promise real.
}

#[test]
fn status_json_is_machine_readable() {
    // --json output parses, and carries the same counts as the human form.
}

#[test]
fn status_on_an_unindexed_directory_says_so_without_creating_one() {
    // Exit non-zero with a message naming `docparse index`, and
    // assert `.docparse` was NOT created — asking for status must not
    // silently initialise.
}

#[test]
fn status_reports_a_config_change_without_acting_on_it() {
    // Index with defaults, run status with --chunk 400, assert the output
    // says every file is stale and names both values.
}
```

- [ ] **Step 2: Run to verify failure, then implement**

`status` reuses `decide_cheap` and `decide_after_hash` — the same pure functions as `index`, which is the point of having extracted them. Map each `Action` to a category: `Unchanged` → unchanged, `TouchOnly` → unchanged (content is current), `Parse` → modified or new depending on whether the path was known, `LinkExisting` → new path for known content, `SkipFailed` → failed.

Paths in the index but absent from the walk are `deleted`.

- [ ] **Step 3: Run the tests, then lint**

```bash
cd docparse && cargo test --workspace --features docparse/pdf && cargo clippy --workspace --all-targets --features docparse/pdf -- -D warnings && cargo fmt --check
```

---

### Task 8: `docparse export`, with a lossless round-trip

**Files:**
- Create: `docparse/docparse-cli/src/cmd_export.rs`
- Modify: `docparse/docparse/src/index/mod.rs`, `docparse/docparse-cli/src/main.rs`
- Create: `docparse/docparse/tests/index_export_test.rs`

**Interfaces:**
- Consumes: `Index`, `Chunk`, `PageOrigin`, `BlockRef`.
- Produces: `pub fn Index::export(&self, out: &mut dyn std::io::Write) -> Result<usize>` — writes JSONL, returns the line count.

**The decisive test is the round-trip.** `index` then `export` must produce JSONL byte-identical to what the streaming path produces for the same inputs and settings. If that holds, the storage layer is proven lossless — stronger than any number of field-by-field assertions, because it checks the whole contract at once including field order and float formatting.

- [ ] **Step 1: Write the failing round-trip test**

```rust
#[test]
fn export_reproduces_the_streaming_output_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    // Several documents, including one with multiple pages, so ordering
    // across files and within a file both get exercised.

    // 1. Stream: `docparse <dir>` -> stdout
    // 2. Index then export: `docparse index <dir>` then `docparse export <dir>`
    // 3. assert_eq!(streamed, exported)
}
```

Ordering must be deterministic for this to hold: export orders by `rel_path`, then `ordinal`, matching the streaming path's `sort_by_file_name` walk. State that in a comment, because it is a contract and not an accident.

The `source` field needs care: the streaming path sets it to the file's *name*, so export must reproduce exactly that from `rel_path` rather than emitting the whole relative path.

- [ ] **Step 2: Add the remaining tests**

```rust
#[test] fn export_emits_one_json_object_per_line() {}
#[test] fn export_of_an_empty_index_writes_nothing_and_exits_zero() {}
#[test] fn exported_ocr_chunks_carry_their_confidence() {}
#[test] fn export_to_a_file_leaves_stdout_empty() {}
```

- [ ] **Step 3: Implement, run, lint**

Reassemble `PageOrigin` from the `origin` and `confidence` columns, and `blocks` from its JSON column, then serialize the same `Chunk` struct the streaming path serializes — reusing the type rather than hand-writing JSON is what guarantees the shapes cannot drift.

```bash
cd docparse && cargo test --workspace --features docparse/pdf && cargo clippy --workspace --all-targets --features docparse/pdf -- -D warnings && cargo fmt --check
```

---

### Task 9: Single-writer lock, memory ceiling, and documentation

**Files:**
- Modify: `docparse/docparse/src/index/mod.rs`
- Modify: `docparse/docparse/tests/memory_ceiling.rs`
- Modify: `README.md`, `BENCHMARKS.md`

**Interfaces:**
- Produces: a lock acquired in `Index::open` when opening for write, released on drop.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_second_writer_is_refused_with_a_clear_message() {
    // Hold one write handle, try to open a second, assert Error::Index
    // whose message says another process is indexing.
}

#[test]
fn a_stale_lock_from_a_dead_process_does_not_block_forever() {
    // Write a lock file naming a pid that cannot exist, assert open succeeds.
    // Without this, one crash makes the directory permanently unindexable —
    // worse than the race the lock prevents.
}

#[test]
fn indexing_memory_does_not_grow_with_the_file_count() {
    // In memory_ceiling.rs, under the peak_alloc allocator: index a
    // synthetic directory of 50 then 500 small documents and assert peak
    // heap does not scale with the count. Chunks are written per file and
    // dropped, so the ceiling should hold.
}
```

- [ ] **Step 2: Implement the lock**

A `.docparse/lock` file containing the pid. On open-for-write: if it exists and names a live process, refuse; otherwise take it. Remove it on drop. `status` and `export` open read-only and take no lock, so they work while an index runs.

- [ ] **Step 3: Update the documentation with measured numbers**

Add an Indexing section to `README.md` covering the three subcommands, `.docparse/` layout, `--index-dir`, and that an index belongs to one root. Add the Task 6 Step 7 timings to `BENCHMARKS.md` — first run versus no-op re-run on the 20-PDF corpus — plus the on-disk index size, which the spec flagged as unmeasured.

**Every number must be one you measured.** Do not carry forward a figure from the spec or this plan without re-running it.

- [ ] **Step 4: Full verification**

```bash
cd docparse \
  && cargo test --workspace --features docparse/pdf \
  && cargo test --workspace --features docparse/pdf,docparse/ocr -- --test-threads=1 \
  && cargo clippy --workspace --all-targets --features docparse/pdf -- -D warnings \
  && cargo clippy --workspace --all-targets --features docparse/pdf,docparse/ocr -- -D warnings \
  && cargo fmt --check \
  && cargo tree --features pdf | grep -Ei "reqwest|hyper|ureq|curl" || echo "no network stack: invariant holds"
```

---

## Spec coverage

| Spec requirement | Task |
|---|---|
| Success criterion 1 — re-run parses zero | 3 (`decide_cheap`), 6 (test) |
| Criterion 2 — one edit parses one | 3, 6 |
| Criterion 3 — rename/copy parses zero | 3 (`decide_after_hash`), 6 |
| Criterion 4 — interrupt leaves no torn index | 4 (transaction + rollback test) |
| Criterion 5 — `status` parses nothing | 7 |
| Schema, FTS5, triggers | 2 |
| `meta.root_path`, one index per root | 2 |
| Every refusal explicit | 2, 9 |
| Three-tier change detection | 3, 6 |
| Rename, copy, dedupe | 3, 4, 6 |
| Deletion pruning | 4, 6 |
| `extraction_version` / config fingerprint | 5 |
| Per-file atomicity, WAL | 4 |
| Failure recording, `--retry-failed` | 4, 6 |
| Subcommands, bare streaming preserved | 1 |
| `export` lossless | 8 |
| Single writer | 9 |
| Memory bounded during indexing | 9 |
| `.docparse/.gitignore` | 2 |
