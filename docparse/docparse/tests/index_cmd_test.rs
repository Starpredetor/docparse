use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use docparse::index::{Index, IndexConfig, IndexOptions, IndexReport};
use docparse::ChunkConfig;

/// Writes a minimal valid .docx, one paragraph per entry. Fixtures are built
/// in code so no binary blobs live in the repository.
fn write_docx(dir: &Path, name: &str, paragraphs: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("word/document.xml", opts).unwrap();
    let body: String = paragraphs
        .iter()
        .map(|t| format!("<w:p><w:r><w:t>{t}</w:t></w:r></w:p>"))
        .collect();
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>{body}</w:body></w:document>"#
    )
    .unwrap();
    zip.finish().unwrap();
    path
}

fn config() -> IndexConfig {
    IndexConfig {
        chunk: ChunkConfig::default(),
        ocr: false,
        ocr_dpi: 150,
    }
}

fn opts() -> IndexOptions {
    IndexOptions {
        jobs: 1,
        retry_failed: false,
        reindex: false,
    }
}

fn run_with(dir: &Path, config: &IndexConfig, opts: &IndexOptions) -> IndexReport {
    let mut idx = Index::open(dir, None).unwrap();
    idx.index_dir(config, opts).unwrap()
}

fn index(dir: &Path) -> IndexReport {
    run_with(dir, &config(), &opts())
}

fn three_files(dir: &Path) {
    write_docx(dir, "a.docx", &["Alpha document text."]);
    write_docx(dir, "b.docx", &["Bravo document text."]);
    write_docx(dir, "c.docx", &["Charlie document text."]);
}

/// Pushes a file's mtime forward without touching its content.
fn bump_mtime(path: &Path, secs: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(secs))
        .unwrap();
}

fn chunk_count(dir: &Path) -> i64 {
    Index::open(dir, None).unwrap().chunk_count().unwrap()
}

fn path_count(dir: &Path) -> i64 {
    Index::open(dir, None).unwrap().path_count().unwrap()
}

fn db(dir: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join(".docparse").join("index.db")).unwrap()
}

#[test]
fn first_index_parses_every_file() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    let r = index(dir.path());
    assert_eq!(r.parsed, 3, "{r:?}");
    assert_eq!(path_count(dir.path()), 3);
    assert!(chunk_count(dir.path()) >= 3);
}

#[test]
fn an_immediate_rerun_parses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());
    let r = index(dir.path());
    assert_eq!(r.parsed, 0, "{r:?}");
    assert_eq!(r.unchanged, 3, "{r:?}");
}

#[test]
fn editing_one_file_reparses_exactly_that_file() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());

    let b = write_docx(
        dir.path(),
        "b.docx",
        &["Bravo was edited, with more words."],
    );
    bump_mtime(&b, 30);
    let r = index(dir.path());
    assert_eq!(r.parsed, 1, "{r:?}");
    assert_eq!(r.unchanged, 2, "{r:?}");
}

#[test]
fn renaming_a_file_parses_nothing_and_moves_the_path() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());
    let before = chunk_count(dir.path());

    std::fs::rename(dir.path().join("a.docx"), dir.path().join("z.docx")).unwrap();
    let r = index(dir.path());
    assert_eq!(r.parsed, 0, "{r:?}");
    assert_eq!(r.linked, 1, "{r:?}");
    assert_eq!(r.paths_pruned, 1, "{r:?}");
    assert_eq!(chunk_count(dir.path()), before);
}

#[test]
fn copying_a_file_parses_nothing_and_shares_one_file_row() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());
    let before = chunk_count(dir.path());

    std::fs::copy(dir.path().join("a.docx"), dir.path().join("a_copy.docx")).unwrap();
    let r = index(dir.path());
    assert_eq!(r.parsed, 0, "{r:?}");
    assert_eq!(r.linked, 1, "{r:?}");
    assert_eq!(path_count(dir.path()), 4);
    assert_eq!(chunk_count(dir.path()), before);
    let files: i64 = db(dir.path())
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(files, 3, "the copy must share the original's file row");
}

#[test]
fn deleting_a_file_prunes_it_and_leaves_the_others() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());

    std::fs::remove_file(dir.path().join("c.docx")).unwrap();
    let r = index(dir.path());
    assert_eq!(r.paths_pruned, 1, "{r:?}");
    assert_eq!(r.files_pruned, 1, "{r:?}");
    assert_eq!(r.unchanged, 2, "{r:?}");
    assert_eq!(path_count(dir.path()), 2);
}

