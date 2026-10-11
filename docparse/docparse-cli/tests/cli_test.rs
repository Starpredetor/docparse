use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn binary() -> PathBuf {
    // Cargo builds integration-test dependencies next to the test binary.
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("docparse{}", std::env::consts::EXE_SUFFIX))
}

fn write_docx(dir: &Path, name: &str, paragraphs: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let body: String = paragraphs
        .iter()
        .map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>"))
        .collect();

    zip.start_file("word/document.xml", opts).unwrap();
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

#[test]
fn emits_one_json_object_per_line_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(dir.path(), "a.docx", &["Hello there.", "Second paragraph."]);

    let out = Command::new(binary()).arg(&path).output().unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "both paragraphs fit one chunk: {stdout}");

    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["page_start"], 1);
    assert_eq!(v["source"], "a.docx");
    assert!(v["text"].as_str().unwrap().contains("Hello there."));
    assert!(v["text"].as_str().unwrap().contains("Second paragraph."));
}

#[test]
fn a_corrupt_file_does_not_stop_the_batch_and_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let good = write_docx(dir.path(), "good.docx", &["Valid content here."]);
    let bad = dir.path().join("bad.docx");
    std::fs::write(&bad, b"this is not a zip archive at all").unwrap();

    let out = Command::new(binary())
        .arg(&bad)
        .arg(&good)
        .output()
        .unwrap();

    // Partial failure, not total.
    assert_eq!(out.status.code(), Some(2));

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "the good file must still be processed");
    assert!(lines[0].contains("Valid content here."));

    // Diagnostics never pollute the JSONL stream.
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("bad.docx"), "stderr: {stderr}");
    assert!(
        !stdout.contains("bad.docx"),
        "errors must not appear on stdout"
    );
}

#[test]
fn all_files_failing_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.docx");
    std::fs::write(&bad, b"garbage").unwrap();

    let out = Command::new(binary()).arg(&bad).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn walks_a_directory_and_skips_unsupported_extensions() {
    let dir = tempfile::tempdir().unwrap();
    write_docx(dir.path(), "one.docx", &["First document here."]);
    write_docx(dir.path(), "two.docx", &["Second document here."]);
    std::fs::write(dir.path().join("notes.txt"), b"ignored").unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    write_docx(
        &dir.path().join("nested"),
        "three.docx",
        &["Nested document here."],
    );

    let out = Command::new(binary()).arg(dir.path()).output().unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3, "expected all three docx files: {stdout}");
}

#[test]
fn output_flag_writes_to_a_file_and_leaves_stdout_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(dir.path(), "a.docx", &["Content for the file."]);
    let dest = dir.path().join("out.jsonl");

    let out = Command::new(binary())
        .arg(&path)
        .arg("--output")
        .arg(&dest)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "stdout should be empty with --output"
    );

    let written = std::fs::read_to_string(&dest).unwrap();
    assert!(written.contains("Content for the file."));
}

#[test]
fn emit_blocks_outputs_blocks_instead_of_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(dir.path(), "a.docx", &["First one.", "Second one."]);

    let out = Command::new(binary())
        .arg(&path)
        .arg("--emit")
        .arg("blocks")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "one line per block: {stdout}");

    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["kind"], "paragraph");
    assert_eq!(v["page"], 1);
    assert_eq!(v["id"], 0);
}

#[test]
fn repeated_runs_produce_byte_identical_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(
        dir.path(),
        "a.docx",
        &["Determinism matters here.", "So does this sentence."],
    );

    let first = Command::new(binary()).arg(&path).output().unwrap().stdout;
    let second = Command::new(binary()).arg(&path).output().unwrap().stdout;
    // Non-empty first: without this the test passes when BOTH runs emit
    // nothing, which makes it blind to a binary that produces no output at all.
    assert!(!first.is_empty(), "no output to compare");
    assert_eq!(first, second);
}

#[test]
fn no_inputs_is_an_error_not_a_silent_success() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(binary()).arg(dir.path()).output().unwrap();

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no supported"), "stderr: {stderr}");
}

#[test]
fn jobs_flag_processes_every_file_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..12 {
        write_docx(
            dir.path(),
            &format!("doc{i:02}.docx"),
            &[&format!("Document number {i} content.")],
        );
    }

    let out = Command::new(binary())
        .arg(dir.path())
        .arg("--jobs")
        .arg("4")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 12, "every file exactly once: {stdout}");

    // Every line is still valid JSON: no interleaved partial writes.
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("corrupt line {line:?}: {e}"));
    }

    for i in 0..12 {
        assert!(
            stdout.contains(&format!("Document number {i} content.")),
            "missing document {i}"
        );
    }
}

