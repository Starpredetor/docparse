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

/// Range-checked conversion into SQLite's signed 64-bit integer. A silent
/// `as` cast would wrap an out-of-range value into a plausible wrong one.
fn to_i64<T: TryInto<i64> + Copy + std::fmt::Display>(v: T, what: &str) -> Result<i64> {
    v.try_into()
        .map_err(|_| db(format!("{what} out of range: {v}")))
}

fn db(detail: impl std::fmt::Display) -> Error {
    Error::Index {
        detail: detail.to_string(),
    }
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
                    byte_size: u64::try_from(row.get::<_, i64>(1).map_err(db)?)
                        .map_err(|_| db("negative byte_size in index"))?,
                    mtime_ns: i128::from(row.get::<_, i64>(2).map_err(db)?),
                    status: match status.as_str() {
                        "indexed" => FileStatus::Indexed,
                        "failed" => FileStatus::Failed,
                        other => {
                            return Err(db(format!(
                                "unknown file status `{other}` in index for {rel_path}"
                            )))
                        }
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
            params![
                content_hash,
                to_i64(byte_size, "byte_size")?,
                page_count,
                to_i64(chunks.len(), "chunk count")?,
                now
            ],
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
                        to_i64(c.id, "chunk id")?,
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
            params![content_hash, to_i64(byte_size, "byte_size")?, error, now],
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
                params![
                    to_i64(mtime_ns, &format!("timestamp for {rel_path}"))?,
                    to_i64(byte_size, "byte_size")?,
                    super::now_iso8601(),
                    rel_path
                ],
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
        params![
            file_id,
            rel_path,
            to_i64(mtime_ns, &format!("timestamp for {rel_path}"))?,
            to_i64(byte_size, "byte_size")?,
            now
        ],
    )
    .map_err(db)?;
    Ok(())
}