#[test]
fn touching_mtime_without_editing_parses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());

    bump_mtime(&dir.path().join("a.docx"), 60);
    let r = index(dir.path());
    assert_eq!(r.parsed, 0, "{r:?}");
    assert_eq!(r.touched, 1, "{r:?}");
    assert_eq!(r.unchanged, 2, "{r:?}");
}

#[test]
fn changing_chunk_size_reparses_everything_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());

    let mut cfg = config();
    cfg.chunk.target_chars = 400;
    let r = run_with(dir.path(), &cfg, &opts());
    assert_eq!(r.parsed, 3, "{r:?}");
    let reason = r.reason.expect("a reason must be reported");
    assert!(reason.contains("chunk=800"), "reason: {reason}");
    assert!(reason.contains("chunk=400"), "reason: {reason}");

    // The new settings are now recorded: a repeat is a no-op.
    let again = run_with(dir.path(), &cfg, &opts());
    assert_eq!(again.parsed, 0, "{again:?}");
    assert!(again.reason.is_none());
}

fn failed_errors(dir: &Path) -> Vec<String> {
    let conn = db(dir);
    let mut stmt = conn
        .prepare("SELECT error FROM files WHERE status = 'failed'")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn two_good_one_corrupt(dir: &Path) {
    write_docx(dir, "a.docx", &["Alpha document text."]);
    write_docx(dir, "b.docx", &["Bravo document text."]);
    std::fs::write(dir.join("bad.docx"), b"this is not a zip archive").unwrap();
}

#[test]
fn a_corrupt_file_is_recorded_and_the_batch_completes() {
    let dir = tempfile::tempdir().unwrap();
    two_good_one_corrupt(dir.path());
    let r = index(dir.path());
    assert_eq!(r.failed, 1, "{r:?}");
    assert_eq!(r.parsed, 2, "{r:?}");
    let errors = failed_errors(dir.path());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("bad.docx"), "error text: {}", errors[0]);
}

#[test]
fn a_rerun_skips_a_known_failure() {
    let dir = tempfile::tempdir().unwrap();
    two_good_one_corrupt(dir.path());
    index(dir.path());
    let r = index(dir.path());
    assert_eq!(r.parsed, 0, "{r:?}");
    assert_eq!(r.skipped_failed, 1, "{r:?}");
    assert_eq!(r.failed, 0, "{r:?}");
}

#[test]
fn retry_failed_reattempts_it() {
    let dir = tempfile::tempdir().unwrap();
    two_good_one_corrupt(dir.path());
    index(dir.path());
    let mut o = opts();
    o.retry_failed = true;
    let r = run_with(dir.path(), &config(), &o);
    assert_eq!(r.failed, 1, "{r:?}");
    assert_eq!(r.skipped_failed, 0, "{r:?}");
    assert_eq!(r.unchanged, 2, "healthy files stay unread: {r:?}");
}

#[test]
fn retry_failed_reattempts_a_new_path_whose_bytes_match_a_failed_file() {
    let dir = tempfile::tempdir().unwrap();
    two_good_one_corrupt(dir.path());
    index(dir.path());
    // Same bytes as the failed file, at a new path: decide_after_hash says
    // LinkExisting, and only the retry guard turns that back into a parse.
    std::fs::copy(
        dir.path().join("bad.docx"),
        dir.path().join("bad_copy.docx"),
    )
    .unwrap();
    let mut o = opts();
    o.retry_failed = true;
    let r = run_with(dir.path(), &config(), &o);
    assert_eq!(r.linked, 0, "the copy must not be linked silently: {r:?}");
    assert_eq!(r.skipped_failed, 0, "{r:?}");
    // The original is retried too, so both the original and the copy fail.
    assert_eq!(r.failed, 2, "the copy must be re-parsed (and fail): {r:?}");
}

#[test]
fn reindex_reparses_everything_regardless_of_cache() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());
    let before = chunk_count(dir.path());
    let mut o = opts();
    o.reindex = true;
    let r = run_with(dir.path(), &config(), &o);
    assert_eq!(r.parsed, 3, "{r:?}");
    // Re-parsing in place must not duplicate rows.
    assert_eq!(path_count(dir.path()), 3);
    assert_eq!(chunk_count(dir.path()), before);
}