#[test]
fn a_files_lines_stay_contiguous_under_parallelism() {
    let dir = tempfile::tempdir().unwrap();
    // Each file needs several chunks so interleaving would be detectable.
    for i in 0..6 {
        let body: Vec<String> = (0..30)
            .map(|j| format!("File {i} sentence {j} of filler prose here."))
            .collect();
        let refs: Vec<&str> = body.iter().map(|s| s.as_str()).collect();
        write_docx(dir.path(), &format!("f{i}.docx"), &refs);
    }

    let out = Command::new(binary())
        .arg(dir.path())
        .arg("--jobs")
        .arg("4")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));

    let stdout = String::from_utf8(out.stdout).unwrap();
    let sources: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["source"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();

    // Each source name appears in exactly one unbroken run.
    let mut runs: Vec<&String> = Vec::new();
    for s in &sources {
        if runs.last() != Some(&s) {
            runs.push(s);
        }
    }
    let mut unique = runs.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        runs.len(),
        unique.len(),
        "a file's lines were split apart: {runs:?}"
    );
}

#[test]
fn single_job_output_is_byte_identical_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        write_docx(
            dir.path(),
            &format!("d{i}.docx"),
            &[&format!("Deterministic content {i}.")],
        );
    }

    let run_once = || {
        Command::new(binary())
            .arg(dir.path())
            .arg("--jobs")
            .arg("1")
            .output()
            .unwrap()
            .stdout
    };

    let first = run_once();
    assert!(!first.is_empty(), "expected output, got none");
    assert_eq!(first, run_once());
}

#[test]
fn blocks_mode_carries_the_page_origin() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(dir.path(), "a.docx", &["Hello there."]);
    let out = Command::new(binary())
        .args(["--emit", "blocks"])
        .arg(&path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"origin\":\"synthesized\""),
        "got {stdout}"
    );
}

#[test]
fn blocks_mode_marks_ocr_text_as_ocr_with_confidence() {
    let image =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../docparse/tests/fixtures/hello_ocr.png");
    let out = Command::new(binary())
        .args(["--emit", "blocks"])
        .arg(&image)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.success() {
        // Built with OCR: the guess must be labelled as one.
        assert!(stdout.contains("\"origin\":\"ocr\""), "got {stdout}");
        assert!(stdout.contains("\"confidence\":"), "got {stdout}");
    } else {
        // Built without OCR: it must refuse, not emit empty success.
        assert!(stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("OCR"));
    }
}

#[test]
fn subcommands_are_listed_in_help() {
    let out = Command::new(binary()).arg("--help").output().unwrap();
    let help = String::from_utf8(out.stdout).unwrap();
    for sub in ["status", "export"] {
        assert!(help.contains(sub), "--help should list `{sub}`:\n{help}");
    }
}

#[test]
fn a_file_named_like_a_subcommand_still_streams() {
    // `index` is a subcommand, but `index.pdf` is a filename. clap matches
    // subcommands on the exact word, so this must stream, not index.
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(
        dir.path(),
        "index.docx",
        &["Content in a file named index."],
    );

    let out = Command::new(binary()).arg(&path).output().unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("Content in a file named index."),
        "expected streamed JSONL, got: {stdout}"
    );
    assert!(
        !dir.path().join(".docparse").exists(),
        "must not create an index"
    );
}

#[test]
fn a_subcommand_word_after_a_flag_gets_a_helpful_error() {
    let out = Command::new(binary())
        .args(["--chunk", "400", "index", "."])
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("is a subcommand"), "stderr: {stderr}");
}

// --- docparse status -------------------------------------------------------

fn run_cli(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(binary()).args(args).arg(dir).output().unwrap()
}

fn index_dir_cli(dir: &Path) {
    let out = run_cli(dir, &["index"]);
    assert_eq!(out.status.code(), Some(0), "index failed: {out:?}");
}

fn three(dir: &Path) {
    write_docx(dir, "a.docx", &["Alpha document text."]);
    write_docx(dir, "b.docx", &["Bravo document text."]);
    write_docx(dir, "c.docx", &["Charlie document text."]);
}

fn push_mtime_forward(path: &Path) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
        .unwrap();
}

fn dir_listing(dir: &Path) -> Vec<std::ffi::OsString> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    v.sort();
    v
}

/// The lines under `heading (n):` up to the next unindented line.
fn section_of(stdout: &str, heading: &str) -> Vec<String> {
    let mut lines = stdout.lines();
    let mut found = Vec::new();
    while let Some(l) = lines.next() {
        if l.starts_with(&format!("{heading} (")) {
            for item in lines.by_ref() {
                if item.starts_with("  ") {
                    found.push(item.trim().to_string());
                } else {
                    break;
                }
            }
            break;
        }
    }
    found
}

