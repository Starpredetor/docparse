//! The incremental index: a SQLite database tracking which files have been
//! parsed, keyed by content hash.

pub mod config;
pub mod detect;
mod schema;
pub mod store;
mod walk;

pub use config::{IndexConfig, Staleness};
pub use schema::{EXTRACTION_VERSION, SCHEMA_VERSION};
pub use walk::{hash_file, IndexOptions, IndexReport, StatusReport};

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use crate::error::{Error, Result};

#[derive(Debug)]
pub struct Index {
    pub(super) conn: Connection,
    pub(super) root: PathBuf,
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
            if let Err(e) = std::fs::write(dir.join(".gitignore"), "*\n") {
                // Not fatal: the index works without it. But silence would hide
                // that the index WILL show up in `git status`.
                eprintln!(
                    "warning: could not write {}: {e}",
                    dir.join(".gitignore").display()
                );
            }
        }

        let db = dir.join("index.db");
        let hint = "Use --index-dir to put it elsewhere.";

        let conn = Connection::open(&db).map_err(|e| Error::Index {
            detail: format!("cannot open {}: {e}. {hint}", db.display()),
        })?;

        // The first read of the file: a corrupt database reveals itself here
        // or at the next statement, and either way it is refused, not rebuilt.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
            .map_err(|e| Error::Index {
                detail: format!(
                    "{} is not a usable database ({e}). Use --reindex to rebuild it.",
                    db.display()
                ),
            })?;

        let index = Index { conn, root };

        // Validate BEFORE the DDL: a refused open must not write to the index.
        let stored_version = index.validate(&db)?;

        // Idempotent (`IF NOT EXISTS`), so on a matching index this only
        // heals missing objects.
        index
            .conn
            .execute_batch(schema::DDL)
            .map_err(|e| Error::Index {
                detail: format!(
                    "cannot initialise schema in {} ({e}). {hint}",
                    dir.display()
                ),
            })?;

        if stored_version.is_none() {
            index.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
            index.set_meta("root_path", &index.root.display().to_string())?;
            index.set_meta("created_at", &now_iso8601())?;
        }

        Ok(index)
    }

    /// Checks that the open database is one this build may use. Reads only.
    /// Returns the stored schema version, `None` for a brand-new database.
    fn validate(&self, db: &Path) -> Result<Option<String>> {
        let index = self;
        let has_meta: bool = index
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta'",
                [],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| Error::Index {
                detail: format!(
                    "{} is not a usable database ({e}). Use --reindex to rebuild it.",
                    db.display()
                ),
            })?
            .unwrap_or(false);

        let stored_version = if has_meta {
            index.meta("schema_version")?
        } else {
            None
        };

        if let Some(found) = &stored_version {
            let found_n: i64 = found.parse().map_err(|_| Error::Index {
                detail: format!(
                    "unparseable schema_version `{found}`. Use --reindex to rebuild it."
                ),
            })?;
            if found_n > SCHEMA_VERSION {
                return Err(Error::Index {
                    detail: format!(
                        "schema version {found_n} is newer than this build understands ({SCHEMA_VERSION}). Upgrade docparse; this index was not modified."
                    ),
                });
            }
            if found_n < SCHEMA_VERSION {
                return Err(Error::Index {
                    detail: format!(
                        "schema version {found_n} predates this build ({SCHEMA_VERSION}) and no migration exists. Use --reindex to rebuild it."
                    ),
                });
            }
            let stored_root = index.meta("root_path")?.unwrap_or_default();
            let this_root = index.root.display().to_string();
            if stored_root != this_root {
                return Err(Error::Index {
                    detail: format!(
                        "this index belongs to root {stored_root}, not {this_root}. An index tracks exactly one directory; use a different --index-dir for this one."
                    ),
                });
            }
        }
        Ok(stored_version)
    }

    /// Opens an existing index read-only. Unlike `open`, it never creates a
    /// directory, an index, or a table: asking about an index must not make one.
    ///
    /// `index.db` and `index.db-wal` are never modified. Caveat: when a
    /// `-wal` exists (a live or crashed writer), SQLite may create or update
    /// `index.db-shm`, its derived shared-memory cache (not index data).
    /// Opening `immutable` there instead would hide uncheckpointed data.
    pub fn open_existing(root: &Path, index_dir: Option<&Path>) -> Result<Index> {
        let root = root.canonicalize().map_err(|source| Error::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let dir = match index_dir {
            Some(d) => d.to_path_buf(),
            None => root.join(".docparse"),
        };
        let db = dir.join("index.db");
        if !db.exists() {
            return Err(Error::Index {
                detail: format!(
                    "no index at {}. Run `docparse index` first (add --index-dir if it lives elsewhere).",
                    db.display()
                ),
            });
        }
        // A WAL-mode database opened even read-only leaves `-wal` and `-shm`
        // files behind, which is a write. When no `-wal` exists the main file
        // is the whole truth, so open it `immutable` and leave nothing. When
        // one exists (a writer is live, or crashed) its contents matter, so
        // take the ordinary read-only path instead of ignoring them.
        let wal = dir.join("index.db-wal");
        let conn = if wal.exists() {
            Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        } else {
            Connection::open_with_flags(
                sqlite_immutable_uri(&db),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
        }
        .map_err(|e| Error::Index {
            detail: format!("cannot open {} read-only: {e}", db.display()),
        })?;
        let index = Index { conn, root };
        if index.validate(&db)?.is_none() {
            return Err(Error::Index {
                detail: format!(
                    "{} holds no index. Run `docparse index` first.",
                    db.display()
                ),
            });
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

pub(super) fn now_iso8601() -> String {
    // No chrono dependency for one timestamp: seconds since the epoch is
    // sortable, unambiguous, and enough for "when was this indexed".
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

/// `file:` URI for `path` with `immutable=1`, percent-encoding what SQLite's
/// URI parser treats specially.
fn sqlite_immutable_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let raw = raw.strip_prefix("//?/").unwrap_or(&raw);
    let mut uri = String::from("file:");
    if !raw.starts_with('/') {
        uri.push('/'); // `C:/x` -> `/C:/x`
    }
    for b in raw.bytes() {
        match b {
            b'%' | b'?' | b'#' | b' ' | 0x80..=0xFF => uri.push_str(&format!("%{b:02X}")),
            _ => uri.push(b as char),
        }
    }
    uri.push_str("?immutable=1");
    uri
}

impl Index {
    /// Writes every indexed chunk as JSONL and returns the line count.
    ///
    /// ORDERING IS A CONTRACT: the walk's per-directory order. The streaming
    /// path walks with `sort_by_file_name`, sorting the entries of EACH
    /// directory and descending depth-first, so a directory's contents sort
    /// where the directory's NAME sorts: `a/x` comes before `a.pdf` because
    /// "a" < "a.pdf". A flat `ORDER BY rel_path` does not implement this
    /// ('.' < '/' puts `a.pdf` first), so paths are ordered here by their
    /// components, then chunks by `ordinal`. Do not replace this with SQL
    /// ordering for convenience.
    ///
    /// A file reachable by several paths (identical content) is emitted once
    /// per path, each with that path's own file name as `source`, exactly as
    /// the streaming path would.
    pub fn export(&self, out: &mut dyn std::io::Write) -> Result<usize> {
        use crate::model::{BlockRef, Chunk, PageOrigin};

        let db = |e: rusqlite::Error| Error::Index {
            detail: format!("cannot read chunks: {e}"),
        };

        // Only paths and ids are held in memory, never chunk text.
        let mut paths: Vec<(String, i64)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT rel_path, file_id FROM paths")
                .map_err(db)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db)?;
            rows.collect::<std::result::Result<_, _>>().map_err(db)?
        };
        paths.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));

        let mut stmt = self
            .conn
            .prepare(
                "SELECT ordinal, page_start, page_end, origin,
                        confidence, hard_split, blocks, text
                 FROM chunks WHERE file_id = ?1 ORDER BY ordinal",
            )
            .map_err(db)?;

        let mut count = 0;
        for (rel_path, file_id) in &paths {
            let mut rows = stmt.query([file_id]).map_err(db)?;
            while let Some(r) = rows.next().map_err(db)? {
                let ordinal: i64 = r.get(0).map_err(db)?;
                let origin: String = r.get(3).map_err(db)?;
                let confidence: Option<f64> = r.get(4).map_err(db)?;
                let blocks: String = r.get(6).map_err(db)?;
                let origin = match (origin.as_str(), confidence) {
                    ("native", _) => PageOrigin::Native,
                    ("synthesized", _) => PageOrigin::Synthesized,
                    ("ocr", Some(c)) => PageOrigin::Ocr {
                        confidence: c as f32,
                    },
                    (other, _) => {
                        return Err(Error::Index {
                            detail: format!("unknown origin {other:?} stored for {rel_path}"),
                        })
                    }
                };
                let blocks: Vec<BlockRef> =
                    serde_json::from_str(&blocks).map_err(|e| Error::Index {
                        detail: format!("corrupt blocks for {rel_path}: {e}"),
                    })?;
                // Streaming sets `source` to the file NAME, not the relative path.
                let source = rel_path.rsplit('/').next().unwrap_or(rel_path).to_string();

                let chunk = Chunk {
                    id: ordinal as u64,
                    source,
                    page_start: r.get(1).map_err(db)?,
                    page_end: r.get(2).map_err(db)?,
                    blocks,
                    text: r.get(7).map_err(db)?,
                    origin,
                    hard_split: r.get::<_, i64>(5).map_err(db)? != 0,
                };
                let line = serde_json::to_string(&chunk).map_err(|e| Error::Index {
                    detail: format!("cannot serialize chunk {ordinal} of {rel_path}: {e}"),
                })?;
                writeln!(out, "{line}").map_err(|source| Error::Io {
                    path: PathBuf::from("<export output>"),
                    source,
                })?;
                count += 1;
            }
        }
        Ok(count)
    }
}