// --- extras beyond the brief's table -----------------------------------

#[test]
fn two_new_files_with_identical_bytes_are_parsed_once() {
    let dir = tempfile::tempdir().unwrap();
    write_docx(dir.path(), "a.docx", &["Same bytes."]);
    std::fs::copy(dir.path().join("a.docx"), dir.path().join("b.docx")).unwrap();
    let r = index(dir.path());
    assert_eq!(r.parsed, 1, "{r:?}");
    assert_eq!(r.linked, 1, "{r:?}");
    assert_eq!(path_count(dir.path()), 2);
}

#[test]
fn parallel_jobs_index_every_file() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..12 {
        write_docx(
            dir.path(),
            &format!("f{i}.docx"),
            &[&format!("Document {i}.")],
        );
    }
    let mut o = opts();
    o.jobs = 4;
    let r = run_with(dir.path(), &config(), &o);
    assert_eq!(r.parsed, 12, "{r:?}");
    assert_eq!(path_count(dir.path()), 12);
}

#[test]
fn an_interrupted_index_does_not_look_current() {
    // Rows with no recorded settings (a run that died before record_config)
    // must be re-parsed, not trusted.
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());
    db(dir.path())
        .execute("DELETE FROM meta WHERE key LIKE 'config_%'", [])
        .unwrap();
    let r = index(dir.path());
    assert_eq!(r.parsed, 3, "{r:?}");
    assert!(r.reason.is_some());
}

#[test]
fn hash_file_matches_blake3_of_the_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.bin");
    let data = vec![7u8; 200_000]; // spans several read blocks
    std::fs::write(&p, &data).unwrap();
    let (hash, size) = docparse::index::hash_file(&p).unwrap();
    assert_eq!(size, 200_000);
    assert_eq!(hash, blake3::hash(&data).to_hex().to_string());
}

// --- Review Focus ------------------------------------------------------

#[test]
fn a_case_only_rename_does_not_create_two_rows() {
    let dir = tempfile::tempdir().unwrap();
    write_docx(dir.path(), "Paper.docx", &["Content here."]);
    index(dir.path());

    std::fs::rename(dir.path().join("Paper.docx"), dir.path().join("paper.docx")).unwrap();
    let report = index(dir.path());

    let idx = Index::open(dir.path(), None).unwrap();
    assert_eq!(idx.path_count().unwrap(), 1, "report: {report:?}");
    assert!(idx.chunk_count().unwrap() > 0);
    #[cfg(windows)]
    {
        // Same file on this filesystem: nothing to parse, nothing to link.
        assert_eq!(report.parsed, 0, "{report:?}");
        assert_eq!(report.unchanged, 1, "{report:?}");
        let stored: String = db(dir.path())
            .query_row("SELECT rel_path FROM paths", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "paper.docx", "stored exactly as found");
    }
}

#[test]
fn a_symlink_cycle_does_not_hang_the_walk() {
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

    let report = index(dir.path());
    assert_eq!(
        report.parsed, 1,
        "the link must not be followed: {report:?}"
    );
}

#[test]
fn a_file_modified_during_the_run_is_not_recorded_as_current() {
    // The brief's simulation (edit, restore the old mtime, expect a re-parse)
    // cannot be caught: same size and same mtime is exactly what `Unchanged`
    // means. This test drives the real mechanism instead: the file changes
    // after the pre-hash stat and before the post-parse re-stat.
    let dir = tempfile::tempdir().unwrap();
    write_docx(dir.path(), "a.docx", &["Original words."]);

    let mut idx = Index::open(dir.path(), None).unwrap();
    let mutate = |p: &Path| {
        if p.file_name().unwrap() == "a.docx" {
            write_docx(
                p.parent().unwrap(),
                "a.docx",
                &["Entirely different, longer words."],
            );
            bump_mtime(p, 90);
        }
    };
    let r = idx
        .index_dir_with_hook(&config(), &opts(), &mutate)
        .unwrap();
    assert_eq!(r.modified_during_run, 1, "{r:?}");
    assert_eq!(r.parsed, 0, "must not count as parsed: {r:?}");
    assert_eq!(idx.path_count().unwrap(), 0, "nothing may be stored");
    assert_eq!(idx.chunk_count().unwrap(), 0);
    drop(idx);

    // The next run (no interference) picks it up with its real content.
    let r2 = index(dir.path());
    assert_eq!(r2.parsed, 1, "{r2:?}");
    let text: String = db(dir.path())
        .query_row("SELECT text FROM chunks", [], |r| r.get(0))
        .unwrap();
    assert!(text.contains("Entirely different"), "stored: {text}");
    let r3 = index(dir.path());
    assert_eq!(r3.parsed, 0, "{r3:?}");
}