#[test]
fn status_reports_each_category() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    index_dir_cli(tmp.path());

    write_docx(tmp.path(), "a.docx", &["Alpha, now edited."]); // modified
    std::fs::remove_file(tmp.path().join("b.docx")).unwrap(); // deleted
    write_docx(tmp.path(), "d.docx", &["Delta is brand new."]); // new

    let out = run_cli(tmp.path(), &["status"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(section_of(&stdout, "modified"), ["a.docx"], "{stdout}");
    assert_eq!(section_of(&stdout, "new"), ["d.docx"], "{stdout}");
    assert_eq!(section_of(&stdout, "deleted"), ["b.docx"], "{stdout}");
    assert!(
        stdout.contains("1 unchanged, 1 modified, 1 new, 1 deleted, 0 failed"),
        "{stdout}"
    );
}

#[test]
fn status_does_not_modify_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    index_dir_cli(tmp.path());
    // Give status every reason to write: an edit, a delete, a new file, and
    // a touched file whose content is unchanged.
    write_docx(tmp.path(), "a.docx", &["Alpha, now edited."]);
    std::fs::remove_file(tmp.path().join("b.docx")).unwrap();
    write_docx(tmp.path(), "d.docx", &["Delta is brand new."]);
    push_mtime_forward(&tmp.path().join("c.docx"));

    let store = tmp.path().join(".docparse");
    let db = store.join("index.db");
    let bytes_before = std::fs::read(&db).unwrap();
    let listing_before = dir_listing(&store);

    assert_eq!(run_cli(tmp.path(), &["status"]).status.code(), Some(0));
    // And with a changed setting, which `index` would act on.
    let out = run_cli(tmp.path(), &["status", "--chunk", "400"]);
    assert_eq!(out.status.code(), Some(0));

    assert_eq!(
        std::fs::read(&db).unwrap(),
        bytes_before,
        "status wrote to index.db"
    );
    assert_eq!(
        dir_listing(&store),
        listing_before,
        "status left files behind"
    );
}

#[test]
fn status_json_is_machine_readable() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    index_dir_cli(tmp.path());
    write_docx(tmp.path(), "a.docx", &["Alpha, now edited."]);
    std::fs::remove_file(tmp.path().join("b.docx")).unwrap();
    write_docx(tmp.path(), "d.docx", &["Delta is brand new."]);

    let human = String::from_utf8(run_cli(tmp.path(), &["status"]).stdout).unwrap();
    let out = run_cli(tmp.path(), &["status", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--json output must be one JSON document");

    assert_eq!(v["counts"]["unchanged"], 1);
    assert_eq!(v["counts"]["modified"], 1);
    assert_eq!(v["counts"]["new"], 1);
    assert_eq!(v["counts"]["deleted"], 1);
    assert_eq!(v["counts"]["failed"], 0);
    assert_eq!(v["modified"][0], "a.docx");
    assert_eq!(v["new"][0], "d.docx");
    assert_eq!(v["deleted"][0], "b.docx");
    assert_eq!(v["staleness"]["state"], "current");
    // Same counts as the human form.
    assert!(
        human.contains("1 unchanged, 1 modified, 1 new, 1 deleted, 0 failed"),
        "{human}"
    );
}

#[test]
fn status_on_an_unindexed_directory_says_so_without_creating_one() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());

    let out = run_cli(tmp.path(), &["status"]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("docparse index"), "stderr: {stderr}");
    assert!(
        !tmp.path().join(".docparse").exists(),
        "status must not create an index"
    );

    // Also with --index-dir pointing at a place that does not exist.
    let elsewhere = tmp.path().join("nowhere");
    let out = Command::new(binary())
        .args(["status", "--index-dir"])
        .arg(&elsewhere)
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
    assert!(!elsewhere.exists(), "status created --index-dir");
}

#[test]
fn status_reports_a_config_change_without_acting_on_it() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    index_dir_cli(tmp.path());

    let out = run_cli(tmp.path(), &["status", "--chunk", "400"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("STALE"), "{stdout}");
    assert!(stdout.contains("all 3 indexed files"), "{stdout}");
    assert!(
        stdout.contains("chunk=800"),
        "names the old value: {stdout}"
    );
    assert!(
        stdout.contains("chunk=400"),
        "names the new value: {stdout}"
    );

    // Not acted on: the recorded settings are still the originals.
    let after = String::from_utf8(run_cli(tmp.path(), &["status"]).stdout).unwrap();
    assert!(!after.contains("STALE"), "{after}");
}

