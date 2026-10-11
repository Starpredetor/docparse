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
    assert!(
        msg.contains("9999"),
        "message should name the version: {msg}"
    );

    // And the index must be untouched: still 9999, not downgraded.
    let conn = rusqlite::Connection::open(dir.path().join(".docparse/index.db")).unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
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
    assert!(
        msg.contains("root"),
        "message should mention the root: {msg}"
    );
}

#[test]
fn a_corrupt_index_is_refused_rather_than_silently_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".docparse")).unwrap();
    std::fs::write(
        dir.path().join(".docparse/index.db"),
        b"this is not a database",
    )
    .unwrap();

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
    assert!(
        !docs.path().join(".docparse").exists(),
        "must not touch the document dir"
    );
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
    assert!(
        names.len() >= 4,
        "expected fts table plus 3 triggers, got {names:?}"
    );
}

#[test]
fn gitignore_is_written_on_creation_only() {
    let dir = tempfile::tempdir().unwrap();
    Index::open(dir.path(), None).unwrap();
    let path = dir.path().join(".docparse/.gitignore");
    std::fs::write(&path, "custom\n").unwrap();

    Index::open(dir.path(), None).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "custom\n");
}

#[test]
fn a_refused_open_does_not_modify_the_index() {
    let dir = tempfile::tempdir().unwrap();
    {
        let index = Index::open(dir.path(), None).unwrap();
        index.set_meta("schema_version", "9999").unwrap();
    }
    let db = dir.path().join(".docparse/index.db");
    let schema = |conn: &rusqlite::Connection| -> Vec<String> {
        conn.prepare("SELECT name FROM sqlite_master ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    // Drop objects the DDL would recreate, so a write-before-check shows up.
    let before = {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TRIGGER chunks_au; DROP INDEX idx_chunks_file;")
            .unwrap();
        schema(&conn)
    };

    Index::open(dir.path(), None).unwrap_err();

    let conn = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(
        schema(&conn),
        before,
        "refused open must not touch the schema"
    );
}

#[test]
fn an_unparseable_schema_version_is_named_as_such() {
    let dir = tempfile::tempdir().unwrap();
    {
        let index = Index::open(dir.path(), None).unwrap();
        index.set_meta("schema_version", "abc").unwrap();
    }
    let msg = Index::open(dir.path(), None).unwrap_err().to_string();
    assert!(msg.contains("unparseable") && msg.contains("abc"), "{msg}");
    assert!(msg.contains("--reindex"), "{msg}");
}