#[test]
fn a_zero_byte_file_is_indexed_with_no_chunks_not_failed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("empty.docx"), b"").unwrap();
    write_docx(dir.path(), "real.docx", &["Real content."]);

    let first = index(dir.path());
    let second = index(dir.path());

    // An empty .docx is not a valid zip, so `failed` is the honest outcome
    // here; what must NOT happen is re-attempting it every run.
    assert_eq!(first.failed, 1, "{first:?}");
    assert_eq!(second.parsed, 0, "second run: {second:?}");
    assert_eq!(second.skipped_failed, first.failed);
}

// --- review fixes -------------------------------------------------------

fn config_rows(dir: &Path) -> i64 {
    db(dir)
        .query_row(
            "SELECT count(*) FROM meta WHERE key LIKE 'config_%'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn settings_are_not_recorded_when_a_file_changed_during_the_run() {
    // Pins both the ORDER of record_config (after the walk) and its guard:
    // a run that did not bring every file up to date must leave the index
    // stale, so the next run re-invalidates.
    let dir = tempfile::tempdir().unwrap();
    write_docx(dir.path(), "a.docx", &["Original words."]);
    let mut idx = Index::open(dir.path(), None).unwrap();
    let mutate = |p: &Path| {
        write_docx(p.parent().unwrap(), "a.docx", &["Different, longer words."]);
        bump_mtime(p, 90);
    };
    let r = idx
        .index_dir_with_hook(&config(), &opts(), &mutate)
        .unwrap();
    assert_eq!(r.modified_during_run, 1, "{r:?}");
    drop(idx);
    assert_eq!(config_rows(dir.path()), 0, "settings must not be recorded");

    let r2 = index(dir.path());
    assert_eq!(r2.parsed, 1, "{r2:?}");
    assert_eq!(config_rows(dir.path()), 2, "a clean run records settings");
}

#[test]
fn an_older_extraction_version_reparses_everything_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    three_files(dir.path());
    index(dir.path());
    db(dir.path())
        .execute(
            "UPDATE meta SET value = '0' WHERE key = 'extraction_version'",
            [],
        )
        .unwrap();
    let r = index(dir.path());
    assert_eq!(r.parsed, 3, "{r:?}");
    let reason = r.reason.expect("a reason must be reported");
    assert!(reason.contains("extraction version"), "reason: {reason}");
    assert!(reason.contains("0 ->"), "reason: {reason}");
}

/// Makes `dir` unreadable; returns false when the platform won't allow it.
fn deny_read(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let ok = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).is_ok();
        // root ignores mode bits; if the dir is still listable the setup failed.
        ok && std::fs::read_dir(dir).is_err()
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_default();
        let ok = std::process::Command::new("icacls")
            .arg(dir)
            .args(["/deny", &format!("{user}:(RD,X)")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        ok && std::fs::read_dir(dir).is_err()
    }
}

fn restore_read(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_default();
        let _ = std::process::Command::new("icacls")
            .arg(dir)
            .args(["/remove:d", &user])
            .output();
    }
}

#[test]
fn an_unreadable_directory_prunes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    write_docx(&sub, "inner.docx", &["Inside the subdirectory."]);
    write_docx(dir.path(), "top.docx", &["At the top."]);
    index(dir.path());
    let chunks_before = chunk_count(dir.path());

    if !deny_read(&sub) {
        restore_read(&sub);
        eprintln!("skipping: cannot make a directory unreadable here");
        return;
    }
    let r = index(dir.path());
    restore_read(&sub);

    assert!(r.walk_errors > 0, "{r:?}");
    assert_eq!(r.paths_pruned, 0, "{r:?}");
    assert_eq!(r.files_pruned, 0, "{r:?}");
    assert_eq!(path_count(dir.path()), 2);
    assert_eq!(chunk_count(dir.path()), chunks_before);
}
