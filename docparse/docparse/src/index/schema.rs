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
