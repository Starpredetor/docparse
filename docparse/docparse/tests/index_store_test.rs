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
        chunk(
            1,
            "Second chunk here.",
            2,
            PageOrigin::Ocr { confidence: 0.75 },
        ),
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
    index
        .store_parsed("h", 10, Some(1), &chunks, "a.pdf", 1)
        .unwrap();

    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let rows: Vec<(String, Option<f64>)> = conn
        .prepare("SELECT origin, confidence FROM chunks ORDER BY ordinal")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(rows[0].0, "native");
    assert_eq!(
        rows[0].1, None,
        "a native chunk must not carry a confidence"
    );
    assert_eq!(rows[1].0, "ocr");
    assert!((rows[1].1.unwrap() - 0.5).abs() < 1e-6);
}

#[test]
fn re_storing_a_file_replaces_its_chunks_rather_than_appending() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    index
        .store_parsed(
            "h1",
            10,
            Some(1),
            &[chunk(0, "old text", 1, PageOrigin::Native)],
            "a.pdf",
            1,
        )
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

    assert_eq!(
        index.chunk_count().unwrap(),
        2,
        "old chunks must be gone, not appended to"
    );
    assert_eq!(index.path_count().unwrap(), 1);
}

#[test]
fn the_fts_index_follows_chunk_replacement() {
    // External-content FTS is maintained by triggers; if they are wrong the
    // index silently returns stale text, which Stage 2 would inherit.
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    index
        .store_parsed(
            "h",
            10,
            Some(1),
            &[chunk(0, "aardvark", 1, PageOrigin::Native)],
            "a.pdf",
            1,
        )
        .unwrap();
    index
        .store_parsed(
            "h",
            10,
            Some(1),
            &[chunk(0, "zeppelin", 1, PageOrigin::Native)],
            "a.pdf",
            2,
        )
        .unwrap();

    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'aardvark'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 0, "the replaced text must be gone from the FTS index");

    let hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'zeppelin'",
            [],
            |r| r.get(0),
        )
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
        .store_parsed(
            "same",
            10,
            Some(1),
            &[chunk(0, "shared", 1, PageOrigin::Native)],
            "a/x.pdf",
            1,
        )
        .unwrap();
    index.link_path(file_id, "b/x.pdf", 1, 10).unwrap();

    assert_eq!(index.path_count().unwrap(), 2);
    assert_eq!(index.chunk_count().unwrap(), 1, "content stored once");
    assert_eq!(
        index.known_path("b/x.pdf").unwrap().unwrap().file_id,
        file_id
    );
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
        .query_row(
            "SELECT error FROM files WHERE content_hash='bad'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(err.contains("not a zip"), "the message must be kept: {err}");
}

#[test]
fn pruning_removes_unseen_paths_and_orphaned_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    index
        .store_parsed(
            "h1",
            10,
            Some(1),
            &[chunk(0, "one", 1, PageOrigin::Native)],
            "keep.pdf",
            1,
        )
        .unwrap();
    index
        .store_parsed(
            "h2",
            10,
            Some(1),
            &[chunk(0, "two", 1, PageOrigin::Native)],
            "drop.pdf",
            1,
        )
        .unwrap();

    let mut seen = HashSet::new();
    seen.insert("keep.pdf".to_string());

    let (paths_removed, files_removed) = index.prune_unseen(&seen).unwrap();

    assert_eq!(paths_removed, 1);
    assert_eq!(
        files_removed, 1,
        "a file with no remaining paths is orphaned"
    );
    assert_eq!(
        index.chunk_count().unwrap(),
        1,
        "the orphan's chunks cascade away"
    );
    assert!(index.known_path("drop.pdf").unwrap().is_none());
    assert!(index.known_path("keep.pdf").unwrap().is_some());
}

#[test]
fn pruning_one_of_two_shared_paths_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    let file_id = index
        .store_parsed(
            "same",
            10,
            Some(1),
            &[chunk(0, "shared", 1, PageOrigin::Native)],
            "a.pdf",
            1,
        )
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
        .store_parsed(
            "h",
            10,
            Some(1),
            &[chunk(0, "text", 1, PageOrigin::Native)],
            "a.pdf",
            1_000,
        )
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
        .store_parsed(
            "h",
            10,
            Some(1),
            &[chunk(0, "text", 1, PageOrigin::Native)],
            awkward,
            1,
        )
        .unwrap();

    let known = index.known_path(awkward).unwrap();
    assert!(known.is_some(), "the exact path must be findable");

    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let stored: String = conn
        .query_row("SELECT rel_path FROM paths", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, awkward, "stored byte-for-byte, not normalised");
}

#[test]
fn a_failed_insert_rolls_back_everything_already_written() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    // Three VALID inserts, then a duplicate ordinal. The failure lands after
    // real rows exist, which is what makes the rollback meaningful.
    let bad = vec![
        chunk(0, "first", 1, PageOrigin::Native),
        chunk(1, "second", 1, PageOrigin::Native),
        chunk(2, "third", 1, PageOrigin::Native),
        chunk(1, "duplicate ordinal", 1, PageOrigin::Native),
    ];
    let result = index.store_parsed("h", 10, Some(1), &bad, "a.pdf", 1);

    assert!(result.is_err(), "a duplicate ordinal must be rejected");
    assert_eq!(
        index.chunk_count().unwrap(),
        0,
        "earlier inserts must roll back"
    );
    assert_eq!(index.path_count().unwrap(), 0);

    // The files upsert happened BEFORE the chunks loop, so this is the
    // assertion that actually demonstrates rollback.
    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let files: i64 = conn
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(files, 0, "the files upsert must roll back too");
}

#[test]
fn a_failed_restore_leaves_the_previous_version_intact() {
    // Spec success criterion 4: an interrupted run leaves files stale, not torn.
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    index
        .store_parsed(
            "h",
            10,
            Some(1),
            &[chunk(0, "aardvark", 1, PageOrigin::Native)],
            "a.pdf",
            1_000,
        )
        .unwrap();

    let bad = vec![
        chunk(0, "zeppelin", 1, PageOrigin::Native),
        chunk(0, "duplicate ordinal", 1, PageOrigin::Native),
    ];
    assert!(index
        .store_parsed("h", 10, Some(1), &bad, "a.pdf", 2_000)
        .is_err());

    assert_eq!(index.chunk_count().unwrap(), 1);
    assert_eq!(index.known_path("a.pdf").unwrap().unwrap().mtime_ns, 1_000);

    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let text: String = conn
        .query_row("SELECT text FROM chunks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(text, "aardvark");
    let old: i64 = conn
        .query_row(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'aardvark'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let new: i64 = conn
        .query_row(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'zeppelin'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((old, new), (1, 0), "old FTS hit survives, new text absent");
}

#[test]
fn hash_owner_reports_a_broken_database_rather_than_no_match() {
    // `.ok()` here would turn a corrupt index into "this content is new",
    // silently re-parsing everything. This test is what forbids that.
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(dir.path(), None).unwrap();

    {
        let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
        conn.execute_batch("DROP TABLE files;").unwrap();
    }

    assert!(
        index.hash_owner("any-hash").is_err(),
        "must error, not answer None"
    );
}

#[test]
fn an_out_of_range_timestamp_is_an_error_not_a_wrapped_value() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();

    let wrapping = 5_i128 + (1_i128 << 64);
    let r = index.store_parsed("h", 10, Some(1), &[], "a.pdf", wrapping);
    assert!(r.is_err());
    assert_eq!(index.path_count().unwrap(), 0);
}