#[test]
fn status_reports_an_unchanged_failed_file_as_failed_not_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    write_docx(tmp.path(), "good.docx", &["Fine document text."]);
    std::fs::write(tmp.path().join("bad.docx"), b"this is not a zip").unwrap();
    let out = run_cli(tmp.path(), &["index"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    let summary = "1 unchanged, 0 modified, 0 new, 0 deleted, 1 failed";

    // Untouched since indexing: decide_cheap says SkipFailed.
    let s = String::from_utf8(run_cli(tmp.path(), &["status"]).stdout).unwrap();
    assert_eq!(section_of(&s, "failed").len(), 1, "{s}");
    assert!(s.contains("bad.docx"), "{s}");
    assert!(s.contains(summary), "{s}");

    // Same bytes, newer mtime: NeedsHash, then decide_after_hash says
    // TouchOnly. Still failed, because the content never parsed.
    push_mtime_forward(&tmp.path().join("bad.docx"));
    let s = String::from_utf8(run_cli(tmp.path(), &["status"]).stdout).unwrap();
    assert_eq!(section_of(&s, "failed").len(), 1, "{s}");
    assert!(s.contains(summary), "{s}");
}

#[test]
fn status_and_index_agree_when_an_earlier_run_did_not_finish() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    index_dir_cli(tmp.path());

    // Simulate a killed run: rows exist but the settings were never recorded.
    {
        let conn =
            rusqlite::Connection::open(tmp.path().join(".docparse").join("index.db")).unwrap();
        conn.execute("DELETE FROM meta WHERE key LIKE 'config_%'", [])
            .unwrap();
    }

    let out = run_cli(tmp.path(), &["status"]);
    assert_eq!(out.status.code(), Some(0));
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("STALE"), "{s}");
    assert!(
        s.contains("earlier run did not finish; its settings were never recorded"),
        "{s}"
    );
    assert!(s.contains("all 3 indexed files"), "{s}");

    let j: serde_json::Value =
        serde_json::from_slice(&run_cli(tmp.path(), &["status", "--json"]).stdout).unwrap();
    assert_eq!(j["staleness"]["state"], "first_run_unfinished");

    // And `index` does what status predicted: re-parse all 3.
    let out = run_cli(tmp.path(), &["index", "--jobs", "1"]);
    let idx = String::from_utf8(out.stdout).unwrap();
    assert!(
        idx.contains("earlier run did not finish; its settings were never recorded"),
        "{idx}"
    );
    assert!(idx.contains("3 parsed"), "{idx}");
}

#[test]
fn status_never_modifies_index_db_or_wal_when_a_wal_exists() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    index_dir_cli(tmp.path());
    let store = tmp.path().join(".docparse");
    let db = store.join("index.db");

    // A second connection holding the WAL open keeps `-wal` on disk and puts
    // real uncheckpointed data in it.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
        .unwrap();
    conn.execute(
        "UPDATE meta SET value = value WHERE key = 'schema_version'",
        [],
    )
    .unwrap();
    let wal = store.join("index.db-wal");
    assert!(wal.exists(), "test needs a live -wal");
    let (db_before, wal_before) = (std::fs::read(&db).unwrap(), std::fs::read(&wal).unwrap());

    let out = run_cli(tmp.path(), &["status"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    assert_eq!(std::fs::read(&db).unwrap(), db_before, "index.db changed");
    assert_eq!(std::fs::read(&wal).unwrap(), wal_before, "-wal changed");
    // `-shm` is SQLite's derived cache and may be created or updated here.
    drop(conn);
}

#[test]
fn status_reports_copies_of_healthy_and_failed_files() {
    let tmp = tempfile::tempdir().unwrap();
    write_docx(tmp.path(), "good.docx", &["Fine document text."]);
    std::fs::write(tmp.path().join("bad.docx"), b"this is not a zip").unwrap();
    run_cli(tmp.path(), &["index"]);

    std::fs::copy(
        tmp.path().join("good.docx"),
        tmp.path().join("good_copy.docx"),
    )
    .unwrap();
    std::fs::copy(
        tmp.path().join("bad.docx"),
        tmp.path().join("bad_copy.docx"),
    )
    .unwrap();

    let s = String::from_utf8(run_cli(tmp.path(), &["status"]).stdout).unwrap();
    // A copy of healthy content is a new path (LinkExisting -> new).
    assert_eq!(section_of(&s, "new"), ["good_copy.docx"], "{s}");
    // A copy of failed content is failed, not new.
    let failed = section_of(&s, "failed");
    assert_eq!(failed.len(), 2, "{s}");
    assert!(failed.iter().any(|l| l.starts_with("bad_copy.docx")), "{s}");
}
